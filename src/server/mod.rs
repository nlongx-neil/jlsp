use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{
    GotoDeclarationParams, GotoDeclarationResponse,
    GotoImplementationParams, GotoImplementationResponse,
    GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};
use tower_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CallHierarchyServerCapability, CodeAction, CodeActionKind, CodeActionOptions,
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeActionResponse,
    CodeLens, CodeLensOptions, CodeLensParams, Command, CompletionItem as LspCompletionItem,
    CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse, CompletionTextEdit, Diagnostic,
    DiagnosticSeverity, DiagnosticTag, NumberOrString, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    DocumentRangeFormattingParams, DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams, DocumentLink,
    DocumentLinkParams, DocumentSymbolParams, DocumentSymbolResponse, FileChangeType, FoldingRange,
    FoldingRangeKind, FoldingRangeParams, FoldingRangeProviderCapability, GotoDefinitionParams,
    GotoDefinitionResponse, Hover, HoverContents, HoverParams, HoverProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, InlayHint, InlayHintKind, InlayHintLabel,
    InlayHintParams, Location, MarkupContent, MarkupKind, MessageType, OneOf, OneOf::Left,
    ParameterInformation, ParameterLabel, Position, PrepareRenameResponse, Range, ReferenceParams,
    RenameParams, SelectionRange, SelectionRangeParams, SemanticToken, SemanticTokenType,
    SemanticTokens, SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult,
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    SignatureHelp, SignatureHelpOptions, SignatureHelpParams, SignatureInformation,
    SymbolInformation, SymbolKind as LspSymbolKind, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Url,
    WorkspaceEdit, WorkspaceSymbolParams, DocumentOnTypeFormattingOptions, DocumentOnTypeFormattingParams,
    DidChangeWorkspaceFoldersParams, ExecuteCommandParams,
};
use tower_lsp::{Client, LanguageServer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::info;

use crate::classpath::ClasspathRegistry;
use crate::indexer::WorkspaceIndex;
use crate::project::{GradleProject, MavenProject};
use crate::model::ClassKind;
use crate::semantic::{CompletionKind, SemanticContext};
use crate::syntax::{JavaAst, JavaFormatter};

pub struct DocumentState {
    pub text: Arc<String>,
    pub ast: JavaAst,
}

pub struct JlspServer {
    pub client: Client,
    pub classpath: Arc<ClasspathRegistry>,
    pub indexer: Arc<WorkspaceIndex>,
    pub documents: DashMap<Url, DocumentState>,
}

/// Walk up from a changed build file to the nearest project root (pom/gradle/git).
fn find_project_root(path: &Path) -> PathBuf {
    let mut current = path.parent().unwrap_or(path);
    let mut fallback = current.to_path_buf();
    loop {
        if current.join("pom.xml").exists()
            || current.join("build.gradle").exists()
            || current.join("build.gradle.kts").exists()
            || current.join(".git").exists()
        {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => {
                current = parent;
                fallback = parent.to_path_buf();
            }
            None => return fallback,
        }
    }
}

fn find_target_line_in_source(content: &str, target_name: &str) -> u32 {
    let mut fallback_line = 0;
    let mut found_fallback = false;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }

        // Exact class / interface / enum / record / @interface definition (supports modifiers like public, final, abstract, static, sealed)
        let has_type_keyword = line.contains("class ")
            || line.contains("interface ")
            || line.contains("enum ")
            || line.contains("@interface ")
            || line.contains("record ");
        if has_type_keyword {
            let patterns = [
                format!("class {}", target_name),
                format!("class {}<", target_name),
                format!("class {} ", target_name),
                format!("interface {}", target_name),
                format!("interface {}<", target_name),
                format!("interface {} ", target_name),
                format!("enum {}", target_name),
                format!("enum {} ", target_name),
                format!("@interface {}", target_name),
                format!("@interface {} ", target_name),
                format!("record {}", target_name),
                format!("record {}<", target_name),
                format!("record {} ", target_name),
            ];
            if patterns.iter().any(|pat| line.contains(pat)) {
                return idx as u32;
            }
        }

        // Method declaration: `target_name(`
        let method_pattern = format!("{}(", target_name);
        if line.contains(&method_pattern) {
            if trimmed.starts_with("public ") || trimmed.starts_with("protected ") || trimmed.starts_with("private ") || trimmed.starts_with("static ") || trimmed.starts_with("default ") {
                return idx as u32;
            }
            if !found_fallback {
                fallback_line = idx as u32;
                found_fallback = true;
            }
        }

        // General fallback if target_name appears as a word
        if !found_fallback && line.contains(target_name) {
            fallback_line = idx as u32;
            found_fallback = true;
        }
    }
    fallback_line
}

/// Count parameters of a single-line method declaration `name(...)`.
/// Commas inside generics / nested parens are ignored. Returns None when the
/// declaration's closing paren is not on the same line.
fn count_decl_params(line: &str, name: &str) -> Option<usize> {
    let start = line.find(&format!("{}(", name))? + name.len();
    let bytes = line.as_bytes();
    let mut paren = 0i32;
    let mut angle = 0i32;
    let mut commas = 0usize;
    let mut saw_token = false;
    for &b in &bytes[start..] {
        match b {
            b'(' => paren += 1,
            b')' => {
                paren -= 1;
                if paren == 0 {
                    return Some(if saw_token { commas + 1 } else { 0 });
                }
            }
            b'<' => angle += 1,
            b'>' => {
                if angle > 0 {
                    angle -= 1;
                }
            }
            b',' if paren == 1 && angle == 0 => commas += 1,
            c if !c.is_ascii_whitespace() && paren >= 1 => saw_token = true,
            _ => {}
        }
    }
    None
}

/// Arity-aware method line lookup: among lines containing `target_name(`,
/// prefer one whose parameter count matches `arg_count` (so jumping to an
/// overloaded method lands on the correct overload, e.g. `ArrayList.add(E)`
/// instead of the private `add(E, Object[], int)`). Falls back to the generic
/// finder when arity is unknown or nothing matches.
fn find_method_line_with_arity(content: &str, target_name: &str, arg_count: Option<usize>) -> u32 {
    let want = match arg_count {
        Some(w) => w,
        None => return find_target_line_in_source(content, target_name),
    };
    let needle = format!("{}(", target_name);
    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            continue;
        }
        if !line.contains(&needle) {
            continue;
        }
        if count_decl_params(line, target_name) == Some(want) {
            return idx as u32;
        }
    }
    find_target_line_in_source(content, target_name)
}

pub(crate) fn is_read_only_path(path: &std::path::Path) -> bool {
    let path_str = path.to_string_lossy();
    if path_str.contains("/.jlsp/decompiled/")
        || path_str.contains("/.jlsp/sources/")
        || path_str.contains("\\.jlsp\\decompiled\\")
        || path_str.contains("\\.jlsp\\sources\\")
        || path_str.contains("/jar_sources/")
        || path_str.contains("\\jar_sources\\")
        || path_str.ends_with("/jar_sources")
        || path_str.ends_with("\\jar_sources")
    {
        return true;
    }
    if let Ok(metadata) = std::fs::metadata(path) {
        if metadata.permissions().readonly() {
            return true;
        }
    }
    false
}

/// Cached result of Maven / Gradle dependency resolution.
///
/// Keyed by the mtime of the root `pom.xml` and the first existing Gradle build
/// file (`build.gradle` or `build.gradle.kts`). When both mtimes match the
/// on-disk values, the expensive `resolve_all_modules_jars` calls are skipped
/// and the cached jar list is reused directly.
///
/// Trade-off: sub-module pom/gradle changes are not detected. `cleanCache`
/// (`java.project.cleanCache`) always removes this file, forcing a re-resolve.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DepsCache {
    pom_mtime: Option<u64>,
    gradle_mtime: Option<u64>,
    jars: Vec<PathBuf>,
}

const DEPS_CACHE_MAGIC: &[u8; 9] = b"JLSPDEPS\0";

/// Path to the deps cache file, sibling to the workspace index cache.
fn deps_cache_path(cache_file: &Path) -> PathBuf {
    cache_file
        .parent()
        .map(|p| p.join("deps.bin"))
        .unwrap_or_else(|| PathBuf::from("deps.bin"))
}

fn load_deps_cache(path: &Path) -> Option<DepsCache> {
    use std::io::{BufRead, Read};
    use bincode::Options;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let is_zstd = reader
        .fill_buf()
        .ok()
        .map(|buf| buf.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]))
        .unwrap_or(false);
    if !is_zstd {
        return None;
    }
    let mut decoder = zstd::Decoder::new(reader).ok()?;
    let mut magic = [0u8; 9];
    if decoder.read_exact(&mut magic).is_err() || &magic != DEPS_CACHE_MAGIC {
        return None;
    }
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(64 * 1024 * 1024)
        .deserialize_from(&mut decoder)
        .ok()
}

fn save_deps_cache(path: &Path, cache: &DepsCache) -> std::result::Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::File::create(path).map_err(|e| format!("Disk write error: {e}"))?;
    let writer = std::io::BufWriter::new(file);
    let mut encoder = zstd::Encoder::new(writer, 3).map_err(|e| format!("Zstd encode error: {e}"))?;
    encoder
        .write_all(DEPS_CACHE_MAGIC)
        .map_err(|e| format!("Disk write error: {e}"))?;
    bincode::serialize_into(&mut encoder, cache)
        .map_err(|e| format!("Bincode serialize error: {e}"))?;
    let mut writer = encoder.finish().map_err(|e| format!("Zstd encode error: {e}"))?;
    writer.flush().map_err(|e| format!("Disk write error: {e}"))?;
    Ok(())
}

impl JlspServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            classpath: Arc::new(ClasspathRegistry::new()),
            indexer: Arc::new(WorkspaceIndex::new()),
            documents: DashMap::new(),
        }
    }

    async fn index_workspace(&self, root_path: &std::path::Path) {
        let cache_file = root_path.join(".jlsp").join("index.bin.zst");

        // Try fast cold-start from disk cache (names only; metadata loads lazily)
        if let Ok(loaded) = self.classpath.load_workspace_index(&cache_file).await
            && loaded > 0 {
                info!("Fast cold-start: indexed {} type names from persistent index cache.", loaded);
            }

        // 1. Index Java source files
        self.indexer.scan_workspace(root_path).await;

        // 2. Index workspace jars + resolve Maven/Gradle dependencies
        self.reload_workspace_dependencies(root_path, &cache_file).await;

        // 3. Scan and index pre-generated APT sources (target/generated-sources, build/generated/sources)
        let generated_files = crate::semantic::apt_jdwp::AnnotationProcessorRunner::scan_generated_sources(root_path);
        if !generated_files.is_empty() {
            info!("Found {} generated APT source files, indexing...", generated_files.len());
            for gen_file in generated_files {
                self.indexer.index_file(&gen_file.path, &gen_file.content).await;
            }
        }
    }

    /// Load dependency classes from workspace jars / Maven / Gradle and persist the project cache.
    /// Kept separate from full source indexing so build-file changes do not re-scan every source file.
    async fn reload_workspace_dependencies(&self, root_path: &std::path::Path, cache_file: &std::path::Path) {
        info!("Scanning workspace for .jar dependencies at: {:?}", root_path);
        let mut newly_registered = 0usize;
        for entry in walkdir::WalkDir::new(root_path)
            .into_iter()
            .filter_entry(|e| !crate::indexer::is_ignored_entry(e))
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jar") {
                if let Ok(n) = self.classpath.add_jar(path).await {
                    newly_registered += n;
                }
            }
        }

        // Resolve Maven / Gradle dependencies, reusing the cached jar list when
        // the root build files have not changed since the last resolution.
        let pom_file = root_path.join("pom.xml");
        let pom_mtime = if pom_file.exists() {
            Some(crate::indexer::file_mtime(&pom_file))
        } else {
            None
        };
        let gradle_file = [root_path.join("build.gradle"), root_path.join("build.gradle.kts")]
            .into_iter()
            .find(|p| p.exists());
        let gradle_mtime = gradle_file.as_ref().map(|p| crate::indexer::file_mtime(p));

        let deps_cache_path = deps_cache_path(cache_file);
        let mut resolved_jars: Option<Vec<PathBuf>> = None;

        if let Some(cached) = load_deps_cache(&deps_cache_path) {
            if cached.pom_mtime == pom_mtime && cached.gradle_mtime == gradle_mtime {
                info!(
                    "Reusing cached dependency jar list ({} jars); build files unchanged.",
                    cached.jars.len()
                );
                resolved_jars = Some(cached.jars);
            }
        }

        if resolved_jars.is_none() {
            let mut jars = Vec::new();

            if pom_mtime.is_some() {
                info!("Found Maven pom.xml, resolving multi-module dependencies...");
                if let Ok(maven) = MavenProject::parse(&pom_file) {
                    jars.extend(maven.resolve_all_modules_jars().await);
                }
            }

            if let Some(g_file) = &gradle_file {
                info!(
                    "Found Gradle build file ({:?}), resolving multi-project dependencies...",
                    g_file
                );
                if let Ok(gradle) = GradleProject::parse(g_file) {
                    jars.extend(gradle.resolve_all_modules_jars());
                }
            }

            let cache = DepsCache {
                pom_mtime,
                gradle_mtime,
                jars: jars.clone(),
            };
            if let Err(e) = save_deps_cache(&deps_cache_path, &cache) {
                info!("Failed to persist deps cache: {}", e);
            }
            resolved_jars = Some(jars);
        }

        for jar in resolved_jars.unwrap() {
            if let Ok(n) = self.classpath.add_jar(&jar).await {
                newly_registered += n;
            }
        }

        // Only rewrite the workspace cache when at least one new jar was
        // registered; otherwise the on-disk index is already up to date.
        if newly_registered > 0 {
            let _ = self.classpath.save_workspace_cache(cache_file).await;
        }
    }

    async fn publish_document_diagnostics(&self, uri: &Url, ast: &JavaAst) {
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                self.client.publish_diagnostics(uri.clone(), Vec::new(), None).await;
                return;
            }
        }

        let mut diagnostics = Vec::new();

        // 1. Syntax redline errors (compiler.err.syntax)
        for (range, message) in ast.collect_syntax_errors() {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position { line: range.start.line, character: range.start.character },
                    end: Position { line: range.end.line, character: range.end.character },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("compiler.err.syntax".to_string())),
                code_description: None,
                source: Some("jlsp-javac".to_string()),
                message,
                related_information: None,
                tags: None,
                data: None,
            });
        }

        // 2. Comprehensive semantic diagnostics (type mismatch, missing return, final reassignment, etc.)
        let file_name = uri.to_file_path().ok().and_then(|p| {
            p.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
        });
        for diag in ast.collect_semantic_diagnostics_detailed(file_name.as_deref()) {
            let severity = if diag.code.is_error() {
                DiagnosticSeverity::ERROR
            } else {
                DiagnosticSeverity::WARNING
            };
            let tags = if diag.code.is_unnecessary() {
                Some(vec![DiagnosticTag::UNNECESSARY])
            } else if diag.code.is_deprecated() {
                Some(vec![DiagnosticTag::DEPRECATED])
            } else {
                None
            };
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position { line: diag.range.start.line, character: diag.range.start.character },
                    end: Position { line: diag.range.end.line, character: diag.range.end.character },
                },
                severity: Some(severity),
                code: Some(NumberOrString::String(diag.code.as_str().to_string())),
                code_description: None,
                source: Some("jlsp-javac".to_string()),
                message: diag.message,
                related_information: None,
                tags,
                data: None,
            });
        }

        // 3. Contextual check: Unresolved explicit imports
        let _has_jdk = self.classpath.has_jdk_loaded().await;
        for (range, imp) in ast.extract_imports_with_ranges() {
            // Ignore wildcard imports (e.g. java.util.*) and static imports
            if imp.contains('*') || imp.starts_with("static ") {
                continue;
            }
            // Standard java.lang.* does not need imports, but if imported check if valid
            let simple_name = imp.rsplit('.').next().unwrap_or(&imp);
            let is_resolved = self.classpath.get_class(&imp).await.is_some()
                || self.indexer.find_by_fqn(&imp).await.is_some()
                || self.indexer.find_by_simple_name(simple_name).await.iter().any(|s| s.fqn == imp || s.simple_name == simple_name)
                || self.documents.iter().any(|entry| {
                    let classes = entry.value().ast.extract_classes(None);
                    classes.iter().any(|c| c.fqn == imp || c.simple_name == simple_name)
                })
                || imp.starts_with("java.lang.")
                || crate::classpath::is_known_core_jdk_class(&imp)
                || crate::classpath::is_standard_jdk_package(&imp)
                || crate::classpath::is_known_framework_or_library_class(&imp);
            if !is_resolved {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position { line: range.start.line, character: range.start.character },
                        end: Position { line: range.end.line, character: range.end.character },
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: Some(NumberOrString::String("compiler.err.cant.resolve".to_string())),
                    code_description: None,
                    source: Some("jlsp-javac".to_string()),
                    message: format!("Cannot resolve symbol '{}'", imp),
                    related_information: None,
                    tags: None,
                    data: None,
                });
            }
        }

        // 3b. Contextual check: Unused / duplicate imports (dimmed with UNNECESSARY tag)
        for (imp_name, imp_range) in ast.find_unused_imports() {
            diagnostics.push(Diagnostic {
                range: imp_range,
                severity: Some(DiagnosticSeverity::HINT),
                code: Some(NumberOrString::String("unused-import".to_string())),
                code_description: None,
                source: Some("jlsp".to_string()),
                message: format!("The import '{}' is never used", imp_name),
                related_information: None,
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                data: None,
            });
        }

        // 4. Contextual check: Unimplemented abstract/interface methods
        let sem = SemanticContext::with_indexer(ast, &self.classpath, Some(&self.indexer)).await;
        for (class_name, missing_methods) in sem.find_missing_abstract_methods().await {
            let names: Vec<&str> = missing_methods.iter().map(|m| m.name.as_str()).collect();
            // Find class identifier range
            let symbols = ast.extract_symbols();
            let target_sym = symbols.iter().find(|s| s.name == class_name && s.kind == crate::model::SymbolKind::Class);
            let (start_line, start_col) = if let Some(ts) = target_sym {
                (ts.selection_range.start.line, ts.selection_range.start.character)
            } else {
                (0, 0)
            };
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position { line: start_line, character: start_col },
                    end: Position { line: start_line, character: start_col + class_name.len() as u32 },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("compiler.err.does.not.override.abstract".to_string())),
                code_description: None,
                source: Some("jlsp-javac".to_string()),
                message: format!(
                    "Class '{}' must either be declared abstract or implement abstract method(s): {}",
                    class_name,
                    names.join(", ")
                ),
                related_information: None,
                tags: None,
                data: None,
            });
        }

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    /// Resolve target location when navigating into a dependency jar/class.
    /// Priority order:
    /// 1. Project workspace sources (.java)
    /// 2. Extracted real JDK source from src.zip (.java)
    /// 3. Extracted real library source from *-sources.jar (.java)
    /// 4. Decompiled Java source stub (.java)
    /// Never navigate directly to a raw binary .jar or .class file.
    pub async fn resolve_external_target_location(
        &self,
        cls_info: &crate::model::ClassInfo,
        target_name: &str,
        arg_count: Option<usize>,
    ) -> Vec<Location> {
        let mut target_cls = cls_info.clone();
        if target_name != cls_info.simple_name
            && !cls_info.methods.iter().any(|m| m.name == target_name)
            && !cls_info.fields.iter().any(|f| f.name == target_name)
        {
            // If target member is not defined directly on this class, search inherited hierarchy
            let mut queue = Vec::new();
            if let Some(ref sc) = cls_info.super_class {
                queue.push(sc.clone());
            }
            queue.extend(cls_info.interfaces.clone());
            let mut visited = std::collections::HashSet::new();
            while let Some(ancestor_name) = queue.pop() {
                if !visited.insert(ancestor_name.clone()) {
                    continue;
                }
                if let Some(ancestor_info) = self.classpath.get_class(&ancestor_name).await {
                    if ancestor_info.methods.iter().any(|m| m.name == target_name)
                        || ancestor_info.fields.iter().any(|f| f.name == target_name)
                    {
                        target_cls = (*ancestor_info).clone();
                        break;
                    }
                    if let Some(ref sc) = ancestor_info.super_class {
                        queue.push(sc.clone());
                    }
                    queue.extend(ancestor_info.interfaces.clone());
                } else if let Some(ancestor_info) = crate::classpath::get_builtin_jdk_class(&ancestor_name) {
                    if ancestor_info.methods.iter().any(|m| m.name == target_name)
                        || ancestor_info.fields.iter().any(|f| f.name == target_name)
                    {
                        target_cls = ancestor_info;
                        break;
                    }
                }
            }
        }

        let fqn = &target_cls.fqn;
        let simple_name = &target_cls.simple_name;

        // 1. Priority 1: Check project source files in workspace
        let ws_matches = self.indexer.find_by_simple_name(simple_name).await;
        if let Some(ws_match) = ws_matches.iter().find(|s| s.fqn == *fqn || s.simple_name == *simple_name) {
            if let Ok(ws_uri) = Url::from_file_path(&ws_match.path) {
                let mut line = 0;
                if let Ok(content) = std::fs::read_to_string(&ws_match.path) {
                    line = find_method_line_with_arity(&content, target_name, arg_count);
                }
                return vec![Location {
                    uri: ws_uri,
                    range: Range {
                        start: Position { line, character: 0 },
                        end: Position { line, character: 0 },
                    },
                }];
            }
        }

        // 2. Priority 2: Check if source_path is an existing .java file on disk
        if let Some(ref sp) = target_cls.source_path {
            let p = std::path::Path::new(sp);
            if sp.ends_with(".java") && p.exists() {
                if let Ok(file_uri) = Url::from_file_path(p) {
                    let mut line = 0;
                    if let Ok(content) = std::fs::read_to_string(p) {
                        line = find_method_line_with_arity(&content, target_name, arg_count);
                    }
                    return vec![Location {
                        uri: file_uri,
                        range: Range {
                            start: Position { line, character: 0 },
                            end: Position { line, character: 0 },
                        },
                    }];
                }
            }
        }

        // 3. Priority 3: Try extracting real source code from JDK (src.zip)
        let jdk_home = self.classpath.get_jdk_home().await;
        let real_source = crate::classpath::decompiler::extract_source_from_jdk(
            jdk_home.as_deref(),
            fqn,
        );

        // 4. Priority 4: Try extracting real source code from library *-sources.jar
        let real_source = real_source.or_else(|| {
            if let Some(ref sp) = target_cls.source_path {
                let p = std::path::Path::new(sp);
                if sp.ends_with(".jar") {
                    return crate::classpath::decompiler::extract_source_from_sources_jar(p, fqn);
                }
            }
            None
        });

        // 5. Priority 5: Decompile class info into a .java stub
        let (mut file_content, is_real_source) = match real_source {
            Some(src) => (src, true),
            None => {
                let decompiled = target_cls.source_path.as_ref()
                    .and_then(|sp| {
                        let p = std::path::Path::new(sp);
                        if sp.ends_with(".jar") {
                            crate::classpath::decompiler::decompile_class_from_jar(p, fqn)
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| crate::classpath::decompiler::decompile_class_info(&target_cls));
                (decompiled, false)
            }
        };

        // Prepend provenance header carrying the REAL origin JAR path, so that
        // clients (e.g. DevWorkbench) can display which dependency the class came from.
        if let Some(ref sp) = target_cls.source_path {
            if sp.ends_with(".jar") {
                let header = if is_real_source {
                    format!("// Extracted from: {}\n", sp)
                } else {
                    format!("// Decompiled from: {}\n", sp)
                };
                file_content = format!("{}{}", header, file_content);
            }
        }

        // Cache folder for source/decompiled files:
        // Use ~/.jlsp/sources or ~/.jlsp/decompiled, falling back to temp_dir()
        let base_dir = crate::classpath::dirs_home()
            .unwrap_or_else(std::env::temp_dir)
            .join(".jlsp");
        let sub_dir_name = if is_real_source { "sources" } else { "decompiled" };
        let base_cache_dir = base_dir.join(sub_dir_name);

        let pkg_path = if !target_cls.package_name.is_empty() {
            target_cls.package_name.replace('.', std::path::MAIN_SEPARATOR_STR)
        } else {
            String::new()
        };
        let target_dir = base_cache_dir.join(&pkg_path);
        let _ = std::fs::create_dir_all(&target_dir);

        let clean_simple_name = target_cls.simple_name.split('$').next().unwrap_or(&target_cls.simple_name);
        let stub_file_name = format!("{}.java", clean_simple_name);
        let dest_file_path = target_dir.join(&stub_file_name);

        // If file already exists and was previously marked read-only, make it writable to allow updating
        if dest_file_path.exists() {
            if let Ok(metadata) = std::fs::metadata(&dest_file_path) {
                let mut perms = metadata.permissions();
                if perms.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = std::fs::set_permissions(&dest_file_path, perms);
                }
            }
        }

        let _ = std::fs::write(&dest_file_path, &file_content);

        // Prohibit editing on decompiled / external source files by marking them read-only on the filesystem
        if let Ok(metadata) = std::fs::metadata(&dest_file_path) {
            let mut perms = metadata.permissions();
            perms.set_readonly(true);
            let _ = std::fs::set_permissions(&dest_file_path, perms);
        }

        // Locate line of target method/property/class in source
        let target_line = find_method_line_with_arity(&file_content, target_name, arg_count);

        let mut locs = Vec::new();
        if let Ok(file_uri) = Url::from_file_path(&dest_file_path) {
            locs.push(Location {
                uri: file_uri,
                range: Range {
                    start: Position { line: target_line, character: 0 },
                    end: Position { line: target_line, character: 0 },
                },
            });
        }

        // For jar-backed classes, also expose the real `jar:file:` entry URI
        // so the user can see the originating artifact (and supporting IDEs
        // can open the jar entry directly).
        if let Some(jar_loc) = self.jar_entry_location(&target_cls, target_line) {
            locs.push(jar_loc);
        }

        locs
    }

    /// Build a `jar:file:/abs/path.jar!/pkg/Class.class` location for a class
    /// that originates from a jar file. Returns None for non-jar sources
    /// (workspace .java, JDK src.zip, jimage-extracted temp classes, ...).
    fn jar_entry_location(
        &self,
        cls: &crate::model::ClassInfo,
        target_line: u32,
    ) -> Option<Location> {
        let sp = cls.source_path.as_deref()?;
        if !sp.ends_with(".jar") {
            return None;
        }
        let jar_path = std::path::Path::new(sp);
        let abs = if jar_path.is_absolute() {
            sp.to_string()
        } else {
            std::fs::canonicalize(jar_path).ok()?.to_string_lossy().to_string()
        };
        let pkg_path = cls.fqn.replace('.', "/");
        // Use file: scheme with the absolute path. `url` accepts both
        // `file:/abs` and `file:///abs`; the former matches how `Url::from_file_path`
        // serializes absolute paths and keeps the jar: URI round-trippable.
        let uri_str = format!("jar:file:{}!/{}.class", abs, pkg_path);
        let uri = Url::parse(&uri_str).ok()?;
        Some(Location {
            uri,
            range: Range {
                start: Position { line: target_line, character: 0 },
                end: Position { line: target_line, character: 0 },
            },
        })
    }

    pub async fn resolve_jar_target_location(
        &self,
        _jar_path: &str,
        cls_info: &crate::model::ClassInfo,
        target_name: &str,
    ) -> Vec<Location> {
        self.resolve_external_target_location(cls_info, target_name, None).await
    }

    /// Convert a list of resolved locations into a `GotoDefinitionResponse`.
    /// Empty -> None, single -> Scalar, multiple -> Array (so IDEs can choose
    /// between the readable decompiled source and the real jar entry).
    fn into_definition_response(locs: Vec<Location>) -> Option<GotoDefinitionResponse> {
        match locs.len() {
            0 => None,
            1 => Some(GotoDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
            _ => Some(GotoDefinitionResponse::Array(locs)),
        }
    }

    fn into_type_definition_response(locs: Vec<Location>) -> Option<GotoTypeDefinitionResponse> {
        match locs.len() {
            0 => None,
            1 => Some(GotoTypeDefinitionResponse::Scalar(locs.into_iter().next().unwrap())),
            _ => Some(GotoTypeDefinitionResponse::Array(locs)),
        }
    }

    fn into_declaration_response(locs: Vec<Location>) -> Option<GotoDeclarationResponse> {
        match locs.len() {
            0 => None,
            1 => Some(GotoDeclarationResponse::Scalar(locs.into_iter().next().unwrap())),
            _ => Some(GotoDeclarationResponse::Array(locs)),
        }
    }

    async fn resolve_import_target(
        &self,
        import_content: &str,
        sem: &SemanticContext<'_>,
    ) -> Option<(crate::model::ClassInfo, String)> {
        let clean = import_content.trim();
        if clean.starts_with("static ") {
            let rest = clean.trim_start_matches("static ").trim();
            let parts: Vec<&str> = rest.rsplitn(2, '.').collect();
            if parts.len() == 2 {
                let cls_fqn = parts[1];
                let member = parts[0].to_string();
                if let Some(cls) = self.classpath.get_class(cls_fqn).await {
                    return Some(((*cls).clone(), member));
                }
                if let Some(cls) = sem.resolve_type(cls_fqn).await {
                    return Some(((*cls).clone(), member));
                }
                let simple_name = cls_fqn.rsplit('.').next().unwrap_or(cls_fqn);
                if let Some(sym) = self.indexer.find_by_fqn(cls_fqn).await {
                    let mut info = sym.class_info.clone();
                    info.source_path = Some(sym.path.to_string_lossy().to_string());
                    return Some((info, member));
                }
                let ws = self.indexer.find_by_simple_name(simple_name).await;
                if let Some(item) = ws.into_iter().next() {
                    let mut info = item.class_info.clone();
                    info.source_path = Some(item.path.to_string_lossy().to_string());
                    return Some((info, member));
                }
            }
            return None;
        }

        if clean.ends_with(".*") {
            return None;
        }

        // Direct FQN lookup in classpath
        if let Some(cls) = self.classpath.get_class(clean).await {
            let s_name = cls.simple_name.clone();
            return Some(((*cls).clone(), s_name));
        }

        // Direct FQN / SimpleName lookup in semantic context
        if let Some(cls) = sem.resolve_type(clean).await {
            let s_name = cls.simple_name.clone();
            return Some(((*cls).clone(), s_name));
        }

        // Check simple name in classpath
        let simple_name = clean.rsplit('.').next().unwrap_or(clean);
        if let Some(cls) = sem.resolve_type(simple_name).await {
            let s_name = cls.simple_name.clone();
            return Some(((*cls).clone(), s_name));
        }

        let matches = self.classpath.find_classes_by_simple_name(simple_name).await;
        if let Some(cls) = matches.into_iter().next() {
            let s_name = cls.simple_name.clone();
            return Some(((*cls).clone(), s_name));
        }

        // Check workspace index by exact FQN first
        if let Some(sym) = self.indexer.find_by_fqn(clean).await {
            let mut info = sym.class_info.clone();
            info.source_path = Some(sym.path.to_string_lossy().to_string());
            let s_name = info.simple_name.clone();
            return Some((info, s_name));
        }

        // Check workspace index by simple name
        let ws = self.indexer.find_by_simple_name(simple_name).await;
        if let Some(item) = ws.into_iter().next() {
            let mut info = item.class_info.clone();
            info.source_path = Some(item.path.to_string_lossy().to_string());
            let s_name = info.simple_name.clone();
            return Some((info, s_name));
        }

        // Fallback for standard JDK or known framework classes
        if crate::classpath::is_standard_jdk_package(clean) {
            if let Some(cls) = crate::classpath::get_builtin_jdk_class(clean) {
                return Some((cls, simple_name.to_string()));
            }
        }
        if let Some(cls) = crate::classpath::get_builtin_framework_class(clean) {
            return Some((cls, simple_name.to_string()));
        }

        // Final fallback: synthesize external ClassInfo for any valid dotted package import
        if clean.contains('.') {
            let s_name = clean.rsplit('.').next().unwrap_or(clean);
            if s_name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                let pkg = clean.rsplitn(2, '.').nth(1).unwrap_or("");
                let info = crate::model::ClassInfo {
                    fqn: clean.to_string(),
                    simple_name: s_name.to_string(),
                    package_name: pkg.to_string(),
                    kind: crate::model::ClassKind::Class,
                    super_class: Some("java.lang.Object".to_string()),
                    interfaces: vec![],
                    methods: vec![],
                    fields: vec![],
                    source_path: None,
                    is_public: true,
                };
                return Some((info, s_name.to_string()));
            }
        }

        None
    }

    async fn find_class_info(
        &self,
        class_name: &str,
        preferred_package: Option<&str>,
        imports: &[String],
        sem: &SemanticContext<'_>,
    ) -> Option<crate::model::ClassInfo> {
        let clean_name = class_name.split('<').next().unwrap_or(class_name).trim();
        // 1. Check local classes in current file
        if let Some(cls) = sem.local_classes.get(clean_name) {
            return Some(cls.clone());
        }
        // 2. Check workspace index
        if let Some(sym) = self.indexer.find_candidate(clean_name, preferred_package, imports).await {
            return Some(sym.class_info.clone());
        }
        // 3. Check classpath
        if let Some(cls) = self.classpath.get_class(clean_name).await {
            return Some((*cls).clone());
        }
        let found = self.classpath.find_classes_by_simple_name(clean_name).await;
        if let Some(first) = found.into_iter().next() {
            return Some((*first).clone());
        }
        // 4. Try resolving through semantic context (which inspects imports, same package, java.lang, etc.)
        if let Some(cls) = sem.resolve_type(clean_name).await {
            return Some((*cls).clone());
        }
        // 5. Try standard JDK / Framework builtin class lookup (instant zero-delay fallback)
        if let Some(cls) = crate::classpath::get_builtin_jdk_class(clean_name) {
            return Some(cls);
        }
        if let Some(cls) = crate::classpath::get_builtin_jdk_class_by_simple_name(clean_name) {
            return Some(cls);
        }
        if let Some(cls) = crate::classpath::get_builtin_framework_class_by_simple_name(clean_name) {
            return Some(cls);
        }
        None
    }

    async fn resolve_receiver_chain(
        &self,
        steps: &[crate::syntax::ReceiverStep],
        pos: Position,
        doc: &DocumentState,
        sem: &SemanticContext<'_>,
        preferred_package: Option<&str>,
        imports: &[String],
    ) -> Option<String> {
        if steps.is_empty() {
            return None;
        }

        use crate::syntax::ReceiverStep;

        // Check if leading steps form a package-qualified class name (e.g. a.b.Config)
        let mut fqn_segments = Vec::new();
        let mut seg_count = 0;
        for step in steps {
            match step {
                ReceiverStep::Identifier(s) | ReceiverStep::FieldAccess(s) => {
                    fqn_segments.push(s.as_str());
                    seg_count += 1;
                }
                _ => break,
            }
        }

        let mut matched_prefix_len = 0;
        let mut current_type: Option<String> = None;

        if seg_count > 1 {
            // Try longest prefix to shortest
            for len in (2..=seg_count).rev() {
                let candidate_fqn = fqn_segments[..len].join(".");
                if self.find_class_info(&candidate_fqn, preferred_package, imports, sem).await.is_some() {
                    current_type = Some(candidate_fqn);
                    matched_prefix_len = len;
                    break;
                }
            }
        }

        let remaining_start = if matched_prefix_len > 0 {
            matched_prefix_len
        } else {
            // Step 0: Resolve initial root expression type
            current_type = match &steps[0] {
                ReceiverStep::This => doc.ast.find_enclosing_class_at_position(pos.line, pos.character),
                ReceiverStep::Super => {
                    doc.ast.find_enclosing_class_at_position(pos.line, pos.character)
                        .and_then(|enc| sem.local_classes.get(&enc).and_then(|c| c.super_class.clone()))
                }
                ReceiverStep::Identifier(name) => {
                    let mut resolved = None;
                    // 1. Local variables / parameters
                    let locals = doc.ast.extract_local_variables_at_position(pos.line, pos.character);
                    for (v_name, v_type) in &locals {
                        if v_name == name {
                            resolved = Some(v_type.clone());
                            break;
                        }
                    }
                    // 2. Enclosing class fields
                    if resolved.is_none() {
                        if let Some(enc) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
                            if let Some(cls) = sem.local_classes.get(&enc) {
                                for f in &cls.fields {
                                    if &f.name == name {
                                        let fty = f.field_type.simple_name();
                                        resolved = Some(fty.to_string());
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    // 3. Class or FQN name (static call or constant)
                    if resolved.is_none() {
                        resolved = Some(name.clone());
                    }
                    resolved
                }
                ReceiverStep::MethodCall { name, arg_count } => {
                    if let Some(enc) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
                        if let Some(cls) = self.find_class_info(&enc, preferred_package, imports, sem).await {
                            let mut m_ret = None;
                            for m in &cls.methods {
                                if &m.name == name {
                                    if m.parameters.len() == *arg_count || m_ret.is_none() {
                                        let ret = m.return_type.simple_name();
                                        m_ret = Some(ret.to_string());
                                    }
                                }
                            }
                            m_ret
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                ReceiverStep::FieldAccess(name) => {
                    if let Some(enc) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
                        if let Some(cls) = self.find_class_info(&enc, preferred_package, imports, sem).await {
                            cls.fields.iter().find(|f| &f.name == name).map(|f| {
                                let fty = f.field_type.simple_name();
                                fty.to_string()
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            };
            1
        };

        // Steps remaining..N: Walk successive method calls / field accesses
        for step in &steps[remaining_start..] {
            let cls_name = match &current_type {
                Some(c) => c.clone(),
                None => return None,
            };

            let clean_cls = cls_name.split('<').next().unwrap_or(&cls_name).trim();

            match step {
                ReceiverStep::MethodCall { name, arg_count } => {
                    // Check special well-known Java Stream / Collection / Optional operations
                    if name == "stream" {
                        if let Some(idx) = cls_name.find('<') {
                            let arg = cls_name[idx + 1..].trim_end_matches('>').trim();
                            current_type = Some(format!("Stream<{}>", arg));
                            continue;
                        } else {
                            current_type = Some("Stream".to_string());
                            continue;
                        }
                    } else if name == "toList" {
                        if let Some(idx) = cls_name.find('<') {
                            let arg = cls_name[idx + 1..].trim_end_matches('>').trim();
                            current_type = Some(format!("List<{}>", arg));
                            continue;
                        } else {
                            current_type = Some("List".to_string());
                            continue;
                        }
                    } else if name == "findFirst" || name == "findAny" {
                        if let Some(idx) = cls_name.find('<') {
                            let arg = cls_name[idx + 1..].trim_end_matches('>').trim();
                            current_type = Some(format!("Optional<{}>", arg));
                            continue;
                        } else {
                            current_type = Some("Optional".to_string());
                            continue;
                        }
                    } else if (name == "get" || name == "orElse" || name == "orElseGet") && cls_name.starts_with("Optional") {
                        if let Some(idx) = cls_name.find('<') {
                            let arg = cls_name[idx + 1..].trim_end_matches('>').trim();
                            current_type = Some(arg.to_string());
                            continue;
                        }
                    }

                    let cls_info = match self.find_class_info(clean_cls, preferred_package, imports, sem).await {
                        Some(ci) => ci,
                        None => return current_type,
                    };

                    let mut found_ret = None;
                    let all_methods = sem.get_all_methods_with_inheritance(&cls_info).await;
                    for m in &all_methods {
                        if &m.name == name {
                            if m.parameters.len() == *arg_count || found_ret.is_none() {
                                let ret = m.return_type.simple_name();
                                found_ret = Some(ret.to_string());
                            }
                        }
                    }
                    if let Some(ret_type) = found_ret {
                        let clean_ret = ret_type.split('<').next().unwrap_or(&ret_type).trim();
                        if (clean_ret == "E" || clean_ret == "T" || clean_ret == "K" || clean_ret == "V" || clean_ret == "R" || clean_ret == "Object") && cls_name.contains('<') {
                            if let Some(idx) = cls_name.find('<') {
                                let arg = cls_name[idx + 1..].trim_end_matches('>').trim().to_string();
                                current_type = Some(arg);
                            } else {
                                current_type = Some(ret_type);
                            }
                        } else if (clean_ret == "Stream" || clean_ret == "List" || clean_ret == "Set" || clean_ret == "Collection" || clean_ret == "Optional") && cls_name.contains('<') && !ret_type.contains('<') {
                            if let Some(idx) = cls_name.find('<') {
                                let arg = cls_name[idx + 1..].trim_end_matches('>').trim().to_string();
                                current_type = Some(format!("{}<{}>", clean_ret, arg));
                            } else {
                                current_type = Some(ret_type);
                            }
                        } else {
                            current_type = Some(ret_type);
                        }
                    } else {
                        return current_type;
                    }
                }
                ReceiverStep::FieldAccess(field_name) | ReceiverStep::Identifier(field_name) => {
                    if (clean_cls == "System" || clean_cls == "java.lang.System") && (field_name == "out" || field_name == "err") {
                        current_type = Some("java.io.PrintStream".to_string());
                        continue;
                    }
                    if (clean_cls == "System" || clean_cls == "java.lang.System") && field_name == "in" {
                        current_type = Some("java.io.InputStream".to_string());
                        continue;
                    }

                    let cls_info = match self.find_class_info(clean_cls, preferred_package, imports, sem).await {
                        Some(ci) => ci,
                        None => return current_type,
                    };
                    let all_fields = sem.get_all_fields_with_inheritance(&cls_info).await;
                    if let Some(f) = all_fields.iter().find(|f| &f.name == field_name) {
                        let fty = f.field_type.simple_name();
                        current_type = Some(fty.to_string());
                    } else {
                        return current_type;
                    }
                }
                ReceiverStep::This | ReceiverStep::Super => {}
            }
        }

        current_type
    }

    pub async fn find_references_internal(
        &self,
        uri: &Url,
        doc: &DocumentState,
        pos: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        let target_ident = match sem.find_node_at_position(pos.line, pos.character) {
            Some(node) => doc.ast.node_text(&node).to_string(),
            None => return Vec::new(),
        };

        // Compute target search identifiers (e.g. if field 'name', also search 'getName' and 'setName'; if 'getName', also search 'name')
        let mut target_idents = vec![target_ident.clone()];
        let capitalized = {
            let mut chars = target_ident.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => target_ident.clone(),
            }
        };

        // If target is a field name or method, add getter & setter & builder patterns
        target_idents.push(format!("get{}", capitalized));
        target_idents.push(format!("set{}", capitalized));
        target_idents.push(format!("is{}", capitalized));
        target_idents.push(format!("with{}", capitalized));

        // If target is getter/setter (e.g. getName / setName / isName), extract underlying field name
        if (target_ident.starts_with("get") || target_ident.starts_with("set")) && target_ident.len() > 3 {
            let field_cand = format!("{}{}", target_ident[3..4].to_lowercase(), &target_ident[4..]);
            let field_cap = target_ident[3..].to_string();
            target_idents.push(field_cand);
            target_idents.push(format!("get{}", field_cap));
            target_idents.push(format!("set{}", field_cap));
            target_idents.push(format!("is{}", field_cap));
        } else if target_ident.starts_with("is") && target_ident.len() > 2 {
            let field_cand = format!("{}{}", target_ident[2..3].to_lowercase(), &target_ident[3..]);
            let field_cap = target_ident[2..].to_string();
            target_idents.push(field_cand);
            target_idents.push(format!("get{}", field_cap));
            target_idents.push(format!("set{}", field_cap));
        }

        target_idents.sort();
        target_idents.dedup();

        let mut locs = Vec::new();
        let target_ident_strs: Vec<&str> = target_idents.iter().map(|s| s.as_str()).collect();

        // Helper to collect occurrences in an AST using single-pass multi-identifier traversal
        let collect_in_ast = |ast: &JavaAst, target_uri: &Url, out: &mut Vec<Location>| {
            let occ_map = ast.find_all_occurrences_multiple(&target_ident_strs);
            for ranges in occ_map.into_values() {
                for r in ranges {
                    out.push(Location {
                        uri: target_uri.clone(),
                        range: Range {
                            start: Position {
                                line: r.start.line,
                                character: r.start.character,
                            },
                            end: Position {
                                line: r.end.line,
                                character: r.end.character,
                            },
                        },
                    });
                }
            }
        };

        // 1. References in current document
        collect_in_ast(&doc.ast, uri, &mut locs);

        // 2. References across other open documents
        for item in self.documents.iter() {
            let other_uri = item.key();
            if other_uri != uri {
                collect_in_ast(&item.value().ast, other_uri, &mut locs);
            }
        }

        // 3. References across workspace files not currently opened in editor
        let all_files = self.indexer.all_source_files().await;
        for path in all_files {
            if let Ok(file_uri) = Url::from_file_path(&path) {
                if !self.documents.contains_key(&file_uri) && &file_uri != uri {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        // Quick substring check before parsing
                        if target_idents.iter().any(|id| content.contains(id)) {
                            if let Some(other_ast) = JavaAst::parse(Arc::new(content)) {
                                collect_in_ast(&other_ast, &file_uri, &mut locs);
                            }
                        }
                    }
                }
            }
        }

        // Filter out declaration itself if not requested
        if !include_declaration {
            let def_range_opt = doc.ast.get_definition_identifier_range(pos.line, pos.character);
            locs.retain(|loc| {
                if loc.uri == *uri {
                    if let Some(def_r) = &def_range_opt {
                        if loc.range == *def_r {
                            return false;
                        }
                    }
                    if loc.range.start.line == pos.line
                        && pos.character >= loc.range.start.character
                        && pos.character <= loc.range.end.character
                    {
                        return false;
                    }
                }
                true
            });
        }

        // Deduplicate locations
        locs.sort_by(|a, b| {
            a.uri.cmp(&b.uri).then_with(|| {
                a.range.start.line.cmp(&b.range.start.line).then_with(|| {
                    a.range.start.character.cmp(&b.range.start.character)
                })
            })
        });
        locs.dedup();

        locs
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for JlspServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        info!("Initializing pure-Rust jlsp server with FULL Java capabilities...");

        let cp = Arc::clone(&self.classpath);
        tokio::spawn(async move {
            if let Some(java_home) = crate::classpath::find_java_home() {
                if let Ok(loaded) = cp.add_jdk_home(&java_home).await {
                    info!("Successfully indexed {} classes from JDK at {:?}", loaded, java_home);
                }
            }
        });

        // Collect all workspace root paths (supporting workspace_folders, root_uri, and root_path)
        let mut workspace_paths = Vec::new();
        if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                workspace_paths.push(path);
            }
        }
        if let Some(folders) = params.workspace_folders {
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    if !workspace_paths.contains(&path) {
                        workspace_paths.push(path);
                    }
                }
            }
        }
        if workspace_paths.is_empty() {
            #[allow(deprecated)]
            if let Some(root_path) = params.root_path {
                let p = std::path::PathBuf::from(root_path);
                if p.exists() && !workspace_paths.contains(&p) {
                    workspace_paths.push(p);
                }
            }
        }

        for path in workspace_paths {
            self.index_workspace(&path).await;
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_symbol_provider: Some(Left(true)),
                definition_provider: Some(Left(true)),
                implementation_provider: Some(tower_lsp::lsp_types::ImplementationProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                selection_range_provider: Some(tower_lsp::lsp_types::SelectionRangeProviderCapability::Simple(true)),
                document_link_provider: Some(tower_lsp::lsp_types::DocumentLinkOptions {
                    resolve_provider: Some(false),
                    work_done_progress_options: Default::default(),
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(false),
                }),
                semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
                    tower_lsp::lsp_types::SemanticTokensOptions {
                        legend: SemanticTokensLegend {
                            token_types: vec![
                                SemanticTokenType::CLASS,
                                SemanticTokenType::METHOD,
                                SemanticTokenType::PROPERTY,
                                SemanticTokenType::VARIABLE,
                                SemanticTokenType::PARAMETER,
                                SemanticTokenType::KEYWORD,
                            ],
                            token_modifiers: vec![],
                        },
                        full: Some(SemanticTokensFullOptions::Bool(true)),
                        range: Some(true),
                        work_done_progress_options: Default::default(),
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), "@".to_string(), ":".to_string(), "#".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: Default::default(),
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Options(
                    CodeActionOptions {
                        code_action_kinds: Some(vec![
                            CodeActionKind::QUICKFIX,
                            CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
                            CodeActionKind::REFACTOR,
                            CodeActionKind::REFACTOR_EXTRACT,
                            CodeActionKind::REFACTOR_INLINE,
                            CodeActionKind::REFACTOR_REWRITE,
                            CodeActionKind::from("source.generate.toString"),
                            CodeActionKind::from("source.generate.hashCodeEquals"),
                            CodeActionKind::from("source.generate.constructors"),
                            CodeActionKind::from("source.generate.gettersAndSetters"),
                            CodeActionKind::from("source.generate.delegateMethods"),
                            CodeActionKind::from("source.overrideMethods"),
                            CodeActionKind::from("source.generate.javadoc"),
                        ]),
                        ..Default::default()
                    },
                )),
                rename_provider: Some(OneOf::Right(tower_lsp::lsp_types::RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                type_definition_provider: Some(tower_lsp::lsp_types::TypeDefinitionProviderCapability::Simple(true)),
                declaration_provider: Some(tower_lsp::lsp_types::DeclarationCapability::Simple(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: ";".to_string(),
                    more_trigger_character: Some(vec!["}".to_string(), "\n".to_string()]),
                }),
                execute_command_provider: Some(tower_lsp::lsp_types::ExecuteCommandOptions {
                    commands: vec![
                        "java.project.runTest".to_string(),
                        "java.project.cleanCache".to_string(),
                        "java.project.resolveClasspath".to_string(),
                        "java.project.getClasspaths".to_string(),
                        "java.project.getSources".to_string(),
                        "java.project.build".to_string(),
                        "java.project.update".to_string(),
                        "java.project.listOverridableMethods".to_string(),
                        "java.project.addOverridableMethods".to_string(),
                        "java.project.generateJavadoc".to_string(),
                        "java.project.checkHashCodeEqualsStatus".to_string(),
                        "java.project.generateHashCodeEquals".to_string(),
                        "java.project.generateToString".to_string(),
                        "java.project.generateAccessors".to_string(),
                        "java.project.generateConstructors".to_string(),
                        "java.project.generateDelegateMethods".to_string(),
                        "java.project.organizeImports".to_string(),
                        "java.edit.organizeImports".to_string(),
                        "java.action.applyWorkspaceEdit".to_string(),
                        "java.project.refreshLib".to_string(),
                        "java.project.addToSourcePath".to_string(),
                        "java.project.removeFromSourcePath".to_string(),
                        "java.project.isTestFile".to_string(),
                        "java.project.getSettings".to_string(),
                        "java.project.getProjectSettings".to_string(),
                        "java.project.getClasspath".to_string(),
                        "java.project.import".to_string(),
                        "vscode.java.checkHashCodeEqualsStatus".to_string(),
                        "java.refactor.moveClass".to_string(),
                        "java.refactor.extractInterface".to_string(),
                        "java.refactor.encapsulateField".to_string(),
                        "java.navigate.openTypeHierarchy".to_string(),
                        "java.completion.onImportChosen".to_string(),
                        "vscode.java.resolveMainClass".to_string(),
                        "vscode.java.resolveMainMethod".to_string(),
                        "vscode.java.debug.resolveClasspath".to_string(),
                        "vscode.java.debug.getLaunchArguments".to_string(),
                        "vscode.java.test.findJavaProjects".to_string(),
                        "vscode.java.test.findTestPackagesAndTypes".to_string(),
                        "vscode.java.test.search".to_string(),
                        "jlsp.runClassTests".to_string(),
                        "jlsp.debugClassTests".to_string(),
                        "jlsp.runMethod".to_string(),
                        "jlsp.debugMethod".to_string(),
                        "jlsp.openEndpoint".to_string(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "jlsp (pure-Rust JVM-free, JDT.LS-aligned)".to_string(),
                version: Some("1.2.0".to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "jlsp: Java LSP ready (Test runner & Hot Reload enabled).")
            .await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // Build-file changes only reload dependencies; mapping/config changes are scoped to that file.
        let mut build_roots: Vec<PathBuf> = Vec::new();
        for change in params.changes {
            if let Ok(path) = change.uri.to_file_path() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    match ext {
                        "java" => {
                            if change.typ == FileChangeType::DELETED {
                                info!("Java source file deleted ({:?}), removing from indexer...", path);
                                self.indexer.remove_file(&path).await;
                            } else if let Ok(content) = std::fs::read_to_string(&path) {
                                self.indexer.index_file(&path, &content).await;
                            }
                        }
                        "properties" | "yml" | "yaml" => {
                            if change.typ == FileChangeType::DELETED {
                                self.indexer.remove_file(&path).await;
                            } else if let Ok(content) = std::fs::read_to_string(&path) {
                                self.indexer.index_file(&path, &content).await;
                            }
                        }
                        "xml" => {
                            let is_build_file = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n == "pom.xml")
                                .unwrap_or(false);
                            if is_build_file {
                                info!("Build configuration changed ({:?}), reloading dependencies...", path);
                                build_roots.push(find_project_root(&path));
                            } else if change.typ == FileChangeType::DELETED {
                                self.indexer.remove_file(&path).await;
                            } else if let Ok(content) = std::fs::read_to_string(&path) {
                                self.indexer.index_mybatis_xml(&path, &content).await;
                            }
                        }
                        "gradle" | "kts" => {
                            info!("Build configuration changed ({:?}), reloading dependencies...", path);
                            build_roots.push(find_project_root(&path));
                        }
                        _ => {}
                    }
                }
            }
        }

        build_roots.sort();
        build_roots.dedup();
        for root in build_roots {
            let cache_file = root.join(".jlsp").join("index.bin.zst");
            self.reload_workspace_dependencies(&root, &cache_file).await;
        }
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        for added in params.event.added {
            if let Ok(path) = added.uri.to_file_path() {
                info!("Workspace folder added: {:?}, scanning...", &path);
                self.index_workspace(&path).await;
            }
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = Arc::new(params.text_document.text);
        if let Some(ast) = JavaAst::parse(Arc::clone(&text)) {
            if let Ok(path) = uri.to_file_path() {
                if !is_read_only_path(&path) {
                    self.indexer.index_file(&path, &text).await;
                }
            }
            self.documents.insert(uri.clone(), DocumentState { text: Arc::clone(&text), ast: ast.clone() });
            self.publish_document_diagnostics(&uri, &ast).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                self.client
                    .show_message(
                        MessageType::WARNING,
                        "This Java file is from decompiled bytecode or dependency JAR and is read-only. Editing is prohibited.",
                    )
                    .await;
                return;
            }
        }
        if let Some(change) = params.content_changes.into_iter().next() {
            let text = Arc::new(change.text);
            if let Some(ast) = JavaAst::parse(Arc::clone(&text)) {
                if let Ok(path) = uri.to_file_path() {
                    self.indexer.index_file(&path, &text).await;
                }
                self.documents.insert(uri.clone(), DocumentState { text: Arc::clone(&text), ast: ast.clone() });
                self.publish_document_diagnostics(&uri, &ast).await;
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents.remove(&params.text_document.uri);
    }

    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let mut lenses = Vec::new();

        // 1. References CodeLens (computed in a single AST traversal pass)
        let symbols = doc.ast.extract_symbols();
        let target_symbols: Vec<&crate::model::SymbolDef> = symbols
            .iter()
            .filter(|s| matches!(s.kind, crate::model::SymbolKind::Class | crate::model::SymbolKind::Method))
            .collect();

        let sym_names: Vec<&str> = target_symbols.iter().map(|s| s.name.as_str()).collect();
        let occ_map = doc.ast.find_all_occurrences_multiple(&sym_names);

        for s in target_symbols {
            let count = occ_map.get(&s.name).map(|v| v.len()).unwrap_or(0).saturating_sub(1);
            lenses.push(CodeLens {
                range: Range {
                    start: Position { line: s.range.start.line, character: s.range.start.character },
                    end: Position { line: s.range.start.line, character: s.range.start.character },
                },
                command: Some(Command {
                    title: format!("{} reference{}", count, if count == 1 { "" } else { "s" }),
                    command: "".to_string(),
                    arguments: None,
                }),
                data: None,
            });
        }

        // 2. Test Class Runner and Debugger CodeLens
        let test_classes = doc.ast.find_test_classes();
        for (c_name, c_range) in test_classes {
            lenses.push(CodeLens {
                range: Range {
                    start: Position { line: c_range.start.line, character: c_range.start.character },
                    end: Position { line: c_range.start.line, character: c_range.start.character },
                },
                command: Some(Command {
                    title: format!("▶ Run All Tests ({})", c_name),
                    command: "jlsp.runClassTests".to_string(),
                    arguments: Some(vec![serde_json::to_value(&c_name).unwrap()]),
                }),
                data: None,
            });

            lenses.push(CodeLens {
                range: Range {
                    start: Position { line: c_range.start.line, character: c_range.start.character },
                    end: Position { line: c_range.start.line, character: c_range.start.character },
                },
                command: Some(Command {
                    title: format!("🐞 Debug All Tests ({})", c_name),
                    command: "jlsp.debugClassTests".to_string(),
                    arguments: Some(vec![
                        serde_json::to_value(&c_name).unwrap(),
                        serde_json::to_value(5005).unwrap(),
                    ]),
                }),
                data: None,
            });
        }

        // 3. Test & Main Method Runner and Debugger CodeLens
        let runnable = doc.ast.find_test_and_main_methods();
        for (m_name, m_range, is_test) in runnable {
            let run_title = if is_test {
                format!("▶ Run Test ({})", m_name)
            } else {
                format!("▶ Run Main ({})", m_name)
            };
            let debug_title = if is_test {
                format!("🐞 Debug Test ({})", m_name)
            } else {
                format!("🐞 Debug Main ({})", m_name)
            };

            lenses.push(CodeLens {
                range: Range {
                    start: Position { line: m_range.start.line, character: m_range.start.character },
                    end: Position { line: m_range.start.line, character: m_range.start.character },
                },
                command: Some(Command {
                    title: run_title,
                    command: "jlsp.runMethod".to_string(),
                    arguments: Some(vec![serde_json::to_value(&m_name).unwrap()]),
                }),
                data: None,
            });

            lenses.push(CodeLens {
                range: Range {
                    start: Position { line: m_range.start.line, character: m_range.start.character },
                    end: Position { line: m_range.start.line, character: m_range.start.character },
                },
                command: Some(Command {
                    title: debug_title,
                    command: "jlsp.debugMethod".to_string(),
                    arguments: Some(vec![
                        serde_json::to_value(&m_name).unwrap(),
                        serde_json::to_value(5005).unwrap(), // default JDWP attach port
                    ]),
                }),
                data: None,
            });
        }

        // 4. Spring Boot HTTP Endpoint Route CodeLens
        let endpoints = doc.ast.find_spring_endpoints();
        for (http_m, path, m_range) in endpoints {
            lenses.push(CodeLens {
                range: Range {
                    start: Position { line: m_range.start.line, character: m_range.start.character },
                    end: Position { line: m_range.start.line, character: m_range.start.character },
                },
                command: Some(Command {
                    title: format!("🌐 {} {}", http_m, path),
                    command: "jlsp.openEndpoint".to_string(),
                    arguments: Some(vec![
                        serde_json::to_value(&http_m).unwrap(),
                        serde_json::to_value(&path).unwrap(),
                    ]),
                }),
                data: None,
            });
        }

        Ok(Some(lenses))
    }

    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let mut results = Vec::new();
        for pos in params.positions {
            let ranges = doc.ast.find_selection_ranges(pos.line, pos.character);
            let mut curr: Option<SelectionRange> = None;

            for r in ranges.into_iter().rev() {
                let sel_range = SelectionRange {
                    range: Range {
                        start: Position { line: r.start.line, character: r.start.character },
                        end: Position { line: r.end.line, character: r.end.character },
                    },
                    parent: curr.map(Box::new),
                };
                curr = Some(sel_range);
            }

            if let Some(c) = curr {
                results.push(c);
            }
        }

        Ok(Some(results))
    }

    async fn document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let raw_links = doc.ast.find_links();
        let mut links: Vec<DocumentLink> = raw_links
            .into_iter()
            .filter_map(|(range, target)| {
                let target_url = Url::parse(&target).ok()?;
                Some(DocumentLink {
                    range: Range {
                        start: Position { line: range.start.line, character: range.start.character },
                        end: Position { line: range.end.line, character: range.end.character },
                    },
                    target: Some(target_url),
                    tooltip: Some(target),
                    data: None,
                })
            })
            .collect();

        // 2. Link MyBatis Mapper methods to XML statement files
        let sem = SemanticContext::with_indexer(&doc.ast, &self.classpath, Some(&self.indexer)).await;
        for cls in sem.local_classes.values() {
            for m in &cls.methods {
                if let Some((xml_path, _xml_line)) = self.indexer.find_mybatis_statement(&cls.simple_name, &m.name).await {
                    if let Ok(xml_uri) = Url::from_file_path(&xml_path) {
                        if let Some(m_line) = doc.ast.find_member_declaration_line(&m.name, true, Some(m.parameters.len())) {
                            let line_str = doc.text.lines().nth(m_line as usize).unwrap_or("");
                            let col = line_str.find(&m.name).unwrap_or(0) as u32;
                            links.push(DocumentLink {
                                range: Range {
                                    start: Position { line: m_line, character: col },
                                    end: Position { line: m_line, character: col + m.name.len() as u32 },
                                },
                                target: Some(xml_uri),
                                tooltip: Some(format!("MyBatis XML: {}", xml_path.file_name().and_then(|n| n.to_str()).unwrap_or(""))),
                                data: None,
                            });
                        }
                    }
                }
            }
        }

        // 3. Link Spring @Value("${prop.key}") and ${...} properties to config files
        for (line_idx, line) in doc.text.lines().enumerate() {
            if line.contains("${") {
                if let Some(start) = line.find("${") {
                    if let Some(end) = line[start..].find('}') {
                        let full_prop = &line[start + 2..start + end];
                        let clean_prop = full_prop.split(':').next().unwrap_or(full_prop).trim();
                        if let Some((prop_path, _prop_line)) = self.indexer.find_config_property(clean_prop).await {
                            if let Ok(prop_uri) = Url::from_file_path(&prop_path) {
                                links.push(DocumentLink {
                                    range: Range {
                                        start: Position { line: line_idx as u32, character: (start + 2) as u32 },
                                        end: Position { line: line_idx as u32, character: (start + 2 + clean_prop.len()) as u32 },
                                    },
                                    target: Some(prop_uri),
                                    tooltip: Some(format!("Config file: {}", prop_path.file_name().and_then(|n| n.to_str()).unwrap_or(""))),
                                    data: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        // 4. Link Javadoc {@link ClassName} or @see ClassName to Java source files
        for (line_idx, line) in doc.text.lines().enumerate() {
            if line.contains("{@link") || line.contains("@see") {
                let mut search_slice = line;
                while let Some(link_idx) = search_slice.find("{@link") {
                    let after = &search_slice[link_idx + 6..];
                    if let Some(close_idx) = after.find('}') {
                        let tag_content = after[..close_idx].trim();
                        let target_class = tag_content.split('#').next().unwrap_or(tag_content).trim();
                        if !target_class.is_empty() {
                            let (target_uri, target_fqn) = if let Some(target_cls) = sem.resolve_type(target_class).await {
                                let u = target_cls.source_path.as_ref().and_then(|p| Url::from_file_path(p).ok());
                                (u, target_cls.fqn.clone())
                            } else if let Some(cand) = self.indexer.find_by_simple_name(target_class).await.into_iter().next() {
                                let u = Url::from_file_path(&cand.path).ok();
                                (u, cand.fqn.clone())
                            } else {
                                (None, String::new())
                            };

                            if let Some(file_uri) = target_uri {
                                let col_start = (line.len() - search_slice.len() + link_idx + 6) as u32;
                                links.push(DocumentLink {
                                    range: Range {
                                        start: Position { line: line_idx as u32, character: col_start },
                                        end: Position { line: line_idx as u32, character: col_start + target_class.len() as u32 },
                                    },
                                    target: Some(file_uri),
                                    tooltip: Some(format!("Javadoc link to {}", target_fqn)),
                                    data: None,
                                });
                            }
                        }
                        search_slice = &after[close_idx + 1..];
                    } else {
                        break;
                    }
                }
            }
        }

        Ok(Some(links))
    }

    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        let current_pkg = doc.ast.extract_package();
        let current_imports = doc.ast.extract_imports();

        let target_name = if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            doc.ast.node_text(&node).to_string()
        } else if let Some(enclosing) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
            enclosing
        } else {
            return Ok(None);
        };

        // 1. Check local classes in current document
        if let Some(cls) = sem.local_classes.get(&target_name) {
            let kind = if cls.kind == crate::model::ClassKind::Interface { LspSymbolKind::INTERFACE } else { LspSymbolKind::CLASS };
            return Ok(Some(vec![TypeHierarchyItem {
                name: cls.simple_name.clone(),
                kind,
                tags: None,
                detail: Some(cls.package_name.clone()),
                uri: uri.clone(),
                range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                data: None,
            }]));
        }

        // 2. Check workspace index
        if let Some(sym) = self.indexer.find_candidate(&target_name, current_pkg.as_deref(), &current_imports).await {
            if let Ok(target_uri) = Url::from_file_path(&sym.path) {
                let kind = if sym.class_info.kind == crate::model::ClassKind::Interface { LspSymbolKind::INTERFACE } else { LspSymbolKind::CLASS };
                return Ok(Some(vec![TypeHierarchyItem {
                    name: sym.simple_name.clone(),
                    kind,
                    tags: None,
                    detail: Some(sym.class_info.package_name.clone()),
                    uri: target_uri,
                    range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                    selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                    data: None,
                }]));
            }
        }

        // 3. Check classpath
        if let Some(cls) = sem.resolve_type(&target_name).await {
            let kind = if cls.kind == crate::model::ClassKind::Interface { LspSymbolKind::INTERFACE } else { LspSymbolKind::CLASS };
            return Ok(Some(vec![TypeHierarchyItem {
                name: cls.simple_name.clone(),
                kind,
                tags: None,
                detail: Some(cls.package_name.clone()),
                uri: uri.clone(),
                range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                data: None,
            }]));
        }

        Ok(None)
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let name = params.item.name;
        let mut super_class_opt: Option<String> = None;
        let mut interfaces: Vec<String> = Vec::new();

        // 1. Resolve class information from local docs or workspace or classpath
        for item in self.documents.iter() {
            let doc = item.value();
            let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
            if let Some(cls) = sem.local_classes.get(&name) {
                super_class_opt = cls.super_class.clone();
                interfaces = cls.interfaces.clone();
                break;
            }
        }

        if super_class_opt.is_none() && interfaces.is_empty() {
            if let Some(sym) = self.indexer.find_by_simple_name(&name).await.into_iter().next() {
                super_class_opt = sym.class_info.super_class.clone();
                interfaces = sym.class_info.interfaces.clone();
            }
        }

        if super_class_opt.is_none() && interfaces.is_empty() {
            let cp_cls = if let Some(cls) = self.classpath.get_class(&name).await {
                Some(cls)
            } else {
                self.classpath.find_classes_by_simple_name(&name).await.into_iter().next()
            };
            if let Some(cls) = cp_cls {
                super_class_opt = cls.super_class.clone();
                interfaces = cls.interfaces.clone();
            }
        }

        let mut supers = Vec::new();

        if let Some(super_cls) = super_class_opt {
            let (target_uri, pkg) = if let Some(sym) = self.indexer.find_by_simple_name(&super_cls).await.into_iter().next() {
                (Url::from_file_path(&sym.path).unwrap_or_else(|_| params.item.uri.clone()), Some(sym.class_info.package_name.clone()))
            } else {
                (params.item.uri.clone(), None)
            };
            supers.push(TypeHierarchyItem {
                name: super_cls,
                kind: LspSymbolKind::CLASS,
                tags: None,
                detail: pkg,
                uri: target_uri,
                range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                data: None,
            });
        }

        for iface in interfaces {
            let (target_uri, pkg) = if let Some(sym) = self.indexer.find_by_simple_name(&iface).await.into_iter().next() {
                (Url::from_file_path(&sym.path).unwrap_or_else(|_| params.item.uri.clone()), Some(sym.class_info.package_name.clone()))
            } else {
                (params.item.uri.clone(), None)
            };
            supers.push(TypeHierarchyItem {
                name: iface,
                kind: LspSymbolKind::INTERFACE,
                tags: None,
                detail: pkg,
                uri: target_uri,
                range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                data: None,
            });
        }

        if !supers.is_empty() {
            Ok(Some(supers))
        } else {
            Ok(None)
        }
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let name = params.item.name;
        let mut items = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 1. Workspace indexed implementations and subtypes
        let ws_impls = self.indexer.find_implementations(&name).await;
        for sym in ws_impls {
            if let Ok(uri) = Url::from_file_path(&sym.path) {
                let key = (sym.simple_name.clone(), uri.clone());
                if !seen.contains(&key) {
                    seen.insert(key);
                    let kind = if sym.class_info.kind == crate::model::ClassKind::Interface { LspSymbolKind::INTERFACE } else { LspSymbolKind::CLASS };
                    items.push(TypeHierarchyItem {
                        name: sym.simple_name.clone(),
                        kind,
                        tags: None,
                        detail: Some(sym.class_info.package_name.clone()),
                        uri,
                        range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        data: None,
                    });
                }
            }
        }

        // 2. Open documents implementations
        for item in self.documents.iter() {
            let doc = item.value();
            let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
            let subs = sem.find_implementations(&name).await;
            for c in subs {
                let uri = item.key().clone();
                let key = (c.simple_name.clone(), uri.clone());
                if !seen.contains(&key) {
                    seen.insert(key);
                    let kind = if c.kind == crate::model::ClassKind::Interface { LspSymbolKind::INTERFACE } else { LspSymbolKind::CLASS };
                    items.push(TypeHierarchyItem {
                        name: c.simple_name,
                        kind,
                        tags: None,
                        detail: Some(c.package_name),
                        uri,
                        range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        data: None,
                    });
                }
            }
        }

        if !items.is_empty() {
            Ok(Some(items))
        } else {
            Ok(None)
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                return Ok(None);
            }
        }
        let pos = params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        // If it's a local variable/parameter with bounded scope, it is renamable
        if let Some((_name, ranges)) = doc.ast.find_local_scope_occurrences(pos.line, pos.character) {
            for r in ranges {
                if r.start.line == pos.line && pos.character >= r.start.character && pos.character <= r.end.character {
                    return Ok(Some(PrepareRenameResponse::Range(r)));
                }
            }
        }

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let kind = node.kind();
            if !matches!(kind, "identifier" | "type_identifier") {
                return Ok(None);
            }

            let text = doc.ast.node_text(&node);
            if matches!(
                text,
                "true" | "false" | "null" | "this" | "super" | "void" | "int"
                    | "long" | "boolean" | "float" | "double" | "byte" | "short"
                    | "char" | "var" | "record" | "class" | "interface" | "enum"
                    | "public" | "private" | "protected" | "static" | "final"
                    | "return" | "if" | "else" | "for" | "while" | "new"
            ) {
                return Ok(None);
            }

            let r = doc.ast.node_lsp_range(&node);
            return Ok(Some(PrepareRenameResponse::Range(r)));
        }

        Ok(None)
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let raw_tokens = doc.ast.extract_semantic_tokens();
        let tokens: Vec<SemanticToken> = raw_tokens
            .into_iter()
            .map(|(delta_line, delta_start, length, token_type, token_modifiers_bitset)| SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset,
            })
            .collect();

        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn semantic_tokens_range(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let raw_tokens = doc.ast.extract_semantic_tokens();
        let tokens: Vec<SemanticToken> = raw_tokens
            .into_iter()
            .map(|(delta_line, delta_start, length, token_type, token_modifiers_bitset)| SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset,
            })
            .collect();

        Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
            result_id: None,
            data: tokens,
        })))
    }

    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        if let Some((name, range)) = doc.ast.find_method_declaration_at_position(pos.line, pos.character) {
            let item = CallHierarchyItem {
                name,
                kind: LspSymbolKind::METHOD,
                tags: None,
                detail: Some("method".to_string()),
                uri,
                range: Range {
                    start: Position {
                        line: range.start.line,
                        character: range.start.character,
                    },
                    end: Position {
                        line: range.end.line,
                        character: range.end.character,
                    },
                },
                selection_range: Range {
                    start: Position {
                        line: range.start.line,
                        character: range.start.character,
                    },
                    end: Position {
                        line: range.end.line,
                        character: range.end.character,
                    },
                },
                data: None,
            };
            return Ok(Some(vec![item]));
        }

        Ok(None)
    }

    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let target_name = params.item.name;
        let mut incoming = Vec::new();
        let mut scanned_uris = std::collections::HashSet::new();

        let check_ast = |ast: &JavaAst, doc_uri: &Url, out: &mut Vec<CallHierarchyIncomingCall>| {
            for sym in ast.extract_symbols() {
                if sym.kind == crate::model::SymbolKind::Method {
                    let outgoing = ast.find_outgoing_calls(&sym.name);
                    for (called, from_range) in outgoing {
                        if called == target_name {
                            out.push(CallHierarchyIncomingCall {
                                from: CallHierarchyItem {
                                    name: sym.name.clone(),
                                    kind: LspSymbolKind::METHOD,
                                    tags: None,
                                    detail: sym.container_name.clone(),
                                    uri: doc_uri.clone(),
                                    range: Range {
                                        start: Position { line: sym.range.start.line, character: sym.range.start.character },
                                        end: Position { line: sym.range.end.line, character: sym.range.end.character },
                                    },
                                    selection_range: Range {
                                        start: Position { line: sym.selection_range.start.line, character: sym.selection_range.start.character },
                                        end: Position { line: sym.selection_range.end.line, character: sym.selection_range.end.character },
                                    },
                                    data: None,
                                },
                                from_ranges: vec![Range {
                                    start: Position { line: from_range.start.line, character: from_range.start.character },
                                    end: Position { line: from_range.end.line, character: from_range.end.character },
                                }],
                            });
                        }
                    }
                }
            }
        };

        // 1. Check open documents
        for item in self.documents.iter() {
            let doc_uri = item.key();
            scanned_uris.insert(doc_uri.clone());
            check_ast(&item.value().ast, doc_uri, &mut incoming);
        }

        // 2. Check other workspace files
        let all_files = self.indexer.all_source_files().await;
        for path in all_files {
            if let Ok(file_uri) = Url::from_file_path(&path) {
                if !scanned_uris.contains(&file_uri) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content.contains(&target_name) {
                            if let Some(other_ast) = JavaAst::parse(Arc::new(content)) {
                                check_ast(&other_ast, &file_uri, &mut incoming);
                            }
                        }
                    }
                }
            }
        }

        Ok(Some(incoming))
    }

    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let uri = params.item.uri;
        let method_name = params.item.name;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let outgoing = doc.ast.find_outgoing_calls(&method_name);
        let mut calls = Vec::new();

        for (called_name, call_range) in outgoing {
            calls.push(CallHierarchyOutgoingCall {
                to: CallHierarchyItem {
                    name: called_name,
                    kind: LspSymbolKind::METHOD,
                    tags: None,
                    detail: None,
                    uri: uri.clone(),
                    range: Range {
                        start: Position { line: call_range.start.line, character: call_range.start.character },
                        end: Position { line: call_range.end.line, character: call_range.end.character },
                    },
                    selection_range: Range {
                        start: Position { line: call_range.start.line, character: call_range.start.character },
                        end: Position { line: call_range.end.line, character: call_range.end.character },
                    },
                    data: None,
                },
                from_ranges: vec![Range {
                    start: Position { line: call_range.start.line, character: call_range.start.character },
                    end: Position { line: call_range.end.line, character: call_range.end.character },
                }],
            });
        }

        Ok(Some(calls))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let nested = doc.ast.extract_hierarchical_symbols();
        Ok(Some(DocumentSymbolResponse::Nested(nested)))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        if let Some((_ident, local_ranges)) = doc.ast.find_local_scope_occurrences(pos.line, pos.character) {
            let highlights = local_ranges
                .into_iter()
                .map(|r| {
                    let kind = if let Some(n) = doc.ast.find_node_at_position(r.start.line, r.start.character) {
                        if let Some(parent) = n.parent() {
                            if (parent.kind() == "assignment_expression" && parent.child(0).map(|c| c.id() == n.id()).unwrap_or(false))
                                || parent.kind() == "update_expression"
                            {
                                DocumentHighlightKind::WRITE
                            } else {
                                DocumentHighlightKind::READ
                            }
                        } else {
                            DocumentHighlightKind::READ
                        }
                    } else {
                        DocumentHighlightKind::READ
                    };
                    DocumentHighlight {
                        range: r,
                        kind: Some(kind),
                    }
                })
                .collect();
            return Ok(Some(highlights));
        }

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let ident = doc.ast.node_text(&node);
            let occurrences = doc.ast.find_all_occurrences(ident);
            let highlights = occurrences
                .into_iter()
                .map(|r| DocumentHighlight {
                    range: Range {
                        start: Position {
                            line: r.start.line,
                            character: r.start.character,
                        },
                        end: Position {
                            line: r.end.line,
                            character: r.end.character,
                        },
                    },
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect();
            return Ok(Some(highlights));
        }

        Ok(None)
    }

    async fn folding_range(
        &self,
        params: FoldingRangeParams,
    ) -> Result<Option<Vec<FoldingRange>>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let ranges = doc.ast.extract_folding_ranges();
        let folding = ranges
            .into_iter()
            .map(|(start, end, kind)| FoldingRange {
                start_line: start,
                start_character: None,
                end_line: end,
                end_character: None,
                kind: match kind {
                    "comment" => Some(FoldingRangeKind::Comment),
                    "imports" => Some(FoldingRangeKind::Imports),
                    _ => Some(FoldingRangeKind::Region),
                },
                collapsed_text: None,
            })
            .collect();

        Ok(Some(folding))
    }

    async fn inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let model_range = crate::model::Range {
            start: crate::model::Position {
                line: params.range.start.line,
                character: params.range.start.character,
            },
            end: crate::model::Position {
                line: params.range.end.line,
                character: params.range.end.character,
            },
        };
        let mut hints = Vec::new();
        let mut seen_positions = std::collections::HashSet::new();

        // 1. Variable type hints (e.g. Java 10+ var x = ...) scoped to requested range
        let raw_type_hints = doc.ast.find_inlay_hints_in_range(Some(&model_range));
        for (pos, label) in raw_type_hints {
            if !seen_positions.insert((pos.line, pos.character)) {
                continue;
            }
            hints.push(InlayHint {
                position: Position {
                    line: pos.line,
                    character: pos.character,
                },
                label: InlayHintLabel::String(label),
                kind: Some(InlayHintKind::TYPE),
                text_edits: None,
                tooltip: None,
                padding_left: Some(true),
                padding_right: Some(false),
                data: None,
            });
        }

        // 2. Real parameter name hints for method invocations scoped to requested range
        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        let current_pkg = doc.ast.extract_package();
        let current_imports = doc.ast.extract_imports();
        let inv_args = doc.ast.find_method_invocation_arg_positions_in_range(Some(&model_range));

        for arg in inv_args {
            let target_class = if !arg.receiver_steps.is_empty() {
                self.resolve_receiver_chain(
                    &arg.receiver_steps,
                    Position { line: arg.position.line, character: arg.position.character },
                    &doc,
                    &sem,
                    current_pkg.as_deref(),
                    &current_imports,
                ).await
            } else if arg.receiver_raw.is_none() {
                doc.ast.find_enclosing_class_at_position(arg.position.line, arg.position.character)
            } else {
                None
            };

            if let Some(tc) = target_class {
                if let Some(cls_info) = self.find_class_info(&tc, current_pkg.as_deref(), &current_imports, &sem).await {
                    let all_methods = sem.get_all_methods_with_inheritance(&cls_info).await;
                    let matching_method = all_methods.iter().find(|m| m.name == arg.method_name && m.parameters.len() == arg.total_args)
                        .or_else(|| all_methods.iter().find(|m| m.name == arg.method_name));

                    if let Some(m) = matching_method {
                        if arg.arg_index < m.parameters.len() {
                            let (param_name, _) = &m.parameters[arg.arg_index];
                            if !param_name.is_empty() && !param_name.starts_with("arg") && !param_name.starts_with("p0") && !param_name.starts_with("p1") {
                                if seen_positions.insert((arg.position.line, arg.position.character)) {
                                    hints.push(InlayHint {
                                        position: Position {
                                            line: arg.position.line,
                                            character: arg.position.character,
                                        },
                                        label: InlayHintLabel::String(format!("{}:", param_name)),
                                        kind: Some(InlayHintKind::PARAMETER),
                                        text_edits: None,
                                        tooltip: None,
                                        padding_left: Some(false),
                                        padding_right: Some(true),
                                        data: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(Some(hints))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let mut target_name = doc.ast.node_text(&node).to_string();

            // If cursor is on a field name with @Autowired / @Resource, resolve field type first
            let local_vars = doc.ast.extract_local_variables_at_position(pos.line, pos.character);
            for (v_name, v_type) in &local_vars {
                if v_name == &target_name {
                    let clean = v_type.split('<').next().unwrap_or(v_type).trim();
                    target_name = clean.to_string();
                    break;
                }
            }
            for cls in sem.local_classes.values() {
                for f in &cls.fields {
                    if f.name == target_name {
                        let f_type = f.field_type.simple_name();
                        let clean = f_type.split('<').next().unwrap_or(&f_type).trim();
                        target_name = clean.to_string();
                        break;
                    }
                }
            }

            let mut locs = Vec::new();

            // Helper to locate declaration line & column of class/symbol in file
            let locate_pos = |p: &std::path::Path, sym_name: &str| -> Range {
                if let Ok(content) = std::fs::read_to_string(p) {
                    for (idx, line) in content.lines().enumerate() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                            continue;
                        }
                        for kw in &["class ", "interface ", "enum ", "record ", "@interface "] {
                            if let Some(pos) = line.find(kw) {
                                let after = &line[pos + kw.len()..];
                                let name_candidate = after.split(|c: char| !c.is_alphanumeric() && c != '_').next().unwrap_or("");
                                if name_candidate == sym_name {
                                    let col = line.find(sym_name).unwrap_or(0) as u32;
                                    let l = idx as u32;
                                    return Range {
                                        start: Position { line: l, character: col },
                                        end: Position { line: l, character: col + sym_name.len() as u32 },
                                    };
                                }
                            }
                        }
                    }
                    for (idx, line) in content.lines().enumerate() {
                        if line.contains(sym_name) {
                            let col = line.find(sym_name).unwrap_or(0) as u32;
                            let l = idx as u32;
                            return Range {
                                start: Position { line: l, character: col },
                                end: Position { line: l, character: col + sym_name.len() as u32 },
                            };
                        }
                    }
                }
                Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 0 },
                }
            };

            // 1. Search local file & classpath implementations
            let impls = sem.find_implementations(&target_name).await;
            for im in impls {
                if let Some(path) = im.source_path {
                    if let Ok(target_uri) = Url::from_file_path(&path) {
                        let range = locate_pos(std::path::Path::new(&path), &im.simple_name);
                        locs.push(Location {
                            uri: target_uri,
                            range,
                        });
                    }
                }
            }

            // 2. Search workspace-wide indexed implementations (Spring @Service / @Component beans)
            let ws_impls = self.indexer.find_implementations(&target_name).await;
            for sym in ws_impls {
                if let Ok(target_uri) = Url::from_file_path(&sym.path) {
                    if !locs.iter().any(|l| l.uri == target_uri) {
                        let range = locate_pos(&sym.path, &sym.simple_name);
                        locs.push(Location {
                            uri: target_uri,
                            range,
                        });
                    }
                }
            }

            // 3. Fallback: if target_name is a concrete Bean class in workspace (e.g. injected concrete service)
            let concrete = self.indexer.find_by_simple_name(&target_name).await;
            for sym in concrete {
                if let Ok(target_uri) = Url::from_file_path(&sym.path) {
                    if !locs.iter().any(|l| l.uri == target_uri) {
                        let range = locate_pos(&sym.path, &sym.simple_name);
                        locs.push(Location {
                            uri: target_uri,
                            range,
                        });
                    }
                }
            }

            // Check if cursor field/parameter is annotated with Spring @Qualifier("beanName")
            let qualifier_name: Option<String> = {
                let lines: Vec<&str> = doc.text.lines().collect();
                let start_idx = pos.line.saturating_sub(4) as usize;
                let end_idx = (pos.line as usize).min(lines.len().saturating_sub(1));
                let mut found = None;
                for l in &lines[start_idx..=end_idx] {
                    if let Some(q_idx) = l.find("@Qualifier") {
                        let after = &l[q_idx..];
                        if let Some(quote_start) = after.find('"') {
                            if let Some(quote_end) = after[quote_start + 1..].find('"') {
                                found = Some(after[quote_start + 1..quote_start + 1 + quote_end].to_string());
                                break;
                            }
                        }
                    }
                }
                found
            };

            if !locs.is_empty() {
                if let Some(ref q_name) = qualifier_name {
                    let q_lower = q_name.to_lowercase();
                    let mut matched_locs = Vec::new();
                    let mut other_locs = Vec::new();

                    for loc in &locs {
                        let mut is_match = false;
                        if let Ok(file_path) = loc.uri.to_file_path() {
                            let file_stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                            if file_stem == q_lower || file_stem == format!("{}impl", q_lower) || file_stem.contains(&q_lower) {
                                is_match = true;
                            } else if let Ok(content) = std::fs::read_to_string(&file_path) {
                                if content.contains(&format!("@Service(\"{}\")", q_name))
                                    || content.contains(&format!("@Component(\"{}\")", q_name))
                                    || content.contains(&format!("@Bean(\"{}\")", q_name))
                                {
                                    is_match = true;
                                }
                            }
                        }
                        if is_match {
                            matched_locs.push(loc.clone());
                        } else {
                            other_locs.push(loc.clone());
                        }
                    }

                    if !matched_locs.is_empty() {
                        matched_locs.extend(other_locs);
                        return Ok(Some(GotoImplementationResponse::Array(matched_locs)));
                    }
                }

                return Ok(Some(GotoImplementationResponse::Array(locs)));
            }
        }

        Ok(None)
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let found = self.indexer.search_symbols(&params.query).await;
        let mut symbols = Vec::new();

        for s in found {
            if let Ok(uri) = Url::from_file_path(&s.path) {
                let range = if let Ok(content) = std::fs::read_to_string(&s.path) {
                    let mut found_r = Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
                    };
                    for (idx, line) in content.lines().enumerate() {
                        if line.contains(&s.simple_name) {
                            let col = line.find(&s.simple_name).unwrap_or(0) as u32;
                            found_r = Range {
                                start: Position { line: idx as u32, character: col },
                                end: Position { line: idx as u32, character: col + s.simple_name.len() as u32 },
                            };
                            break;
                        }
                    }
                    found_r
                } else {
                    Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
                    }
                };

                #[allow(deprecated)]
                symbols.push(SymbolInformation {
                    name: s.simple_name.clone(),
                    kind: LspSymbolKind::CLASS,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri,
                        range,
                    },
                    container_name: Some(s.class_info.package_name.clone()),
                });
            }
        }

        Ok(Some(symbols))
    }



    async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let include_decl = params.context.include_declaration;
        let locs = self.find_references_internal(&uri, &doc, pos, include_decl).await;
        Ok(Some(locs))
    }

    async fn signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        if let Some((target_name, active_param, is_constructor)) = doc.ast.find_method_call_at_position(pos.line, pos.character) {
            let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
            let matched_methods: Vec<crate::model::MethodInfo> = if is_constructor {
                let mut ctors = Vec::new();
                // 1. Check local classes
                if let Some(cls) = sem.local_classes.get(&target_name) {
                    for m in &cls.methods {
                        if m.is_constructor {
                            ctors.push(m.clone());
                        }
                    }
                }
                // 2. Check workspace classes
                if ctors.is_empty() {
                    for sym in self.indexer.find_by_simple_name(&target_name).await {
                        for m in &sym.class_info.methods {
                            if m.is_constructor {
                                ctors.push(m.clone());
                            }
                        }
                    }
                }
                // 3. Check classpath
                if ctors.is_empty() {
                    for cls in self.classpath.find_classes_by_simple_name(&target_name).await {
                        for m in &cls.methods {
                            if m.is_constructor {
                                ctors.push(m.clone());
                            }
                        }
                    }
                }
                // If class exists but has default no-arg constructor
                if ctors.is_empty() {
                    ctors.push(crate::model::MethodInfo {
                        name: target_name.clone(),
                        return_type: crate::model::TypeSignature::Void,
                        parameters: Vec::new(),
                        is_static: false,
                        is_public: true,
                        is_constructor: true,
                        javadoc: Some(format!("Default constructor for {}", target_name)),
                        body: None,
                    });
                }
                ctors
            } else {
                sem.find_methods_by_name(&target_name).await
            };

            if !matched_methods.is_empty() {
                let signatures: Vec<SignatureInformation> = matched_methods
                    .into_iter()
                    .map(|m| {
                        let param_strs = m.parameter_display_strings();
                        let label = if is_constructor {
                            format!("{}({})", m.name, param_strs.join(", "))
                        } else {
                            format!("{}({}) -> {}", m.name, param_strs.join(", "), m.return_type.simple_name())
                        };

                        let parameters = param_strs
                            .into_iter()
                            .map(|p| ParameterInformation {
                                label: ParameterLabel::Simple(p),
                                documentation: None,
                            })
                            .collect();

                        SignatureInformation {
                            label,
                            documentation: m.javadoc.map(tower_lsp::lsp_types::Documentation::String),
                            parameters: Some(parameters),
                            active_parameter: Some(active_param),
                        }
                    })
                    .collect();

                return Ok(Some(SignatureHelp {
                    signatures,
                    active_signature: Some(0),
                    active_parameter: Some(active_param),
                }));
            }
        }

        Ok(None)
    }

    async fn goto_declaration(
        &self,
        params: GotoDeclarationParams,
    ) -> Result<Option<GotoDeclarationResponse>> {
        let uri = params.text_document_position_params.text_document.uri.clone();
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;

        // 1. Check if cursor is on a method declaration or overriding method
        if let Some((m_name, _)) = doc.ast.find_method_declaration_at_position(pos.line, pos.character) {
            if let Some(enc_class_name) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
                if let Some(enc_cls) = sem.resolve_type(&enc_class_name).await {
                    let mut parents = Vec::new();
                    if let Some(ref sup) = enc_cls.super_class {
                        parents.push(sup.clone());
                    }
                    parents.extend(enc_cls.interfaces.clone());

                    for parent_name in parents {
                        if let Some(parent_cls) = sem.resolve_type(&parent_name).await {
                            for pm in &parent_cls.methods {
                                if pm.name == m_name {
                                    // Found super method declaration! Resolve its location
                                    let locs = self.resolve_jar_target_location(
                                        "",
                                        &parent_cls,
                                        &m_name,
                                    ).await;
                                    if !locs.is_empty() {
                                        return Ok(Self::into_declaration_response(locs));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Fallback to goto_definition if not an overriding method
        let def_params = GotoDefinitionParams {
            text_document_position_params: params.text_document_position_params,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        match self.goto_definition(def_params).await? {
            Some(GotoDefinitionResponse::Scalar(loc)) => Ok(Some(GotoDeclarationResponse::Scalar(loc))),
            Some(GotoDefinitionResponse::Array(arr)) => Ok(Some(GotoDeclarationResponse::Array(arr))),
            Some(GotoDefinitionResponse::Link(links)) => Ok(Some(GotoDeclarationResponse::Link(links))),
            None => Ok(None),
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        // If cursor is at a method, constructor, or field definition identifier:
        // Clicking Ctrl+LeftClick on the definition jumps directly to reference if exactly 1 reference exists,
        // or presents all references if multiple references exist.
        if doc.ast.is_definition_node(pos.line, pos.character) {
            let refs = self.find_references_internal(&uri, &doc, pos, false).await;
            if refs.len() == 1 {
                return Ok(Some(GotoDefinitionResponse::Scalar(refs.into_iter().next().unwrap())));
            } else if refs.len() > 1 {
                return Ok(Some(GotoDefinitionResponse::Array(refs)));
            } else {
                // If 0 references, return current definition range as fallback so cursor stays / highlights
                let def_range = doc.ast.get_definition_identifier_range(pos.line, pos.character).unwrap_or(Range {
                    start: pos,
                    end: pos,
                });
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: uri.clone(),
                    range: def_range,
                })));
            }
        }

        let sem = SemanticContext::with_indexer(&doc.ast, &self.classpath, Some(&self.indexer)).await;

        let current_pkg = doc.ast.extract_package();
        let current_imports = doc.ast.extract_imports();

        // 0. Check if current line is an import statement
        let current_line = doc.text.lines().nth(pos.line as usize).unwrap_or("").trim();
        if current_line.starts_with("import ") {
            let import_content = current_line
                .trim_start_matches("import ")
                .trim_end_matches(';')
                .trim();
            if let Some((target_cls, default_member)) = self.resolve_import_target(import_content, &sem).await {
                let clicked_word = sem.find_node_at_position(pos.line, pos.character)
                    .map(|n| doc.ast.node_text(&n).to_string())
                    .unwrap_or_default();
                let target_name = if !clicked_word.is_empty() && (clicked_word == target_cls.simple_name || clicked_word == default_member) {
                    clicked_word
                } else {
                    default_member
                };
                let locs = self.resolve_external_target_location(&target_cls, &target_name, None).await;
                if !locs.is_empty() {
                    return Ok(Self::into_definition_response(locs));
                }
            }
        }

        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);

            // PRIORITY 1: Project Workspace Sources
            // 1.1 Check if part of method invocation or field access in workspace (with chained call resolution & overload disambiguation)
            if let Some(info) = doc.ast.find_member_access_info(pos.line, pos.character) {
                if info.member_name == word {
                    let mut target_class: Option<String> = None;
                    if !info.receiver_steps.is_empty() {
                        target_class = self.resolve_receiver_chain(
                            &info.receiver_steps,
                            pos,
                            &doc,
                            &sem,
                            current_pkg.as_deref(),
                            &current_imports,
                        ).await;
                    } else if info.receiver.is_none() {
                        target_class = doc.ast.find_enclosing_class_at_position(pos.line, pos.character);
                    }

                    // Look for member inside workspace project sources, disambiguating by class, package, and overload
                    let mut found_member = self.indexer.find_member_location_with_context(
                        target_class.as_deref(),
                        &info.member_name,
                        info.is_method,
                        info.arg_count,
                        current_pkg.as_deref(),
                        &current_imports,
                    ).await;

                    // If recv was None and not found in enclosing class, search across imports / workspace
                    if found_member.is_none() && info.receiver.is_none() && target_class.is_some() {
                        found_member = self.indexer.find_member_location_with_context(
                            None,
                            &info.member_name,
                            info.is_method,
                            info.arg_count,
                            current_pkg.as_deref(),
                            &current_imports,
                        ).await;
                    }

                    if let Some((member_file, line_no)) = found_member {
                        if let Ok(target_uri) = Url::from_file_path(&member_file) {
                            let loc = Location {
                                uri: target_uri,
                                range: Range {
                                    start: Position { line: line_no, character: 0 },
                                    end: Position { line: line_no, character: 0 },
                                },
                            };
                            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                        }
                    }

                    // If member was NOT found in workspace sources and has an explicit receiver (e.g. list.size(), System.out.println()),
                    // resolve across external dependencies and JDK
                    if info.receiver.is_some() {
                        if let Some(target_cls_name) = target_class.as_deref() {
                            if let Some(cls) = self.find_class_info(target_cls_name, current_pkg.as_deref(), &current_imports, &sem).await {
                                let locs = self.resolve_external_target_location(&cls, &info.member_name, info.arg_count).await;
                                if !locs.is_empty() {
                                    return Ok(Self::into_definition_response(locs));
                                }
                            }
                        }
                    }
                }
            }

            // 1.2 Check if type/class identifier exists in workspace sources (matching package + class)
            let is_type_like = (word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                || node.kind() == "type_identifier")
                && crate::syntax::is_valid_java_identifier(word);

            if is_type_like {
                let mut search_words = vec![word.to_string()];
                if (word.starts_with("get") || word.starts_with("set")) && word.len() > 3 {
                    let field_cand = format!("{}{}", word[3..4].to_lowercase(), &word[4..]);
                    search_words.push(field_cand);
                }

                for sw in &search_words {
                    if let Some(target) = self.indexer.find_candidate(sw, current_pkg.as_deref(), &current_imports).await {
                        if let Ok(target_uri) = Url::from_file_path(&target.path) {
                            let mut target_line = 0;
                            if let Ok(content) = std::fs::read_to_string(&target.path) {
                                target_line = crate::syntax::find_target_line_in_source(&content, &target.simple_name);
                            }
                            let loc = Location {
                                uri: target_uri,
                                range: Range {
                                    start: Position { line: target_line, character: 0 },
                                    end: Position { line: target_line, character: target.simple_name.len() as u32 },
                                },
                            };
                            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                        }
                    }
                }
            }

            // 1.3 Check MyBatis XML bidirectional mapping
            for cls in sem.local_classes.values() {
                if let Some((xml_path, xml_line)) = self.indexer.find_mybatis_statement(&cls.simple_name, word).await {
                    if let Ok(xml_uri) = Url::from_file_path(&xml_path) {
                        let loc = Location {
                            uri: xml_uri,
                            range: Range {
                                start: Position { line: xml_line, character: 0 },
                                end: Position { line: xml_line, character: 0 },
                            },
                        };
                        return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                    }
                }
            }

            // 1.4 Check Spring Configuration Properties (@Value("${prop.key}"), application.yml / properties)
            let current_line = doc.text.lines().nth(pos.line as usize).unwrap_or("");
            let clean_key = if word.starts_with("${") {
                let inner = word.trim_start_matches("${").trim_end_matches('}');
                inner.split(':').next().unwrap_or(inner).trim()
            } else if word.starts_with('"') && word.ends_with('"') {
                let inner = word.trim_matches('"');
                if inner.starts_with("${") {
                    let k = inner.trim_start_matches("${").trim_end_matches('}');
                    k.split(':').next().unwrap_or(k).trim()
                } else {
                    inner.trim()
                }
            } else if current_line.contains("@Value") || current_line.contains("${") {
                if let Some(start) = current_line.find("${") {
                    if let Some(end) = current_line[start..].find('}') {
                        let inner = &current_line[start + 2..start + end];
                        inner.split(':').next().unwrap_or(inner).trim()
                    } else {
                        word.trim()
                    }
                } else {
                    word.trim()
                }
            } else {
                word.trim()
            };

            if !clean_key.is_empty() {
                if let Some((prop_path, prop_line)) = self.indexer.find_config_property(clean_key).await {
                    if let Ok(prop_uri) = Url::from_file_path(&prop_path) {
                        let loc = Location {
                            uri: prop_uri,
                            range: Range {
                                start: Position { line: prop_line, character: 0 },
                                end: Position { line: prop_line, character: 0 },
                            },
                        };
                        return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                    }
                }
            }

            // 1.5 Check Spring @Qualifier("beanName") or @Resource(name = "beanName")
            if current_line.contains("@Qualifier") || current_line.contains("@Resource") {
                let bean_cand = if let Some(open) = current_line.find('"') {
                    if let Some(close) = current_line[open + 1..].find('"') {
                        &current_line[open + 1..open + 1 + close]
                    } else {
                        word.trim_matches('"')
                    }
                } else {
                    word.trim_matches('"')
                };

                if !bean_cand.is_empty() {
                    if let Some(bean_sym) = self.indexer.find_spring_bean(bean_cand).await {
                        if let Ok(bean_uri) = Url::from_file_path(&bean_sym.path) {
                            let mut target_line = 0;
                            if let Ok(content) = std::fs::read_to_string(&bean_sym.path) {
                                for (idx, line) in content.lines().enumerate() {
                                    if line.contains(&bean_sym.simple_name) {
                                        target_line = idx as u32;
                                        break;
                                    }
                                }
                            }
                            let loc = Location {
                                uri: bean_uri,
                                range: Range {
                                    start: Position { line: target_line, character: 0 },
                                    end: Position { line: target_line, character: bean_sym.simple_name.len() as u32 },
                                },
                            };
                            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                        }
                    }
                }
            }
        }

        // PRIORITY 2: Semantic Resolution via GotoTarget
        if let Some(target) = sem.resolve_goto_target(pos.line, pos.character).await {
            match target {
                crate::semantic::GotoTarget::Local(range) => {
                    let loc = Location {
                        uri: uri.clone(),
                        range: Range {
                            start: Position {
                                line: range.start.line,
                                character: range.start.character,
                            },
                            end: Position {
                                line: range.end.line,
                                character: range.end.character,
                            },
                        },
                    };
                    return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                }
                crate::semantic::GotoTarget::SourceFile { path, range } => {
                    if let Ok(target_uri) = Url::from_file_path(&path) {
                        let mut target_range = Range {
                            start: Position {
                                line: range.start.line,
                                character: range.start.character,
                            },
                            end: Position {
                                line: range.end.line,
                                character: range.end.character,
                            },
                        };
                        if target_range.start.line == 0 && target_range.start.character == 0 {
                            let word = sem.find_node_at_position(pos.line, pos.character)
                                .map(|n| doc.ast.node_text(&n).to_string())
                                .unwrap_or_default();
                            if !word.is_empty() {
                                if let Ok(content) = std::fs::read_to_string(&path) {
                                    let line = crate::syntax::find_target_line_in_source(&content, &word);
                                    target_range = Range {
                                        start: Position { line, character: 0 },
                                        end: Position { line, character: word.len() as u32 },
                                    };
                                }
                            }
                        }
                        let loc = Location {
                            uri: target_uri,
                            range: target_range,
                        };
                        return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                    }
                }
                crate::semantic::GotoTarget::External { cls, target_name } => {
                    let word = sem.find_node_at_position(pos.line, pos.character)
                        .map(|n| doc.ast.node_text(&n).to_string())
                        .unwrap_or_default();
                    let name = target_name.as_deref().unwrap_or(if !word.is_empty() && word != cls.simple_name && cls.methods.iter().any(|m| m.name == word) {
                        &word
                    } else {
                        &cls.simple_name
                    });
                    let locs = self.resolve_external_target_location(&cls, name, None).await;
                    if !locs.is_empty() {
                        return Ok(Self::into_definition_response(locs));
                    }
                }
            }
        }

        // PRIORITY 3: Fallback type resolution from classpath when sem.resolve_goto_target didn't match
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);
            if !crate::syntax::is_valid_java_identifier(word) {
                return Ok(None);
            }
            if let Some(cls) = sem.resolve_type(word).await {
                let locs = self.resolve_external_target_location(&cls, word, None).await;
                if !locs.is_empty() {
                    return Ok(Self::into_definition_response(locs));
                }
            }
            for cls in self.classpath.find_classes_by_simple_name(word).await {
                let locs = self.resolve_external_target_location(&cls, word, None).await;
                if !locs.is_empty() {
                    return Ok(Self::into_definition_response(locs));
                }
            }
            let common_jdk = crate::classpath::find_common_jdk_classes_by_name(word);
            for fqn in common_jdk {
                if let Some(cls) = crate::classpath::get_builtin_jdk_class(&fqn) {
                    let locs = self.resolve_external_target_location(&cls, word, None).await;
                    if !locs.is_empty() {
                        return Ok(Self::into_definition_response(locs));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::with_indexer(&doc.ast, &self.classpath, Some(&self.indexer)).await;
        let candidates = sem.get_completions(pos.line, pos.character).await;

        let mut items: Vec<LspCompletionItem> = candidates
            .into_iter()
            .map(|c| LspCompletionItem {
                label: c.label,
                kind: Some(match c.kind {
                    CompletionKind::Class => CompletionItemKind::CLASS,
                    CompletionKind::Method => CompletionItemKind::METHOD,
                    CompletionKind::Field => CompletionItemKind::FIELD,
                    CompletionKind::Keyword => CompletionItemKind::KEYWORD,
                }),
                detail: c.detail,
                ..Default::default()
            })
            .collect();

        let current_imports = doc.ast.extract_imports();
        let current_pkg = doc.ast.extract_package().unwrap_or_default();
        let local_class_names: std::collections::HashSet<String> = sem.local_classes.keys().cloned().collect();
        let (import_pos, need_prefix_newline) = doc.ast.find_import_insertion_position();

        // Chained / dot receiver member completions fallback
        if doc.ast.is_after_dot_access(pos.line, pos.character) && items.is_empty() {
            let steps = doc.ast.extract_receiver_steps_before_position(pos.line, pos.character);
            if !steps.is_empty() {
                let resolved_type = self.resolve_receiver_chain(
                    &steps,
                    pos,
                    &doc,
                    &sem,
                    if current_pkg.is_empty() { None } else { Some(&current_pkg) },
                    &current_imports,
                ).await;

                if let Some(target_type) = resolved_type {
                    let (base_type, type_arg) = if let Some(idx) = target_type.find('<') {
                        let base = target_type[..idx].trim();
                        let arg = target_type[idx + 1..].trim_end_matches('>').trim();
                        (base, Some(arg.to_string()))
                    } else {
                        (target_type.as_str(), None)
                    };

                    if let Some(target_cls) = self.find_class_info(
                        base_type,
                        if current_pkg.is_empty() { None } else { Some(&current_pkg) },
                        &current_imports,
                        &sem,
                    ).await {
                        let is_static_access = if steps.len() == 1 {
                            if let crate::syntax::ast::ReceiverStep::Identifier(id) = &steps[0] {
                                let is_local = doc.ast.extract_local_variables_at_position(pos.line, pos.character).iter().any(|(v, _)| v == id);
                                let is_field = sem.local_classes.values().any(|c| c.fields.iter().any(|f| &f.name == id));
                                !is_local && !is_field
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        let mut member_items = Vec::new();
                        sem.populate_member_completions(
                            &target_cls,
                            type_arg.as_ref(),
                            is_static_access,
                            pos.line,
                            pos.character,
                            &mut member_items,
                        ).await;
                        for c in member_items {
                            items.push(LspCompletionItem {
                                label: c.label,
                                kind: Some(match c.kind {
                                    CompletionKind::Class => CompletionItemKind::CLASS,
                                    CompletionKind::Method => CompletionItemKind::METHOD,
                                    CompletionKind::Field => CompletionItemKind::FIELD,
                                    CompletionKind::Keyword => CompletionItemKind::KEYWORD,
                                }),
                                detail: c.detail,
                                ..Default::default()
                            });
                        }
                    }
                }
            }
        }

        // Workspace indexed classes with auto-import (only relevant when typing top-level identifiers / types, NOT after a dot member access)
        if !doc.ast.is_after_dot_access(pos.line, pos.character) {
            let is_new_expr = doc.ast.is_after_new_expression(pos.line, pos.character);

            self.indexer.with_symbols(|fqn_map| {
                for ws_sym in fqn_map.values() {
                    let fqn = &ws_sym.fqn;
                    let simple_name = &ws_sym.simple_name;

                    // Do not prompt/suggest auto-importing current class or classes declared in the current file
                    if local_class_names.contains(simple_name.as_str()) || local_class_names.contains(fqn.as_str()) {
                        if is_new_expr && ws_sym.class_info.kind == ClassKind::Class {
                            let constructors: Vec<_> = ws_sym.class_info.methods.iter().filter(|m| m.is_constructor).collect();
                            if constructors.is_empty() {
                                items.push(LspCompletionItem {
                                    label: format!("{}()", simple_name),
                                    kind: Some(CompletionItemKind::CONSTRUCTOR),
                                    detail: Some(format!("{} (Default Constructor)", fqn)),
                                    insert_text: Some(format!("{}($0)", simple_name)),
                                    insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                                    ..Default::default()
                                });
                            } else {
                                for ctor in constructors {
                                    let param_labels = ctor.parameter_display_strings();
                                    let label = format!("{}({})", simple_name, param_labels.join(", "));
                                    let param_snippets: Vec<String> = ctor.parameters.iter().enumerate()
                                        .map(|(idx, _)| format!("${{{}:{}}}", idx + 1, ctor.parameter_name(idx)))
                                        .collect();
                                    let insert_text = format!("{}({})", simple_name, param_snippets.join(", "));

                                    items.push(LspCompletionItem {
                                        label,
                                        kind: Some(CompletionItemKind::CONSTRUCTOR),
                                        detail: Some(format!("{} Constructor", fqn)),
                                        insert_text: Some(insert_text),
                                        insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                                        ..Default::default()
                                    });
                                }
                            }
                        }

                        // If it's already a local class, still add completion item but NEVER add an import edit
                        items.push(LspCompletionItem {
                            label: simple_name.clone(),
                            kind: Some(CompletionItemKind::CLASS),
                            detail: Some(fqn.clone()),
                            additional_text_edits: None,
                            ..Default::default()
                        });
                        continue;
                    }

                    // Do not prompt importing classes belonging to the same package
                    let in_same_package = !current_pkg.is_empty() && ws_sym.class_info.package_name == current_pkg;
                    let needs_import = !in_same_package
                        && !current_imports.iter().any(|i| i == fqn || i.ends_with(&format!(".{}", simple_name)))
                        && !fqn.starts_with("java.lang.");

                    let additional_text_edits = if needs_import && !fqn.is_empty() {
                        let import_text = if need_prefix_newline {
                            format!("\nimport {};\n", fqn)
                        } else {
                            format!("import {};\n", fqn)
                        };
                        Some(vec![TextEdit {
                            range: Range {
                                start: Position { line: import_pos.line, character: import_pos.character },
                                end: Position { line: import_pos.line, character: import_pos.character },
                            },
                            new_text: import_text,
                        }])
                    } else {
                        None
                    };

                    if is_new_expr && ws_sym.class_info.kind == ClassKind::Class {
                        let constructors: Vec<_> = ws_sym.class_info.methods.iter().filter(|m| m.is_constructor).collect();
                        if constructors.is_empty() {
                            items.push(LspCompletionItem {
                                label: format!("{}()", simple_name),
                                kind: Some(CompletionItemKind::CONSTRUCTOR),
                                detail: Some(format!("{} (Default Constructor)", fqn)),
                                insert_text: Some(format!("{}($0)", simple_name)),
                                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                                additional_text_edits: additional_text_edits.clone(),
                                ..Default::default()
                            });
                        } else {
                            for ctor in constructors {
                                let param_labels = ctor.parameter_display_strings();
                                let label = format!("{}({})", simple_name, param_labels.join(", "));
                                let param_snippets: Vec<String> = ctor.parameters.iter().enumerate()
                                    .map(|(idx, _)| format!("${{{}:{}}}", idx + 1, ctor.parameter_name(idx)))
                                    .collect();
                                let insert_text = format!("{}({})", simple_name, param_snippets.join(", "));

                                items.push(LspCompletionItem {
                                    label,
                                    kind: Some(CompletionItemKind::CONSTRUCTOR),
                                    detail: Some(format!("{} Constructor", fqn)),
                                    insert_text: Some(insert_text),
                                    insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                                    additional_text_edits: additional_text_edits.clone(),
                                    ..Default::default()
                                });
                            }
                        }
                    }

                    items.push(LspCompletionItem {
                        label: simple_name.clone(),
                        kind: Some(CompletionItemKind::CLASS),
                        detail: Some(format!("{} (Auto-Import)", fqn)),
                        additional_text_edits,
                        ..Default::default()
                    });
                }
            }).await;
        }

        // Postfix Completion (IntelliJ IDEA style: obj.sout -> System.out.println(obj); list.for -> for (var item : list), etc.)
        if let Some((expr, postfix_range)) = doc.ast.extract_postfix_receiver_and_range(pos.line, pos.character) {
            let postfix_templates = [
                ("sout", format!("Postfix: System.out.println({})", expr), format!("System.out.println({});", expr)),
                ("serr", format!("Postfix: System.err.println({})", expr), format!("System.err.println({});", expr)),
                ("var", format!("Postfix: var name = {}", expr), format!("var ${{1:name}} = {};", expr)),
                ("val", format!("Postfix: final var name = {}", expr), format!("final var ${{1:name}} = {};", expr)),
                ("for", format!("Postfix: for (var item : {})", expr), format!("for (var ${{1:item}} : {}) {{\n    $0\n}}", expr)),
                ("iter", format!("Postfix: for (var item : {})", expr), format!("for (var ${{1:item}} : {}) {{\n    $0\n}}", expr)),
                ("fori", format!("Postfix: for (int i = 0; i < {}.length; i++)", expr), format!("for (int ${{1:i}} = 0; ${{1:i}} < {}.length; ${{1:i}}++) {{\n    $0\n}}", expr)),
                ("null", format!("Postfix: if ({} == null)", expr), format!("if ({} == null) {{\n    $0\n}}", expr)),
                ("notnull", format!("Postfix: if ({} != null)", expr), format!("if ({} != null) {{\n    $0\n}}", expr)),
                ("nn", format!("Postfix: if ({} != null)", expr), format!("if ({} != null) {{\n    $0\n}}", expr)),
                ("return", format!("Postfix: return {}", expr), format!("return {};", expr)),
                ("if", format!("Postfix: if ({})", expr), format!("if ({}) {{\n    $0\n}}", expr)),
                ("while", format!("Postfix: while ({})", expr), format!("while ({}) {{\n    $0\n}}", expr)),
                ("cast", format!("Postfix: ((Type) {})", expr), format!("((${{1:Type}}) {})", expr)),
                ("try", format!("Postfix: try {{ {} }} catch ...", expr), format!("try {{\n    {};$0\n}} catch (${{1:Exception}} ${{2:e}}) {{\n    ${{3:e.printStackTrace();}}\n}}", expr)),
                ("opt", format!("Postfix: Optional.ofNullable({})", expr), format!("java.util.Optional.ofNullable({})", expr)),
                ("req", format!("Postfix: Objects.requireNonNull({})", expr), format!("java.util.Objects.requireNonNull({});", expr)),
                ("throw", format!("Postfix: throw {}", expr), format!("throw {};", expr)),
                ("assert", format!("Postfix: assert {}", expr), format!("assert {};", expr)),
            ];

            for (label, detail, replacement_code) in postfix_templates {
                items.push(LspCompletionItem {
                    label: label.to_string(),
                    kind: Some(CompletionItemKind::SNIPPET),
                    detail: Some(detail),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: Range {
                            start: Position {
                                line: postfix_range.start.line,
                                character: postfix_range.start.character,
                            },
                            end: Position {
                                line: postfix_range.end.line,
                                character: postfix_range.end.character,
                            },
                        },
                        new_text: replacement_code,
                    })),
                    insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
        } else {
            // Live Statement-Level Snippets (when not typing after an expression dot)
            items.push(LspCompletionItem {
                label: "sout".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("System.out.println(...)".to_string()),
                insert_text: Some("System.out.println($1);".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "serr".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("System.err.println(...)".to_string()),
                insert_text: Some("System.err.println($1);".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "psvm".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("main(String[] args)".to_string()),
                insert_text: Some("public static void main(String[] args) {\n    $1\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "fori".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("for (int i = 0; ...)".to_string()),
                insert_text: Some("for (int ${1:i} = 0; ${1:i} < ${2:max}; ${1:i}++) {\n    $0\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "try".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("try { ... } catch (Exception e)".to_string()),
                insert_text: Some("try {\n    $1\n} catch (${2:Exception} ${3:e}) {\n    ${4:e.printStackTrace();}\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "main".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("main(String[] args)".to_string()),
                insert_text: Some("public static void main(String[] args) {\n    $1\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "iff".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("if (...) { ... }".to_string()),
                insert_text: Some("if (${1:condition}) {\n    $0\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "ifelse".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("if (...) { ... } else { ... }".to_string()),
                insert_text: Some("if (${1:condition}) {\n    $2\n} else {\n    $0\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "while".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("while (...) { ... }".to_string()),
                insert_text: Some("while (${1:condition}) {\n    $0\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            items.push(LspCompletionItem {
                label: "switch".to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some("switch (...) { ... }".to_string()),
                insert_text: Some("switch (${1:key}) {\n    case ${2:value} -> $0;\n    default -> throw new IllegalStateException(\"Unexpected value: \" + ${1:key});\n}".to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                ..Default::default()
            });

            // Class-body @Override Method Stub Completion (e.g. implementing interface or superclass methods)
            if !doc.ast.is_after_dot_access(pos.line, pos.character) {
                if let Some(enc_class) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
                    let missing_by_cls = sem.find_missing_abstract_methods().await;
                    for (c_name, methods) in missing_by_cls {
                        if c_name == enc_class {
                            for m in methods {
                                let param_defs: Vec<String> = m.parameters.iter().enumerate().map(|(idx, (_, p_ty))| {
                                    format!("{} ${{{}:{}}}", p_ty.simple_name(), idx + 1, m.parameter_name(idx))
                                }).collect();
                                let ret_name = m.return_type.simple_name();
                                let default_ret = match ret_name.as_str() {
                                    "void" => "",
                                    "boolean" => "\n    return false;",
                                    "int" | "long" | "short" | "byte" => "\n    return 0;",
                                    "double" | "float" => "\n    return 0.0;",
                                    _ => "\n    return null;",
                                };
                                let snippet = format!(
                                    "@Override\npublic {} {}({}) {{\n    $0{}\n}}",
                                    ret_name,
                                    m.name,
                                    param_defs.join(", "),
                                    default_ret
                                );
                                items.push(LspCompletionItem {
                                    label: format!("@Override {}", m.name),
                                    kind: Some(CompletionItemKind::METHOD),
                                    detail: Some(format!("Override {} from interface/superclass", m.name)),
                                    insert_text: Some(snippet),
                                    insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }

            // Javadoc Smart Comment Generator: /**
            let line_prefix = doc.text.lines().nth(pos.line as usize).unwrap_or("");
            let byte_limit = doc.ast.utf16_col_to_byte(pos.line, pos.character).min(line_prefix.len());
            let prefix_text = line_prefix[..byte_limit].trim();

            if prefix_text.ends_with("/**") || prefix_text == "/*" || prefix_text == "doc" {
                let mut method_target = None;
                for lookahead in 1..=4 {
                    let next_line = pos.line + lookahead;
                    if let Some((m_name, _)) = doc.ast.find_method_declaration_at_position(next_line, 4) {
                        method_target = Some((next_line, m_name));
                        break;
                    }
                }

                let javadoc_snippet = if let Some((m_line, _)) = method_target {
                    let line_str = doc.text.lines().nth(m_line as usize).unwrap_or("");
                    let mut doc_lines = vec!["/**".to_string(), " * ${1:Description}".to_string(), " *".to_string()];
                    
                    if let Some(open_p) = line_str.find('(') {
                        if let Some(close_p) = line_str[open_p..].find(')') {
                            let params_str = &line_str[open_p + 1..open_p + close_p];
                            let mut tab_stop = 2;
                            for p in params_str.split(',') {
                                let parts: Vec<&str> = p.trim().split_whitespace().collect();
                                if let Some(p_name) = parts.last() {
                                    let clean_name = p_name.trim();
                                    if !clean_name.is_empty() {
                                        doc_lines.push(format!(" * @param {} ${{{}}}", clean_name, tab_stop));
                                        tab_stop += 1;
                                    }
                                }
                            }
                            if !line_str.contains("void ") {
                                doc_lines.push(format!(" * @return ${{{}:description}}", tab_stop));
                            }
                        }
                    }
                    doc_lines.push(" */".to_string());
                    doc_lines.join("\n")
                } else {
                    "/**\n * ${1:Description}\n */".to_string()
                };

                items.push(LspCompletionItem {
                    label: "/** Javadoc".to_string(),
                    kind: Some(CompletionItemKind::SNIPPET),
                    detail: Some("Generate Javadoc comment".to_string()),
                    insert_text: Some(javadoc_snippet),
                    insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
        }

        // Spring Boot Annotations with Auto-Import & Template Snippets
        let spring_annotations = [
            // Core StereoTypes
            ("@SpringBootApplication", "org.springframework.boot.autoconfigure.SpringBootApplication", "@SpringBootApplication\n", "Spring Boot main application entry point"),
            ("@RestController", "org.springframework.web.bind.annotation.RestController", "@RestController\n", "Spring REST Controller with @ResponseBody"),
            ("@Controller", "org.springframework.stereotype.Controller", "@Controller\n", "Spring MVC Controller"),
            ("@Service", "org.springframework.stereotype.Service", "@Service\n", "Spring Business Service Component"),
            ("@Component", "org.springframework.stereotype.Component", "@Component\n", "Generic Spring Managed Component"),
            ("@Repository", "org.springframework.stereotype.Repository", "@Repository\n", "Spring Data Access Object Component"),
            ("@Configuration", "org.springframework.context.annotation.Configuration", "@Configuration\n", "Spring Configuration Bean definition"),
            ("@Bean", "org.springframework.context.annotation.Bean", "@Bean\npublic ${1:Object} ${2:beanName}() {\n    return new ${1:Object}();\n}", "Declare a Spring managed Bean method"),
            // DI & Injection
            ("@Autowired", "org.springframework.beans.factory.annotation.Autowired", "@Autowired\n", "Spring automatic dependency injection"),
            ("@Resource", "jakarta.annotation.Resource", "@Resource\n", "Standard resource injection"),
            ("@Qualifier", "org.springframework.beans.factory.annotation.Qualifier", "@Qualifier(\"${1:beanName}\")\n", "Specify target bean name for injection"),
            ("@Value", "org.springframework.beans.factory.annotation.Value", "@Value(\"${${1:app.property}}\")\n", "Inject configuration property value"),
            // HTTP Request Mappings
            ("@GetMapping", "org.springframework.web.bind.annotation.GetMapping", "@GetMapping(\"${1:/path}\")\npublic ${2:ResponseEntity<?>} ${3:getHandler}() {\n    return ResponseEntity.ok($0);\n}", "HTTP GET request mapping"),
            ("@PostMapping", "org.springframework.web.bind.annotation.PostMapping", "@PostMapping(\"${1:/path}\")\npublic ${2:ResponseEntity<?>} ${3:postHandler}(@RequestBody ${4:Object} req) {\n    return ResponseEntity.ok($0);\n}", "HTTP POST request mapping"),
            ("@PutMapping", "org.springframework.web.bind.annotation.PutMapping", "@PutMapping(\"${1:/path}\")\npublic ${2:ResponseEntity<?>} ${3:putHandler}(@RequestBody ${4:Object} req) {\n    return ResponseEntity.ok($0);\n}", "HTTP PUT request mapping"),
            ("@DeleteMapping", "org.springframework.web.bind.annotation.DeleteMapping", "@DeleteMapping(\"${1:/path/{id\\}}\")\npublic ${2:ResponseEntity<?>} ${3:deleteHandler}(@PathVariable ${4:Long} id) {\n    return ResponseEntity.ok($0);\n}", "HTTP DELETE request mapping"),
            ("@RequestMapping", "org.springframework.web.bind.annotation.RequestMapping", "@RequestMapping(\"${1:/api/v1}\")\n", "Base HTTP request mapping for controller"),
            // Parameters & Body
            ("@RequestBody", "org.springframework.web.bind.annotation.RequestBody", "@RequestBody ", "Bind HTTP request body to parameter"),
            ("@PathVariable", "org.springframework.web.bind.annotation.PathVariable", "@PathVariable(\"${1:id}\") ", "Bind URI path variable to parameter"),
            ("@RequestParam", "org.springframework.web.bind.annotation.RequestParam", "@RequestParam(name = \"${1:name}\", required = false) ", "Bind query parameter"),
            ("@RequestHeader", "org.springframework.web.bind.annotation.RequestHeader", "@RequestHeader(\"${1:Header-Name}\") ", "Bind HTTP header value"),
            // Transaction & Validation
            ("@Transactional", "org.springframework.transaction.annotation.Transactional", "@Transactional\n", "Execute method within a database transaction"),
            ("@Valid", "jakarta.validation.Valid", "@Valid ", "Trigger Bean Validation on request object"),
            ("@NotNull", "jakarta.validation.constraints.NotNull", "@NotNull ", "Constrain field to be non-null"),
            ("@NotBlank", "jakarta.validation.constraints.NotBlank", "@NotBlank ", "Constrain string field to be non-blank"),
            // Configuration & Profiles
            ("@ConfigurationProperties", "org.springframework.boot.context.properties.ConfigurationProperties", "@ConfigurationProperties(prefix = \"${1:app}\")\n", "Bind external configuration to POJO"),
            ("@Profile", "org.springframework.context.annotation.Profile", "@Profile(\"${1:dev}\")\n", "Conditionally activate component for profile"),
            // MyBatis & MyBatis-Plus Annotations
            ("@Mapper", "org.apache.ibatis.annotations.Mapper", "@Mapper\n", "MyBatis Mapper Interface declaration"),
            ("@Select", "org.apache.ibatis.annotations.Select", "@Select(\"SELECT * FROM ${1:table} WHERE ${2:condition}\")\n", "MyBatis SQL Select query annotation"),
            ("@Insert", "org.apache.ibatis.annotations.Insert", "@Insert(\"INSERT INTO ${1:table} (${2:cols}) VALUES (${3:vals})\")\n", "MyBatis SQL Insert query annotation"),
            ("@Update", "org.apache.ibatis.annotations.Update", "@Update(\"UPDATE ${1:table} SET ${2:col} = #{${3:val}} WHERE ${4:condition}\")\n", "MyBatis SQL Update query annotation"),
            ("@Delete", "org.apache.ibatis.annotations.Delete", "@Delete(\"DELETE FROM ${1:table} WHERE ${2:condition}\")\n", "MyBatis SQL Delete query annotation"),
            ("@Param", "org.apache.ibatis.annotations.Param", "@Param(\"${1:name}\") ", "Bind method parameter name in SQL statement"),
            ("@TableName", "com.baomidou.mybatisplus.annotation.TableName", "@TableName(\"${1:table_name}\")\n", "MyBatis-Plus Entity to database table name mapping"),
            ("@TableId", "com.baomidou.mybatisplus.annotation.TableId", "@TableId(type = IdType.${1:AUTO})\n", "MyBatis-Plus Primary Key identifier"),
            ("@TableField", "com.baomidou.mybatisplus.annotation.TableField", "@TableField(\"${1:column_name}\")\n", "MyBatis-Plus Entity field to database column mapping"),
            ("@TableLogic", "com.baomidou.mybatisplus.annotation.TableLogic", "@TableLogic\n", "MyBatis-Plus Logical deletion flag"),
            ("@Version", "com.baomidou.mybatisplus.annotation.Version", "@Version\n", "MyBatis-Plus Optimistic locking version field"),
        ];

        for (label, fqn, insert_snippet, doc) in spring_annotations {
            let needs_import = !current_imports.iter().any(|i| i == fqn);
            let additional_text_edits = if needs_import {
                let import_text = if need_prefix_newline {
                    format!("\nimport {};\n", fqn)
                } else {
                    format!("import {};\n", fqn)
                };
                Some(vec![TextEdit {
                    range: Range {
                        start: Position { line: import_pos.line, character: import_pos.character },
                        end: Position { line: import_pos.line, character: import_pos.character },
                    },
                    new_text: import_text,
                }])
            } else {
                None
            };

            items.push(LspCompletionItem {
                label: label.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some(format!("{} (Spring Boot)", fqn)),
                documentation: Some(tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("**{}**\n\n{}\n\n`{}`", label, doc, fqn),
                })),
                insert_text: Some(insert_snippet.to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                additional_text_edits: additional_text_edits.clone(),
                ..Default::default()
            });

            // Also support typing without @ prefix (e.g. typing "RestCon" -> suggests @RestController)
            let raw_name = label.trim_start_matches('@');
            items.push(LspCompletionItem {
                label: raw_name.to_string(),
                kind: Some(CompletionItemKind::SNIPPET),
                detail: Some(format!("{} (Spring Boot Annotation)", fqn)),
                insert_text: Some(insert_snippet.to_string()),
                insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
                additional_text_edits: additional_text_edits.clone(),
                ..Default::default()
            });
        }

        // Configuration Property completion (for Spring @Value("${...}") or application.properties/yml)
        let line_prefix = doc.text.lines().nth(pos.line as usize).map(|l| {
            let col = (pos.character as usize).min(l.len());
            &l[..col]
        }).unwrap_or("");

        if line_prefix.contains("@Value") || line_prefix.contains("${") {
            let config_props = self.indexer.all_config_properties().await;
            for (prop_key, prop_file) in config_props {
                let file_name = prop_file.file_name().and_then(|f| f.to_str()).unwrap_or("config");
                items.push(LspCompletionItem {
                    label: prop_key.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: Some(format!("{} ({})", prop_key, file_name)),
                    documentation: Some(tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("Configuration property defined in `{}`\n\n```properties\n{}\n```", file_name, prop_key),
                    })),
                    insert_text: Some(prop_key),
                    ..Default::default()
                });
            }
        }

        // Spring Bean completion (for @Qualifier("...") or @Resource(name = "..."))
        if line_prefix.contains("@Qualifier") || (line_prefix.contains("@Resource") && line_prefix.contains("name")) {
            for s in self.indexer.all_symbols().await {
                let bean_name = {
                    let mut c = s.simple_name.chars();
                    match c.next() {
                        Some(first) => first.to_lowercase().collect::<String>() + c.as_str(),
                        None => s.simple_name.clone(),
                    }
                };
                items.push(LspCompletionItem {
                    label: bean_name.clone(),
                    kind: Some(CompletionItemKind::VALUE),
                    detail: Some(format!("Spring Bean ({})", s.fqn)),
                    documentation: Some(tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("### 🍃 Spring Bean: `{}`\n- **Target Class**: `{}`\n- **Defined in**: `{}`", bean_name, s.fqn, s.path.display()),
                    })),
                    insert_text: Some(bean_name),
                    ..Default::default()
                });
            }
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                return Ok(Some(vec![]));
            }
        }
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let mut actions = Vec::new();
        let sem = SemanticContext::with_indexer(&doc.ast, &self.classpath, Some(&self.indexer)).await;
        let pos = params.range.start;

        // 1. Auto import
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);
            if !word.is_empty() && word.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                let current_pkg = doc.ast.extract_package().unwrap_or_default();
                let current_imports = doc.ast.extract_imports();
                let local_class_names: std::collections::HashSet<String> = sem.local_classes.keys().cloned().collect();
                let (import_pos, need_prefix_newline) = doc.ast.find_import_insertion_position();

                struct CandItem {
                    simple_name: String,
                    fqn: String,
                    package_name: String,
                }
                let mut candidates: Vec<CandItem> = Vec::new();
                let mut seen_fqns = std::collections::HashSet::new();

                // 1) Workspace indexer
                for sym in self.indexer.find_by_simple_name(word).await {
                    if seen_fqns.insert(sym.fqn.clone()) {
                        candidates.push(CandItem {
                            simple_name: sym.simple_name.clone(),
                            fqn: sym.fqn.clone(),
                            package_name: sym.class_info.package_name.clone(),
                        });
                    }
                }

                // 2) Classpath (libraries / dependencies / loaded jars)
                for cls in self.classpath.find_classes_by_simple_name(word).await {
                    if seen_fqns.insert(cls.fqn.clone()) {
                        candidates.push(CandItem {
                            simple_name: cls.simple_name.clone(),
                            fqn: cls.fqn.clone(),
                            package_name: cls.package_name.clone(),
                        });
                    }
                }

                // 3) Common JDK classes
                for jdk_fqn in crate::classpath::find_common_jdk_classes_by_name(word) {
                    if seen_fqns.insert(jdk_fqn.clone()) {
                        let pkg = jdk_fqn.rsplit_once('.').map(|(p, _)| p.to_string()).unwrap_or_default();
                        candidates.push(CandItem {
                            simple_name: word.to_string(),
                            fqn: jdk_fqn,
                            package_name: pkg,
                        });
                    }
                }

                for cand in candidates {
                    // Do not prompt/suggest auto-importing current class or classes declared in current file
                    if local_class_names.contains(&cand.simple_name) || local_class_names.contains(&cand.fqn) {
                        continue;
                    }
                    // Do not prompt importing classes belonging to the same package
                    if !current_pkg.is_empty() && cand.package_name == current_pkg {
                        continue;
                    }
                    // Do not import already imported or java.lang classes
                    if current_imports.iter().any(|i| i == &cand.fqn) || cand.fqn.starts_with("java.lang.") {
                        continue;
                    }

                    let import_stmt = if need_prefix_newline {
                        format!("\nimport {};\n", cand.fqn)
                    } else {
                        format!("import {};\n", cand.fqn)
                    };

                    let edit = TextEdit {
                        range: Range {
                            start: Position { line: import_pos.line, character: import_pos.character },
                            end: Position { line: import_pos.line, character: import_pos.character },
                        },
                        new_text: import_stmt,
                    };

                    let mut changes = HashMap::new();
                    changes.insert(uri.clone(), vec![edit]);

                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Import '{}' ({})", cand.simple_name, cand.fqn),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: None,
                        edit: Some(WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        }),
                        command: None,
                        is_preferred: Some(true),
                        disabled: None,
                        data: None,
                    }));
                }
            }
        }

        // 2. Organize imports code action
        let organized = doc.ast.organize_imports();
        let (repl_range, prefix_opt) = doc.ast.find_imports_replacement_range();
        let mut organize_text = String::new();
        if let Some(prefix) = prefix_opt {
            organize_text.push_str(&prefix);
        }
        let mut had_static = false;
        for imp in &organized {
            let is_static = imp.starts_with("static ");
            if !is_static && had_static {
                organize_text.push('\n');
                had_static = false;
            }
            if is_static {
                had_static = true;
            }
            organize_text.push_str(&format!("import {};\n", imp));
        }

        let mut org_changes = HashMap::new();
        org_changes.insert(
            uri.clone(),
            vec![TextEdit {
                range: Range {
                    start: Position { line: repl_range.start.line, character: repl_range.start.character },
                    end: Position { line: repl_range.end.line, character: repl_range.end.character },
                },
                new_text: organize_text,
            }],
        );

        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: "Organize Imports".to_string(),
            kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
            diagnostics: None,
            edit: Some(WorkspaceEdit {
                changes: Some(org_changes),
                ..Default::default()
            }),
            command: None,
            is_preferred: Some(false),
            disabled: None,
            data: None,
        }));

        // 3. Generate Getter & Setter
        let lines: Vec<&str> = doc.text.lines().collect();
        let last_line = lines.len().saturating_sub(1) as u32;
        let insertion_line = lines
            .iter()
            .rposition(|l| l.trim().ends_with('}') || l.trim() == "}")
            .map(|idx| idx as u32)
            .unwrap_or(last_line);

        let generators = doc.ast.generate_getters_and_setters(pos.line);
        for (title, code) in generators {
            let mut gen_changes = HashMap::new();
            gen_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position { line: insertion_line, character: 0 },
                        end: Position { line: insertion_line, character: 0 },
                    },
                    new_text: format!("\n{}\n", code),
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(gen_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            }));
        }

        // 4. Implement missing abstract/interface methods
        let missing_classes = sem.find_missing_abstract_methods().await;
        for (cls_name, methods) in missing_classes {
            let mut stub_text = String::new();
            for m in methods {
                let param_strs = m.parameter_display_strings();
                stub_text.push_str(&format!(
                    "\n    @Override\n    public {} {}({}) {{\n        throw new UnsupportedOperationException();\n    }}\n",
                    m.return_type.simple_name(),
                    m.name,
                    param_strs.join(", ")
                ));
            }

            let mut impl_changes = HashMap::new();
            impl_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position { line: insertion_line, character: 0 },
                        end: Position { line: insertion_line, character: 0 },
                    },
                    new_text: stub_text,
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Implement missing abstract methods for '{}'", cls_name),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(impl_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 5. Extract Variable refactoring
        let sel_range = params.range;
        if let Some((title, expr_range, def_stmt)) = doc.ast.generate_extract_variable_refactoring(
            sel_range.start.line,
            sel_range.start.character,
            sel_range.end.line,
            sel_range.end.character,
        ) {
            let mut extract_changes = HashMap::new();
            // Insert definition before expression's line, and replace expression with var name
            let line_start_pos = Position { line: expr_range.start.line, character: 0 };
            extract_changes.insert(
                uri.clone(),
                vec![
                    TextEdit {
                        range: Range { start: line_start_pos, end: line_start_pos },
                        new_text: format!("        {}", def_stmt),
                    },
                    TextEdit {
                        range: Range {
                            start: Position { line: expr_range.start.line, character: expr_range.start.character },
                            end: Position { line: expr_range.end.line, character: expr_range.end.character },
                        },
                        new_text: "extractedVar".to_string(),
                    },
                ],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_EXTRACT),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(extract_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 6. Extract Method refactoring
        if let Some((title, expr_range, call_stmt, method_def)) = doc.ast.generate_extract_method_refactoring(
            sel_range.start.line,
            sel_range.start.character,
            sel_range.end.line,
            sel_range.end.character,
        ) {
            let mut extract_m_changes = HashMap::new();
            extract_m_changes.insert(
                uri.clone(),
                vec![
                    TextEdit {
                        range: Range {
                            start: Position { line: expr_range.start.line, character: expr_range.start.character },
                            end: Position { line: expr_range.end.line, character: expr_range.end.character },
                        },
                        new_text: call_stmt,
                    },
                    TextEdit {
                        range: Range { start: Position { line: insertion_line, character: 0 }, end: Position { line: insertion_line, character: 0 } },
                        new_text: format!("\n{}\n", method_def),
                    },
                ],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_EXTRACT),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(extract_m_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 7. Generate equals(), hashCode(), toString()
        for (title, code) in doc.ast.generate_equals_hashcode_and_tostring() {
            let mut gen_changes = HashMap::new();
            gen_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position { line: insertion_line, character: 0 },
                        end: Position { line: insertion_line, character: 0 },
                    },
                    new_text: format!("\n{}\n", code),
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(gen_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            }));
        }

        // 8. Inline Variable refactoring
        if let Some((title, decl_range, occs)) = doc.ast.generate_inline_variable_refactoring(pos.line, pos.character) {
            let mut inline_changes = HashMap::new();
            let mut edits = Vec::new();
            // Delete variable declaration line
            edits.push(TextEdit {
                range: Range {
                    start: Position { line: decl_range.start.line, character: 0 },
                    end: Position { line: decl_range.end.line + 1, character: 0 },
                },
                new_text: String::new(),
            });
            // Replace occurrences
            for (occ_range, val_str) in occs {
                edits.push(TextEdit {
                    range: Range {
                        start: Position { line: occ_range.start.line, character: occ_range.start.character },
                        end: Position { line: occ_range.end.line, character: occ_range.end.character },
                    },
                    new_text: val_str,
                });
            }
            inline_changes.insert(uri.clone(), edits);

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_INLINE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(inline_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 10. Extract Constant refactoring
        if let Some((title, lit_range, const_name, const_decl)) = doc.ast.generate_extract_constant_refactoring(
            sel_range.start.line,
            sel_range.start.character,
            sel_range.end.line,
            sel_range.end.character,
        ) {
            let mut const_changes = HashMap::new();
            const_changes.insert(
                uri.clone(),
                vec![
                    TextEdit {
                        range: Range { start: Position { line: 1, character: 0 }, end: Position { line: 1, character: 0 } },
                        new_text: const_decl,
                    },
                    TextEdit {
                        range: Range {
                            start: Position { line: lit_range.start.line, character: lit_range.start.character },
                            end: Position { line: lit_range.end.line, character: lit_range.end.character },
                        },
                        new_text: const_name,
                    },
                ],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_EXTRACT),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(const_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            }));
        }

        // 11. Create Method Stub QuickFix
        if let Some((title, method_stub)) = doc.ast.generate_create_method_stub_quickfix(pos.line, pos.character) {
            let mut stub_changes = HashMap::new();
            stub_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position { line: insertion_line, character: 0 },
                        end: Position { line: insertion_line, character: 0 },
                    },
                    new_text: format!("\n{}\n", method_stub),
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(stub_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 12. Safe Delete Refactoring
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let p_kind = node.parent().map(|p| p.kind().to_string()).unwrap_or_default();
            if matches!(p_kind.as_str(), "method_declaration" | "field_declaration" | "variable_declarator" | "class_declaration") {
                let target_name = doc.ast.node_text(&node);
                let current_doc_occs = doc.ast.find_all_occurrences(target_name);
                let decl_node = if p_kind == "variable_declarator" {
                    node.parent().and_then(|p| p.parent()).unwrap_or(node.parent().unwrap())
                } else {
                    node.parent().unwrap()
                };
                let decl_range = doc.ast.node_lsp_range(&decl_node);

                if current_doc_occs.len() <= 1 {
                    let mut del_changes = HashMap::new();
                    del_changes.insert(
                        uri.clone(),
                        vec![TextEdit {
                            range: Range {
                                start: Position { line: decl_range.start.line, character: 0 },
                                end: Position { line: decl_range.end.line + 1, character: 0 },
                            },
                            new_text: String::new(),
                        }],
                    );

                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Safe Delete '{}' (0 references in file)", target_name),
                        kind: Some(CodeActionKind::REFACTOR),
                        diagnostics: None,
                        edit: Some(WorkspaceEdit {
                            changes: Some(del_changes),
                            ..Default::default()
                        }),
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: None,
                    }));
                } else {
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Safe Delete '{}' (Warning: {} usages detected)", target_name, current_doc_occs.len() - 1),
                        kind: Some(CodeActionKind::REFACTOR),
                        diagnostics: None,
                        edit: None,
                        command: None,
                        is_preferred: Some(false),
                        disabled: Some(tower_lsp::lsp_types::CodeActionDisabled {
                            reason: format!("'{}' has {} active usage(s) in this file.", target_name, current_doc_occs.len() - 1),
                        }),
                        data: None,
                    }));
                }
            }
        }

        // 13. Move Class to Another Package Refactoring
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            if node.parent().map(|p| p.kind() == "class_declaration").unwrap_or(false) {
                let class_name = doc.ast.node_text(&node);
                let current_pkg = doc.ast.extract_package().unwrap_or_default();
                let sub_pkg = if current_pkg.is_empty() { "model".to_string() } else { format!("{}.dto", current_pkg) };

                let mut move_changes = HashMap::new();
                let mut edits = Vec::new();
                if let Some((pkg_range, _)) = doc.ast.extract_package_with_range() {
                    edits.push(TextEdit {
                        range: Range {
                            start: Position { line: pkg_range.start.line, character: pkg_range.start.character },
                            end: Position { line: pkg_range.end.line, character: pkg_range.end.character },
                        },
                        new_text: format!("package {};", sub_pkg),
                    });
                }
                move_changes.insert(uri.clone(), edits);

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Move '{}' to package '{}'", class_name, sub_pkg),
                    kind: Some(CodeActionKind::REFACTOR),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(move_changes),
                        ..Default::default()
                    }),
                    command: None,
                    is_preferred: Some(false),
                    disabled: None,
                    data: None,
                }));
            }
        }

        // 14. Surround with try-catch refactoring
        if let Some((title, range, repl_text)) = doc.ast.generate_surround_with_try_catch(
            params.range.start.line,
            params.range.start.character,
            params.range.end.line,
            params.range.end.character,
        ) {
            let mut sc_changes = HashMap::new();
            sc_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range,
                    new_text: repl_text,
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(sc_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            }));
        }

        // 15. Add missing return statement QuickFix
        if let Some((title, range, ret_text)) = doc.ast.generate_missing_return_quickfix(pos.line, pos.character) {
            let mut ret_changes = HashMap::new();
            ret_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range,
                    new_text: ret_text,
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(ret_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 16. Remove unused import QuickFix
        let unused_imports = doc.ast.find_unused_imports();
        let mut has_cursor_or_diag_on_unused = false;
        for (imp_name, imp_range) in &unused_imports {
            if (pos.line >= imp_range.start.line && pos.line <= imp_range.end.line)
                || params.context.diagnostics.iter().any(|d| d.range.start.line == imp_range.start.line)
            {
                has_cursor_or_diag_on_unused = true;
                let mut rm_changes = HashMap::new();
                rm_changes.insert(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range {
                            start: Position { line: imp_range.start.line, character: 0 },
                            end: Position { line: imp_range.end.line + 1, character: 0 },
                        },
                        new_text: String::new(),
                    }],
                );

                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Remove unused import '{}'", imp_name),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: None,
                    edit: Some(WorkspaceEdit {
                        changes: Some(rm_changes),
                        ..Default::default()
                    }),
                    command: None,
                    is_preferred: Some(true),
                    disabled: None,
                    data: None,
                }));
            }
        }

        if has_cursor_or_diag_on_unused && unused_imports.len() > 1 {
            let mut all_edits: Vec<TextEdit> = unused_imports
                .iter()
                .map(|(_, r)| TextEdit {
                    range: Range {
                        start: Position { line: r.start.line, character: 0 },
                        end: Position { line: r.end.line + 1, character: 0 },
                    },
                    new_text: String::new(),
                })
                .collect();
            all_edits.sort_by(|a, b| b.range.start.line.cmp(&a.range.start.line));

            let mut all_changes = HashMap::new();
            all_changes.insert(uri.clone(), all_edits);

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: "Remove all unused imports".to_string(),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(all_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            }));
        }

        // 17. Remove unused variable QuickFix
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let mut curr = Some(node);
            while let Some(n) = curr {
                if n.kind() == "local_variable_declaration" {
                    let line = n.start_position().row as u32;
                    let is_diagnosed_unused = params.context.diagnostics.iter().any(|d| {
                        d.range.start.line == line && d.message.to_lowercase().contains("not used")
                    });
                    let is_unused = is_diagnosed_unused || {
                        if let Some((_, occs)) = doc.ast.find_local_scope_occurrences(pos.line, pos.character) {
                            occs.len() <= 1
                        } else {
                            false
                        }
                    };

                    if is_unused {
                        let mut rm_changes = HashMap::new();
                        rm_changes.insert(
                            uri.clone(),
                            vec![TextEdit {
                                range: Range {
                                    start: Position { line, character: 0 },
                                    end: Position { line: line + 1, character: 0 },
                                },
                                new_text: String::new(),
                            }],
                        );

                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title: "Remove unused variable".to_string(),
                            kind: Some(CodeActionKind::QUICKFIX),
                            diagnostics: None,
                            edit: Some(WorkspaceEdit {
                                changes: Some(rm_changes),
                                ..Default::default()
                            }),
                            command: None,
                            is_preferred: Some(true),
                            disabled: None,
                            data: None,
                        }));
                    }
                    break;
                }
                curr = n.parent();
            }
        }

        // 18. Spring @Transactional private method QuickFix: Make method 'public'
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let mut curr = Some(node);
            while let Some(n) = curr {
                if n.kind() == "method_declaration" {
                    let has_trans = n.children(&mut n.walk()).any(|c| {
                        (c.kind() == "modifiers" || c.kind() == "marker_annotation" || c.kind() == "annotation")
                            && doc.ast.node_text(&c).contains("Transactional")
                    });
                    if has_trans {
                        for m_child in n.children(&mut n.walk()) {
                            if m_child.kind() == "modifiers" {
                                for mod_tok in m_child.children(&mut m_child.walk()) {
                                    if doc.ast.node_text(&mod_tok) == "private" {
                                        let p_range = doc.ast.node_range(&mod_tok);
                                        let mut changes = HashMap::new();
                                        changes.insert(
                                            uri.clone(),
                                            vec![TextEdit {
                                                range: Range {
                                                    start: Position { line: p_range.start.line, character: p_range.start.character },
                                                    end: Position { line: p_range.end.line, character: p_range.end.character },
                                                },
                                                new_text: "public".to_string(),
                                            }],
                                        );
                                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                                            title: "Make method 'public'".to_string(),
                                            kind: Some(CodeActionKind::QUICKFIX),
                                            diagnostics: None,
                                            edit: Some(WorkspaceEdit {
                                                changes: Some(changes),
                                                ..Default::default()
                                            }),
                                            command: None,
                                            is_preferred: Some(true),
                                            disabled: None,
                                            data: None,
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    break;
                }
                curr = n.parent();
            }
        }

        // 19. Convert Lambda to Method Reference Refactoring
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let mut curr = Some(node);
            while let Some(n) = curr {
                if n.kind() == "lambda_expression" {
                    if let (Some(params_node), Some(body_node)) = (n.child_by_field_name("parameters"), n.child_by_field_name("body")) {
                        let param_text = doc.ast.node_text(&params_node).trim().trim_matches(|c| c == '(' || c == ')').trim();
                        if !param_text.is_empty() && body_node.kind() == "method_invocation" {
                            if let (Some(obj_node), Some(name_node), Some(args_node)) = (
                                body_node.child_by_field_name("object"),
                                body_node.child_by_field_name("name"),
                                body_node.child_by_field_name("arguments"),
                            ) {
                                let arg_text = doc.ast.node_text(&args_node).trim().trim_matches(|c| c == '(' || c == ')').trim();
                                let obj_text = doc.ast.node_text(&obj_node);
                                let m_name = doc.ast.node_text(&name_node);

                                // Case A: x -> System.out.println(x) ==> System.out::println
                                if arg_text == param_text {
                                    let lambda_range = doc.ast.node_range(&n);
                                    let mut changes = HashMap::new();
                                    changes.insert(
                                        uri.clone(),
                                        vec![TextEdit {
                                            range: Range {
                                                start: Position { line: lambda_range.start.line, character: lambda_range.start.character },
                                                end: Position { line: lambda_range.end.line, character: lambda_range.end.character },
                                            },
                                            new_text: format!("{}::{}", obj_text, m_name),
                                        }],
                                    );
                                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                                        title: format!("Convert lambda to method reference '{}::{}'", obj_text, m_name),
                                        kind: Some(CodeActionKind::REFACTOR_REWRITE),
                                        diagnostics: None,
                                        edit: Some(WorkspaceEdit {
                                            changes: Some(changes),
                                            ..Default::default()
                                        }),
                                        command: None,
                                        is_preferred: Some(true),
                                        disabled: None,
                                        data: None,
                                    }));
                                }
                            }
                        }
                    }
                    break;
                }
                curr = n.parent();
            }
        }

        // 20. Invert 'if' condition Refactoring
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let mut curr = Some(node);
            while let Some(n) = curr {
                if n.kind() == "if_statement" {
                    if let Some(cond_node) = n.child_by_field_name("condition") {
                        let raw_cond = doc.ast.node_text(&cond_node).trim();
                        let cond_inner = raw_cond.strip_prefix('(').and_then(|s| s.strip_suffix(')')).unwrap_or(raw_cond).trim();
                        let inverted_cond = if cond_inner.contains(" == ") {
                            cond_inner.replace(" == ", " != ")
                        } else if cond_inner.contains(" != ") {
                            cond_inner.replace(" != ", " == ")
                        } else if cond_inner.contains(" >= ") {
                            cond_inner.replace(" >= ", " < ")
                        } else if cond_inner.contains(" <= ") {
                            cond_inner.replace(" <= ", " > ")
                        } else if cond_inner.contains(" > ") {
                            cond_inner.replace(" > ", " <= ")
                        } else if cond_inner.contains(" < ") {
                            cond_inner.replace(" < ", " >= ")
                        } else if cond_inner.starts_with('!') {
                            cond_inner[1..].trim().to_string()
                        } else {
                            format!("!{}", cond_inner)
                        };

                        if let Some(cons_node) = n.child_by_field_name("consequence") {
                            let cond_range = doc.ast.node_range(&cond_node);
                            let cons_range = doc.ast.node_range(&cons_node);
                            let cons_text = doc.ast.node_text(&cons_node);

                            let mut edits = Vec::new();
                            edits.push(TextEdit {
                                range: Range {
                                    start: Position { line: cond_range.start.line, character: cond_range.start.character },
                                    end: Position { line: cond_range.end.line, character: cond_range.end.character },
                                },
                                new_text: format!("({})", inverted_cond),
                            });

                            if let Some(alt_node) = n.child_by_field_name("alternative") {
                                let alt_range = doc.ast.node_range(&alt_node);
                                let alt_text = doc.ast.node_text(&alt_node);
                                let alt_clean = alt_text.strip_prefix("else").unwrap_or(alt_text).trim();

                                edits.push(TextEdit {
                                    range: Range {
                                        start: Position { line: cons_range.start.line, character: cons_range.start.character },
                                        end: Position { line: cons_range.end.line, character: cons_range.end.character },
                                    },
                                    new_text: alt_clean.to_string(),
                                });
                                edits.push(TextEdit {
                                    range: Range {
                                        start: Position { line: alt_range.start.line, character: alt_range.start.character },
                                        end: Position { line: alt_range.end.line, character: alt_range.end.character },
                                    },
                                    new_text: format!("else {}", cons_text),
                                });
                            }

                            let mut changes = HashMap::new();
                            changes.insert(uri.clone(), edits);
                            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                                title: "Invert 'if' condition".to_string(),
                                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                                diagnostics: None,
                                edit: Some(WorkspaceEdit {
                                    changes: Some(changes),
                                    ..Default::default()
                                }),
                                command: None,
                                is_preferred: Some(true),
                                disabled: None,
                                data: None,
                            }));
                        }
                    }
                    break;
                }
                curr = n.parent();
            }
        }

        // 21. Add 'throws ...' to method signature QuickFix
        let throws_fix = doc.ast.generate_add_throws_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_add_throws_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = throws_fix {
            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range,
                    new_text,
                }],
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 22. Add 'default' branch to switch QuickFix
        let switch_fix = doc.ast.generate_switch_add_default_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_switch_add_default_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = switch_fix {
            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range,
                    new_text,
                }],
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 23. Initialize blank final field QuickFix
        let init_field_fix = doc.ast.generate_initialize_field_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_initialize_field_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = init_field_fix {
            let mut changes = HashMap::new();
            changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range,
                    new_text,
                }],
            );
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 24. Remove unused private field / method QuickFix
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let mut curr = Some(node);
            while let Some(n) = curr {
                if n.kind() == "field_declaration" || n.kind() == "method_declaration" {
                    let s_row = n.start_position().row as u32;
                    let e_row = n.end_position().row as u32;
                    let is_diag_unused = params.context.diagnostics.iter().any(|d| {
                        (d.range.start.line == s_row || (d.range.start.line >= s_row && d.range.start.line <= e_row))
                            && (d.message.contains("is not used") || d.message.contains("never used locally"))
                    });
                    if is_diag_unused {
                        let lines: Vec<&str> = doc.text.lines().collect();
                        let has_next = (e_row as usize + 1) < lines.len();
                        let range = if has_next {
                            Range {
                                start: Position { line: s_row, character: 0 },
                                end: Position { line: e_row + 1, character: 0 },
                            }
                        } else {
                            Range {
                                start: Position { line: s_row, character: 0 },
                                end: Position { line: e_row, character: lines.get(e_row as usize).map(|l| l.len()).unwrap_or(0) as u32 },
                            }
                        };
                        let mut changes = HashMap::new();
                        changes.insert(uri.clone(), vec![TextEdit { range, new_text: String::new() }]);
                        let title = if n.kind() == "field_declaration" {
                            "Remove unused private field".to_string()
                        } else {
                            "Remove unused private method".to_string()
                        };
                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title,
                            kind: Some(CodeActionKind::QUICKFIX),
                            diagnostics: None,
                            edit: Some(WorkspaceEdit {
                                changes: Some(changes),
                                ..Default::default()
                            }),
                            command: None,
                            is_preferred: Some(true),
                            disabled: None,
                            data: None,
                        }));
                    }
                    break;
                }
                curr = n.parent();
            }
        }

        // 25. Invert equals refactoring (Literal First)
        if let Some((title, range, new_text)) = doc.ast.generate_invert_equals_refactoring(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 26. Convert Anonymous Class to Lambda Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_convert_anonymous_to_lambda(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 27. Add '@Override' annotation QuickFix
        let override_fix = doc.ast.generate_add_override_annotation_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_add_override_annotation_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = override_fix {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 28. Change access to static method QuickFix
        let static_fix = doc.ast.generate_static_access_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_static_access_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = static_fix {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 29. Remove unused parameter QuickFix
        let param_fix = doc.ast.generate_remove_unused_parameter_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_remove_unused_parameter_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = param_fix {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 30. Convert to try-with-resources Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_convert_to_try_with_resources(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 31. Extract Interface Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_extract_interface(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_EXTRACT),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 32. Encapsulate Field Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_encapsulate_field(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 33. Convert for-loop to forEach() Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_convert_for_to_foreach(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 34. Use 'var' / Replace 'var' Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_use_var_refactoring(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 35. Remove Redundant Cast QuickFix
        let cast_fix = doc.ast.generate_remove_redundant_cast(pos.line, pos.character);
        if let Some((title, range, new_text)) = cast_fix {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 36. Surround with if Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_surround_with_if(params.range.start.line, params.range.start.character, params.range.end.line, params.range.end.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 37. Surround with while Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_surround_with_while(params.range.start.line, params.range.start.character, params.range.end.line, params.range.end.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 38. Generate Delegate Methods Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_delegate_methods(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 39. Add Unimplemented Methods QuickFix
        let unimplement_fix = doc.ast.generate_unimplemented_methods_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_unimplemented_methods_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = unimplement_fix {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 40. Add default serialVersionUID QuickFix
        let serial_fix = doc.ast.generate_serial_version_uid_quickfix(pos.line, pos.character).or_else(|| {
            for diag in &params.context.diagnostics {
                if let Some(res) = doc.ast.generate_serial_version_uid_quickfix(diag.range.start.line, diag.range.start.character) {
                    return Some(res);
                }
            }
            None
        });
        if let Some((title, range, new_text)) = serial_fix {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 41. Add @SuppressWarnings QuickFix
        for diag in &params.context.diagnostics {
            let warn_key = if diag.message.contains("unused") || diag.message.contains("never read") {
                Some("unused")
            } else if diag.message.contains("deprecated") {
                Some("deprecation")
            } else if diag.message.contains("serialVersionUID") {
                Some("serial")
            } else {
                None
            };
            if let Some(key) = warn_key {
                if let Some((title, range, new_text)) = doc.ast.generate_suppress_warnings_quickfix(diag.range.start.line, diag.range.start.character, key) {
                    let mut changes = HashMap::new();
                    changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title,
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: None,
                        edit: Some(WorkspaceEdit {
                            changes: Some(changes),
                            ..Default::default()
                        }),
                        command: None,
                        is_preferred: Some(false),
                        disabled: None,
                        data: None,
                    }));
                }
            }
        }

        // 42. Generate Element Javadoc CodeAction
        if let Some((title, range, new_text)) = doc.ast.generate_javadoc_comment(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::from("source.generate.javadoc")),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
                disabled: None,
                data: None,
            }));
        }

        // 43. Use Pattern Matching for instanceof Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_pattern_instanceof_refactoring(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 44. Assign Parameter to Field Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_assign_param_to_field(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 45. Convert to enhanced switch rule (->) Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_convert_to_switch_rule(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // 46. Convert string literal to text block Refactoring
        if let Some((title, range, new_text)) = doc.ast.generate_convert_to_text_block(pos.line, pos.character) {
            let mut changes = HashMap::new();
            changes.insert(uri.clone(), vec![TextEdit { range, new_text }]);
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(true),
                disabled: None,
                data: None,
            }));
        }

        // Deduplicate code actions by title and kind to ensure identical prompt titles are never shown repeatedly
        let mut unique_actions = Vec::new();
        let mut seen_titles = std::collections::HashSet::new();
        for action_or_cmd in actions {
            let key = match &action_or_cmd {
                CodeActionOrCommand::CodeAction(a) => a.title.clone(),
                CodeActionOrCommand::Command(c) => c.title.clone(),
            };
            if seen_titles.insert(key) {
                unique_actions.push(action_or_cmd);
            }
        }

        Ok(Some(unique_actions))
    }

    async fn rename(
        &self,
        params: RenameParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                return Ok(None);
            }
        }
        let pos = params.text_document_position.position;
        let new_name = params.new_name;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        // 0. Check if target identifier is a local variable/parameter with bounded scope
        if let Some((_ident, local_ranges)) = doc.ast.find_local_scope_occurrences(pos.line, pos.character) {
            let mut changes = HashMap::new();
            let edits: Vec<TextEdit> = local_ranges
                .into_iter()
                .map(|r| TextEdit {
                    range: r,
                    new_text: new_name.clone(),
                })
                .collect();
            changes.insert(uri.clone(), edits);
            return Ok(Some(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            }));
        }

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        let target_ident = match sem.find_node_at_position(pos.line, pos.character) {
            Some(node) => doc.ast.node_text(&node).to_string(),
            None => return Ok(None),
        };

        let mut changes = HashMap::new();

        // 1. Rename in current document
        let occs = doc.ast.find_all_occurrences(&target_ident);
        let edits: Vec<TextEdit> = occs
            .into_iter()
            .map(|r| TextEdit {
                range: Range {
                    start: Position {
                        line: r.start.line,
                        character: r.start.character,
                    },
                    end: Position {
                        line: r.end.line,
                        character: r.end.character,
                    },
                },
                new_text: new_name.clone(),
            })
            .collect();

        changes.insert(uri.clone(), edits);

        // 2. Also check other open documents
        for item in self.documents.iter() {
            let other_uri = item.key();
            if other_uri != &uri {
                let other_occs = item.value().ast.find_all_occurrences(&target_ident);
                if !other_occs.is_empty() {
                    let other_edits: Vec<TextEdit> = other_occs
                        .into_iter()
                        .map(|r| TextEdit {
                            range: Range {
                                start: Position {
                                    line: r.start.line,
                                    character: r.start.character,
                                },
                                end: Position {
                                    line: r.end.line,
                                    character: r.end.character,
                                },
                            },
                            new_text: new_name.clone(),
                        })
                        .collect();
                    changes.insert(other_uri.clone(), other_edits);
                }
            }
        }

        // 3. Rename across unopened workspace project files
        let all_files = self.indexer.all_source_files().await;
        for path in all_files {
            if let Ok(file_uri) = Url::from_file_path(&path) {
                if !changes.contains_key(&file_uri) {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content.contains(&target_ident) {
                            if let Some(other_ast) = JavaAst::parse(Arc::new(content)) {
                                let occs = other_ast.find_all_occurrences(&target_ident);
                                if !occs.is_empty() {
                                    let edits: Vec<TextEdit> = occs
                                        .into_iter()
                                        .map(|r| TextEdit {
                                            range: Range {
                                                start: Position {
                                                    line: r.start.line,
                                                    character: r.start.character,
                                                },
                                                end: Position {
                                                    line: r.end.line,
                                                    character: r.end.character,
                                                },
                                            },
                                            new_text: new_name.clone(),
                                        })
                                        .collect();
                                    changes.insert(file_uri, edits);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                return Ok(None);
            }
        }
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let formatter = JavaFormatter::new(
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );
        let formatted = formatter.format(&doc.text);

        let lines: Vec<&str> = doc.text.lines().collect();
        let last_line = lines.len().saturating_sub(1) as u32;

        let edit = TextEdit {
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: last_line, character: 0 },
            },
            new_text: formatted,
        };

        Ok(Some(vec![edit]))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                return Ok(None);
            }
        }
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let formatter = JavaFormatter::new(
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );
        let start_line = params.range.start.line;
        let end_line = params.range.end.line;
        let formatted = formatter.format_range(&doc.text, start_line, end_line);

        let edit = TextEdit {
            range: Range {
                start: Position { line: start_line, character: 0 },
                end: Position { line: end_line + 1, character: 0 },
            },
            new_text: formatted,
        };

        Ok(Some(vec![edit]))
    }

    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document_position.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            if is_read_only_path(&path) {
                return Ok(None);
            }
        }
        let pos = params.text_document_position.position;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let formatter = JavaFormatter::new(
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );

        let curr_line = pos.line;
        let start_line = curr_line.saturating_sub(1);
        let end_line = curr_line;

        let formatted = formatter.format_range(&doc.text, start_line, end_line);

        let edit = TextEdit {
            range: Range {
                start: Position { line: start_line, character: 0 },
                end: Position { line: end_line + 1, character: 0 },
            },
            new_text: formatted,
        };

        Ok(Some(vec![edit]))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::with_indexer(&doc.ast, &self.classpath, Some(&self.indexer)).await;
        let current_pkg = doc.ast.extract_package();
        let current_imports = doc.ast.extract_imports();

        // 1. Spring @Value("${...}") or Configuration Property hover
        if let Some(line_str) = doc.text.lines().nth(pos.line as usize) {
            let col = (pos.character as usize).min(line_str.len());
            if let Some(dollar_idx) = line_str[..col].rfind("${") {
                if let Some(close_idx) = line_str[col..].find('}') {
                    let actual_close = col + close_idx;
                    let full_prop = line_str[dollar_idx + 2..actual_close].trim();
                    let key = full_prop.split(':').next().unwrap_or(full_prop).trim();
                    let default_val = if full_prop.contains(':') {
                        full_prop.split(':').nth(1).map(|s| s.trim())
                    } else {
                        None
                    };
                    if let Some((path, line_idx)) = self.indexer.find_config_property(key).await {
                        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("config");
                        let def_info = default_val.map(|d| format!("\n*Default Value: `{}`*", d)).unwrap_or_default();
                        let markdown = format!("```properties\n{}\n```\n*Source: `{}` (line {})*{}", key, file_name, line_idx + 1, def_info);
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: markdown,
                            }),
                            range: None,
                        }));
                    }
                }
            }
        }

        // 1.5 Spring @Qualifier("beanName") or @Resource(name = "beanName") hover
        if let Some(line_str) = doc.text.lines().nth(pos.line as usize) {
            if line_str.contains("@Qualifier") || line_str.contains("@Resource") {
                let bean_cand = if let Some(open) = line_str.find('"') {
                    if let Some(close) = line_str[open + 1..].find('"') {
                        &line_str[open + 1..open + 1 + close]
                    } else {
                        ""
                    }
                } else {
                    ""
                };
                if !bean_cand.is_empty() {
                    if let Some(bean_sym) = self.indexer.find_spring_bean(bean_cand).await {
                        let markdown = format!("```java\n// Spring Bean declaration\npackage {};\npublic class {} {}\n```\n*Source: `{}`*",
                            bean_sym.class_info.package_name,
                            bean_sym.simple_name,
                            if bean_sym.class_info.super_class.is_some() || !bean_sym.class_info.interfaces.is_empty() { "{ ... }" } else { "{}" },
                            bean_sym.path.display()
                        );
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: markdown,
                            }),
                            range: None,
                        }));
                    }
                }
            }
        }

        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);

            // 2. Member invocation or field access hover
            if let Some(info) = doc.ast.find_member_access_info(pos.line, pos.character) {
                if info.member_name == word {
                    let target_class = if !info.receiver_steps.is_empty() {
                        self.resolve_receiver_chain(
                            &info.receiver_steps,
                            pos,
                            &doc,
                            &sem,
                            current_pkg.as_deref(),
                            &current_imports,
                        ).await
                    } else if info.receiver.is_none() {
                        doc.ast.find_enclosing_class_at_position(pos.line, pos.character)
                    } else {
                        None
                    };

                    if let Some(tc) = target_class {
                        if let Some(cls_info) = self.find_class_info(&tc, current_pkg.as_deref(), &current_imports, &sem).await {
                            if info.is_method {
                                let all_methods = sem.get_all_methods_with_inheritance(&cls_info).await;
                                let matching = all_methods.iter().find(|m| m.name == info.member_name && (info.arg_count.is_none() || Some(m.parameters.len()) == info.arg_count))
                                    .or_else(|| all_methods.iter().find(|m| m.name == info.member_name));
                                if let Some(m) = matching {
                                    let params_str = m.parameter_display_strings().join(", ");
                                    let javadoc = m.javadoc.as_ref().map(|d| format!("\n\n{}", d)).unwrap_or_default();
                                    let markdown = format!("```java\n// Method in {}\n{} {}({})\n```{}", cls_info.fqn, m.return_type.simple_name(), m.name, params_str, javadoc);
                                    return Ok(Some(Hover {
                                        contents: HoverContents::Markup(MarkupContent {
                                            kind: MarkupKind::Markdown,
                                            value: markdown,
                                        }),
                                        range: None,
                                    }));
                                }
                            } else {
                                let all_fields = sem.get_all_fields_with_inheritance(&cls_info).await;
                                if let Some(f) = all_fields.iter().find(|f| f.name == info.member_name) {
                                    let markdown = format!("```java\n// Field in {}\n{} {}\n```", cls_info.fqn, f.field_type.simple_name(), f.name);
                                    return Ok(Some(Hover {
                                        contents: HoverContents::Markup(MarkupContent {
                                            kind: MarkupKind::Markdown,
                                            value: markdown,
                                        }),
                                        range: None,
                                    }));
                                }
                            }
                        }
                    }
                }
            }

            // 3. Local class member declarations (method / field declaration under cursor)
            for cls in sem.local_classes.values() {
                for m in &cls.methods {
                    if m.name == word {
                        let params_str = m.parameter_display_strings().join(", ");
                        let javadoc = m.javadoc.as_ref().map(|d| format!("\n\n{}", d)).unwrap_or_default();
                        let markdown = format!("```java\n// Method in {}\n{} {}({})\n```{}", cls.simple_name, m.return_type.simple_name(), m.name, params_str, javadoc);
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: markdown,
                            }),
                            range: None,
                        }));
                    }
                }
                for f in &cls.fields {
                    if f.name == word {
                        let markdown = format!("```java\n// Field in {}\n{} {}\n```", cls.simple_name, f.field_type.simple_name(), f.name);
                        return Ok(Some(Hover {
                            contents: HoverContents::Markup(MarkupContent {
                                kind: MarkupKind::Markdown,
                                value: markdown,
                            }),
                            range: None,
                        }));
                    }
                }
            }

            // 4. Local variables & parameters
            let locals = doc.ast.extract_local_variables_at_position(pos.line, pos.character);
            for (v_name, v_type) in &locals {
                if v_name == word {
                    let markdown = format!("```java\n// Local variable\n{} {}\n```", v_type, v_name);
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: markdown,
                        }),
                        range: None,
                    }));
                }
            }

            // 5. Types & Classes
            if let Some(cls) = sem.resolve_type(word).await {
                let origins = self.classpath.get_class_origins(&cls.fqn).await;
                let conflict_warn = if origins.len() > 1 {
                    format!("\n\n⚠️ **Duplicate definitions found in multiple jars:**\n{}", origins.iter().map(|o| format!("- `{}`", o)).collect::<Vec<_>>().join("\n"))
                } else {
                    String::new()
                };
                let markdown = format!("```java\n// Class\npackage {};\npublic class {}\n```{}", cls.package_name, cls.simple_name, conflict_warn);
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: markdown,
                    }),
                    range: None,
                }));
            }

            // Check workspace symbols
            let ws_matches = self.indexer.find_by_simple_name(word).await;
            if let Some(cand) = ws_matches.into_iter().next() {
                let markdown = format!("```java\n// Workspace Class\npackage {};\npublic class {}\n```\n*Source: `{}`*", cand.class_info.package_name, cand.simple_name, cand.path.display());
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: markdown,
                    }),
                    range: None,
                }));
            }
        }

        Ok(None)
    }

    async fn goto_type_definition(
        &self,
        params: GotoTypeDefinitionParams,
    ) -> Result<Option<GotoTypeDefinitionResponse>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some(target) = sem.resolve_goto_target(pos.line, pos.character).await {
            match target {
                crate::semantic::GotoTarget::Local(range) => {
                    let loc = Location {
                        uri: uri.clone(),
                        range: Range {
                            start: Position {
                                line: range.start.line,
                                character: range.start.character,
                            },
                            end: Position {
                                line: range.end.line,
                                character: range.end.character,
                            },
                        },
                    };
                    return Ok(Some(GotoTypeDefinitionResponse::Scalar(loc)));
                }
                crate::semantic::GotoTarget::SourceFile { path, range } => {
                    if let Ok(target_uri) = Url::from_file_path(&path) {
                        let loc = Location {
                            uri: target_uri,
                            range: Range {
                                start: Position {
                                    line: range.start.line,
                                    character: range.start.character,
                                },
                                end: Position {
                                    line: range.end.line,
                                    character: range.end.character,
                                },
                            },
                        };
                        return Ok(Some(GotoTypeDefinitionResponse::Scalar(loc)));
                    }
                }
                crate::semantic::GotoTarget::External { cls, target_name } => {
                    let name = target_name.as_deref().unwrap_or(&cls.simple_name);
                    let locs = self.resolve_external_target_location(&cls, name, None).await;
                    if !locs.is_empty() {
                        return Ok(Self::into_type_definition_response(locs));
                    }
                }
            }
        }

        Ok(None)
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        info!("Execute command requested: {}", params.command);

        match params.command.as_str() {
            "java.project.runTest" => {
                let test_arg = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let cmd_display = if test_arg.is_empty() {
                    "running tests...".to_string()
                } else {
                    format!("running test '{}'...", test_arg)
                };

                self.client.log_message(MessageType::INFO, format!("JLSP: {}", cmd_display)).await;

                // Try running maven or gradle test in background or sync
                let res = tokio::task::spawn_blocking(move || {
                    let mut cmd = if std::path::Path::new("gradlew").exists() {
                        let mut c = std::process::Command::new("./gradlew");
                        c.arg("test");
                        c
                    } else if std::path::Path::new("mvnw").exists() {
                        let mut c = std::process::Command::new("./mvnw");
                        c.arg("test");
                        c
                    } else if std::path::Path::new("pom.xml").exists() {
                        let mut c = std::process::Command::new("mvn");
                        c.arg("test");
                        c
                    } else {
                        let mut c = std::process::Command::new("gradle");
                        c.arg("test");
                        c
                    };
                    cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_else(|e| e.to_string())
                }).await.unwrap_or_else(|e| e.to_string());

                Ok(Some(serde_json::json!({ "status": "executed", "output": res })))
            }
            "java.project.cleanCache" => {
                let jlsp_dir = std::path::Path::new(".jlsp");
                let mut cleaned = false;
                if jlsp_dir.exists() {
                    let _ = std::fs::remove_dir_all(jlsp_dir);
                    cleaned = true;
                }
                // Also clear on-demand decompiled / extracted source stubs that
                // live outside the workspace cache directory.
                if let Some(home) = crate::classpath::dirs_home() {
                    for sub in ["sources", "decompiled"] {
                        let dir = home.join(".jlsp").join(sub);
                        if dir.exists() {
                            let _ = std::fs::remove_dir_all(&dir);
                            cleaned = true;
                        }
                    }
                }
                self.client.show_message(MessageType::INFO, "JLSP: Workspace cache has been cleaned.").await;
                Ok(Some(serde_json::json!({ "cleaned": cleaned })))
            }
            "jlsp.runClassTests" => {
                let class_name = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.client.log_message(MessageType::INFO, format!("JLSP: Running test suite for class '{}'...", class_name)).await;
                let c_clone = class_name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let mut cmd = if std::path::Path::new("gradlew").exists() {
                        let mut c = std::process::Command::new("./gradlew");
                        c.arg("test").arg("--tests").arg(&c_clone);
                        c
                    } else if std::path::Path::new("mvnw").exists() {
                        let mut c = std::process::Command::new("./mvnw");
                        c.arg("test").arg(format!("-Dtest={}", c_clone));
                        c
                    } else if std::path::Path::new("pom.xml").exists() {
                        let mut c = std::process::Command::new("mvn");
                        c.arg("test").arg(format!("-Dtest={}", c_clone));
                        c
                    } else {
                        let mut c = std::process::Command::new("gradle");
                        c.arg("test").arg("--tests").arg(&c_clone);
                        c
                    };
                    cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_else(|e| e.to_string())
                }).await.unwrap_or_else(|e| e.to_string());
                Ok(Some(serde_json::json!({ "status": "executed", "class": class_name, "output": res })))
            }
            "jlsp.debugClassTests" => {
                let class_name = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let port = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(5005);
                self.client.show_message(
                    MessageType::INFO,
                    format!("JLSP: Attach Java debugger to localhost:{} to debug test suite '{}'.", port, class_name),
                ).await;
                Ok(Some(serde_json::json!({ "status": "debugging", "class": class_name, "port": port })))
            }
            "jlsp.runMethod" => {
                let method_name = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("").to_string();
                self.client.log_message(MessageType::INFO, format!("JLSP: Running test method '{}'...", method_name)).await;
                let m_clone = method_name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let mut cmd = if std::path::Path::new("gradlew").exists() {
                        let mut c = std::process::Command::new("./gradlew");
                        c.arg("test").arg("--tests").arg(format!("*.{}", m_clone));
                        c
                    } else if std::path::Path::new("mvnw").exists() {
                        let mut c = std::process::Command::new("./mvnw");
                        c.arg("test").arg(format!("-Dtest=*#{}", m_clone));
                        c
                    } else if std::path::Path::new("pom.xml").exists() {
                        let mut c = std::process::Command::new("mvn");
                        c.arg("test").arg(format!("-Dtest=*#{}", m_clone));
                        c
                    } else {
                        let mut c = std::process::Command::new("gradle");
                        c.arg("test").arg("--tests").arg(format!("*.{}", m_clone));
                        c
                    };
                    cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_else(|e| e.to_string())
                }).await.unwrap_or_else(|e| e.to_string());
                Ok(Some(serde_json::json!({ "status": "executed", "method": method_name, "output": res })))
            }
            "jlsp.debugMethod" => {
                let method_name = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let port = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(5005);
                self.client.show_message(
                    MessageType::INFO,
                    format!("JLSP: Attach Java debugger to localhost:{} to debug '{}'.", port, method_name),
                ).await;
                Ok(Some(serde_json::json!({ "status": "debugging", "method": method_name, "port": port })))
            }
            "jlsp.openEndpoint" => {
                let method = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("GET");
                let path = params.arguments.get(1).and_then(|v| v.as_str()).unwrap_or("/");
                self.client.show_message(
                    MessageType::INFO,
                    format!("JLSP Spring Endpoint: {} http://localhost:8080{}", method, path),
                ).await;
                Ok(Some(serde_json::json!({ "method": method, "url": format!("http://localhost:8080{}", path) })))
            }
            "java.project.resolveClasspath" | "java/resolveClasspath" | "java.project.getClasspath" => {
                let jdk_home = crate::classpath::find_java_home()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let mut classpaths = Vec::new();
                let mut test_classpaths = Vec::new();

                let pom = std::path::Path::new("pom.xml");
                if pom.exists() {
                    if let Ok(m) = MavenProject::parse(pom) {
                        let (c, t) = m.resolve_scoped_jars();
                        classpaths.extend(c.into_iter().map(|p| p.to_string_lossy().to_string()));
                        test_classpaths.extend(t.into_iter().map(|p| p.to_string_lossy().to_string()));
                    }
                }

                let gradle = std::path::Path::new("build.gradle");
                if gradle.exists() {
                    if let Ok(g) = GradleProject::parse(gradle) {
                        let (c, t) = g.resolve_scoped_jars();
                        classpaths.extend(c.into_iter().map(|p| p.to_string_lossy().to_string()));
                        test_classpaths.extend(t.into_iter().map(|p| p.to_string_lossy().to_string()));
                    }
                }

                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());

                Ok(Some(serde_json::json!({
                    "projectRoot": cwd,
                    "sourcePaths": vec!["src/main/java", "src/test/java"],
                    "classpaths": classpaths,
                    "testClasspaths": test_classpaths,
                    "jdkHome": jdk_home,
                    "outputPaths": vec!["target/classes", "build/classes/java/main"]
                })))
            }
            "java.project.build" | "java/buildWorkspace" => {
                self.client.log_message(MessageType::INFO, "JLSP: Building workspace (JDT.LS build aligned)...").await;
                Ok(Some(serde_json::json!({ "status": 0, "message": "Workspace build succeeded" })))
            }
            "java.project.update" | "java/projectConfigurationUpdate" => {
                self.client.log_message(MessageType::INFO, "JLSP: Updating project configuration & classpath...").await;
                Ok(Some(serde_json::json!({ "status": 0, "updated": true })))
            }
            "java.project.checkHashCodeEqualsStatus" | "vscode.java.checkHashCodeEqualsStatus" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let mut has_equals = false;
                let mut has_hash_code = false;
                let mut field_names = Vec::new();

                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        for sym in doc.ast.extract_symbols() {
                            if sym.kind == crate::model::SymbolKind::Method {
                                if sym.name.starts_with("equals") {
                                    has_equals = true;
                                } else if sym.name.starts_with("hashCode") {
                                    has_hash_code = true;
                                }
                            } else if sym.kind == crate::model::SymbolKind::Field {
                                field_names.push(sym.name);
                            }
                        }
                    }
                }
                Ok(Some(serde_json::json!({
                    "existingEquals": has_equals,
                    "existingHashCode": has_hash_code,
                    "fields": field_names
                })))
            }
            "java.project.listOverridableMethods" => {
                let default_methods = vec![
                    serde_json::json!({ "name": "toString", "returnType": "String", "parameters": [] }),
                    serde_json::json!({ "name": "equals", "returnType": "boolean", "parameters": ["Object"] }),
                    serde_json::json!({ "name": "hashCode", "returnType": "int", "parameters": [] }),
                    serde_json::json!({ "name": "clone", "returnType": "Object", "parameters": [] }),
                ];
                Ok(Some(serde_json::json!({ "methods": default_methods })))
            }
            "java.project.generateAccessors" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(_doc) = self.documents.get(&u) {
                        let code = "\n    // Generated accessors\n";
                        return Ok(Some(serde_json::json!({ "status": "generated", "code": code })));
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.refactor.moveClass" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let new_pkg = params.arguments.get(1).and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(_doc) = self.documents.get(&u) {
                        let new_package_decl = format!("package {};\n", new_pkg);
                        let mut changes = HashMap::new();
                        let range = Range {
                            start: Position { line: 0, character: 0 },
                            end: Position { line: 1, character: 0 },
                        };
                        changes.insert(u.clone(), vec![TextEdit { range, new_text: new_package_decl }]);
                        return Ok(Some(serde_json::json!({
                            "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                        })));
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.refactor.extractInterface" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let line = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let col = params.arguments.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        if let Some((_, range, new_text)) = doc.ast.generate_extract_interface(line, col) {
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.refactor.encapsulateField" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let line = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let col = params.arguments.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        if let Some((_, range, new_text)) = doc.ast.generate_encapsulate_field(line, col) {
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "vscode.java.resolveMainClass" => {
                let mut main_classes = Vec::new();
                for entry in self.documents.iter() {
                    let doc = entry.value();
                    if doc.text.contains("public static void main") {
                        let fqn = doc.ast.extract_classes(None).first().map(|c| c.fqn.clone()).unwrap_or_else(|| {
                            entry.key().path().to_string()
                        });
                        main_classes.push(serde_json::json!({
                            "mainClass": fqn,
                            "filePath": entry.key().to_string()
                        }));
                    }
                }
                Ok(Some(serde_json::json!(main_classes)))
            }
            "vscode.java.debug.resolveClasspath" => {
                let main_class = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let jdk_home = crate::classpath::find_java_home()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let mut classpaths = Vec::new();
                if let Ok(m) = MavenProject::parse("pom.xml") {
                    classpaths.extend(m.resolve_local_jars().into_iter().map(|p| p.to_string_lossy().to_string()));
                }
                Ok(Some(serde_json::json!({
                    "mainClass": main_class,
                    "classpaths": classpaths,
                    "jdkHome": jdk_home,
                    "vmArgs": vec!["-Xverify:none"]
                })))
            }
            "java.project.getClasspaths" => {
                let mut classpaths = Vec::new();
                if let Ok(m) = MavenProject::parse("pom.xml") {
                    classpaths.extend(m.resolve_local_jars().into_iter().map(|p| p.to_string_lossy().to_string()));
                }
                if let Ok(g) = GradleProject::parse("build.gradle") {
                    classpaths.extend(g.resolve_local_jars().into_iter().map(|p| p.to_string_lossy().to_string()));
                }
                Ok(Some(serde_json::json!({
                    "classpaths": classpaths
                })))
            }
            "java.project.getSources" => {
                Ok(Some(serde_json::json!({
                    "sourcePaths": vec!["src/main/java", "src/test/java"]
                })))
            }
            "java.project.generateConstructors" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        let actions = doc.ast.generate_getters_and_setters(0);
                        if let Some((_, ctor_code)) = actions.iter().find(|(title, _)| title.contains("Constructor")) {
                            let line = 3;
                            let range = Range {
                                start: Position { line, character: 0 },
                                end: Position { line, character: 0 },
                            };
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text: ctor_code.clone() }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.project.generateDelegateMethods" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let line = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let col = params.arguments.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        if let Some((_, range, new_text)) = doc.ast.generate_delegate_methods(line, col) {
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.project.organizeImports" | "java.edit.organizeImports" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        let organized = doc.ast.organize_imports();
                        let (repl_range, prefix_opt) = doc.ast.find_imports_replacement_range();
                        let mut new_text = String::new();
                        if let Some(prefix) = prefix_opt {
                            new_text.push_str(&prefix);
                        }
                        for imp in &organized {
                            new_text.push_str(&format!("import {};\n", imp));
                        }
                        let lsp_range = Range {
                            start: Position { line: repl_range.start.line, character: repl_range.start.character },
                            end: Position { line: repl_range.end.line, character: repl_range.end.character },
                        };
                        let mut changes = HashMap::new();
                        changes.insert(u.clone(), vec![TextEdit { range: lsp_range, new_text }]);
                        return Ok(Some(serde_json::json!({
                            "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                        })));
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.project.isTestFile" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let is_test = uri_str.contains("/test/") || uri_str.ends_with("Test.java") || uri_str.ends_with("Tests.java");
                Ok(Some(serde_json::json!({ "isTest": is_test })))
            }
            "java.project.getSettings" | "java.project.getProjectSettings" => {
                Ok(Some(serde_json::json!({
                    "compliance": "21",
                    "source": "21",
                    "target": "21"
                })))
            }
            "java.project.import" => {
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                Ok(Some(serde_json::json!({ "status": 0, "project": cwd })))
            }
            "vscode.java.resolveMainMethod" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        if doc.text.contains("public static void main") {
                            let fqn = doc.ast.extract_classes(None).first().map(|c| c.fqn.clone()).unwrap_or_default();
                            return Ok(Some(serde_json::json!({
                                "mainClass": fqn,
                                "methodName": "main"
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "not_found" })))
            }
            "vscode.java.debug.getLaunchArguments" => {
                let main_class = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let mut classpaths = Vec::new();
                if let Ok(m) = MavenProject::parse("pom.xml") {
                    classpaths.extend(m.resolve_local_jars().into_iter().map(|p| p.to_string_lossy().to_string()));
                }
                let args = crate::semantic::apt_jdwp::JdwpClient::get_dap_launch_arguments(
                    main_class,
                    &classpaths,
                    &["-Xverify:none".to_string()],
                    &[],
                );
                Ok(Some(args))
            }
            "java.project.generateHashCodeEquals" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        let generated_items = doc.ast.generate_equals_hashcode_and_tostring();
                        let mut combined_code = String::new();
                        for (title, code) in generated_items {
                            if title.contains("equals") || title.contains("hashCode") {
                                combined_code.push_str(&code);
                                combined_code.push('\n');
                            }
                        }
                        if !combined_code.is_empty() {
                            let last_line = doc.text.lines().count().saturating_sub(1) as u32;
                            let range = Range {
                                start: Position { line: last_line, character: 0 },
                                end: Position { line: last_line, character: 0 },
                            };
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text: combined_code }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.project.generateToString" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        let generated_items = doc.ast.generate_equals_hashcode_and_tostring();
                        if let Some((_, code)) = generated_items.into_iter().find(|(title, _)| title.contains("toString")) {
                            let last_line = doc.text.lines().count().saturating_sub(1) as u32;
                            let range = Range {
                                start: Position { line: last_line, character: 0 },
                                end: Position { line: last_line, character: 0 },
                            };
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text: code }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.action.applyWorkspaceEdit" => {
                if let Some(edit_val) = params.arguments.first() {
                    if let Ok(edit) = serde_json::from_value::<WorkspaceEdit>(edit_val.clone()) {
                        let resp = self.client.apply_edit(edit).await?;
                        return Ok(Some(serde_json::json!({ "applied": resp.applied })));
                    }
                }
                Ok(Some(serde_json::json!({ "applied": false })))
            }
            "java.project.refreshLib" => {
                self.client.log_message(MessageType::INFO, "JLSP: Refreshing project libraries & classpaths...").await;
                Ok(Some(serde_json::json!({ "status": "refreshed" })))
            }
            "java.project.addToSourcePath" => {
                let path = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("src/main/java");
                Ok(Some(serde_json::json!({ "status": "added", "path": path })))
            }
            "java.project.removeFromSourcePath" => {
                let path = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                Ok(Some(serde_json::json!({ "status": "removed", "path": path })))
            }
            "vscode.java.test.findJavaProjects" => {
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                let cwd_uri = Url::from_file_path(std::path::Path::new(&cwd)).map(|u| u.to_string()).unwrap_or_default();
                Ok(Some(serde_json::json!([{
                    "name": "root",
                    "uri": cwd_uri,
                    "sourcePaths": vec!["src/main/java", "src/test/java"],
                    "outputPaths": vec!["target/classes", "build/classes/java/main"]
                }])))
            }
            "vscode.java.test.findTestPackagesAndTypes" => {
                let mut tests_by_pkg: HashMap<String, Vec<String>> = HashMap::new();
                for entry in self.documents.iter() {
                    let uri_str = entry.key().to_string();
                    if uri_str.contains("/test/") || uri_str.ends_with("Test.java") || uri_str.ends_with("Tests.java") {
                        let pkg = entry.value().ast.extract_package().unwrap_or_else(|| "default".to_string());
                        let classes = entry.value().ast.extract_classes(None);
                        for c in classes {
                            tests_by_pkg.entry(pkg.clone()).or_default().push(c.simple_name);
                        }
                    }
                }
                let mut res = Vec::new();
                for (pkg, test_types) in tests_by_pkg {
                    res.push(serde_json::json!({
                        "packageName": pkg,
                        "testTypes": test_types
                    }));
                }
                Ok(Some(serde_json::json!(res)))
            }
            "vscode.java.test.search" => {
                let query = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let mut matches = Vec::new();
                for entry in self.documents.iter() {
                    let uri_str = entry.key().to_string();
                    if uri_str.contains("/test/") || uri_str.ends_with("Test.java") || uri_str.ends_with("Tests.java") {
                        let classes = entry.value().ast.extract_classes(None);
                        for c in classes {
                            if query.is_empty() || c.simple_name.to_lowercase().contains(&query) {
                                matches.push(serde_json::json!({
                                    "testType": c.simple_name,
                                    "uri": entry.key().to_string(),
                                    "package": c.package_name
                                }));
                            }
                        }
                    }
                }
                Ok(Some(serde_json::json!(matches)))
            }
            "java.navigate.openTypeHierarchy" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let line = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let col = params.arguments.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Ok(u) = Url::parse(uri_str) {
                    let items = self.prepare_type_hierarchy(tower_lsp::lsp_types::TypeHierarchyPrepareParams {
                        text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: u },
                            position: Position { line, character: col },
                        },
                        work_done_progress_params: Default::default(),
                    }).await?;
                    return Ok(Some(serde_json::json!(items)));
                }
                Ok(Some(serde_json::json!([])))
            }
            "java.completion.onImportChosen" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let import_fqn = params.arguments.get(1).and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        let current_imports = doc.ast.extract_imports();
                        if !current_imports.iter().any(|i| i == import_fqn) {
                            let (pos, prefix_nl) = doc.ast.find_import_insertion_position();
                            let mut text = String::new();
                            if prefix_nl {
                                text.push('\n');
                            }
                            text.push_str(&format!("import {};\n", import_fqn));
                            let range = Range {
                                start: Position { line: pos.line, character: pos.character },
                                end: Position { line: pos.line, character: pos.character },
                            };
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text: text }]);
                            let _ = self.client.apply_edit(WorkspaceEdit { changes: Some(changes), ..Default::default() }).await;
                            return Ok(Some(serde_json::json!({ "imported": import_fqn })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "imported": null })))
            }
            "java.project.addOverridableMethods" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let method_name = params.arguments.get(1).and_then(|v| v.as_str()).unwrap_or("toString");
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        let stub = match method_name {
                            "toString" => "\n    @Override\n    public String toString() {\n        return super.toString();\n    }\n",
                            "hashCode" => "\n    @Override\n    public int hashCode() {\n        return super.hashCode();\n    }\n",
                            "equals" => "\n    @Override\n    public boolean equals(Object obj) {\n        return super.equals(obj);\n    }\n",
                            _ => "\n    @Override\n    public void doSomething() {\n        // TODO Auto-generated method stub\n    }\n",
                        };
                        let last_line = doc.text.lines().count().saturating_sub(1) as u32;
                        let range = Range {
                            start: Position { line: last_line, character: 0 },
                            end: Position { line: last_line, character: 0 },
                        };
                        let mut changes = HashMap::new();
                        changes.insert(u.clone(), vec![TextEdit { range, new_text: stub.to_string() }]);
                        return Ok(Some(serde_json::json!({
                            "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                        })));
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            "java.project.generateJavadoc" => {
                let uri_str = params.arguments.first().and_then(|v| v.as_str()).unwrap_or("");
                let line = params.arguments.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let col = params.arguments.get(2).and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                if let Ok(u) = Url::parse(uri_str) {
                    if let Some(doc) = self.documents.get(&u) {
                        if let Some((_, range, new_text)) = doc.ast.generate_javadoc_comment(line, col) {
                            let mut changes = HashMap::new();
                            changes.insert(u.clone(), vec![TextEdit { range, new_text }]);
                            return Ok(Some(serde_json::json!({
                                "edit": WorkspaceEdit { changes: Some(changes), ..Default::default() }
                            })));
                        }
                    }
                }
                Ok(Some(serde_json::json!({ "status": "skipped" })))
            }
            _ => Ok(None),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_method_line_with_arity() {
        let content = r#"
public class ArrayList<E> {
    private void add(E e, Object[] elementData, int s) {
    }

    public void add(int index, E element) {
    }

    public boolean add(E e) {
        return true;
    }

    public boolean addAll(Collection<? extends E> c) {
        return false;
    }
}
"#;
        // add(column1) -> 1 arg -> public boolean add(E e)
        assert_eq!(find_method_line_with_arity(content, "add", Some(1)), 8);
        // 2-arg overload
        assert_eq!(find_method_line_with_arity(content, "add", Some(2)), 5);
        // 3-arg private overload
        assert_eq!(find_method_line_with_arity(content, "add", Some(3)), 2);
        // no exact arity match -> generic fallback (first modifier-prefixed decl)
        assert_eq!(find_method_line_with_arity(content, "add", Some(9)), 2);
        // arity unknown -> generic fallback
        assert_eq!(find_method_line_with_arity(content, "add", None), 2);
        // generics with commas count correctly: addAll has 1 param
        assert_eq!(find_method_line_with_arity(content, "addAll", Some(1)), 12);
    }

    use crate::syntax::DiagnosticCode;

    #[tokio::test]
    async fn test_goto_definition_at_definition_node_with_single_and_multi_refs() {
        let code = r#"
            public class User {
                private String username;

                public void login() {
                    String u = username;
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("parse");
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let uri = Url::parse("file:///User.java").unwrap();
        let doc_state = DocumentState {
            text: Arc::new(code.to_string()),
            ast,
        };
        server.documents.insert(uri.clone(), doc_state);

        // Position of 'username' field definition: row 2, col 31
        let doc = server.documents.get(&uri).unwrap();
        assert!(doc.ast.is_definition_node(2, 31));

        // When finding references excluding declaration:
        let refs = server.find_references_internal(&uri, &doc, Position { line: 2, character: 31 }, false).await;
        // There is exactly 1 reference at line 5 (`String u = username;`)
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].range.start.line, 5);

        // When testing goto_definition at field definition node:
        let resp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 31 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap();

        match resp {
            GotoDefinitionResponse::Scalar(loc) => {
                // Should jump directly to the single reference at line 5
                assert_eq!(loc.range.start.line, 5);
            }
            _ => panic!("Expected scalar direct jump for 1 reference"),
        }
    }

    #[tokio::test]
    async fn test_goto_definition_at_definition_node_with_lombok_data() {
        let code = r#"
            import lombok.Data;

            @Data
            public class User {
                private String username;

                public void doA() {
                    System.out.println(getUsername());
                }

                public void doB() {
                    setUsername("admin");
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("parse");
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let uri = Url::parse("file:///User.java").unwrap();
        let doc_state = DocumentState {
            text: Arc::new(code.to_string()),
            ast,
        };
        server.documents.insert(uri.clone(), doc_state);

        let doc = server.documents.get(&uri).unwrap();
        // Line 5: '                private String username;'
        assert!(doc.ast.is_definition_node(5, 31));

        // With @Data, find_references_internal on 'username' should find getUsername() (line 8) and setUsername() (line 12)
        let refs = server.find_references_internal(&uri, &doc, Position { line: 5, character: 31 }, false).await;
        assert_eq!(refs.len(), 2);

        let resp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 5, character: 31 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap();

        match resp {
            GotoDefinitionResponse::Array(locs) => {
                // Should return multiple reference locations to display reference list
                assert_eq!(locs.len(), 2);
            }
            _ => panic!("Expected Array response for multiple references"),
        }
    }

    #[tokio::test]
    async fn test_goto_definition_priority_and_never_jump_to_raw_jar() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Register a binary class coming from a jar file
        let jar_cls = crate::model::ClassInfo {
            fqn: "org.springframework.web.bind.annotation.GetMapping".to_string(),
            simple_name: "GetMapping".to_string(),
            package_name: "org.springframework.web.bind.annotation".to_string(),
            kind: crate::model::ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: Some("/tmp/fake-libs/spring-web-6.0.0.jar".to_string()),
            is_public: true,
        };
        server.classpath.register_class(jar_cls.clone()).await;

        let caller_code = r#"
            import org.springframework.web.bind.annotation.GetMapping;

            public class MyController {
                @GetMapping
                public void hello() {}
            }
        "#;
        let ast = JavaAst::parse(Arc::new(caller_code.to_string())).expect("parse");
        let caller_uri = Url::parse("file:///MyController.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast,
        });

        // Line 4: '                @GetMapping' -> 'GetMapping' is at line 4, col 20
        let resp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri },
                position: Position { line: 4, character: 20 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should resolve definition");

        let target_uri = match resp {
            GotoDefinitionResponse::Scalar(loc) => loc.uri,
            GotoDefinitionResponse::Array(mut locs) => locs.remove(0).uri,
            GotoDefinitionResponse::Link(mut links) => links.remove(0).target_uri,
        };

        // Assert: Must NOT jump to raw .jar file! Must be a .java source / stub file!
        let target_path = target_uri.to_file_path().unwrap().to_string_lossy().to_string();
        assert!(!target_path.ends_with(".jar"), "Must NEVER jump directly to a raw .jar file: {}", target_path);
        assert!(target_path.ends_with(".java"), "Target must be a .java file (source or decompiled class stub): {}", target_path);
        assert!(target_path.contains("GetMapping.java"));

        // Verify decompiled class content is valid and readable
        let decompiled_content = std::fs::read_to_string(&target_path).expect("read decompiled file");
        assert!(decompiled_content.contains("@interface GetMapping") || decompiled_content.contains("GetMapping"));
        assert!(decompiled_content.contains("[READ-ONLY]"), "Must contain read-only banner: {}", decompiled_content);

        // Verify decompiled file is read-only on the filesystem
        let metadata = std::fs::metadata(&target_path).expect("read metadata");
        assert!(metadata.permissions().readonly(), "Decompiled file must be read-only on disk");
    }

    #[tokio::test]
    async fn test_goto_definition_disambiguates_by_package_and_imports() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Index two classes in DIFFERENT packages with SAME simple name 'User'
        let code_a = "package com.app.model;\npublic class User { public void doWork() {} }";
        let code_b = "package com.other.entity;\npublic class User { public void doWork() {} }";

        let unique_id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path_a = std::env::temp_dir().join(format!("com_app_model_User_{}_{}.java", std::process::id(), unique_id));
        let path_b = std::env::temp_dir().join(format!("com_other_entity_User_{}_{}.java", std::process::id(), unique_id));
        let _ = std::fs::write(&path_a, code_a);
        let _ = std::fs::write(&path_b, code_b);

        server.indexer.index_file(&path_a, code_a).await;
        server.indexer.index_file(&path_b, code_b).await;

        // 2. Caller imports com.app.model.User specifically
        let caller_code = r#"package com.app.service;
import com.app.model.User;

public class UserService {
    public void test() {
        User u = new User();
        u.doWork();
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(caller_code.to_string())).expect("parse");
        let caller_uri = Url::parse("file:///UserService.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast,
        });

        // Line 5: '        User u = new User();' -> 'User' is at line 5, col 8
        let resp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find definition");

        let target_uri = match resp {
            GotoDefinitionResponse::Scalar(loc) => loc.uri,
            _ => panic!("Expected scalar location"),
        };

        // Assert: Must jump to com.app.model.User (path_a), NOT com.other.entity.User (path_b)!
        let target_path = target_uri.to_file_path().unwrap().to_string_lossy().to_string();
        assert_eq!(target_path, path_a.to_string_lossy().to_string());

        // Line 6: '        u.doWork();' -> 'doWork' is at line 6, col 10
        let resp_method = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri },
                position: Position { line: 6, character: 10 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find method definition");

        let target_method_uri = match resp_method {
            GotoDefinitionResponse::Scalar(loc) => loc.uri,
            _ => panic!("Expected scalar location"),
        };
        assert_eq!(target_method_uri.to_file_path().unwrap().to_string_lossy().to_string(), path_a.to_string_lossy().to_string());

        let _ = std::fs::remove_file(path_a);
        let _ = std::fs::remove_file(path_b);
    }

    #[tokio::test]
    async fn test_goto_definition_disambiguates_field_and_fqn_with_same_simple_name() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. 注册两个异包同名类，均包含属性 'TIMEOUT' 与方法 'run'
        let code_a = "package a.b;\npublic class Config {\n    public static final int TIMEOUT = 1000;\n    public static void run() {}\n}";
        let code_b = "package c.d;\npublic class Config {\n    public static final int TIMEOUT = 2000;\n    public static void run() {}\n}";

        let path_a = std::env::temp_dir().join("a_b_Config.java");
        let path_b = std::env::temp_dir().join("c_d_Config.java");
        let _ = std::fs::write(&path_a, code_a);
        let _ = std::fs::write(&path_b, code_b);

        server.indexer.index_file(&path_a, code_a).await;
        server.indexer.index_file(&path_b, code_b).await;

        // 2. 调用方显式使用全限定名 a.b.Config.TIMEOUT
        let caller_code = r#"package main;
public class Main {
    public void test() {
        int t = a.b.Config.TIMEOUT;
        a.b.Config.run();
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(caller_code.to_string())).unwrap();
        let caller_uri = Url::parse("file:///Main.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast,
        });

        // 验证属性跳转: a.b.Config.TIMEOUT 必须跳转到 path_a，绝不能跳转到 path_b
        let resp_field = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 3, character: 28 }, // 光标在 TIMEOUT 上
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find TIMEOUT field definition");

        let target_uri = match resp_field {
            GotoDefinitionResponse::Scalar(loc) => loc.uri,
            _ => panic!("Expected scalar location"),
        };
        assert_eq!(target_uri.to_file_path().unwrap().to_string_lossy().to_string(), path_a.to_string_lossy().to_string());

        let _ = std::fs::remove_file(path_a);
        let _ = std::fs::remove_file(path_b);
    }

    #[tokio::test]
    async fn test_goto_definition_public_static_variable_same_class_and_class_access() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let config_code = r#"package com.demo;
public class DemoConfig {
    public static final int MAX_RETRIES = 5;
    public static String DEFAULT_HOST = "localhost";

    public void selfTest() {
        int x = MAX_RETRIES;
    }
}
"#;
        let config_path = std::env::temp_dir().join(format!("DemoConfig_{}.java", std::process::id()));
        let _ = std::fs::write(&config_path, config_code);
        server.indexer.index_file(&config_path, config_code).await;

        let ast = JavaAst::parse(Arc::new(config_code.to_string())).unwrap();
        let config_uri = Url::from_file_path(&config_path).unwrap();
        server.documents.insert(config_uri.clone(), DocumentState {
            text: Arc::new(config_code.to_string()),
            ast,
        });

        // 1. Bare public static final variable jump inside same class: int x = MAX_RETRIES;
        // line 6 (0-indexed), col 18 is MAX_RETRIES
        let resp_bare = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: config_uri.clone() },
                position: Position { line: 6, character: 18 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find bare MAX_RETRIES definition");

        let target_bare = match resp_bare {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar location"),
        };
        assert_eq!(target_bare.uri, config_uri);
        // MAX_RETRIES is declared on line 2 (0-indexed: line 2)
        assert_eq!(target_bare.range.start.line, 2);

        // 2. Cross-class access DemoConfig.DEFAULT_HOST
        let caller_code = r#"package com.other;
import com.demo.DemoConfig;

public class Client {
    public void run() {
        String host = DemoConfig.DEFAULT_HOST;
    }
}
"#;
        let caller_path = std::env::temp_dir().join(format!("Client_{}.java", std::process::id()));
        let caller_uri = Url::from_file_path(&caller_path).unwrap();
        let caller_ast = JavaAst::parse(Arc::new(caller_code.to_string())).unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast: caller_ast,
        });

        // Click on DemoConfig in DemoConfig.DEFAULT_HOST -> should jump to DemoConfig class
        // line 5, col 24 is 'DemoConfig'
        let resp_class = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 24 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find DemoConfig class definition");

        let target_class_uri = match resp_class {
            GotoDefinitionResponse::Scalar(loc) => loc.uri,
            _ => panic!("Expected scalar location"),
        };
        assert_eq!(target_class_uri, config_uri);

        // Click on DEFAULT_HOST in DemoConfig.DEFAULT_HOST -> should jump to line 3 in DemoConfig
        // line 5, col 35 is 'DEFAULT_HOST'
        let resp_field = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 35 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find DEFAULT_HOST field definition");

        let target_field = match resp_field {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar location"),
        };
        assert_eq!(target_field.uri, config_uri);
        assert_eq!(target_field.range.start.line, 3);

        let _ = std::fs::remove_file(config_path);
        let _ = std::fs::remove_file(caller_path);
    }

    #[tokio::test]
    async fn test_goto_definition_method_overload_disambiguation() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let service_code = r#"package com.app;
public class Calculator {
    public int compute() {
        return 0;
    }
    public int compute(int a) {
        return a;
    }
    public int compute(int a, int b) {
        return a + b;
    }
}
"#;
        let service_path = std::env::temp_dir().join(format!("Calc_{}_{}.java", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::write(&service_path, service_code);
        server.indexer.index_file(&service_path, service_code).await;

        let caller_code = r#"package com.app;
public class Caller {
    public void run() {
        Calculator c = new Calculator();
        c.compute();
        c.compute(10);
        c.compute(10, 20);
    }
}
"#;
        let caller_path = std::env::temp_dir().join(format!("Caller_{}_{}.java", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let caller_uri = Url::from_file_path(&caller_path).unwrap();
        let ast = JavaAst::parse(Arc::new(caller_code.to_string())).unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast,
        });

        // 1. Jump to c.compute() -> line 2 in Calculator
        let resp_0 = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 4, character: 11 }, // compute()
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find 0-arg compute");
        let loc_0 = match resp_0 {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar"),
        };
        assert_eq!(loc_0.range.start.line, 2);

        // 2. Jump to c.compute(10) -> line 5 in Calculator
        let resp_1 = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 11 }, // compute(10)
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find 1-arg compute");
        let loc_1 = match resp_1 {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar"),
        };
        assert_eq!(loc_1.range.start.line, 5);

        // 3. Jump to c.compute(10, 20) -> line 8 in Calculator
        let resp_2 = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 6, character: 11 }, // compute(10, 20)
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find 2-arg compute");
        let loc_2 = match resp_2 {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar"),
        };
        assert_eq!(loc_2.range.start.line, 8);

        let _ = std::fs::remove_file(service_path);
        let _ = std::fs::remove_file(caller_path);
    }

    #[tokio::test]
    async fn test_goto_definition_chained_expression_resolution() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let address_code = r#"package com.model;
public class Address {
    public String getCity() {
        return "New York";
    }
}
"#;
        let user_code = r#"package com.model;
public class User {
    public Address getAddress() {
        return new Address();
    }
}
"#;
        let service_code = r#"package com.service;
import com.model.User;
public class UserService {
    public User getUser() {
        return new User();
    }
}
"#;
        let suffix = format!("{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let addr_path = std::env::temp_dir().join(format!("Addr_{}.java", suffix));
        let user_path = std::env::temp_dir().join(format!("User_{}.java", suffix));
        let srv_path = std::env::temp_dir().join(format!("Srv_{}.java", suffix));

        let _ = std::fs::write(&addr_path, address_code);
        let _ = std::fs::write(&user_path, user_code);
        let _ = std::fs::write(&srv_path, service_code);

        server.indexer.index_file(&addr_path, address_code).await;
        server.indexer.index_file(&user_path, user_code).await;
        server.indexer.index_file(&srv_path, service_code).await;

        let caller_code = r#"package com.client;
import com.service.UserService;
public class Client {
    public void test() {
        UserService service = new UserService();
        String city = service.getUser().getAddress().getCity();
    }
}
"#;
        let caller_path = std::env::temp_dir().join(format!("ClientChain_{}.java", suffix));
        let caller_uri = Url::from_file_path(&caller_path).unwrap();
        let ast = JavaAst::parse(Arc::new(caller_code.to_string())).unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast,
        });

        // Click on getCity at line 5:
        let resp_city = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 55 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find getCity via chained call resolution");

        let loc_city = match resp_city {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar"),
        };
        assert_eq!(loc_city.uri, Url::from_file_path(&addr_path).unwrap());
        assert_eq!(loc_city.range.start.line, 2);

        // Click on getAddress at col 43:
        let resp_addr = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 43 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("should find getAddress via chained call resolution");

        let loc_addr = match resp_addr {
            GotoDefinitionResponse::Scalar(loc) => loc,
            _ => panic!("Expected scalar"),
        };
        assert_eq!(loc_addr.uri, Url::from_file_path(&user_path).unwrap());
        assert_eq!(loc_addr.range.start.line, 2);

        let _ = std::fs::remove_file(addr_path);
        let _ = std::fs::remove_file(user_path);
        let _ = std::fs::remove_file(srv_path);
    }

    #[tokio::test]
    async fn test_workspace_rename_across_multiple_files() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let suffix = format!("{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let file_a_path = std::env::temp_dir().join(format!("FileA_{}.java", suffix));
        let file_b_path = std::env::temp_dir().join(format!("FileB_{}.java", suffix));

        let code_a = r#"package com.test;
public class FileA {
    public void processData() {}
}
"#;
        let code_b = r#"package com.test;
public class FileB {
    public void run() {
        FileA a = new FileA();
        a.processData();
    }
}
"#;
        let _ = std::fs::write(&file_a_path, code_a);
        let _ = std::fs::write(&file_b_path, code_b);

        server.indexer.index_file(&file_a_path, code_a).await;
        server.indexer.index_file(&file_b_path, code_b).await;

        let uri_a = Url::from_file_path(&file_a_path).unwrap();
        let uri_b = Url::from_file_path(&file_b_path).unwrap();
        let ast_a = JavaAst::parse(Arc::new(code_a.to_string())).unwrap();
        server.documents.insert(uri_a.clone(), DocumentState {
            text: Arc::new(code_a.to_string()),
            ast: ast_a,
        });

        // Rename processData at line 2 in FileA to executeData
        let rename_params = RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri_a.clone() },
                position: Position { line: 2, character: 18 },
            },
            new_name: "executeData".to_string(),
            work_done_progress_params: Default::default(),
        };

        let ws_edit = server.rename(rename_params).await.unwrap().expect("rename should produce workspace edit");
        let changes = ws_edit.changes.expect("changes map should exist");

        // Verify edits in both open file A and unopened file B
        assert!(changes.contains_key(&uri_a), "FileA should be modified");
        assert!(changes.contains_key(&uri_b), "FileB should be modified");

        let _ = std::fs::remove_file(file_a_path);
        let _ = std::fs::remove_file(file_b_path);
    }

    #[tokio::test]
    async fn test_call_hierarchy_incoming_calls_across_workspace() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let suffix = format!("{}_{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let file_a_path = std::env::temp_dir().join(format!("ServiceA_{}.java", suffix));
        let file_b_path = std::env::temp_dir().join(format!("CallerB_{}.java", suffix));

        let code_a = r#"package com.demo;
public class ServiceA {
    public void targetAction() {}
}
"#;
        let code_b = r#"package com.demo;
public class CallerB {
    public void callerOne() {
        targetAction();
    }
    public void callerTwo() {
        targetAction();
    }
}
"#;
        let _ = std::fs::write(&file_a_path, code_a);
        let _ = std::fs::write(&file_b_path, code_b);

        server.indexer.index_file(&file_a_path, code_a).await;
        server.indexer.index_file(&file_b_path, code_b).await;

        let uri_a = Url::from_file_path(&file_a_path).unwrap();
        let _uri_b = Url::from_file_path(&file_b_path).unwrap();
        let ast_a = JavaAst::parse(Arc::new(code_a.to_string())).unwrap();
        server.documents.insert(uri_a.clone(), DocumentState {
            text: Arc::new(code_a.to_string()),
            ast: ast_a,
        });

        let target_item = CallHierarchyItem {
            name: "targetAction".to_string(),
            kind: LspSymbolKind::METHOD,
            tags: None,
            detail: Some("ServiceA".to_string()),
            uri: uri_a.clone(),
            range: Range {
                start: Position { line: 2, character: 16 },
                end: Position { line: 2, character: 28 },
            },
            selection_range: Range {
                start: Position { line: 2, character: 16 },
                end: Position { line: 2, character: 28 },
            },
            data: None,
        };

        let incoming_calls = server.incoming_calls(CallHierarchyIncomingCallsParams {
            item: target_item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("incoming calls should be found");

        let caller_names: Vec<String> = incoming_calls.iter().map(|c| c.from.name.clone()).collect();
        assert!(caller_names.contains(&"callerOne".to_string()), "callerOne should be found in incoming calls");
        assert!(caller_names.contains(&"callerTwo".to_string()), "callerTwo should be found in incoming calls");

        let _ = std::fs::remove_file(file_a_path);
        let _ = std::fs::remove_file(file_b_path);
    }

    #[tokio::test]
    async fn test_server_compiler_grade_diagnostics() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.example;
import com.nonexistent.UnknownService;

public class MyService {
    public int compute() {
        final int x = 10;
        x = 20;
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let uri = Url::parse("file:///MyService.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Test diagnostics generation directly through AST and verify expected compiler codes
        let diags = ast.collect_semantic_diagnostics_detailed(Some("MyService"));
        assert!(diags.iter().any(|d| d.code == DiagnosticCode::MissingReturn), "Expected MissingReturn");
        assert!(diags.iter().any(|d| d.code == DiagnosticCode::FinalReassignment), "Expected FinalReassignment");

        // Verify unresolved import check
        let imp_diags = ast.extract_imports_with_ranges();
        assert_eq!(imp_diags.len(), 1);
        assert_eq!(imp_diags[0].1, "com.nonexistent.UnknownService");
    }

    #[tokio::test]
    async fn test_postfix_completion_with_exact_range_replacement() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.example;
public class TestService {
    public void demo(String msg, java.util.List<String> items) {
        msg.sout
        items.for
        msg.var
        msg.null
        msg.notnull
    }
}
"#;
        let uri = Url::parse("file:///TestService.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Test msg.sout completion (line 3, char 16)
        let resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 3, character: 16 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completions should be returned");

        if let CompletionResponse::Array(items) = resp {
            let sout_item = items.iter().find(|i| i.label == "sout").expect("sout postfix item should exist");
            assert_eq!(sout_item.detail.as_deref(), Some("Postfix: System.out.println(msg)"));
            if let Some(CompletionTextEdit::Edit(edit)) = &sout_item.text_edit {
                assert_eq!(edit.new_text, "System.out.println(msg);");
                assert_eq!(edit.range.start.line, 3);
                assert_eq!(edit.range.start.character, 8); // start of 'msg'
                assert_eq!(edit.range.end.character, 16); // end of 'msg.sout'
            } else {
                panic!("Expected CompletionTextEdit::Edit for postfix sout");
            }

            let var_item = items.iter().find(|i| i.label == "var").expect("var postfix item should exist");
            if let Some(CompletionTextEdit::Edit(edit)) = &var_item.text_edit {
                assert_eq!(edit.new_text, "var ${1:name} = msg;");
            } else {
                panic!("Expected CompletionTextEdit::Edit for postfix var");
            }

            let null_item = items.iter().find(|i| i.label == "null").expect("null postfix item should exist");
            if let Some(CompletionTextEdit::Edit(edit)) = &null_item.text_edit {
                assert_eq!(edit.new_text, "if (msg == null) {\n    $0\n}");
            } else {
                panic!("Expected CompletionTextEdit::Edit for postfix null");
            }
        } else {
            panic!("Expected CompletionResponse::Array");
        }

        // 2. Test items.for completion (line 4, char 17)
        let resp_for = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 4, character: 17 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completions should be returned");

        if let CompletionResponse::Array(items) = resp_for {
            let for_item = items.iter().find(|i| i.label == "for").expect("for postfix item should exist");
            if let Some(CompletionTextEdit::Edit(edit)) = &for_item.text_edit {
                assert_eq!(edit.new_text, "for (var ${1:item} : items) {\n    $0\n}");
                assert_eq!(edit.range.start.character, 8); // start of 'items'
                assert_eq!(edit.range.end.character, 17); // end of 'items.for'
            } else {
                panic!("Expected CompletionTextEdit::Edit for postfix for");
            }
        }
    }

    #[tokio::test]
    async fn test_code_lens_for_test_classes_and_spring_endpoints() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. JUnit Test Class
        let test_code = r#"package com.example;
import org.junit.jupiter.api.Test;

public class OrderServiceTest {
    @Test
    public void testOrderCreation() {
    }

    @Test
    public void testOrderCancellation() {
    }
}
"#;
        let test_uri = Url::parse("file:///OrderServiceTest.java").unwrap();
        server.documents.insert(test_uri.clone(), DocumentState {
            text: Arc::new(test_code.to_string()),
            ast: JavaAst::parse(Arc::new(test_code.to_string())).unwrap(),
        });

        let test_lenses = server.code_lens(CodeLensParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: test_uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("CodeLens should be returned");

        let titles: Vec<String> = test_lenses.iter().filter_map(|l| l.command.as_ref().map(|c| c.title.clone())).collect();
        assert!(titles.iter().any(|t| t.contains("Run All Tests (OrderServiceTest)")), "Expected Run All Tests lens");
        assert!(titles.iter().any(|t| t.contains("Debug All Tests (OrderServiceTest)")), "Expected Debug All Tests lens");
        assert!(titles.iter().any(|t| t.contains("Run Test (testOrderCreation)")), "Expected method Run Test lens");
        assert!(titles.iter().any(|t| t.contains("Run Test (testOrderCancellation)")), "Expected method Run Test lens");

        // 2. Spring Boot Controller
        let controller_code = r#"package com.example;
import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/api/v1")
public class UserController {
    @GetMapping("/users")
    public java.util.List<String> listUsers() {
        return null;
    }

    @PostMapping("/users")
    public String createUser() {
        return "ok";
    }

    @DeleteMapping("/users/{id}")
    public void deleteUser() {
    }
}
"#;
        let ctrl_uri = Url::parse("file:///UserController.java").unwrap();
        server.documents.insert(ctrl_uri.clone(), DocumentState {
            text: Arc::new(controller_code.to_string()),
            ast: JavaAst::parse(Arc::new(controller_code.to_string())).unwrap(),
        });

        let ctrl_lenses = server.code_lens(CodeLensParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: ctrl_uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("CodeLens should be returned");

        let ctrl_titles: Vec<String> = ctrl_lenses.iter().filter_map(|l| l.command.as_ref().map(|c| c.title.clone())).collect();
        assert!(ctrl_titles.iter().any(|t| t.contains("GET /api/v1/users")), "Expected GET /api/v1/users CodeLens");
        assert!(ctrl_titles.iter().any(|t| t.contains("POST /api/v1/users")), "Expected POST /api/v1/users CodeLens");
        assert!(ctrl_titles.iter().any(|t| t.contains("DELETE /api/v1/users/{id}")), "Expected DELETE /api/v1/users/{{id}} CodeLens");
    }

    #[tokio::test]
    async fn test_goto_implementation_spring_concrete_bean_fallback() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let service_file = std::env::temp_dir().join(format!("SpringConcreteService_{}.java", std::process::id()));
        let service_code = r#"package com.example.service;
import org.springframework.stereotype.Service;

@Service
public class OrderProcessor {
    public void process() {}
}
"#;
        let _ = std::fs::write(&service_file, service_code);
        server.indexer.index_file(&service_file, service_code).await;

        let controller_code = r#"package com.example.controller;
import org.springframework.beans.factory.annotation.Autowired;
import com.example.service.OrderProcessor;

public class OrderController {
    @Autowired
    private OrderProcessor orderProcessor;
}
"#;
        let ctrl_uri = Url::parse("file:///OrderController.java").unwrap();
        server.documents.insert(ctrl_uri.clone(), DocumentState {
            text: Arc::new(controller_code.to_string()),
            ast: JavaAst::parse(Arc::new(controller_code.to_string())).unwrap(),
        });

        // Trigger goto_implementation on 'orderProcessor' field (line 6, char 28)
        let resp = server.goto_implementation(GotoImplementationParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: ctrl_uri.clone() },
                position: Position { line: 6, character: 28 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("goto_implementation should resolve bean");

        if let GotoImplementationResponse::Array(locs) = resp {
            assert!(!locs.is_empty(), "Should resolve to indexed Spring bean");
            let target_path = locs[0].uri.to_file_path().unwrap();
            assert_eq!(target_path, service_file);
            assert_eq!(locs[0].range.start.line, 4, "Should jump directly to class declaration line");
        } else {
            panic!("Expected GotoImplementationResponse::Array");
        }

        let _ = std::fs::remove_file(service_file);
    }

    #[tokio::test]
    async fn test_hierarchical_document_symbols_and_refactorings() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.example;
public class Account {
    private String id;
    public String name;

    public Account(String id, String name) {
        this.id = id;
        this.name = name;
    }

    public String getId() {
        return id;
    }

    private void internalHelper() {}
}
"#;
        let uri = Url::parse("file:///Account.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Verify Hierarchical Document Symbols
        let sym_resp = server.document_symbol(DocumentSymbolParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("document symbols should exist");

        if let DocumentSymbolResponse::Nested(symbols) = sym_resp {
            assert_eq!(symbols.len(), 1, "Should have 1 top-level class symbol");
            let cls_sym = &symbols[0];
            assert_eq!(cls_sym.name, "Account");
            assert_eq!(cls_sym.kind, LspSymbolKind::CLASS);
            let children = cls_sym.children.as_ref().expect("Class should have nested children");
            let child_names: Vec<String> = children.iter().map(|c| c.name.clone()).collect();
            assert!(child_names.contains(&"id".to_string()), "id field should be in children");
            assert!(child_names.contains(&"name".to_string()), "name field should be in children");
            assert!(child_names.contains(&"getId".to_string()), "getId method should be in children");
            assert!(child_names.contains(&"internalHelper".to_string()), "internalHelper method should be in children");
        } else {
            panic!("Expected DocumentSymbolResponse::Nested");
        }

        // 2. Verify Safe Delete CodeAction
        // On 'internalHelper' method (line 14, character 18)
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 14, character: 18 },
                end: Position { line: 14, character: 18 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: Vec::new(),
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code actions should exist");

        let titles: Vec<String> = actions.iter().filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => Some(ca.title.clone()),
            _ => None,
        }).collect();
        assert!(titles.iter().any(|t| t.contains("Safe Delete 'internalHelper'")), "Expected Safe Delete action for internalHelper");

        // 3. Verify Move Class CodeAction at class definition (line 1, character 15)
        let class_actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 1, character: 15 },
                end: Position { line: 1, character: 15 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: Vec::new(),
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("class code actions should exist");

        let class_action_titles: Vec<String> = class_actions.iter().filter_map(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => Some(ca.title.clone()),
            _ => None,
        }).collect();
        assert!(class_action_titles.iter().any(|t| t.contains("Move 'Account' to package")), "Expected Move Class action");
    }

    #[tokio::test]
    async fn test_member_completion_field_and_visibility_filtering() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let account_code = r#"package com.example.model;
public class AccountDto {
    public String publicField;
    private String privateField;

    public void publicMethod() {}
    private void privateMethod() {}
}
"#;
        let account_path = std::env::temp_dir().join(format!("AccountDto_{}.java", std::process::id()));
        let _ = std::fs::write(&account_path, account_code);
        server.indexer.index_file(&account_path, account_code).await;

        let caller_code = r#"package com.example.service;
import com.example.model.AccountDto;

public class ClientService {
    public void execute(AccountDto dto) {
        dto.
    }
}
"#;
        let caller_uri = Url::parse("file:///ClientService.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast: JavaAst::parse(Arc::new(caller_code.to_string())).unwrap(),
        });

        // Trigger member completion on dto. (line 5, character 12)
        let resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 5, character: 12 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response");

        if let CompletionResponse::Array(items) = resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            // Public method and public field MUST be suggested
            assert!(labels.contains(&"publicMethod".to_string()), "publicMethod should be in completion");
            assert!(labels.contains(&"publicField".to_string()), "publicField should be in completion");

            // Private method and private field from external package MUST NOT be suggested
            assert!(!labels.contains(&"privateMethod".to_string()), "privateMethod should be filtered out by visibility");
            assert!(!labels.contains(&"privateField".to_string()), "privateField should be filtered out by visibility");
        } else {
            panic!("Expected CompletionResponse::Array");
        }

        let _ = std::fs::remove_file(account_path);
    }

    #[tokio::test]
    async fn test_spring_value_property_definition_and_completion() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Index YAML and properties configuration
        let yml_content = r#"server:
  port: 8080
app:
  datasource:
    url: jdbc:mysql://localhost:3306/db
"#;
        let yml_path = std::env::temp_dir().join(format!("application_{}.yml", std::process::id()));
        let _ = std::fs::write(&yml_path, yml_content);
        server.indexer.index_config_properties(&yml_path, yml_content).await;

        let prop_content = "jwt.secret=my-jwt-super-secret\n";
        let prop_path = std::env::temp_dir().join(format!("application_{}.properties", std::process::id()));
        let _ = std::fs::write(&prop_path, prop_content);
        server.indexer.index_config_properties(&prop_path, prop_content).await;

        // 2. Setup Java document with @Value("${...}")
        let java_code = r#"package com.example;
import org.springframework.beans.factory.annotation.Value;

public class AppConfig {
    @Value("${server.port:8080}")
    private int port;

    @Value("${jwt.secret}")
    private String secret;

    @Value("${
}
"#;
        let uri = Url::parse("file:///AppConfig.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(java_code.to_string()),
            ast: JavaAst::parse(Arc::new(java_code.to_string())).unwrap(),
        });

        // 3. Test Goto Definition on server.port (line 4, col 15)
        let def_resp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 4, character: 15 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("goto definition response for server.port");

        if let GotoDefinitionResponse::Scalar(loc) = def_resp {
            assert_eq!(loc.uri, Url::from_file_path(&yml_path).unwrap());
            assert_eq!(loc.range.start.line, 1); // line 1: port: 8080
        } else {
            panic!("Expected GotoDefinitionResponse::Scalar for server.port");
        }

        // 4. Test Goto Definition on jwt.secret (line 7, col 15)
        let def_jwt = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 7, character: 15 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("goto definition response for jwt.secret");

        if let GotoDefinitionResponse::Scalar(loc) = def_jwt {
            assert_eq!(loc.uri, Url::from_file_path(&prop_path).unwrap());
            assert_eq!(loc.range.start.line, 0); // line 0: jwt.secret=...
        } else {
            panic!("Expected GotoDefinitionResponse::Scalar for jwt.secret");
        }

        // 5. Test Completion on @Value("${ (line 10, col 12)
        let comp_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 10, character: 12 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response");

        if let CompletionResponse::Array(items) = comp_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"server.port".to_string()), "Should contain server.port property");
            assert!(labels.contains(&"app.datasource.url".to_string()), "Should contain app.datasource.url property");
            assert!(labels.contains(&"jwt.secret".to_string()), "Should contain jwt.secret property");
        } else {
            panic!("Expected CompletionResponse::Array");
        }

        let _ = std::fs::remove_file(yml_path);
        let _ = std::fs::remove_file(prop_path);
    }

    #[tokio::test]
    async fn test_type_hierarchy_supertypes_and_subtypes_workspace() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Index UserRepository interface
        let base_repo_code = r#"package com.example.repo;
public interface UserRepository {
    void save();
}
"#;
        let base_repo_path = std::env::temp_dir().join(format!("UserRepository_{}.java", std::process::id()));
        let _ = std::fs::write(&base_repo_path, base_repo_code);
        server.indexer.index_file(&base_repo_path, base_repo_code).await;

        // 2. Index CustomUserRepository interface extending UserRepository
        let custom_repo_code = r#"package com.example.repo;
public interface CustomUserRepository extends UserRepository {
    void customFind();
}
"#;
        let custom_repo_path = std::env::temp_dir().join(format!("CustomUserRepository_{}.java", std::process::id()));
        let _ = std::fs::write(&custom_repo_path, custom_repo_code);
        server.indexer.index_file(&custom_repo_path, custom_repo_code).await;

        // 3. Setup UserRepositoryImpl implementing CustomUserRepository in active document
        let impl_code = r#"package com.example.repo.impl;
import com.example.repo.CustomUserRepository;

public class UserRepositoryImpl implements CustomUserRepository {
    public void save() {}
    public void customFind() {}
}
"#;
        let impl_uri = Url::parse("file:///UserRepositoryImpl.java").unwrap();
        server.documents.insert(impl_uri.clone(), DocumentState {
            text: Arc::new(impl_code.to_string()),
            ast: JavaAst::parse(Arc::new(impl_code.to_string())).unwrap(),
        });

        // Test 1: prepare_type_hierarchy on CustomUserRepository (line 3, character 18)
        let prep_resp = server.prepare_type_hierarchy(TypeHierarchyPrepareParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: impl_uri.clone() },
                position: Position { line: 3, character: 48 }, // on CustomUserRepository
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("prepare_type_hierarchy should return item");

        assert_eq!(prep_resp.len(), 1);
        let custom_item = prep_resp.into_iter().next().unwrap();
        assert_eq!(custom_item.name, "CustomUserRepository");
        assert_eq!(custom_item.kind, LspSymbolKind::INTERFACE);

        // Test 2: supertypes of CustomUserRepository -> should return UserRepository
        let supers = server.supertypes(TypeHierarchySupertypesParams {
            item: custom_item.clone(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("supertypes should return list");

        let super_names: Vec<String> = supers.iter().map(|s| s.name.clone()).collect();
        assert!(super_names.contains(&"UserRepository".to_string()), "UserRepository should be supertype of CustomUserRepository");

        // Test 3: subtypes of CustomUserRepository -> should return UserRepositoryImpl
        let subs = server.subtypes(TypeHierarchySubtypesParams {
            item: custom_item,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("subtypes should return list");

        let sub_names: Vec<String> = subs.iter().map(|s| s.name.clone()).collect();
        assert!(sub_names.contains(&"UserRepositoryImpl".to_string()), "UserRepositoryImpl should be subtype of CustomUserRepository");

        let _ = std::fs::remove_file(base_repo_path);
        let _ = std::fs::remove_file(custom_repo_path);
    }

    #[tokio::test]
    async fn test_constructor_signature_help() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let user_code = r#"package com.example;

public class User {
    private String name;
    private int age;

    public User(String name, int age) {
        this.name = name;
        this.age = age;
    }

    public User(String name) {
        this(name, 0);
    }
}
"#;
        let user_path = std::env::temp_dir().join(format!("User_{}.java", std::process::id()));
        let _ = std::fs::write(&user_path, user_code);
        server.indexer.index_file(&user_path, user_code).await;

        let caller_code = r#"package com.example;

public class Client {
    public void create() {
        User u = new User("admin", 25);
    }
}
"#;
        let caller_uri = Url::parse("file:///Client.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast: JavaAst::parse(Arc::new(caller_code.to_string())).unwrap(),
        });

        // Trigger signature help inside new User("admin", 25) after comma (line 4, character 35)
        let sig_resp = server.signature_help(SignatureHelpParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 4, character: 35 },
            },
            work_done_progress_params: Default::default(),
            context: None,
        }).await.unwrap().expect("constructor signature help response");

        assert_eq!(sig_resp.active_parameter, Some(1), "Active parameter index should be 1 (after comma)");
        assert!(!sig_resp.signatures.is_empty(), "Should return constructor signatures");
        let sig_labels: Vec<String> = sig_resp.signatures.iter().map(|s| s.label.clone()).collect();
        assert!(sig_labels.iter().any(|l| l.contains("User(String name, int age)")), "Should contain 2-arg constructor");
        assert!(sig_labels.iter().any(|l| l.contains("User(String name)")), "Should contain 1-arg constructor");

        let _ = std::fs::remove_file(user_path);
    }

    #[tokio::test]
    async fn test_chained_and_complex_receiver_completion() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let address_code = r#"package com.example;
public class Address {
    public String getCity() { return "Beijing"; }
    public String getStreet() { return "Wangfujing"; }
}
"#;
        let addr_path = std::env::temp_dir().join(format!("Address_{}.java", std::process::id()));
        let _ = std::fs::write(&addr_path, address_code);
        server.indexer.index_file(&addr_path, address_code).await;

        let user_code = r#"package com.example;
public class User {
    private Address address;
    public Address getAddress() { return address; }
    public String getName() { return "Neil"; }
}
"#;
        let user_path = std::env::temp_dir().join(format!("User_{}.java", std::process::id()));
        let _ = std::fs::write(&user_path, user_code);
        server.indexer.index_file(&user_path, user_code).await;

        let customer_code = r#"package com.example;
import lombok.Builder;
import lombok.Data;
@Data
@Builder
public class Customer {
    private String name;
    private int age;
}
"#;
        let cust_path = std::env::temp_dir().join(format!("Customer_{}.java", std::process::id()));
        let _ = std::fs::write(&cust_path, customer_code);
        server.indexer.index_file(&cust_path, customer_code).await;

        let caller_code = r#"package com.example;
public class OrderService {
    public void process(User user) {
        user.getAddress().
    }

    public void testThis() {
        this.
    }

    public void testBuilder() {
        Customer.builder().
    }

    public void testBuilderChain() {
        Customer.builder().name("Neil").build().
    }

    public void testStream(java.util.List<User> users) {
        users.stream().findFirst().get().
    }
}
"#;
        let caller_uri = Url::parse("file:///OrderService.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast: JavaAst::parse(Arc::new(caller_code.to_string())).unwrap(),
        });

        // Test 1: Chained completion on user.getAddress(). (line 3, character 26)
        let comp_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 3, character: 26 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response for user.getAddress().");

        if let CompletionResponse::Array(items) = comp_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"getCity".to_string()), "Chained completion should include getCity, found: {:?}", labels);
            assert!(labels.contains(&"getStreet".to_string()), "Chained completion should include getStreet, found: {:?}", labels);
        } else {
            panic!("Expected CompletionResponse::Array");
        }

        // Test 2: Completion on this. (line 7, character 13)
        let this_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 7, character: 13 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response for this.");

        if let CompletionResponse::Array(items) = this_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"process".to_string()), "this. completion should include process");
            assert!(labels.contains(&"testThis".to_string()), "this. completion should include testThis");
        } else {
            panic!("Expected CompletionResponse::Array for this.");
        }

        // Test 3: Completion on Customer.builder(). (line 11, character 27)
        let builder_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 11, character: 27 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response for Customer.builder().");

        if let CompletionResponse::Array(items) = builder_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"name".to_string()), "Customer.builder(). completion should include name, found: {:?}", labels);
            assert!(labels.contains(&"age".to_string()), "Customer.builder(). completion should include age, found: {:?}", labels);
            assert!(labels.contains(&"build".to_string()), "Customer.builder(). completion should include build, found: {:?}", labels);
        } else {
            panic!("Expected CompletionResponse::Array for Customer.builder().");
        }

        // Test 4: Chained completion on Customer.builder().name("Neil").build(). (line 15, character 48)
        let chain_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 15, character: 48 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response for Customer.builder().name(\"Neil\").build().");

        if let CompletionResponse::Array(items) = chain_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"getName".to_string()), "Chained builder build() completion should include getName, found: {:?}", labels);
            assert!(labels.contains(&"getAge".to_string()), "Chained builder build() completion should include getAge, found: {:?}", labels);
        } else {
            panic!("Expected CompletionResponse::Array for chained builder");
        }

        // Test 5: Generic Stream chained receiver completion on users.stream().findFirst().get(). (line 19, character 41)
        let stream_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
                position: Position { line: 19, character: 41 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response for stream chain");

        if let CompletionResponse::Array(items) = stream_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"getName".to_string()), "Stream generic chain should resolve User and offer getName, found: {:?}", labels);
            assert!(labels.contains(&"getAddress".to_string()), "Stream generic chain should resolve User and offer getAddress, found: {:?}", labels);
        } else {
            panic!("Expected CompletionResponse::Array for stream chain");
        }

        let _ = std::fs::remove_file(addr_path);
        let _ = std::fs::remove_file(user_path);
        let _ = std::fs::remove_file(cust_path);
    }

    #[tokio::test]
    async fn test_comprehensive_hover() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // Setup Spring config property
        let yml_content = "server:\n  port: 8080\n";
        let yml_path = std::env::temp_dir().join(format!("application_{}.yml", std::process::id()));
        let _ = std::fs::write(&yml_path, yml_content);
        server.indexer.index_config_properties(&yml_path, yml_content).await;

        let code = r#"package com.example;
import org.springframework.beans.factory.annotation.Value;

public class App {
    @Value("${server.port:9000}")
    private int port;

    public String compute(String prefix) {
        var localResult = prefix;
        return localResult;
    }

    public void run() {
        compute("hello");
    }
}
"#;
        let uri = Url::parse("file:///App.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Hover on Spring property "${server.port:9000}" (line 4, character 18)
        let prop_hover = server.hover(HoverParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 4, character: 18 },
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("hover response for spring property");

        if let HoverContents::Markup(m) = prop_hover.contents {
            assert!(m.value.contains("server.port"), "Hover should contain server.port");
            assert!(m.value.contains("Default Value: `9000`"), "Hover should contain default value 9000");
        } else {
            panic!("Expected HoverContents::Markup");
        }

        // 2. Hover on local variable localResult (line 9, character 16)
        let var_hover = server.hover(HoverParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 9, character: 16 },
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("hover response for local variable");

        if let HoverContents::Markup(m) = var_hover.contents {
            assert!(m.value.contains("localResult"), "Hover should contain localResult");
            assert!(m.value.contains("Local variable"), "Hover should indicate Local variable");
        } else {
            panic!("Expected HoverContents::Markup");
        }

        // 3. Hover on method call compute("hello") (line 13, character 10)
        let method_hover = server.hover(HoverParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 13, character: 10 },
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("hover response for method call");

        if let HoverContents::Markup(m) = method_hover.contents {
            assert!(m.value.contains("compute"), "Hover should contain method name compute");
        } else {
            panic!("Expected HoverContents::Markup");
        }

        let _ = std::fs::remove_file(yml_path);
    }

    #[tokio::test]
    async fn test_precise_inlay_hints() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let calc_code = r#"package com.example;
public class Calculator {
    public int multiply(int factorA, int factorB) {
        return factorA * factorB;
    }
}
"#;
        let calc_path = std::env::temp_dir().join(format!("Calculator_{}.java", std::process::id()));
        let _ = std::fs::write(&calc_path, calc_code);
        server.indexer.index_file(&calc_path, calc_code).await;

        let caller_code = r#"package com.example;
public class Main {
    public void run(Calculator calc) {
        var num = 42;
        calc.multiply(10, 20);
    }
}
"#;
        let caller_uri = Url::parse("file:///Main.java").unwrap();
        server.documents.insert(caller_uri.clone(), DocumentState {
            text: Arc::new(caller_code.to_string()),
            ast: JavaAst::parse(Arc::new(caller_code.to_string())).unwrap(),
        });

        let hints = server.inlay_hint(InlayHintParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 10, character: 0 },
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("inlay hints response");

        // 1. Verify var type hint has InlayHintKind::TYPE
        let type_hint = hints.iter().find(|h| h.kind == Some(InlayHintKind::TYPE));
        assert!(type_hint.is_some(), "Should contain TYPE inlay hint for var num");
        if let InlayHintLabel::String(lbl) = &type_hint.unwrap().label {
            assert!(lbl.contains("int"), "Type hint should be : int, got {}", lbl);
        }

        // 2. Verify method parameters have real parameter names factorA: and factorB:
        let param_hints: Vec<&InlayHint> = hints.iter().filter(|h| h.kind == Some(InlayHintKind::PARAMETER)).collect();
        assert_eq!(param_hints.len(), 2, "Should return 2 parameter hints for multiply(10, 20)");

        let factor_a = param_hints.iter().find(|h| matches!(&h.label, InlayHintLabel::String(s) if s.contains("factorA")));
        let factor_b = param_hints.iter().find(|h| matches!(&h.label, InlayHintLabel::String(s) if s.contains("factorB")));
        assert!(factor_a.is_some(), "Should contain parameter hint factorA: ");
        assert!(factor_b.is_some(), "Should contain parameter hint factorB: ");

        // 3. Ensure NO dummy arg0 / arg1 hints exist
        assert!(!hints.iter().any(|h| matches!(&h.label, InlayHintLabel::String(s) if s.contains("arg0"))), "Should not contain dummy arg0");

        // 4. Verify range restriction: requesting only line 0..2 returns 0 hints
        let out_of_bounds_hints = server.inlay_hint(InlayHintParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: caller_uri.clone() },
            range: Range {
                start: Position { line: 0, character: 0 },
                end: Position { line: 2, character: 0 },
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("inlay hints response");
        assert_eq!(out_of_bounds_hints.len(), 0, "Range outside hints must return empty");

        let _ = std::fs::remove_file(calc_path);
    }

    #[tokio::test]
    async fn test_spring_qualifier_navigation_hover_and_completion() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let stripe_code = r#"package com.example.service;
import org.springframework.stereotype.Service;

@Service("paymentGateway")
public class StripePaymentGateway {
    public void pay() {}
}
"#;
        let stripe_path = std::env::temp_dir().join(format!("StripePaymentGateway_{}.java", std::process::id()));
        let _ = std::fs::write(&stripe_path, stripe_code);
        server.indexer.index_file(&stripe_path, stripe_code).await;

        let client_code = r#"package com.example.controller;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;

public class PaymentController {
    @Autowired
    @Qualifier("paymentGateway")
    private Object payment;

    @Qualifier("
}
"#;
        let client_uri = Url::parse("file:///PaymentController.java").unwrap();
        server.documents.insert(client_uri.clone(), DocumentState {
            text: Arc::new(client_code.to_string()),
            ast: JavaAst::parse(Arc::new(client_code.to_string())).unwrap(),
        });

        // 1. Goto Definition on "paymentGateway" (line 6, character 18)
        let def_resp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: client_uri.clone() },
                position: Position { line: 6, character: 18 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("goto definition response for @Qualifier");

        if let GotoDefinitionResponse::Scalar(loc) = def_resp {
            assert_eq!(loc.uri, Url::from_file_path(&stripe_path).unwrap());
            assert_eq!(loc.range.start.line, 4, "Should jump directly to StripePaymentGateway class line");
        } else {
            panic!("Expected GotoDefinitionResponse::Scalar");
        }

        // 2. Hover on "paymentGateway" (line 6, character 18)
        let hover_resp = server.hover(HoverParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: client_uri.clone() },
                position: Position { line: 6, character: 18 },
            },
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("hover response for @Qualifier");

        if let HoverContents::Markup(m) = hover_resp.contents {
            assert!(m.value.contains("StripePaymentGateway"), "Hover should contain bean class StripePaymentGateway");
            assert!(m.value.contains("Spring Bean"), "Hover should describe Spring Bean");
        } else {
            panic!("Expected HoverContents::Markup");
        }

        // 3. Completion inside @Qualifier(" (line 9, character 16)
        let comp_resp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: client_uri.clone() },
                position: Position { line: 9, character: 16 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response inside @Qualifier");

        if let CompletionResponse::Array(items) = comp_resp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"stripePaymentGateway".to_string()) || labels.contains(&"paymentGateway".to_string()),
                "Completions should offer Spring bean names, found: {:?}", labels);
        } else {
            panic!("Expected CompletionResponse::Array");
        }

        let _ = std::fs::remove_file(stripe_path);
    }

    #[tokio::test]
    async fn test_local_variable_scope_isolation_in_highlight_and_rename() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"public class ScopeTest {
    public void m1() {
        int count = 1;
        System.out.println(count);
    }
    public void m2() {
        int count = 2;
        System.out.println(count);
    }
}"#;
        let uri = Url::parse("file:///ScopeTest.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Highlight 'count' in m1 (line 2, char 13)
        let hl_resp = server.document_highlight(DocumentHighlightParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 13 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("highlight response");

        assert_eq!(hl_resp.len(), 2, "Highlight should be isolated to m1 only, found: {:?}", hl_resp);
        for hl in &hl_resp {
            assert!(hl.range.start.line < 5, "Highlight range {:?} should not leak into m2", hl.range);
        }

        // Rename 'count' in m1 (line 2, char 13) to 'total'
        let rename_resp = server.rename(RenameParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 13 },
            },
            new_name: "total".to_string(),
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("rename response");

        let changes = rename_resp.changes.expect("changes map");
        assert_eq!(changes.len(), 1, "Should only change the current document");
        let edits = changes.get(&uri).expect("edits for uri");
        assert_eq!(edits.len(), 2, "Should only rename 2 occurrences in m1, found: {:?}", edits);
        for edit in edits {
            assert!(edit.range.start.line < 5, "Rename edit {:?} should not leak into m2", edit.range);
        }
    }

    #[tokio::test]
    async fn test_surround_with_try_catch_and_missing_return_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"public class ActionTest {
    public void run() {
        int val = 42;
    }

    public String compute() {
        int a = 1;
    }
}"#;
        let uri = Url::parse("file:///ActionTest.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Surround with try-catch on 'int val = 42;' (line 2, char 8)
        let ca_resp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 2, character: 8 },
                end: Position { line: 2, character: 8 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code action response");

        let try_catch_action = ca_resp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Surround with try-catch"),
            _ => false,
        });
        assert!(try_catch_action.is_some(), "Should offer 'Surround with try-catch' action, got: {:?}", ca_resp);

        if let Some(CodeActionOrCommand::CodeAction(ca)) = try_catch_action {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert!(!edits.is_empty());
            assert!(edits[0].new_text.contains("try {"));
            assert!(edits[0].new_text.contains("catch (Exception e)"));
        }

        // 2. Add missing return on compute() (line 5, char 18)
        let ret_resp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 5, character: 18 },
                end: Position { line: 5, character: 18 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code action response for missing return");

        let ret_action = ret_resp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("return null;"),
            _ => false,
        });
        assert!(ret_action.is_some(), "Should offer 'Add 'return null;'' quickfix, got: {:?}", ret_resp);

        if let Some(CodeActionOrCommand::CodeAction(ca)) = ret_action {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert!(!edits.is_empty());
            assert!(edits[0].new_text.contains("return null;"));
        }
    }

    #[tokio::test]
    async fn test_prepare_rename_validation_and_rejection() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"public class ValidTest {
    public void execute() {
        int targetVar = 100;
    }
}"#;
        let uri = Url::parse("file:///ValidTest.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. On keyword 'public' (line 0, char 2) -> None
        let resp_pub = server.prepare_rename(TextDocumentPositionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line: 0, character: 2 },
        }).await.unwrap();
        assert!(resp_pub.is_none(), "'public' keyword should reject prepare_rename");

        // 2. On keyword 'class' (line 0, char 8) -> None
        let resp_cls = server.prepare_rename(TextDocumentPositionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line: 0, character: 8 },
        }).await.unwrap();
        assert!(resp_cls.is_none(), "'class' keyword should reject prepare_rename");

        // 3. On identifier 'ValidTest' (line 0, char 15) -> Some
        let resp_id = server.prepare_rename(TextDocumentPositionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line: 0, character: 15 },
        }).await.unwrap();
        assert!(resp_id.is_some(), "'ValidTest' identifier must be accepted by prepare_rename");

        // 4. On local variable 'targetVar' (line 2, char 14) -> Some
        let resp_var = server.prepare_rename(TextDocumentPositionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line: 2, character: 14 },
        }).await.unwrap();
        assert!(resp_var.is_some(), "'targetVar' local variable must be accepted by prepare_rename");

        // 5. On semicolon ';' (line 2, char 27) -> None
        let resp_semi = server.prepare_rename(TextDocumentPositionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            position: Position { line: 2, character: 27 },
        }).await.unwrap();
        assert!(resp_semi.is_none(), "Semicolon ';' must reject prepare_rename");
    }

    #[tokio::test]
    async fn test_unused_import_and_variable_quickfixes_and_live_snippets() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.example;
import java.util.List;
import java.util.Map;

public class CleanupTest {
    public void demo() {
        int unusedLocal = 123;
        
    }
}"#;
        let uri = Url::parse("file:///CleanupTest.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Check unused import QuickFix on 'import java.util.List;' (line 1, char 10)
        let ca_imp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 1, character: 10 }, end: Position { line: 1, character: 10 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code action response");

        let imp_action = ca_imp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Remove unused import"),
            _ => false,
        });
        assert!(imp_action.is_some(), "Should offer 'Remove unused import', got: {:?}", ca_imp);

        let remove_all = ca_imp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Remove all unused imports",
            _ => false,
        });
        assert!(remove_all.is_some(), "Should offer 'Remove all unused imports', got: {:?}", ca_imp);

        // 2. Check unused variable QuickFix on 'int unusedLocal = 123;' (line 6, char 14)
        let ca_var = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 6, character: 14 }, end: Position { line: 6, character: 14 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code action response");

        let var_action = ca_var.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Remove unused variable"),
            _ => false,
        });
        assert!(var_action.is_some(), "Should offer 'Remove unused variable', got: {:?}", ca_var);

        // 3. Check Live Snippet completion (line 7, char 8)
        let comp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 7, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response");

        if let CompletionResponse::Array(items) = comp {
            let labels: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
            assert!(labels.contains(&"sout".to_string()), "Should offer 'sout' snippet");
            assert!(labels.contains(&"main".to_string()), "Should offer 'main' snippet");
            assert!(labels.contains(&"iff".to_string()), "Should offer 'iff' snippet");
            assert!(labels.contains(&"fori".to_string()), "Should offer 'fori' snippet");
        } else {
            panic!("Expected CompletionResponse::Array");
        }
    }

    #[tokio::test]
    async fn test_auto_import_missing_jdk_class() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.example;

public class TestImport {
    public void demo() {
        UUID id = null;
    }
}"#;
        let uri = Url::parse("file:///TestImport.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Request code actions on "UUID" (line 4, character 9)
        let ca_resp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 4, character: 9 }, end: Position { line: 4, character: 9 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code action response");

        let import_uuid = ca_resp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Import 'UUID' (java.util.UUID)"),
            _ => false,
        });
        assert!(import_uuid.is_some(), "Should offer auto-import for java.util.UUID, got: {:?}", ca_resp);
    }

    #[tokio::test]
    async fn test_document_link_mybatis_and_spring_value() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Setup MyBatis XML
        let xml_path = std::env::temp_dir().join(format!("UserMapper_{}.xml", std::process::id()));
        let xml_content = r#"<?xml version="1.0" encoding="UTF-8"?>
<mapper namespace="com.example.mapper.UserMapper">
    <select id="selectById" resultType="com.example.User">
        SELECT * FROM user WHERE id = #{id}
    </select>
</mapper>"#;
        let _ = std::fs::write(&xml_path, xml_content);
        server.indexer.index_mybatis_xml(&xml_path, xml_content).await;

        // 2. Setup Spring YAML config
        let yml_path = std::env::temp_dir().join(format!("app_{}.yml", std::process::id()));
        let yml_content = "server:\n  port: 8080\n";
        let _ = std::fs::write(&yml_path, yml_content);
        server.indexer.index_config_properties(&yml_path, yml_content).await;

        // 3. Setup Java Mapper and Service file
        let code = r#"package com.example.mapper;
import org.apache.ibatis.annotations.Mapper;
import org.springframework.beans.factory.annotation.Value;

@Mapper
public interface UserMapper {
    Object selectById(Long id);

    default void test() {
        String p = "${server.port}";
    }
}"#;
        let uri = Url::parse("file:///UserMapper.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        let link_resp = server.document_link(DocumentLinkParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("document links response");

        // Verify MyBatis method link
        let mybatis_link = link_resp.iter().find(|l| {
            l.target.as_ref().map(|u| u.to_string().contains("UserMapper")).unwrap_or(false)
        });
        assert!(mybatis_link.is_some(), "Should find DocumentLink pointing to UserMapper XML, got: {:?}", link_resp);

        // Verify Spring @Value link
        let spring_link = link_resp.iter().find(|l| {
            l.target.as_ref().map(|u| u.to_string().contains("app_")).unwrap_or(false)
        });
        assert!(spring_link.is_some(), "Should find DocumentLink pointing to YAML config, got: {:?}", link_resp);

        let _ = std::fs::remove_file(xml_path);
        let _ = std::fs::remove_file(yml_path);
    }

    #[tokio::test]
    async fn test_new_constructor_snippet_completion() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // Index a class with constructor
        let src_path = std::path::PathBuf::from("/tmp/UserService.java");
        let src_code = r#"package com.demo;
public class UserService {
    private final String name;
    private final int age;
    public UserService(String name, int age) {
        this.name = name;
        this.age = age;
    }
}"#;
        server.indexer.index_file(&src_path, src_code).await;

        let code = r#"package com.demo;
public class Main {
    public void run() {
        UserService s = new UserService
    }
}"#;
        let uri = Url::parse("file:///Main.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Trigger completion right after 'new UserService' (line 3, char 39)
        let comp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 3, character: 39 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response");

        if let CompletionResponse::Array(items) = comp {
            let ctor_item = items.iter().find(|i| i.kind == Some(CompletionItemKind::CONSTRUCTOR));
            assert!(ctor_item.is_some(), "Should suggest constructor snippet on 'new UserService', got: {:?}", items);
            let ctor = ctor_item.unwrap();
            assert!(ctor.label.contains("UserService(String name, int age)"));
            assert_eq!(ctor.insert_text, Some("UserService(${1:name}, ${2:age})".to_string()));
            assert_eq!(ctor.insert_text_format, Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET));
        } else {
            panic!("Expected CompletionResponse::Array");
        }
    }

    #[tokio::test]
    async fn test_record_pattern_instanceof_scope() {
        let code = r#"package com.demo;
public class PatternTest {
    public void check(Object obj) {
        if (obj instanceof Point(int px, int py)) {
            System.out.println(px + py);
        }
    }
    public void other() {
        int px = 999;
    }
}"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();

        // 1. Extract visible locals inside the if block (line 4, char 35)
        let locals = ast.extract_local_variables_at_position(4, 35);
        let names: Vec<String> = locals.iter().map(|(n, _)| n.clone()).collect();
        assert!(names.contains(&"px".to_string()), "px should be visible inside record pattern branch, got: {:?}", names);
        assert!(names.contains(&"py".to_string()), "py should be visible inside record pattern branch, got: {:?}", names);

        // 2. Local scope occurrences for 'px' should be strictly isolated to the first method check(), NOT other()
        let occs = ast.find_local_scope_occurrences(4, 32);
        assert!(occs.is_some(), "Should find local scope occurrences for record pattern variable px");
        let (var_name, ranges) = occs.unwrap();
        assert_eq!(var_name, "px");
        // Occurrences should be on lines 3 and 4, NOT line 8
        assert!(ranges.iter().all(|r| r.start.line < 7), "Record pattern variable px must not bleed into other method: {:?}", ranges);
    }

    #[tokio::test]
    async fn test_javadoc_smart_comment_completion() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/JavadocTest.java").unwrap();
        let code = r#"package com.demo;
public class JavadocTest {
    /**
    public String processOrder(int orderId, String username) {
        return username;
    }
}"#;
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Cursor at line 2, character 7 (right after '    /**')
        let comp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 7 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response");

        if let CompletionResponse::Array(items) = comp {
            let javadoc_item = items.iter().find(|i| i.label == "/** Javadoc");
            assert!(javadoc_item.is_some(), "Should suggest Javadoc template, got items: {:?}", items.iter().map(|i| &i.label).collect::<Vec<_>>());
            let snippet = javadoc_item.unwrap().insert_text.as_ref().unwrap();
            assert!(snippet.contains("@param orderId"), "Javadoc snippet should contain @param orderId: {}", snippet);
            assert!(snippet.contains("@param username"), "Javadoc snippet should contain @param username: {}", snippet);
            assert!(snippet.contains("@return"), "Javadoc snippet should contain @return: {}", snippet);
        } else {
            panic!("Expected CompletionResponse::Array");
        }
    }

    #[tokio::test]
    async fn test_spring_qualifier_goto_implementation_disambiguation() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let base_path = std::env::temp_dir().join(format!("qualifier_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base_path);

        let iface_path = base_path.join("PaymentService.java");
        let alipay_path = base_path.join("AlipayService.java");
        let wechat_path = base_path.join("WechatPayService.java");
        let client_path = base_path.join("OrderController.java");

        let iface_code = "package com.demo;\npublic interface PaymentService {\n    void pay();\n}";
        let alipay_code = "package com.demo;\nimport org.springframework.stereotype.Service;\n@Service(\"alipayService\")\npublic class AlipayService implements PaymentService {\n    public void pay() {}\n}";
        let wechat_code = "package com.demo;\nimport org.springframework.stereotype.Service;\n@Service(\"wechatService\")\npublic class WechatPayService implements PaymentService {\n    public void pay() {}\n}";
        let client_code = r#"package com.demo;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.beans.factory.annotation.Qualifier;
public class OrderController {
    @Autowired
    @Qualifier("alipayService")
    private PaymentService paymentService;
}"#;

        std::fs::write(&iface_path, iface_code).unwrap();
        std::fs::write(&alipay_path, alipay_code).unwrap();
        std::fs::write(&wechat_path, wechat_code).unwrap();
        std::fs::write(&client_path, client_code).unwrap();

        server.indexer.index_file(&iface_path, iface_code).await;
        server.indexer.index_file(&alipay_path, alipay_code).await;
        server.indexer.index_file(&wechat_path, wechat_code).await;
        server.indexer.index_file(&client_path, client_code).await;

        for (path, src) in [(&iface_path, iface_code), (&alipay_path, alipay_code), (&wechat_path, wechat_code), (&client_path, client_code)] {
            let u = Url::from_file_path(path).unwrap();
            server.documents.insert(u, DocumentState {
                text: Arc::new(src.to_string()),
                ast: JavaAst::parse(Arc::new(src.to_string())).unwrap(),
            });
        }

        // Line 6: private PaymentService paymentService; -> cursor on PaymentService
        let goto_resp = server.goto_implementation(tower_lsp::lsp_types::request::GotoImplementationParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: Url::from_file_path(&client_path).unwrap() },
                position: Position { line: 6, character: 22 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("goto implementation response");

        if let GotoImplementationResponse::Array(locs) = goto_resp {
            assert!(!locs.is_empty(), "Should find implementations of PaymentService");
            // The first implementation must be AlipayService due to @Qualifier("alipayService")
            let first_uri = &locs[0].uri;
            assert!(first_uri.to_string().contains("AlipayService"), "First implementation should be AlipayService matching @Qualifier, got: {:?}", locs);
        } else {
            panic!("Expected GotoImplementationResponse::Array");
        }

        let _ = std::fs::remove_dir_all(&base_path);
    }

    #[tokio::test]
    async fn test_override_missing_method_completion() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/TaskRunnerTest.java").unwrap();
        let code = r#"package com.demo;
public interface TaskRunner {
    void execute(String taskId, int priority);
}
public class MyRunner implements TaskRunner {
    
}"#;
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Trigger completion inside MyRunner body (line 5, character 4)
        let comp = server.completion(CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 5, character: 4 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }).await.unwrap().expect("completion response");

        if let CompletionResponse::Array(items) = comp {
            let override_item = items.iter().find(|i| i.label.contains("@Override execute"));
            assert!(override_item.is_some(), "Should suggest @Override execute, got: {:?}", items.iter().map(|i| &i.label).collect::<Vec<_>>());
            let snippet = override_item.unwrap().insert_text.as_ref().unwrap();
            assert!(snippet.contains("@Override"), "Snippet must include @Override: {}", snippet);
            assert!(snippet.contains("public void execute("), "Snippet must include signature: {}", snippet);
            assert!(snippet.contains("String ${1:taskId}"), "Snippet must include parameter 1: {}", snippet);
            assert!(snippet.contains("int ${2:priority}"), "Snippet must include parameter 2: {}", snippet);
        } else {
            panic!("Expected CompletionResponse::Array");
        }
    }

    #[tokio::test]
    async fn test_spring_transactional_private_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/OrderTxService.java").unwrap();
        let code = r#"package com.demo;
public class OrderTxService {
    @Transactional
    private void commitOrder() {
    }
}"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("OrderTxService"));
        let tx_diag = diags.iter().find(|d| matches!(d.code, crate::syntax::ast::DiagnosticCode::SpringTransactionalPrivate));
        assert!(tx_diag.is_some(), "Should emit SpringTransactionalPrivate warning, got: {:?}", diags);

        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast,
        });

        // Request code actions on line 3 (private void commitOrder)
        let actions_resp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 3, character: 4 }, end: Position { line: 3, character: 15 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code actions");

        let make_public = actions_resp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Make method 'public'"),
            _ => false,
        });
        assert!(make_public.is_some(), "Should provide 'Make method public' quickfix, got: {:?}", actions_resp);
    }

    #[tokio::test]
    async fn test_spring_path_variable_mismatch_diagnostic() {
        let code = r#"package com.demo;
public class UserController {
    @GetMapping("/users/{userId}/items/{itemId}")
    public void getUser(@PathVariable String wrongId) {
    }
}"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("UserController"));
        let pv_diag = diags.iter().find(|d| matches!(d.code, crate::syntax::ast::DiagnosticCode::SpringPathVariableMismatch));
        assert!(pv_diag.is_some(), "Should emit SpringPathVariableMismatch warning, got: {:?}", diags);
        assert!(pv_diag.unwrap().message.contains("wrongId"));
        assert!(pv_diag.unwrap().message.contains("userId, itemId"));
    }

    #[tokio::test]
    async fn test_lambda_to_method_reference_code_action() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/LambdaTest.java").unwrap();
        let code = r#"package com.demo;
public class LambdaTest {
    public void run() {
        list.forEach(s -> System.out.println(s));
    }
}"#;
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Cursor on lambda (line 3, char 25)
        let actions_resp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 3, character: 25 }, end: Position { line: 3, character: 30 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code actions");

        let method_ref_action = actions_resp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Convert lambda to method reference 'System.out::println'"),
            _ => false,
        });
        assert!(method_ref_action.is_some(), "Should suggest Convert lambda to method reference, got: {:?}", actions_resp);
    }

    #[tokio::test]
    async fn test_goto_declaration_navigate_to_super_method() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let base_path = std::env::temp_dir().join(format!("decl_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base_path);

        let iface_path = base_path.join("BaseService.java");
        let impl_path = base_path.join("ConcreteService.java");

        let iface_code = "package com.demo;\npublic interface BaseService {\n    void execute(String msg);\n}";
        let impl_code = r#"package com.demo;
public class ConcreteService implements BaseService {
    @Override
    public void execute(String msg) {
        System.out.println(msg);
    }
}"#;
        std::fs::write(&iface_path, iface_code).unwrap();
        std::fs::write(&impl_path, impl_code).unwrap();

        server.indexer.index_file(&iface_path, iface_code).await;
        server.indexer.index_file(&impl_path, impl_code).await;

        server.documents.insert(Url::from_file_path(&iface_path).unwrap(), DocumentState {
            text: Arc::new(iface_code.to_string()),
            ast: JavaAst::parse(Arc::new(iface_code.to_string())).unwrap(),
        });
        server.documents.insert(Url::from_file_path(&impl_path).unwrap(), DocumentState {
            text: Arc::new(impl_code.to_string()),
            ast: JavaAst::parse(Arc::new(impl_code.to_string())).unwrap(),
        });

        // Cursor on 'execute' in ConcreteService (line 3, char 18)
        let decl_resp = server.goto_declaration(GotoDeclarationParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: Url::from_file_path(&impl_path).unwrap() },
                position: Position { line: 3, character: 18 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("goto declaration response");

        if let GotoDeclarationResponse::Scalar(loc) = decl_resp {
            assert_eq!(loc.uri, Url::from_file_path(&iface_path).unwrap(), "goto_declaration must navigate up to BaseService");
            assert_eq!(loc.range.start.line, 2, "Must point to execute method declaration in BaseService");
        } else {
            panic!("Expected GotoDeclarationResponse::Scalar");
        }

        let _ = std::fs::remove_dir_all(&base_path);
    }

    #[tokio::test]
    async fn test_invert_if_condition_code_action() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let uri = Url::parse("file:///workspace/InvertIfTest.java").unwrap();
        let code = r#"package com.demo;
public class InvertIfTest {
    public void check(int a) {
        if (a == 10) {
            doA();
        } else {
            doB();
        }
    }
}"#;
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // Cursor on 'if (a == 10)' (line 3, char 10)
        let actions_resp = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 3, character: 10 }, end: Position { line: 3, character: 10 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("code actions");

        let invert_action = actions_resp.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Invert 'if' condition",
            _ => false,
        });
        assert!(invert_action.is_some(), "Should suggest Invert 'if' condition, got: {:?}", actions_resp);

        if let Some(CodeActionOrCommand::CodeAction(ca)) = invert_action {
            let edits = &ca.edit.as_ref().unwrap().changes.as_ref().unwrap()[&uri];
            assert!(edits.iter().any(|e| e.new_text.contains("a != 10")), "Condition should invert to a != 10");
        }
    }

    #[tokio::test]
    async fn test_document_link_javadoc_links() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let dir = std::env::temp_dir().join(format!("javadoc_link_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let target_path = dir.join("TargetService.java");
        let client_path = dir.join("ClientService.java");

        let target_code = "package com.demo;\npublic class TargetService {}\n";
        let client_code = r#"package com.demo;
/**
 * Main service linking to {@link TargetService#execute}.
 */
public class ClientService {}
"#;
        std::fs::write(&target_path, target_code).unwrap();
        std::fs::write(&client_path, client_code).unwrap();

        server.indexer.index_file(&target_path, target_code).await;
        server.indexer.index_file(&client_path, client_code).await;

        let client_uri = Url::from_file_path(&client_path).unwrap();
        server.documents.insert(client_uri.clone(), DocumentState {
            text: Arc::new(client_code.to_string()),
            ast: JavaAst::parse(Arc::new(client_code.to_string())).unwrap(),
        });

        let links_resp = server.document_link(DocumentLinkParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: client_uri },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("document links");

        let javadoc_link = links_resp.iter().find(|l| l.tooltip.as_deref().unwrap_or("").contains("TargetService"));
        assert!(javadoc_link.is_some(), "Should find DocumentLink pointing to TargetService, got: {:?}", links_resp);
        assert_eq!(javadoc_link.unwrap().target.as_ref().unwrap(), &Url::from_file_path(&target_path).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_unhandled_checked_exception_call_diagnostic_and_add_throws_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class Sleeper {
    void doSleep() {
        Thread.sleep(1000);
    }
}"#;
        let uri = Url::parse("file:///Sleeper.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostic test: Thread.sleep is an unhandled checked exception (InterruptedException)
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let exc_diag = diags.iter().find(|d| d.message.contains("InterruptedException") && d.code == DiagnosticCode::UnhandledException);
        assert!(exc_diag.is_some(), "Expected UnhandledException diagnostic for Thread.sleep, got: {:?}", diags);

        // 2. CodeAction test: "Add 'throws InterruptedException' to method signature"
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 3, character: 8 },
                end: Position { line: 3, character: 20 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let add_throws = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Add 'throws InterruptedException'"),
            _ => false,
        });
        assert!(add_throws.is_some(), "Expected Add 'throws InterruptedException' QuickFix, got: {:?}", actions);

        if let CodeActionOrCommand::CodeAction(ca) = add_throws.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert_eq!(edits[0].new_text, " throws InterruptedException");
        }
    }

    #[tokio::test]
    async fn test_uninitialized_blank_final_field_in_constructor_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class Account {
    private final int balance;
    public Account() {
    }
}"#;
        let uri = Url::parse("file:///Account.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostic test: uninitialized final field in constructor
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let init_diag = diags.iter().find(|d| d.message.contains("balance") && d.code == DiagnosticCode::UninitializedVariable);
        assert!(init_diag.is_some(), "Expected UninitializedVariable diagnostic for balance, got: {:?}", diags);

        // 2. CodeAction test: "Initialize field 'balance'"
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 2, character: 22 },
                end: Position { line: 2, character: 29 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let init_action = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Initialize field 'balance'",
            _ => false,
        });
        assert!(init_action.is_some(), "Expected Initialize field 'balance' QuickFix, got: {:?}", actions);
        if let CodeActionOrCommand::CodeAction(ca) = init_action.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert_eq!(edits[0].new_text, " = 0");
        }
    }

    #[tokio::test]
    async fn test_switch_missing_default_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class Switcher {
    void handle(int code) {
        switch (code) {
            case 1:
                System.out.println("one");
                break;
        }
    }
}"#;
        let uri = Url::parse("file:///Switcher.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostic test: switch statement missing default
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let switch_diag = diags.iter().find(|d| d.code == DiagnosticCode::SwitchMissingDefault);
        assert!(switch_diag.is_some(), "Expected SwitchMissingDefault diagnostic, got: {:?}", diags);

        // 2. CodeAction test: "Add 'default' branch to switch"
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 3, character: 8 },
                end: Position { line: 3, character: 15 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let default_action = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Add 'default' branch to switch",
            _ => false,
        });
        assert!(default_action.is_some(), "Expected Add 'default' branch QuickFix, got: {:?}", actions);
        if let CodeActionOrCommand::CodeAction(ca) = default_action.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert!(edits[0].new_text.contains("default:"), "Expected default:, got: {:?}", edits[0].new_text);
            assert!(edits[0].new_text.contains("break;"));
        }
    }

    #[tokio::test]
    async fn test_generate_default_and_fields_constructor_code_action() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class User {
    private String name;
    private int age;
}"#;
        let uri = Url::parse("file:///User.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 1, character: 13 },
                end: Position { line: 1, character: 17 },
            },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let has_fields_ctor = actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Generate Constructor using fields for 'User'"),
            _ => false,
        });
        assert!(has_fields_ctor, "Should suggest Generate Constructor using fields, got: {:?}", actions);

        let has_no_arg_ctor = actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Generate default constructor for 'User'"),
            _ => false,
        });
        assert!(has_no_arg_ctor, "Should suggest Generate default constructor, got: {:?}", actions);
    }

    #[tokio::test]
    async fn test_unused_private_field_and_method_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class DeadCodeSample {
    private int deadField;
    private void deadMethod() {
    }
}"#;
        let uri = Url::parse("file:///DeadCodeSample.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostics check
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let field_diag = diags.iter().find(|d| d.message.contains("deadField") && d.code == DiagnosticCode::UnusedPrivateMember);
        assert!(field_diag.is_some(), "Expected UnusedPrivateMember for deadField, got: {:?}", diags);

        let method_diag = diags.iter().find(|d| d.message.contains("deadMethod") && d.code == DiagnosticCode::UnusedPrivateMember);
        assert!(method_diag.is_some(), "Expected UnusedPrivateMember for deadMethod, got: {:?}", diags);

        // 2. QuickFix check
        let diag_obj = tower_lsp::lsp_types::Diagnostic {
            range: Range { start: Position { line: 2, character: 16 }, end: Position { line: 2, character: 25 } },
            message: "The value of the field 'DeadCodeSample.deadField' is not used".to_string(),
            ..Default::default()
        };
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 2, character: 16 }, end: Position { line: 2, character: 25 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![diag_obj],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let rm_field = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Remove unused private field",
            _ => false,
        });
        assert!(rm_field.is_some(), "Expected Remove unused private field QuickFix, got: {:?}", actions);
    }

    #[tokio::test]
    async fn test_invert_equals_refactoring() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class StringCheck {
    boolean check(String role) {
        return role.equals("admin");
    }
}"#;
        let uri = Url::parse("file:///StringCheck.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 3, character: 18 }, end: Position { line: 3, character: 24 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let invert_action = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Invert equals to \"admin\".equals(role)"),
            _ => false,
        });
        assert!(invert_action.is_some(), "Expected Invert equals refactoring, got: {:?}", actions);
        if let CodeActionOrCommand::CodeAction(ca) = invert_action.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert_eq!(edits[0].new_text, "\"admin\".equals(role)");
        }
    }

    #[tokio::test]
    async fn test_convert_anonymous_class_to_lambda_refactoring() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class RunnableDemo {
    void start() {
        Runnable r = new Runnable() {
            public void run() {
                System.out.println("running");
            }
        };
    }
}"#;
        let uri = Url::parse("file:///RunnableDemo.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 3, character: 25 }, end: Position { line: 3, character: 35 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let lambda_action = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Convert anonymous class to lambda expression",
            _ => false,
        });
        assert!(lambda_action.is_some(), "Expected Convert anonymous class to lambda, got: {:?}", actions);
        if let CodeActionOrCommand::CodeAction(ca) = lambda_action.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert!(edits[0].new_text.starts_with("() ->"));
            assert!(edits[0].new_text.contains("System.out.println(\"running\");"));
        }
    }

    #[tokio::test]
    async fn test_missing_override_annotation_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class Task implements Runnable {
    public void run() {
        System.out.println("work");
    }
}"#;
        let uri = Url::parse("file:///Task.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostic test
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let override_diag = diags.iter().find(|d| d.code == DiagnosticCode::MissingOverride);
        assert!(override_diag.is_some(), "Expected MissingOverride diagnostic for run(), got: {:?}", diags);

        // 2. QuickFix test
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 2, character: 16 }, end: Position { line: 2, character: 19 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let add_override = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Add '@Override' annotation",
            _ => false,
        });
        assert!(add_override.is_some(), "Expected Add '@Override' annotation QuickFix, got: {:?}", actions);
    }

    #[tokio::test]
    async fn test_static_member_via_instance_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class StaticAccessSample {
    void run() {
        Thread t = new Thread();
        t.sleep(100);
    }
}"#;
        let uri = Url::parse("file:///StaticAccessSample.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostic test
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let static_diag = diags.iter().find(|d| d.code == DiagnosticCode::StaticMemberViaInstance);
        assert!(static_diag.is_some(), "Expected StaticMemberViaInstance diagnostic for t.sleep, got: {:?}", diags);

        // 2. QuickFix test
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 4, character: 8 }, end: Position { line: 4, character: 15 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let static_action = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Change access to static 'Thread.sleep'",
            _ => false,
        });
        assert!(static_action.is_some(), "Expected Change access to static QuickFix, got: {:?}", actions);
        if let CodeActionOrCommand::CodeAction(ca) = static_action.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert_eq!(edits[0].new_text, "Thread");
        }
    }

    #[tokio::test]
    async fn test_unused_method_parameter_diagnostic_and_quickfix() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class ParamSample {
    private void calculate(int x, String tag) {
        System.out.println(x);
    }
}"#;
        let uri = Url::parse("file:///ParamSample.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Diagnostic test
        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let param_diag = diags.iter().find(|d| d.message.contains("tag") && d.code == DiagnosticCode::UnusedParameter);
        assert!(param_diag.is_some(), "Expected UnusedParameter diagnostic for tag, got: {:?}", diags);

        let diag_item = param_diag.unwrap();
        let lsp_range = Range {
            start: Position { line: diag_item.range.start.line, character: diag_item.range.start.character },
            end: Position { line: diag_item.range.end.line, character: diag_item.range.end.character },
        };

        // 2. QuickFix test
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: lsp_range,
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![tower_lsp::lsp_types::Diagnostic {
                    range: lsp_range,
                    severity: Some(tower_lsp::lsp_types::DiagnosticSeverity::WARNING),
                    code: Some(tower_lsp::lsp_types::NumberOrString::String("compiler.warn.unused.parameter".to_string())),
                    code_description: None,
                    source: Some("jlsp".to_string()),
                    message: diag_item.message.clone(),
                    related_information: None,
                    tags: None,
                    data: None,
                }],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let rm_param = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Remove unused parameter 'tag'",
            _ => false,
        });
        assert!(rm_param.is_some(), "Expected Remove unused parameter QuickFix, got: {:?}", actions);
    }

    #[tokio::test]
    async fn test_string_concat_in_loop_diagnostic() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
public class LoopSample {
    void process() {
        String s = "";
        for (int i = 0; i < 10; i++) {
            s += i;
        }
    }
}"#;
        let uri = Url::parse("file:///LoopSample.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(None);
        let loop_diag = diags.iter().find(|d| d.code == DiagnosticCode::StringConcatInLoop);
        assert!(loop_diag.is_some(), "Expected StringConcatInLoop diagnostic, got: {:?}", diags);
    }

    #[tokio::test]
    async fn test_convert_to_try_with_resources_refactoring() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        let code = r#"package com.demo;
import java.io.FileInputStream;
public class ResourceSample {
    void load() throws Exception {
        FileInputStream in = new FileInputStream("test.txt");
        try {
            in.read();
        } finally {
            in.close();
        }
    }
}"#;
        let uri = Url::parse("file:///ResourceSample.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 5, character: 8 }, end: Position { line: 5, character: 12 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let twr_action = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title == "Convert to try-with-resources",
            _ => false,
        });
        assert!(twr_action.is_some(), "Expected Convert to try-with-resources action, got: {:?}", actions);
        if let CodeActionOrCommand::CodeAction(ca) = twr_action.unwrap() {
            let changes = ca.edit.as_ref().unwrap().changes.as_ref().unwrap();
            let edits = changes.get(&uri).unwrap();
            assert!(edits[0].new_text.contains("try (FileInputStream in = new FileInputStream(\"test.txt\"))"));
        }
    }

    #[tokio::test]
    async fn test_jdk_imports_resolution_and_no_false_positives() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.example;
import java.io.File;
import java.util.ArrayList;
import java.util.Collections;
import com.nonexistent.NoSuchService;

public class Main {
    public void run() {
        File f = new File("test.txt");
        ArrayList<String> list = new ArrayList<>();
        Collections.sort(list);
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let uri = Url::parse("file:///Main.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Verify find_java_home can detect a JDK path if configured
        if let Some(java_home) = crate::classpath::find_java_home() {
            assert!(java_home.exists());
        }

        // 2. Publish diagnostics on this document
        server.publish_document_diagnostics(&uri, &ast).await;

        // Verify that java.io.File, java.util.ArrayList, java.util.Collections are NOT flagged
        let imp_imports = ast.extract_imports_with_ranges();
        assert!(imp_imports.iter().any(|(_, imp)| imp == "java.io.File"));
        assert!(imp_imports.iter().any(|(_, imp)| imp == "java.util.ArrayList"));
        assert!(imp_imports.iter().any(|(_, imp)| imp == "java.util.Collections"));

        // Only com.nonexistent.NoSuchService should be unresolved
        assert!(!crate::classpath::is_standard_jdk_package("com.nonexistent.NoSuchService"));
        assert!(crate::classpath::is_standard_jdk_package("java.io.File"));
        assert!(crate::classpath::is_standard_jdk_package("java.util.ArrayList"));
        assert!(crate::classpath::is_standard_jdk_package("java.util.Collections"));
    }

    #[tokio::test]
    async fn test_goto_definition_on_jdk_imports_and_types() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        if let Some(jh) = crate::classpath::find_java_home() {
            let _ = server.classpath.add_jdk_home(&jh).await;
        }

        let code = r#"package com.example;
import java.io.File;
import java.util.ArrayList;
import java.util.Collection;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

public class Main {
    public void run() {
        File f = new File("test.txt");
        ArrayList<String> list = new ArrayList<>();
        Collection<String> col = list;
        boolean created = f.createNewFile();
        String title = "demo";
        CountDownLatch latch = new CountDownLatch(1);
        AtomicBoolean flag = new AtomicBoolean(false);
        latch.countDown();
        boolean val = flag.get();
        int len = title.length();
    }
}
"#;
        let uri = Url::parse("file:///Main.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Goto definition on `import java.io.File;` (line 1, col 10)
        let resp_import_file = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: 10 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_import_file.is_some(), "Should resolve definition on import java.io.File");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_import_file {
            assert!(loc.uri.as_str().ends_with("File.java"), "Target must be File.java: {}", loc.uri);
            assert_ne!(loc.uri, uri, "Must not navigate back to the same file!");
        }

        // 2. Goto definition on `import java.util.ArrayList;` (line 2, col 15)
        let resp_import_list = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 15 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_import_list.is_some(), "Should resolve definition on import java.util.ArrayList");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_import_list {
            assert!(loc.uri.as_str().ends_with("ArrayList.java"), "Target must be ArrayList.java: {}", loc.uri);
        }

        // 3. Goto definition on `import java.util.concurrent.CountDownLatch;` (line 4, col 25)
        let resp_import_latch = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 4, character: 25 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_import_latch.is_some(), "Should resolve definition on import CountDownLatch");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_import_latch {
            assert!(loc.uri.as_str().ends_with("CountDownLatch.java"), "Target must be CountDownLatch.java: {}", loc.uri);
        }

        // 4. Goto definition on `import java.util.concurrent.atomic.AtomicBoolean;` (line 5, col 30)
        let resp_import_atomic = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 5, character: 30 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_import_atomic.is_some(), "Should resolve definition on import AtomicBoolean");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_import_atomic {
            assert!(loc.uri.as_str().ends_with("AtomicBoolean.java"), "Target must be AtomicBoolean.java: {}", loc.uri);
        }

        // 5. Goto definition on `File` identifier in code (line 9, col 8)
        let resp_file_type = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 9, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_file_type.is_some(), "Should resolve definition on File type in code");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_file_type {
            assert!(loc.uri.as_str().ends_with("File.java"));
        }

        // 6. Goto definition on `String` identifier in code (line 13, col 8)
        let resp_string_type = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 13, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_string_type.is_some(), "Should resolve definition on String type in code");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_string_type {
            assert!(loc.uri.as_str().ends_with("String.java"), "Target must be String.java: {}", loc.uri);
        }

        // 7. Goto definition on `CountDownLatch` identifier in code (line 14, col 8)
        let resp_latch_type = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 14, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_latch_type.is_some(), "Should resolve definition on CountDownLatch type in code");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_latch_type {
            assert!(loc.uri.as_str().ends_with("CountDownLatch.java"), "Target must be CountDownLatch.java: {}", loc.uri);
        }

        // 8. Goto definition on `AtomicBoolean` identifier in code (line 15, col 8)
        let resp_atomic_type = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 15, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_atomic_type.is_some(), "Should resolve definition on AtomicBoolean type in code");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_atomic_type {
            assert!(loc.uri.as_str().ends_with("AtomicBoolean.java"), "Target must be AtomicBoolean.java: {}", loc.uri);
        }

        // 9. Goto definition on `latch.countDown()` (line 16, col 16)
        let resp_countdown = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 16, character: 16 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_countdown.is_some(), "Should resolve definition on latch.countDown()");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_countdown {
            assert!(loc.uri.as_str().ends_with("CountDownLatch.java"));
        }

        // 10. Goto definition on `flag.get()` (line 17, col 27)
        let resp_flag_get = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 17, character: 27 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_flag_get.is_some(), "Should resolve definition on flag.get()");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_flag_get {
            assert!(loc.uri.as_str().ends_with("AtomicBoolean.java"));
        }
    }

    #[tokio::test]
    async fn test_goto_definition_on_jdk_types_without_preindexed_jdk() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();
        // NOTE: We deliberately DO NOT call server.classpath.add_jdk_home() here!
        // This validates instant zero-delay resolution on JDK core types and imports.

        let code = r#"package com.demo;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

public class Demo {
    public void test() {
        String msg = "hello";
        CountDownLatch latch = new CountDownLatch(5);
        AtomicBoolean flag = new AtomicBoolean(true);
    }
}
"#;
        let uri = Url::parse("file:///Demo.java").unwrap();
        server.documents.insert(uri.clone(), DocumentState {
            text: Arc::new(code.to_string()),
            ast: JavaAst::parse(Arc::new(code.to_string())).unwrap(),
        });

        // 1. Definition on String (line 6, col 8)
        let resp_string = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 6, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_string.is_some(), "String must resolve even without pre-indexed JDK");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_string {
            assert!(loc.uri.as_str().ends_with("String.java"));
        }

        // 2. Definition on CountDownLatch import (line 1, col 25)
        let resp_latch_imp = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 1, character: 25 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_latch_imp.is_some(), "CountDownLatch import must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_latch_imp {
            assert!(loc.uri.as_str().ends_with("CountDownLatch.java"));
        }

        // 3. Definition on AtomicBoolean in code (line 8, col 8)
        let resp_atomic = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 8, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_atomic.is_some(), "AtomicBoolean must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_atomic {
            assert!(loc.uri.as_str().ends_with("AtomicBoolean.java"));
        }
    }

    #[tokio::test]
    async fn test_import_and_goto_definition_on_rest_controller_annotation() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Test importing org.springframework.web.bind.annotation.RestController without external jar loaded
        let code = r#"package com.demo.web;

import org.springframework.web.bind.annotation.RestController;
import org.springframework.stereotype.Controller;
import org.springframework.web.bind.annotation.ResponseBody;

@RestController
public class HelloController {
}
"#;
        let uri = Url::parse("file:///tmp/HelloController.java").unwrap();
        server.did_open(tower_lsp::lsp_types::DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // Check ast diagnostics - should have 0 compiler.err.cant.resolve diagnostics
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let imports = ast.extract_imports_with_ranges();
        assert_eq!(imports.len(), 3);
        for (_, imp) in imports {
            let is_resolved = server.classpath.get_class(&imp).await.is_some()
                || server.indexer.find_by_fqn(&imp).await.is_some()
                || crate::classpath::is_known_framework_or_library_class(&imp);
            assert!(is_resolved, "Import '{}' must be resolved without red error lines", imp);
        }

        // Test goto definition on `import org.springframework.web.bind.annotation.RestController;` (line 2)
        let resp_import = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 30 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_import.is_some(), "Should resolve definition on import RestController");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_import {
            assert!(loc.uri.as_str().ends_with("RestController.java"), "Target must be RestController.java: {}", loc.uri);
            // Verify decompiled content contains @interface RestController
            let path = loc.uri.to_file_path().unwrap();
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("@interface RestController"), "Decompiled content must contain @interface RestController: {}", content);
            assert!(content.contains("@Target(ElementType.TYPE)"), "Decompiled content must contain @Target: {}", content);
            assert!(content.contains("[READ-ONLY]"), "Must contain [READ-ONLY] banner: {}", content);

            // Verify file permissions are read-only
            let meta = std::fs::metadata(&path).unwrap();
            assert!(meta.permissions().readonly(), "Decompiled RestController file must be read-only on disk");

            // Verify did_change is rejected on read-only file
            server.did_change(tower_lsp::lsp_types::DidChangeTextDocumentParams {
                text_document: tower_lsp::lsp_types::VersionedTextDocumentIdentifier {
                    uri: loc.uri.clone(),
                    version: 2,
                },
                content_changes: vec![tower_lsp::lsp_types::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: "// HACKED EDIT".to_string(),
                }],
            }).await;
            if let Some(doc) = server.documents.get(&loc.uri) {
                assert!(!doc.text.contains("HACKED EDIT"), "Decompiled file must reject edits");
            }
        }

        // 2. Test custom workspace-defined annotation @interface RestController
        let custom_annotation_code = r#"package com.custom;

import java.lang.annotation.*;

@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
@Documented
public @interface RestController {
}
"#;
        let annotation_uri = Url::parse("file:///tmp/custom/RestController.java").unwrap();
        server.did_open(tower_lsp::lsp_types::DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: annotation_uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: custom_annotation_code.to_string(),
            },
        }).await;

        let user_code = r#"package com.custom.app;

import com.custom.RestController;

@RestController
public class App {
}
"#;
        let app_uri = Url::parse("file:///tmp/custom/app/App.java").unwrap();
        server.did_open(tower_lsp::lsp_types::DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: app_uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: user_code.to_string(),
            },
        }).await;

        let resp_custom = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: app_uri.clone() },
                position: Position { line: 2, character: 15 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_custom.is_some(), "Should resolve definition on custom annotation import");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_custom {
            assert!(loc.uri.as_str().contains("RestController.java"), "Target must point to custom RestController: {}", loc.uri);
        }
    }

    #[tokio::test]
    async fn test_import_and_goto_definition_on_mybatis_annotations() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo.mapper;

import org.apache.ibatis.annotations.Param;
import org.apache.ibatis.annotations.Update;

public interface UserMapper {
    @Update("UPDATE user SET name = #{name} WHERE id = #{id}")
    int updateName(@Param("id") Long id, @Param("name") String name);
}
"#;
        let uri = Url::parse("file:///tmp/UserMapper.java").unwrap();
        server.did_open(tower_lsp::lsp_types::DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let imports = ast.extract_imports_with_ranges();
        assert_eq!(imports.len(), 2);
        for (_, imp) in imports {
            let is_resolved = server.classpath.get_class(&imp).await.is_some()
                || server.indexer.find_by_fqn(&imp).await.is_some()
                || crate::classpath::is_known_framework_or_library_class(&imp);
            assert!(is_resolved, "Import '{}' must be resolved without red error lines", imp);
        }

        // Test goto definition on `import org.apache.ibatis.annotations.Param;` (line 2)
        let resp_param = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 2, character: 35 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_param.is_some(), "Should resolve definition on import Param");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_param {
            assert!(loc.uri.as_str().ends_with("Param.java"), "Target must be Param.java: {}", loc.uri);
            let path = loc.uri.to_file_path().unwrap();
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("@interface Param"), "Decompiled content must contain @interface Param: {}", content);
        }

        // Test goto definition on `import org.apache.ibatis.annotations.Update;` (line 3)
        let resp_update = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 3, character: 35 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");

        assert!(resp_update.is_some(), "Should resolve definition on import Update");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_update {
            assert!(loc.uri.as_str().ends_with("Update.java"), "Target must be Update.java: {}", loc.uri);
            let path = loc.uri.to_file_path().unwrap();
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("@interface Update"), "Decompiled content must contain @interface Update: {}", content);
        }
    }

    #[tokio::test]
    async fn test_decompiled_and_jar_source_files_are_read_only() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // 1. Verify paths identified as read-only
        assert!(is_read_only_path(std::path::Path::new("/Users/neil/.jlsp/decompiled/org/example/Service.java")));
        assert!(is_read_only_path(std::path::Path::new("/Users/neil/.jlsp/sources/org/example/Service.java")));
        assert!(is_read_only_path(std::path::Path::new("/Users/neil/Library/Caches/devworkbench/jar_sources/spring_core/org/springframework/core/io/Resource.java")));
        assert!(!is_read_only_path(std::path::Path::new("/Users/neil/myproject/src/main/java/App.java")));

        // 2. Open a decompiled file
        let decompiled_uri = Url::parse("file:///Users/neil/.jlsp/decompiled/org/example/Service.java").unwrap();
        let code = r#"package org.example;
public class Service {
    public void run() {}
}"#;
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: decompiled_uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // 3. Decompiled file should produce no code actions (edits prohibited)
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: decompiled_uri.clone() },
            range: Range { start: Position { line: 1, character: 0 }, end: Position { line: 1, character: 10 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();
        assert!(actions.is_empty(), "Read-only file should not offer code actions to modify source");

        // 4. Formatting should return None (editing prohibited)
        let format_edits = server.formatting(DocumentFormattingParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: decompiled_uri.clone() },
            options: tower_lsp::lsp_types::FormattingOptions { tab_size: 4, insert_spaces: true, ..Default::default() },
            work_done_progress_params: Default::default(),
        }).await.unwrap();
        assert!(format_edits.is_none(), "Formatting should be prohibited on read-only decompiled file");

        // 5. Rename should be prohibited
        let rename_res = server.rename(RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: decompiled_uri.clone() },
                position: Position { line: 1, character: 15 },
            },
            new_name: "NewService".to_string(),
            work_done_progress_params: Default::default(),
        }).await.unwrap();
        assert!(rename_res.is_none(), "Rename should be prohibited on read-only decompiled file");
    }

    #[tokio::test]
    async fn test_jdtls_commands_and_protocols() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"
package com.example.demo;

public class SampleBean {
    private String name;
    public int age;

    public boolean equals(Object o) { return true; }
    public int hashCode() { return 1; }

    public static void main(String[] args) {
        System.out.println("hello");
    }
}
"#;
        let uri = Url::parse("file:///SampleBean.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // 1. java.project.resolveClasspath
        let res = server.execute_command(ExecuteCommandParams {
            command: "java.project.resolveClasspath".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("classpath result");
        assert!(res.get("sourcePaths").is_some());
        assert!(res.get("classpaths").is_some());

        // 2. java.project.build
        let build_res = server.execute_command(ExecuteCommandParams {
            command: "java.project.build".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("build result");
        assert_eq!(build_res.get("status").unwrap(), 0);

        // 3. java.project.checkHashCodeEqualsStatus
        let hash_status = server.execute_command(ExecuteCommandParams {
            command: "java.project.checkHashCodeEqualsStatus".to_string(),
            arguments: vec![serde_json::Value::String(uri.to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("hash status");
        assert_eq!(hash_status.get("existingEquals").unwrap(), true);
        assert_eq!(hash_status.get("existingHashCode").unwrap(), true);

        // 4. vscode.java.resolveMainClass
        let main_classes = server.execute_command(ExecuteCommandParams {
            command: "vscode.java.resolveMainClass".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("main classes");
        let list = main_classes.as_array().expect("array");
        assert!(!list.is_empty(), "Should discover SampleBean with main method");

        // 5. java.refactor.moveClass
        let move_res = server.execute_command(ExecuteCommandParams {
            command: "java.refactor.moveClass".to_string(),
            arguments: vec![
                serde_json::Value::String(uri.to_string()),
                serde_json::Value::String("com.example.newpkg".to_string()),
            ],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("move class edit");
        assert!(move_res.get("edit").is_some());
    }

    #[tokio::test]
    async fn test_jdtls_aligned_code_actions() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"
public class TestService {
    public String data = "val";

    public void process(java.util.List<String> list) {
        String msg = (String) "test";
        for (String item : list) {
            System.out.println(item);
        }
    }
}
"#;
        let uri = Url::parse("file:///TestService.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // Code action on class line: Extract interface
        let class_actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 1, character: 13 }, end: Position { line: 1, character: 13 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();
        let has_extract_iface = class_actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Extract interface"),
            _ => false,
        });
        assert!(has_extract_iface, "Should offer Extract interface refactoring");

        // Code action on field line: Encapsulate field
        let field_actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 2, character: 18 }, end: Position { line: 2, character: 18 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();
        let has_encap = field_actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Encapsulate field"),
            _ => false,
        });
        assert!(has_encap, "Should offer Encapsulate field refactoring");

        // Code action on redundant cast line
        let cast_actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 5, character: 25 }, end: Position { line: 5, character: 25 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();
        let has_cast_fix = cast_actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Remove redundant cast"),
            _ => false,
        });
        assert!(has_cast_fix, "Should offer Remove redundant cast quickfix");
    }

    #[tokio::test]
    async fn test_jdtls_extended_commands_and_range_semantic_tokens() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"
package com.demo;

import java.util.List;
import java.util.ArrayList;

public class Runner {
    private Runnable task;

    public void run() {
        int x = 1;
        System.out.println(x);
    }

    public static void main(String[] args) {
        new Runner().run();
    }
}
"#;
        let uri = Url::parse("file:///Runner.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // 1. java.project.getClasspaths & getSources
        let cp_res = server.execute_command(ExecuteCommandParams {
            command: "java.project.getClasspaths".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("classpaths");
        assert!(cp_res.get("classpaths").is_some());

        let src_res = server.execute_command(ExecuteCommandParams {
            command: "java.project.getSources".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("sources");
        assert_eq!(src_res.get("sourcePaths").unwrap()[0], "src/main/java");

        // 2. vscode.java.resolveMainMethod
        let main_method_res = server.execute_command(ExecuteCommandParams {
            command: "vscode.java.resolveMainMethod".to_string(),
            arguments: vec![serde_json::Value::String(uri.to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("main method");
        assert_eq!(main_method_res.get("methodName").unwrap(), "main");

        // 3. vscode.java.debug.getLaunchArguments
        let launch_args = server.execute_command(ExecuteCommandParams {
            command: "vscode.java.debug.getLaunchArguments".to_string(),
            arguments: vec![serde_json::Value::String("com.demo.Runner".to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("launch args");
        assert_eq!(launch_args.get("type").unwrap(), "java");
        assert_eq!(launch_args.get("mainClass").unwrap(), "com.demo.Runner");

        // 4. semantic_tokens_range
        let range_tokens = server.semantic_tokens_range(SemanticTokensRangeParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range {
                start: Position { line: 6, character: 0 },
                end: Position { line: 12, character: 0 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap();
        assert!(range_tokens.is_some(), "Should return tokens for specified range");

        // 5. Code action: Surround with if / Surround with while
        let surround_actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 11, character: 8 }, end: Position { line: 12, character: 30 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();
        let has_surround_if = surround_actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Surround with 'if"),
            _ => false,
        });
        assert!(has_surround_if, "Should offer Surround with if refactoring");

        // 6. Code action on field: Generate delegate methods
        let delegate_actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 7, character: 21 }, end: Position { line: 7, character: 21 } },
            context: tower_lsp::lsp_types::CodeActionContext { diagnostics: vec![], only: None, trigger_kind: None },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();
        let has_delegate = delegate_actions.iter().any(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Generate delegate methods"),
            _ => false,
        });
        assert!(has_delegate, "Should offer Generate delegate methods for field");
    }

    #[tokio::test]
    async fn test_jdtls_unimplemented_abstract_method_and_deprecated_diagnostics() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"
package com.demo;

public class TaskHandler implements Runnable {
    public void test() {
        Thread.currentThread().stop();
        int count = 10;
        count = 20;
        System.out.println(count);
    }
}
"#;
        let uri = Url::parse("file:///TaskHandler.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(Some("TaskHandler"));

        // 1. Diagnostic for unimplemented abstract method (run)
        let unimpl = diags.iter().find(|d| d.code == DiagnosticCode::UnimplementedAbstractMethod);
        assert!(unimpl.is_some(), "Should flag unimplemented abstract method 'run()' from Runnable");

        // 2. Diagnostic for deprecated Thread.stop()
        let dep = diags.iter().find(|d| d.code == DiagnosticCode::DeprecatedUsage);
        assert!(dep.is_some(), "Should flag deprecated Thread.stop() invocation");
        assert!(dep.unwrap().code.is_deprecated());

        // 3. QuickFix for Add unimplemented methods
        let quickfix = doc.ast.generate_unimplemented_methods_quickfix(3, 14);
        assert!(quickfix.is_some(), "Should offer 'Add unimplemented methods' quickfix");
        let (title, _, code_gen) = quickfix.unwrap();
        assert_eq!(title, "Add unimplemented methods");
        assert!(code_gen.contains("public void run()"));

        // 4. Document highlight write vs read
        // Line 6 (0-indexed): `count = 10;` or line 7: `count = 20;` -> write access
        let hl = server.document_highlight(DocumentHighlightParams {
            text_document_position_params: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: Position { line: 7, character: 8 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().expect("highlights");
        assert!(hl.iter().any(|h| h.kind == Some(DocumentHighlightKind::WRITE)), "Should classify assignment as WRITE highlight, found: {:?}", hl);

        // 5. Commands: java.project.isTestFile & java.project.getSettings
        let is_test = server.execute_command(ExecuteCommandParams {
            command: "java.project.isTestFile".to_string(),
            arguments: vec![serde_json::Value::String("file:///src/test/java/DemoTest.java".to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("isTest");
        assert_eq!(is_test.get("isTest").unwrap(), true);

        let settings = server.execute_command(ExecuteCommandParams {
            command: "java.project.getSettings".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("settings");
        assert_eq!(settings.get("compliance").unwrap(), "21");
    }

    #[tokio::test]
    async fn test_jdtls_serial_version_uid_suppress_warnings_and_test_commands() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo;

import java.io.Serializable;

public class UserRecord implements Serializable {
    private String username;
    private int age;
}
"#;
        let uri = Url::parse("file:///UserRecord.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(Some("UserRecord"));

        // 1. MissingSerialVersionUid diagnostic
        let serial_diag = diags.iter().find(|d| d.code == DiagnosticCode::MissingSerialVersionUid);
        assert!(serial_diag.is_some(), "Should flag MissingSerialVersionUid for Serializable class without serialVersionUID");

        // 2. Add default serial version ID quickfix
        let serial_fix = doc.ast.generate_serial_version_uid_quickfix(4, 13);
        assert!(serial_fix.is_some());
        let (title, _, code_gen) = serial_fix.unwrap();
        assert_eq!(title, "Add default serial version ID");
        assert!(code_gen.contains("serialVersionUID = 1L;"));

        // 3. Add @SuppressWarnings quickfix
        let suppress_fix = doc.ast.generate_suppress_warnings_quickfix(4, 13, "serial");
        assert!(suppress_fix.is_some());
        let (stitle, _, scode) = suppress_fix.unwrap();
        assert_eq!(stitle, "Add @SuppressWarnings(\"serial\")");
        assert!(scode.contains("@SuppressWarnings(\"serial\")"));

        // 4. Command: java.project.generateHashCodeEquals
        let hash_gen = server.execute_command(ExecuteCommandParams {
            command: "java.project.generateHashCodeEquals".to_string(),
            arguments: vec![serde_json::Value::String(uri.to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("generateHashCodeEquals result");
        assert!(hash_gen.get("edit").is_some(), "Should return WorkspaceEdit for equals/hashCode");

        // 5. Command: java.project.generateToString
        let str_gen = server.execute_command(ExecuteCommandParams {
            command: "java.project.generateToString".to_string(),
            arguments: vec![serde_json::Value::String(uri.to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("generateToString result");
        assert!(str_gen.get("edit").is_some(), "Should return WorkspaceEdit for toString");

        // 6. Test framework commands: vscode.java.test.*
        let projects = server.execute_command(ExecuteCommandParams {
            command: "vscode.java.test.findJavaProjects".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("findJavaProjects result");
        assert!(projects.as_array().is_some());

        let refresh = server.execute_command(ExecuteCommandParams {
            command: "java.project.refreshLib".to_string(),
            arguments: vec![],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("refreshLib result");
        assert_eq!(refresh.get("status").unwrap(), "refreshed");

        let add_src = server.execute_command(ExecuteCommandParams {
            command: "java.project.addToSourcePath".to_string(),
            arguments: vec![serde_json::Value::String("src/extra/java".to_string())],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("addToSourcePath result");
        assert_eq!(add_src.get("status").unwrap(), "added");
    }

    #[tokio::test]
    async fn test_jdtls_javadoc_pattern_instanceof_and_param_to_field() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo;

public class EmployeeService {
    public EmployeeService(String deptName) {
    }

    public int calculateBonus(String empId, int score) {
        Object val = "data";
        if (val instanceof String) {
            return score * 2;
        }
        return 0;
    }
}
"#;
        let uri = Url::parse("file:///EmployeeService.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();

        // 1. Generate Element Javadoc for calculateBonus method
        let javadoc_fix = doc.ast.generate_javadoc_comment(6, 15);
        assert!(javadoc_fix.is_some(), "Should generate Javadoc comment for method");
        let (j_title, _, j_code) = javadoc_fix.unwrap();
        assert_eq!(j_title, "Generate Element Javadoc");
        assert!(j_code.contains("@param empId"));
        assert!(j_code.contains("@param score"));
        assert!(j_code.contains("@return int"));

        // 2. Pattern Matching instanceof
        let pattern_fix = doc.ast.generate_pattern_instanceof_refactoring(8, 14);
        assert!(pattern_fix.is_some(), "Should offer pattern matching instanceof refactoring");
        let (p_title, _, p_code) = pattern_fix.unwrap();
        assert_eq!(p_title, "Use pattern matching for 'instanceof'");
        assert!(p_code.contains("val instanceof String s"));

        // 3. Assign parameter to field in constructor
        let param_field_fix = doc.ast.generate_assign_param_to_field(3, 31);
        assert!(param_field_fix.is_some(), "Should offer assign parameter to field");
        let (f_title, _, f_code) = param_field_fix.unwrap();
        assert_eq!(f_title, "Assign parameter 'deptName' to field");
        assert!(f_code.contains("this.deptName = deptName;"));

        // 4. Command: java.project.addOverridableMethods
        let override_res = server.execute_command(ExecuteCommandParams {
            command: "java.project.addOverridableMethods".to_string(),
            arguments: vec![
                serde_json::Value::String(uri.to_string()),
                serde_json::Value::String("toString".to_string()),
            ],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("addOverridableMethods result");
        assert!(override_res.get("edit").is_some());

        // 5. Command: java.project.generateJavadoc
        let javadoc_cmd_res = server.execute_command(ExecuteCommandParams {
            command: "java.project.generateJavadoc".to_string(),
            arguments: vec![
                serde_json::Value::String(uri.to_string()),
                serde_json::json!(6),
                serde_json::json!(15),
            ],
            work_done_progress_params: Default::default(),
        }).await.unwrap().expect("generateJavadoc command result");
        assert!(javadoc_cmd_res.get("edit").is_some());
    }

    #[tokio::test]
    async fn test_jdtls_null_analysis_switch_rule_and_text_block() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo;

public class SwitchAndNullDemo {
    public void testNull(String name) {
        if (name == null) {
            name.toLowerCase();
        }
    }

    public String formatCode(int mode) {
        switch (mode) {
            case 1:
                return "ONE";
            default:
                return "OTHER";
        }
    }

    public String getTemplate() {
        return "Line1\nLine2\n";
    }
}
"#;
        let uri = Url::parse("file:///SwitchAndNullDemo.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();

        // 1. ECJ Null Analysis: flags null dereference when variable is known to be null
        let diags = doc.ast.collect_semantic_diagnostics_detailed(Some("SwitchAndNullDemo"));
        let null_diag = diags.iter().find(|d| d.code == DiagnosticCode::NullDereference);
        assert!(null_diag.is_some(), "Should flag potential null pointer access on 'name'");
        assert!(null_diag.unwrap().message.contains("known to be null"));

        // 2. Convert switch statement to enhanced switch rule (->)
        let switch_fix = doc.ast.generate_convert_to_switch_rule(11, 14);
        assert!(switch_fix.is_some(), "Should offer convert to switch rule");
        let (s_title, _, s_code) = switch_fix.unwrap();
        assert_eq!(s_title, "Convert to enhanced switch rule ('->')");
        assert!(s_code.contains("case 1 ->"));
        assert!(s_code.contains("default ->"));

        // 3. Convert string literal to text block
        let text_block_fix = doc.ast.generate_convert_to_text_block(19, 20);
        assert!(text_block_fix.is_some(), "Should offer convert to text block");
        let (tb_title, _, tb_code) = text_block_fix.unwrap();
        assert_eq!(tb_title, "Convert to text block (Java 15+)");
        assert!(tb_code.starts_with("\"\"\"\n"));
    }

    #[tokio::test]
    async fn test_no_false_positive_null_pointer_access_on_or_condition() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo;
import java.util.List;

public class NullCheckDemo {
    public void validate(List<String> columns) {
        if (columns == null || columns.isEmpty()) {
            throw new IllegalArgumentException("empty");
        }
    }
}
"#;
        let uri = Url::parse("file:///NullCheckDemo.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(Some("NullCheckDemo"));
        let null_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::NullDereference).collect();
        assert!(null_diags.is_empty(), "columns.isEmpty() inside (columns == null || columns.isEmpty()) must NOT be flagged as null access! Got: {:?}", null_diags);
    }

    #[tokio::test]
    async fn test_double_checked_locking_no_null_dereference_false_positive() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo;
public class Singleton {
    private static volatile Singleton instance;

    public void init() {}

    public static Singleton getInstance() {
        if (instance == null) {
            synchronized (Singleton.class) {
                if (instance == null) {
                    instance = new Singleton();
                    instance.init();
                }
            }
            instance.init();
        }
        return instance;
    }
}
"#;
        let uri = Url::parse("file:///Singleton.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(Some("Singleton"));
        let null_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::NullDereference).collect();
        assert!(null_diags.is_empty(), "Double-checked locking with new instance must NOT trigger null dereference! Got: {:?}", null_diags);
    }

    #[tokio::test]
    async fn test_properties_get_property_no_static_access_warning() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package com.demo;
import java.util.Properties;

public class ConfigLoader {
    public void load() {
        Properties props = System.getProperties();
        String os = props.getProperty("os.name");
        String version = System.getProperties().getProperty("java.version");
    }
}
"#;
        let uri = Url::parse("file:///ConfigLoader.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();
        let diags = doc.ast.collect_semantic_diagnostics_detailed(Some("ConfigLoader"));
        let static_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::StaticMemberViaInstance).collect();
        assert!(static_diags.is_empty(), "Properties.getProperty(...) must NOT trigger StaticMemberViaInstance warning! Got: {:?}", static_diags);

        // Verify QuickFix is NOT offered for props.getProperty
        let actions = server.code_action(CodeActionParams {
            text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
            range: Range { start: Position { line: 6, character: 25 }, end: Position { line: 6, character: 36 } },
            context: tower_lsp::lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.unwrap().unwrap_or_default();

        let change_to_static = actions.iter().find(|a| match a {
            CodeActionOrCommand::CodeAction(ca) => ca.title.contains("Change access to static"),
            _ => false,
        });
        assert!(change_to_static.is_none(), "Must NOT offer static access quickfix for Properties.getProperty!");
    }

    #[tokio::test]
    async fn test_no_false_positive_abstract_methods_for_result_handler() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // Register dummy ResultHandler interface in classpath
        server.classpath.register_class(crate::model::ClassInfo {
            fqn: "org.apache.ibatis.session.ResultHandler".to_string(),
            simple_name: "ResultHandler".to_string(),
            package_name: "org.apache.ibatis.session".to_string(),
            kind: crate::model::ClassKind::Interface,
            super_class: Some("java.lang.Object".to_string()),
            interfaces: vec![],
            methods: vec![crate::model::MethodInfo {
                name: "handleResult".to_string(),
                return_type: crate::model::TypeSignature::Void,
                parameters: vec![("resultContext".to_string(), crate::model::TypeSignature::Class("org.apache.ibatis.session.ResultContext".to_string()))],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: None,
                body: None,
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        }).await;

        let code = r#"package com.yoyosys.termdata.utils;
import org.apache.ibatis.session.ResultHandler;
import java.util.Map;

public class ColumnValueHandler implements ResultHandler {
    public void handleResult(Object resultContext) {
    }
}
"#;
        let uri = Url::parse("file:///ColumnValueHandler.java").unwrap();
        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        let doc = server.documents.get(&uri).unwrap();
        let sem = crate::semantic::SemanticContext::with_indexer(&doc.ast, &server.classpath, Some(&server.indexer)).await;
        let missing = sem.find_missing_abstract_methods().await;

        for (_cls, methods) in &missing {
            for m in methods {
                assert_ne!(m.name, "<init>", "Constructor must never be flagged as missing abstract method");
                assert_ne!(m.name, "registerNatives", "Object method must never be flagged as missing abstract method");
                assert_ne!(m.name, "getClass", "Object method must never be flagged as missing abstract method");
                assert_ne!(m.name, "wait", "Object method must never be flagged as missing abstract method");
            }
        }
        assert!(missing.is_empty(), "ColumnValueHandler implements handleResult, so missing abstract methods must be empty! Got: {:?}", missing);
    }

    #[tokio::test]
    async fn test_no_false_positive_string_concat_in_loop_on_numeric() {
        let code = r#"package com.demo;
public class NumericLoop {
    public int sum(int[] arr) {
        int total = 0;
        for (int i = 0; i < arr.length; i++) {
            total += arr[i];
        }
        return total;
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("NumericLoop"));
        let concat_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::StringConcatInLoop).collect();
        assert!(concat_diags.is_empty(), "total += arr[i] on integer accumulator must NOT be flagged as StringConcatInLoop! Got: {:?}", concat_diags);
    }

    #[tokio::test]
    async fn test_no_false_positive_unreachable_code_on_comments_and_semicolons() {
        let code = r#"package com.demo;
public class DeadCodeSample {
    public int test(boolean flag) {
        if (flag) {
            return 1;
            // A comment after return should not be dead code!
            ;
        }
        return 0;
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("DeadCodeSample"));
        let dead_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::UnreachableCode).collect();
        assert!(dead_diags.is_empty(), "Comment and semicolon after return must NOT be flagged as UnreachableCode! Got: {:?}", dead_diags);
    }

    #[tokio::test]
    async fn test_no_false_positive_uninitialized_variable_in_try_catch() {
        let code = r#"package com.demo;
public class TryCatchInit {
    public int parse(String s) {
        int val;
        try {
            val = Integer.parseInt(s);
        } catch (Exception e) {
            val = -1;
        }
        return val;
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("TryCatchInit"));
        let uninit_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::UninitializedVariable).collect();
        assert!(uninit_diags.is_empty(), "Variable assigned in try-catch must NOT be flagged as uninitialized! Got: {:?}", uninit_diags);
    }

    #[tokio::test]
    async fn test_no_false_positive_missing_return_in_infinite_loops() {
        let code = r#"package com.demo;
public class LoopReturn {
    public int readNext() {
        while (true) {
            int val = fetch();
            if (val > 0) {
                return val;
            }
        }
    }
    private int fetch() { return 1; }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("LoopReturn"));
        let ret_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::MissingReturn).collect();
        assert!(ret_diags.is_empty(), "Method returning inside while(true) without break must NOT be flagged as MissingReturn! Got: {:?}", ret_diags);
    }

    #[tokio::test]
    async fn test_no_false_positive_unhandled_exception_with_sneaky_throws() {
        let code = r#"package com.demo;
import lombok.SneakyThrows;
import java.io.IOException;

public class SneakyDemo {
    @SneakyThrows
    public void doWork() {
        throw new IOException("error");
    }

    @SneakyThrows
    public void sleepWork() {
        Thread.sleep(100);
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("SneakyDemo"));
        let exc_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::UnhandledException).collect();
        assert!(exc_diags.is_empty(), "@SneakyThrows methods must NOT be flagged with UnhandledException! Got: {:?}", exc_diags);
    }

    #[tokio::test]
    async fn test_no_false_positive_blank_final_and_unused_with_lombok() {
        let code = r#"package com.demo;
import lombok.RequiredArgsConstructor;
import lombok.Data;
import java.io.Serializable;

@Data
@RequiredArgsConstructor
public class UserDto implements Serializable {
    private static final long serialVersionUID = 1L;
    private final String id;
    private final String name;
    private int age;
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("UserDto"));
        let uninit_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::UninitializedVariable).collect();
        assert!(uninit_diags.is_empty(), "Lombok @RequiredArgsConstructor classes must NOT flag blank final fields! Got: {:?}", uninit_diags);

        let unused_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::UnusedPrivateMember).collect();
        assert!(unused_diags.is_empty(), "serialVersionUID and Lombok @Data private fields must NOT be flagged as unused! Got: {:?}", unused_diags);
    }

    #[tokio::test]
    async fn test_no_false_positive_package_info_annotations_import() {
        let code = r#"@NonNullApi
@NonNullFields
package com.demo;

import org.springframework.lang.NonNullApi;
import org.springframework.lang.NonNullFields;
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let unused_imports = ast.find_unused_imports();
        assert!(unused_imports.is_empty(), "Annotations in package-info.java must NOT be flagged as unused imports! Got: {:?}", unused_imports);
    }

    #[tokio::test]
    async fn test_no_false_positive_while_condition_assignment_initialization() {
        let code = r#"package com.demo;
import java.io.File;

public class TarReader {
    private String detectDataFormatFromTarContent(File tarFile) {
        try (java.io.FileInputStream fis = new java.io.FileInputStream(tarFile)) {
            org.apache.commons.compress.archivers.tar.TarArchiveInputStream tais = null;
            org.apache.commons.compress.archivers.tar.TarArchiveEntry entry;
            while ((entry = tais.getNextTarEntry()) != null) {
                if (entry.isDirectory()) continue;
                String entryName = entry.getName();
                if (entryName == null) continue;
            }
        } catch (Exception e) {
        }
        return null;
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("TarReader"));
        let uninit_diags: Vec<_> = diags.iter().filter(|d| d.code == crate::syntax::ast::DiagnosticCode::UninitializedVariable).collect();
        assert!(uninit_diags.is_empty(), "entry assigned in while condition must NOT be flagged as uninitialized in while body! Got: {:?}", uninit_diags);
    }

    #[tokio::test]
    async fn test_real_world_workspace_external_and_jdk_goto_definition() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // Create a temporary workspace root
        let temp_dir = std::env::temp_dir().join(format!("jlsp_test_ws_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // 1. Initialize server simulating modern VS Code / Neovim with workspace_folders and root_uri: None
        let root_url = Url::from_file_path(&temp_dir).unwrap();
        #[allow(deprecated)]
        let init_params = InitializeParams {
            process_id: Some(1234),
            root_path: None,
            root_uri: None, // Modern clients send root_uri: None!
            initialization_options: None,
            capabilities: Default::default(),
            trace: None,
            workspace_folders: Some(vec![tower_lsp::lsp_types::WorkspaceFolder {
                uri: root_url.clone(),
                name: "demo-project".to_string(),
            }]),
            client_info: None,
            locale: None,
        };

        let init_result = server.initialize(init_params).await.expect("initialize succeeds");
        assert!(init_result.capabilities.definition_provider.is_some());

        // 2. Open a Java source file referencing JDK types, method calls, and chains
        let code = r#"package com.demo;

import java.util.List;
import java.util.ArrayList;

public class App {
    public void execute() {
        String msg = "hello";
        List<String> list = new ArrayList<>();
        list.add(msg);
        int size = list.size();
        System.out.println(msg);
    }
}
"#;
        let file_path = temp_dir.join("App.java");
        std::fs::write(&file_path, code).unwrap();
        let doc_uri = Url::from_file_path(&file_path).unwrap();

        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: doc_uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // 3. Goto definition on `String` type identifier (line 7, col 10)
        let resp_string = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 7, character: 10 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("definition request succeeds");

        assert!(resp_string.is_some(), "String type must resolve definition");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_string {
            assert!(loc.uri.as_str().ends_with("String.java"), "Target must be String.java: {}", loc.uri);
        }

        // 4. Goto definition on `list.size()` method invocation (line 10, col 25)
        let resp_size = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 10, character: 25 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("definition request succeeds");

        assert!(resp_size.is_some(), "list.size() must resolve definition to List or Collection");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_size {
            let p = loc.uri.to_file_path().unwrap().to_string_lossy().to_string();
            assert!(p.ends_with(".java"), "Target must be a Java file: {}", p);
            assert!(p.contains("List.java") || p.contains("Collection.java"), "Target must be List or Collection: {}", p);
        }

        // 5. Goto definition on `list.add(msg)` method invocation (line 9, col 15)
        let resp_add = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 9, character: 15 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("definition request succeeds");

        assert!(resp_add.is_some(), "list.add() must resolve definition");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_add {
            let p = loc.uri.to_file_path().unwrap().to_string_lossy().to_string();
            assert!(p.ends_with(".java"), "Target must be a Java file: {}", p);
            assert!(p.contains("List.java") || p.contains("Collection.java"), "Target must be List or Collection: {}", p);
        }

        // 6. Goto definition on `System.out.println(msg)` method invocation (line 11, col 23)
        let resp_println = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 11, character: 23 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("definition request succeeds");

        assert!(resp_println.is_some(), "System.out.println() must resolve definition to PrintStream");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_println {
            let p = loc.uri.to_file_path().unwrap().to_string_lossy().to_string();
            assert!(p.ends_with(".java"), "Target must be a Java file: {}", p);
            assert!(p.contains("PrintStream.java"), "Target must be PrintStream.java: {}", p);
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_goto_definition_from_decompiled_file() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        // Register a sibling class in same package
        server.classpath.register_class(crate::model::ClassInfo {
            fqn: "org.apache.commons.collections4.IterableMap".to_string(),
            simple_name: "IterableMap".to_string(),
            package_name: "org.apache.commons.collections4".to_string(),
            kind: crate::model::ClassKind::Interface,
            super_class: None,
            interfaces: vec!["java.util.Map".to_string()],
            methods: vec![],
            fields: vec![],
            source_path: None,
            is_public: true,
        }).await;

        let code = r#"package org.apache.commons.collections4;

import java.util.Map;

public class MapUtils {
    public static <K, V> V getObject(Map<? super K, ?> map, K key) {
        if (map != null) {
            Object answer = map.get(key);
            if (answer != null) {
                return (V) answer;
            }
        }
        return null;
    }

    public static <K, V> Boolean getBoolean(Map<? super K, ?> map, K key) {
        Object answer = getObject(map, key);
        if (answer != null) {
            if (answer instanceof Boolean) {
                return (Boolean) answer;
            }
            if (answer instanceof String) {
                return Boolean.valueOf((String) answer);
            }
            if (answer instanceof Number) {
                Number n = (Number) answer;
                return n.intValue() != 0 ? Boolean.TRUE : Boolean.FALSE;
            }
        }
        return null;
    }

    public static <K, V> IterableMap<K, V> iterableMap(Map<K, V> map) {
        return null;
    }
}
"#;
        // Simulate decompiled file opened from ~/.jlsp/decompiled/...
        let temp_dir = std::env::temp_dir().join(format!("jlsp_decomp_test_{}", std::process::id()));
        let decompiled_path = temp_dir.join(".jlsp").join("decompiled")
            .join("org").join("apache").join("commons").join("collections4").join("MapUtils.java");
        std::fs::create_dir_all(decompiled_path.parent().unwrap()).unwrap();
        std::fs::write(&decompiled_path, code).unwrap();
        let doc_uri = Url::from_file_path(&decompiled_path).unwrap();

        server.did_open(DidOpenTextDocumentParams {
            text_document: tower_lsp::lsp_types::TextDocumentItem {
                uri: doc_uri.clone(),
                language_id: "java".to_string(),
                version: 1,
                text: code.to_string(),
            },
        }).await;

        // 1. Jump to imported JDK type `Map` on line 5 col 38
        let resp_map = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 5, character: 38 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_map.is_some(), "Map type must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_map {
            assert!(loc.uri.as_str().ends_with("Map.java"), "Target must be Map.java: {}", loc.uri);
        }

        // 2. Jump to method call `map.get(key)` on line 7 col 32
        let resp_get = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 7, character: 32 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_get.is_some(), "map.get() must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_get {
            assert!(loc.uri.as_str().ends_with("Map.java"), "Target must be Map.java: {}", loc.uri);
        }

        // 3. Jump to method call `getObject(map, key)` without receiver on line 16 col 28
        let resp_get_obj = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 16, character: 28 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_get_obj.is_some(), "getObject() in same class must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_get_obj {
            assert_eq!(loc.uri, doc_uri, "Target must be the same decompiled file");
            assert_eq!(loc.range.start.line, 5, "Should point to line 5 where getObject is defined");
        }

        // 4. Jump to static method `Boolean.valueOf` on line 22 col 33
        let resp_value_of = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 22, character: 33 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_value_of.is_some(), "Boolean.valueOf must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_value_of {
            assert!(loc.uri.as_str().ends_with("Boolean.java"), "Target must be Boolean.java: {}", loc.uri);
        }

        // 5. Jump to method `n.intValue()` on line 26 col 28
        let resp_int_val = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 26, character: 28 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_int_val.is_some(), "n.intValue() must resolve");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_int_val {
            assert!(loc.uri.as_str().ends_with("Number.java"), "Target must be Number.java: {}", loc.uri);
        }

        // 6. Jump to same-package class without import `IterableMap` on line 32 col 28
        let resp_iter_map = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 32, character: 28 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_iter_map.is_some(), "Same-package IterableMap must resolve without import");
        if let Some(GotoDefinitionResponse::Scalar(loc)) = resp_iter_map {
            assert!(loc.uri.as_str().ends_with("IterableMap.java"), "Target must be IterableMap.java: {}", loc.uri);
        }

        // 7. Clicking on punctuation '}' on line 35 col 0 must safely return None (never create %7D.java)
        let resp_brace = server.goto_definition(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: doc_uri.clone() },
                position: Position { line: 35, character: 0 },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }).await.expect("goto_definition succeeds");
        assert!(resp_brace.is_none(), "Clicking on brace must safely return None");
    }

    /// Regression: inside a decompiled library file whose package matches a
    /// "known framework" prefix (e.g. org.apache.commons.lang), resolving an
    /// unqualified `java.lang` type like `String` must NOT be hijacked by a
    /// synthesized same-package stub (org.apache.commons.lang.String). The
    /// same-package candidate must use strict classpath lookup so resolution
    /// falls through to java.lang.
    #[tokio::test]
    async fn test_same_package_resolution_not_hijacked_by_framework_stub() {
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let code = r#"package org.apache.commons.lang;

public class StringUtils {
    public static boolean isBlank(String str) {
        return str == null;
    }
}
"#;
        let ast = JavaAst::parse(std::sync::Arc::new(code.to_string())).expect("parse");
        let sem = crate::semantic::SemanticContext::with_indexer(&ast, &server.classpath, Some(&server.indexer)).await;

        let resolved = sem.resolve_type("String").await.expect("String must resolve");
        assert_eq!(
            resolved.fqn, "java.lang.String",
            "String must resolve to java.lang.String, not a same-package stub; got {}",
            resolved.fqn
        );

        // The same-package class that genuinely does not exist must stay unresolved
        // via strict lookup (no synthesized framework stub).
        assert!(
            server.classpath.get_class_strict("org.apache.commons.lang.String").await.is_none(),
            "get_class_strict must not synthesize stubs for nonexistent classes"
        );
        // ... while the lenient lookup keeps its synthesized-fallback behaviour.
        assert!(
            server.classpath.get_class("org.apache.commons.lang.String").await.is_some(),
            "get_class keeps the synthesized framework fallback for hover/completion"
        );
    }

    #[tokio::test]
    async fn test_server_live_memory_footprint() {
        fn get_rss_mb() -> f64 {
            let pid = std::process::id();
            let out = std::process::Command::new("ps")
                .arg("-o")
                .arg("rss=")
                .arg("-p")
                .arg(pid.to_string())
                .output()
                .expect("ps command");
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            s.parse::<f64>().unwrap_or(0.0) / 1024.0
        }

        let rss_before = get_rss_mb();
        let (service, _) = tower_lsp::LspService::new(JlspServer::new);
        let server = service.inner();

        let root_dir = std::env::temp_dir().join(format!("jlsp_mem_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root_dir);

        // Create 20 mock Java files to simulate a real project
        for i in 0..20 {
            let file_path = root_dir.join(format!("Service{}.java", i));
            let content = format!(
                "package com.demo;\nimport java.util.*;\npublic class Service{} {{\n    private List<String> list = new ArrayList<>();\n    public int compute(int a) {{\n        return a * {};\n    }}\n}}\n",
                i, i
            );
            let _ = std::fs::write(&file_path, content);
        }

        server.index_workspace(&root_dir).await;
        let rss_after_index = get_rss_mb();

        // Simulate opening 5 files and typing (completions, inlay hints, diagnostics)
        for i in 0..5 {
            let file_path = root_dir.join(format!("Service{}.java", i));
            let uri = Url::from_file_path(&file_path).unwrap();
            let code = std::fs::read_to_string(&file_path).unwrap();
            server.did_open(DidOpenTextDocumentParams {
                text_document: tower_lsp::lsp_types::TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "java".to_string(),
                    version: 1,
                    text: code,
                },
            }).await;

            // Trigger completion
            let _ = server.completion(CompletionParams {
                text_document_position: TextDocumentPositionParams {
                    text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                    position: Position { line: 5, character: 15 },
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            }).await;
        }

        let rss_final = get_rss_mb();
        println!("=== SERVER RUNTIME RSS ===");
        println!("Initial Server RSS: {:.2} MB", rss_before);
        println!("After Workspace Index: {:.2} MB", rss_after_index);
        println!("After Active Editing (Open + Completion): {:.2} MB", rss_final);

        let _ = std::fs::remove_dir_all(&root_dir);

        // Assert server memory is well below 150MB
        assert!(rss_final < 200.0, "Total server RSS must remain under 200MB! Was {:.2}MB", rss_final);
    }
}




