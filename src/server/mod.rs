use std::collections::HashMap;
use std::sync::Arc;
use dashmap::DashMap;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{
    GotoImplementationParams, GotoImplementationResponse,
    GotoTypeDefinitionParams, GotoTypeDefinitionResponse,
};
use tower_lsp::lsp_types::{
    CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
    CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
    CallHierarchyServerCapability, CodeAction, CodeActionKind, CodeActionOptions,
    CodeActionOrCommand, CodeActionParams, CodeActionProviderCapability, CodeActionResponse,
    CodeLens, CodeLensOptions, CodeLensParams, Command, CompletionItem as LspCompletionItem,
    CompletionItemKind, CompletionOptions, CompletionParams, CompletionResponse, Diagnostic,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
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
    SemanticTokensResult, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    SignatureHelp, SignatureHelpOptions, SignatureHelpParams, SignatureInformation,
    SymbolInformation, SymbolKind as LspSymbolKind, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, TypeHierarchyItem,
    TypeHierarchyPrepareParams, TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, Url,
    WorkspaceEdit, WorkspaceSymbolParams, DocumentOnTypeFormattingOptions, DocumentOnTypeFormattingParams,
    DidChangeWorkspaceFoldersParams, ExecuteCommandParams,
};
use tower_lsp::{Client, LanguageServer};
use serde_json::Value;
use tracing::info;

use crate::classpath::ClasspathRegistry;
use crate::indexer::WorkspaceIndex;
use crate::project::{GradleProject, MavenProject};
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
        let cache_file = root_path.join(".jlsp").join("index.json");

        // Try fast cold-start from disk cache
        if let Ok(loaded) = self.classpath.load_cache(&cache_file).await
            && loaded > 0 {
                info!("Fast cold-start: loaded {} types from persistent index cache.", loaded);
            }

        // 1. Index Java source files
        self.indexer.scan_workspace(root_path).await;

        // 2. Index workspace jars
        info!("Scanning workspace for .jar dependencies at: {:?}", root_path);
        for entry in walkdir::WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("jar") {
                let _ = self.classpath.add_jar(path).await;
            }
        }

        // 3. Resolve Maven pom.xml (including multi-modules) if present
        let pom_file = root_path.join("pom.xml");
        if pom_file.exists() {
            info!("Found Maven pom.xml, resolving multi-module dependencies...");
            if let Ok(maven) = MavenProject::parse(&pom_file) {
                let jars = maven.resolve_all_modules_jars().await;
                for jar in jars {
                    let _ = self.classpath.add_jar(&jar).await;
                }
            }
        }

        // 4. Resolve Gradle build.gradle / build.gradle.kts (including multi-projects) if present
        let gradle_files = [root_path.join("build.gradle"), root_path.join("build.gradle.kts")];
        for g_file in gradle_files {
            if g_file.exists() {
                info!("Found Gradle build file ({:?}), resolving multi-project dependencies...", g_file);
                if let Ok(gradle) = GradleProject::parse(&g_file) {
                    let jars = gradle.resolve_all_modules_jars();
                    for jar in jars {
                        let _ = self.classpath.add_jar(&jar).await;
                    }
                }
                break;
            }
        }

        // 5. Persist updated registry to disk cache asynchronously
        let _ = self.classpath.save_cache(&cache_file).await;
    }

    async fn publish_document_diagnostics(&self, uri: &Url, ast: &JavaAst) {
        let mut diagnostics = Vec::new();

        // 1. Syntax redline errors
        for (range, message) in ast.collect_syntax_errors() {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position { line: range.start.line, character: range.start.character },
                    end: Position { line: range.end.line, character: range.end.character },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                code: None,
                code_description: None,
                source: Some("jlsp-syntax".to_string()),
                message,
                related_information: None,
                tags: None,
                data: None,
            });
        }

        // 2. Semantic type checking diagnostics
        for (range, message) in ast.collect_semantic_diagnostics() {
            diagnostics.push(Diagnostic {
                range: Range {
                    start: Position { line: range.start.line, character: range.start.character },
                    end: Position { line: range.end.line, character: range.end.character },
                },
                severity: Some(DiagnosticSeverity::WARNING),
                code: None,
                code_description: None,
                source: Some("jlsp-typecheck".to_string()),
                message,
                related_information: None,
                tags: None,
                data: None,
            });
        }

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for JlspServer {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        info!("Initializing pure-Rust jlsp server with FULL Java capabilities...");

        if let Some(root_uri) = params.root_uri {
            if let Ok(path) = root_uri.to_file_path() {
                let cp = Arc::clone(&self.classpath);
                tokio::spawn(async move {
                    if let Ok(java_home) = std::env::var("JAVA_HOME") {
                        let _ = cp.add_jdk_home(java_home).await;
                    }
                });
                self.index_workspace(&path).await;
            }
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
                        range: None,
                        work_done_progress_options: Default::default(),
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), "@".to_string()]),
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
                        ]),
                        ..Default::default()
                    },
                )),
                rename_provider: Some(OneOf::Right(tower_lsp::lsp_types::RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                type_definition_provider: Some(tower_lsp::lsp_types::TypeDefinitionProviderCapability::Simple(true)),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: Some(DocumentOnTypeFormattingOptions {
                    first_trigger_character: ";".to_string(),
                    more_trigger_character: Some(vec!["}".to_string(), "\n".to_string()]),
                }),
                execute_command_provider: Some(tower_lsp::lsp_types::ExecuteCommandOptions {
                    commands: vec!["java.project.runTest".to_string(), "java.project.cleanCache".to_string()],
                    work_done_progress_options: Default::default(),
                }),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "jlsp (pure-Rust JVM-free)".to_string(),
                version: Some("1.1.0".to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "jlsp: Java LSP ready (Test runner & Hot Reload enabled).")
            .await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        for change in params.changes {
            if let Ok(path) = change.uri.to_file_path() {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    match ext {
                        "java" => {
                            if change.typ == FileChangeType::DELETED {
                                // Handled automatically on next scan
                            } else if let Ok(content) = std::fs::read_to_string(&path) {
                                self.indexer.index_file(&path, &content).await;
                            }
                        }
                        "xml" | "gradle" | "kts" => {
                            info!("Build configuration changed ({:?}), reloading dependencies...", path);
                            if let Some(parent) = path.parent() {
                                self.index_workspace(parent).await;
                            }
                        }
                        _ => {}
                    }
                }
            }
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
            self.publish_document_diagnostics(&uri, &ast).await;
            if let Ok(path) = uri.to_file_path() {
                self.indexer.index_file(&path, &text).await;
            }
            self.documents.insert(uri, DocumentState { text, ast });
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Some(change) = params.content_changes.into_iter().next() {
            let text = Arc::new(change.text);
            if let Some(ast) = JavaAst::parse(Arc::clone(&text)) {
                self.publish_document_diagnostics(&uri, &ast).await;
                if let Ok(path) = uri.to_file_path() {
                    self.indexer.index_file(&path, &text).await;
                }
                self.documents.insert(uri, DocumentState { text, ast });
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

        // 1. References CodeLens
        let symbols = doc.ast.extract_symbols();
        for s in &symbols {
            if matches!(s.kind, crate::model::SymbolKind::Class | crate::model::SymbolKind::Method) {
                let count = doc.ast.find_all_occurrences(&s.name).len().saturating_sub(1);
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
        }

        // 2. Test & Main Method Runner and Debugger CodeLens
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
        let links = raw_links
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
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let name = doc.ast.node_text(&node);
            if let Some(cls) = sem.resolve_type(name).await {
                return Ok(Some(vec![TypeHierarchyItem {
                    name: cls.simple_name.clone(),
                    kind: LspSymbolKind::CLASS,
                    tags: None,
                    detail: Some(cls.package_name.clone()),
                    uri: uri.clone(),
                    range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                    selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                    data: None,
                }]));
            }
        }

        Ok(None)
    }

    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let name = params.item.name;
        for item in self.documents.iter() {
            let doc = item.value();
            let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
            if let Some(cls) = sem.resolve_type(&name).await {
                let mut supers = Vec::new();
                if let Some(super_cls) = &cls.super_class {
                    supers.push(TypeHierarchyItem {
                        name: super_cls.clone(),
                        kind: LspSymbolKind::CLASS,
                        tags: None,
                        detail: None,
                        uri: item.key().clone(),
                        range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        data: None,
                    });
                }
                for iface in &cls.interfaces {
                    supers.push(TypeHierarchyItem {
                        name: iface.clone(),
                        kind: LspSymbolKind::INTERFACE,
                        tags: None,
                        detail: None,
                        uri: item.key().clone(),
                        range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                        data: None,
                    });
                }
                return Ok(Some(supers));
            }
        }
        Ok(None)
    }

    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let name = params.item.name;
        for item in self.documents.iter() {
            let doc = item.value();
            let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
            let subs = sem.find_implementations(&name).await;
            let items = subs
                .into_iter()
                .map(|c| TypeHierarchyItem {
                    name: c.simple_name,
                    kind: LspSymbolKind::CLASS,
                    tags: None,
                    detail: Some(c.package_name),
                    uri: item.key().clone(),
                    range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                    selection_range: Range { start: Position { line: 0, character: 0 }, end: Position { line: 0, character: 0 } },
                    data: None,
                })
                .collect();
            return Ok(Some(items));
        }
        Ok(None)
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri;
        let pos = params.position;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let r = doc.ast.node_range(&node);
            return Ok(Some(PrepareRenameResponse::Range(Range {
                start: Position { line: r.start.line, character: r.start.character },
                end: Position { line: r.end.line, character: r.end.character },
            })));
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

        for item in self.documents.iter() {
            let doc_uri = item.key();
            let doc = item.value();
            for (line, _) in doc.text.lines().enumerate() {
                if let Some((method_name, range)) = doc.ast.find_method_declaration_at_position(line as u32, 0) {
                    let outgoing = doc.ast.find_outgoing_calls(&method_name);
                    for (called, from_range) in outgoing {
                        if called == target_name {
                            incoming.push(CallHierarchyIncomingCall {
                                from: CallHierarchyItem {
                                    name: method_name.clone(),
                                    kind: LspSymbolKind::METHOD,
                                    tags: None,
                                    detail: None,
                                    uri: doc_uri.clone(),
                                    range: Range {
                                        start: Position { line: range.start.line, character: range.start.character },
                                        end: Position { line: range.end.line, character: range.end.character },
                                    },
                                    selection_range: Range {
                                        start: Position { line: range.start.line, character: range.start.character },
                                        end: Position { line: range.end.line, character: range.end.character },
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

        let symbols = doc.ast.extract_symbols();
        let lsp_symbols = symbols
            .into_iter()
            .map(|s| {
                #[allow(deprecated)]
                SymbolInformation {
                    name: s.name,
                    kind: match s.kind {
                        crate::model::SymbolKind::Class => LspSymbolKind::CLASS,
                        crate::model::SymbolKind::Interface => LspSymbolKind::INTERFACE,
                        crate::model::SymbolKind::Enum => LspSymbolKind::ENUM,
                        crate::model::SymbolKind::Method => LspSymbolKind::METHOD,
                        crate::model::SymbolKind::Field => LspSymbolKind::FIELD,
                        _ => LspSymbolKind::VARIABLE,
                    },
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: Range {
                            start: Position {
                                line: s.range.start.line,
                                character: s.range.start.character,
                            },
                            end: Position {
                                line: s.range.end.line,
                                character: s.range.end.character,
                            },
                        },
                    },
                    container_name: s.container_name,
                }
            })
            .collect();

        Ok(Some(DocumentSymbolResponse::Flat(lsp_symbols)))
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

        let raw_hints = doc.ast.find_inlay_hints();
        let hints = raw_hints
            .into_iter()
            .map(|(pos, label)| InlayHint {
                position: Position {
                    line: pos.line,
                    character: pos.character,
                },
                label: InlayHintLabel::String(label),
                kind: Some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: Some(false),
                padding_right: Some(true),
                data: None,
            })
            .collect();

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

            // 1. Search local file & classpath implementations
            let impls = sem.find_implementations(&target_name).await;
            for im in impls {
                if let Some(path) = im.source_path {
                    if let Ok(target_uri) = Url::from_file_path(path) {
                        locs.push(Location {
                            uri: target_uri,
                            range: Range {
                                start: Position { line: 0, character: 0 },
                                end: Position { line: 0, character: 0 },
                            },
                        });
                    }
                }
            }

            // 2. Search workspace-wide indexed implementations (Spring @Service / @Component beans)
            let ws_impls = self.indexer.find_implementations(&target_name).await;
            for sym in ws_impls {
                if let Ok(target_uri) = Url::from_file_path(&sym.path) {
                    if !locs.iter().any(|l| l.uri == target_uri) {
                        locs.push(Location {
                            uri: target_uri,
                            range: Range {
                                start: Position { line: 0, character: 0 },
                                end: Position { line: 0, character: 0 },
                            },
                        });
                    }
                }
            }

            if !locs.is_empty() {
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
                #[allow(deprecated)]
                symbols.push(SymbolInformation {
                    name: s.simple_name,
                    kind: LspSymbolKind::CLASS,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri,
                        range: Range {
                            start: Position { line: 0, character: 0 },
                            end: Position { line: 0, character: 0 },
                        },
                    },
                    container_name: Some(s.class_info.package_name),
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

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        let target_ident = match sem.find_node_at_position(pos.line, pos.character) {
            Some(node) => doc.ast.node_text(&node).to_string(),
            None => return Ok(None),
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

        // If target is a field name, add getter & setter
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

        // Helper to collect occurrences in an AST
        let collect_in_ast = |ast: &JavaAst, target_uri: &Url, out: &mut Vec<Location>| {
            for ident in &target_idents {
                for r in ast.find_all_occurrences(ident) {
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
        collect_in_ast(&doc.ast, &uri, &mut locs);

        // 2. References across other open documents
        for item in self.documents.iter() {
            let other_uri = item.key();
            if other_uri != &uri {
                collect_in_ast(&item.value().ast, other_uri, &mut locs);
            }
        }

        // 3. References across workspace files not currently opened in editor
        let all_files = self.indexer.all_source_files().await;
        for path in all_files {
            if let Ok(file_uri) = Url::from_file_path(&path) {
                if !self.documents.contains_key(&file_uri) && file_uri != uri {
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

        if let Some((method_name, active_param)) = doc.ast.find_method_call_at_position(pos.line, pos.character) {
            let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
            let matched_methods = sem.find_methods_by_name(&method_name).await;

            if !matched_methods.is_empty() {
                let signatures: Vec<SignatureInformation> = matched_methods
                    .into_iter()
                    .map(|m| {
                        let param_strs: Vec<String> = m
                            .parameters
                            .iter()
                            .map(|(pname, pty)| format!("{} {}", pty.simple_name(), pname))
                            .collect();
                        let label = format!("{}({}) -> {}", m.name, param_strs.join(", "), m.return_type.simple_name());

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

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some((target_path, range)) = sem.goto_definition(pos.line, pos.character).await {
            let target_uri = if target_path.is_empty() {
                uri.clone()
            } else if target_path.ends_with(".jar") {
                // Generate a decompiled stub file in cache
                if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
                    let word = doc.ast.node_text(&node);
                    if let Some(cls) = sem.resolve_type(word).await {
                        // Check if real sources are available in local *-sources.jar
                        let real_source = crate::classpath::decompiler::extract_source_from_sources_jar(
                            std::path::Path::new(&target_path),
                            &cls.fqn,
                        );
                        let file_content = real_source.unwrap_or_else(|| {
                            crate::classpath::decompiler::decompile_class_info(&cls)
                        });

                        let stub_dir = std::env::temp_dir().join("jlsp_stubs");
                        let _ = std::fs::create_dir_all(&stub_dir);
                        let stub_path = stub_dir.join(format!("{}.java", cls.simple_name));
                        let _ = std::fs::write(&stub_path, file_content);
                        Url::from_file_path(stub_path).unwrap_or_else(|_| uri.clone())
                    } else {
                        Url::from_file_path(target_path).unwrap_or_else(|_| uri.clone())
                    }
                } else {
                    Url::from_file_path(target_path).unwrap_or_else(|_| uri.clone())
                }
            } else {
                Url::from_file_path(target_path).unwrap_or_else(|_| uri.clone())
            };

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
            return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
        }

        // Check workspace index
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);

            // Check if this node is part of a method invocation
            if let Some((recv_opt, m_name)) = doc.ast.find_method_call_receiver(pos.line, pos.character) {
                if m_name == word {
                    // Try to resolve the specific receiver type first
                    let mut target_class: Option<String> = None;
                    if let Some(recv) = recv_opt {
                        if recv == "this" || recv == "super" {
                            target_class = doc.ast.find_enclosing_class_at_position(pos.line, pos.character);
                        } else {
                            let locals = doc.ast.extract_local_variables_at_position(pos.line, pos.character);
                            for (v_name, v_type) in &locals {
                                if v_name == &recv {
                                    let clean = v_type.split('<').next().unwrap_or(v_type).trim();
                                    target_class = Some(clean.to_string());
                                    break;
                                }
                            }
                            if target_class.is_none() {
                                if let Some(enc) = doc.ast.find_enclosing_class_at_position(pos.line, pos.character) {
                                    if let Some(cls) = sem.local_classes.get(&enc) {
                                        for f in &cls.fields {
                                            if f.name == recv {
                                                let fty = f.field_type.simple_name();
                                                let clean = fty.split('<').next().unwrap_or(&fty).trim();
                                                target_class = Some(clean.to_string());
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            if target_class.is_none() {
                                target_class = Some(recv);
                            }
                        }
                    } else {
                        target_class = doc.ast.find_enclosing_class_at_position(pos.line, pos.character);
                    }

                    // Look for method inside the target class in the workspace
                    if let Some((method_file, line_no)) = self.indexer.find_method_location(target_class.as_deref(), &m_name).await {
                        if let Ok(target_uri) = Url::from_file_path(&method_file) {
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
                }
            } else {
                // If it's a type name or class identifier (starts with uppercase or is a type identifier)
                let is_type_like = word.chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
                    || node.kind() == "type_identifier";

                if is_type_like {
                    let mut search_words = vec![word.to_string()];
                    if (word.starts_with("get") || word.starts_with("set")) && word.len() > 3 {
                        let field_cand = format!("{}{}", word[3..4].to_lowercase(), &word[4..]);
                        search_words.push(field_cand);
                    }

                    for sw in search_words {
                        let matches = self.indexer.find_by_simple_name(&sw).await;
                        if let Some(target) = matches.into_iter().next() {
                            if let Ok(target_uri) = Url::from_file_path(&target.path) {
                                let loc = Location {
                                    uri: target_uri,
                                    range: Range {
                                        start: Position { line: 0, character: 0 },
                                        end: Position { line: 0, character: 0 },
                                    },
                                };
                                return Ok(Some(GotoDefinitionResponse::Scalar(loc)));
                            }
                        }
                    }
                }
            }

            // Check MyBatis XML bidirectional mapping
            // 1. From Java Mapper interface/method -> jump to MyBatis XML tag
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

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
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

        // Workspace indexed classes with auto-import
        for ws_sym in self.indexer.search_symbols("").await {
            let fqn = ws_sym.fqn.clone();
            let simple_name = ws_sym.simple_name.clone();
            let needs_import = !current_imports.iter().any(|i| i == &fqn || i.ends_with(&format!(".{}", simple_name)))
                && ws_sym.class_info.package_name != current_pkg
                && !fqn.starts_with("java.lang.");

            let additional_text_edits = if needs_import && !fqn.is_empty() {
                Some(vec![TextEdit {
                    range: Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
                    },
                    new_text: format!("import {};\n", fqn),
                }])
            } else {
                None
            };

            items.push(LspCompletionItem {
                label: simple_name,
                kind: Some(CompletionItemKind::CLASS),
                detail: Some(format!("{} (Auto-Import)", fqn)),
                additional_text_edits,
                ..Default::default()
            });
        }

        // Live Snippets
        items.push(LspCompletionItem {
            label: "sout".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("System.out.println(...)".to_string()),
            insert_text: Some("System.out.println($1);".to_string()),
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

        // Postfix Completion Snippets
        items.push(LspCompletionItem {
            label: "var".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("Postfix: var x = expr".to_string()),
            insert_text: Some("var ${1:name} = $0;".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });

        items.push(LspCompletionItem {
            label: "notnull".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("Postfix: if (expr != null)".to_string()),
            insert_text: Some("if (${1:expr} != null) {\n    $0\n}".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });

        items.push(LspCompletionItem {
            label: "null".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("Postfix: if (expr == null)".to_string()),
            insert_text: Some("if (${1:expr} == null) {\n    $0\n}".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });

        items.push(LspCompletionItem {
            label: "iter".to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("Postfix: for (var item : expr)".to_string()),
            insert_text: Some("for (var ${1:item} : ${2:list}) {\n    $0\n}".to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            ..Default::default()
        });

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
                Some(vec![TextEdit {
                    range: Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
                    },
                    new_text: format!("import {};\n", fqn),
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

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let uri = params.text_document.uri;
        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

        let mut actions = Vec::new();
        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        let pos = params.range.start;

        // 1. Auto import
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);
            let candidates = self.indexer.find_by_simple_name(word).await;

            for cand in candidates {
                let import_stmt = format!("import {};\n", cand.fqn);
                let edit = TextEdit {
                    range: Range {
                        start: Position { line: 0, character: 0 },
                        end: Position { line: 0, character: 0 },
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

        // 2. Organize imports code action
        let organized = doc.ast.organize_imports();
        let mut organize_text = String::new();
        for imp in organized {
            organize_text.push_str(&format!("import {};\n", imp));
        }

        let mut org_changes = HashMap::new();
        org_changes.insert(
            uri.clone(),
            vec![TextEdit {
                range: Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 0 },
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

        let generators = doc.ast.generate_getters_and_setters(pos.line);
        for (title, code) in generators {
            let mut gen_changes = HashMap::new();
            gen_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position { line: last_line, character: 0 },
                        end: Position { line: last_line, character: 0 },
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
                let param_strs: Vec<String> = m
                    .parameters
                    .iter()
                    .map(|(pname, pty)| format!("{} {}", pty.simple_name(), pname))
                    .collect();
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
                        start: Position { line: last_line, character: 0 },
                        end: Position { line: last_line, character: 0 },
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
                        range: Range { start: Position { line: last_line, character: 0 }, end: Position { line: last_line, character: 0 } },
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
                        start: Position { line: last_line, character: 0 },
                        end: Position { line: last_line, character: 0 },
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

        // 9. Change Method Signature refactoring
        if let Some((title, params_range, new_params_str)) = doc.ast.generate_change_signature_refactoring(
            pos.line,
            pos.character,
            "Object",
            "extraParam",
        ) {
            let mut sig_changes = HashMap::new();
            sig_changes.insert(
                uri.clone(),
                vec![TextEdit {
                    range: Range {
                        start: Position { line: params_range.start.line, character: params_range.start.character },
                        end: Position { line: params_range.end.line, character: params_range.end.character },
                    },
                    new_text: new_params_str,
                }],
            );

            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title,
                kind: Some(CodeActionKind::REFACTOR_REWRITE),
                diagnostics: None,
                edit: Some(WorkspaceEdit {
                    changes: Some(sig_changes),
                    ..Default::default()
                }),
                command: None,
                is_preferred: Some(false),
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
                        start: Position { line: last_line, character: 0 },
                        end: Position { line: last_line, character: 0 },
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

        Ok(Some(actions))
    }

    async fn rename(
        &self,
        params: RenameParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let new_name = params.new_name;

        let doc = match self.documents.get(&uri) {
            Some(d) => d,
            None => return Ok(None),
        };

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

        let sem = SemanticContext::new(&doc.ast, &self.classpath).await;
        if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
            let word = doc.ast.node_text(&node);
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
        if let Some((target_path, range)) = sem.goto_type_definition(pos.line, pos.character).await {
            let target_uri = if target_path.is_empty() {
                uri.clone()
            } else if target_path.ends_with(".jar") {
                if let Some(node) = sem.find_node_at_position(pos.line, pos.character) {
                    let word = doc.ast.node_text(&node);
                    if let Some(cls) = sem.resolve_type(word).await {
                        let real_source = crate::classpath::decompiler::extract_source_from_sources_jar(
                            std::path::Path::new(&target_path),
                            &cls.fqn,
                        );
                        let file_content = real_source.unwrap_or_else(|| {
                            crate::classpath::decompiler::decompile_class_info(&cls)
                        });

                        let stub_dir = std::env::temp_dir().join("jlsp_stubs");
                        let _ = std::fs::create_dir_all(&stub_dir);
                        let stub_path = stub_dir.join(format!("{}.java", cls.simple_name));
                        let _ = std::fs::write(&stub_path, file_content);
                        Url::from_file_path(stub_path).unwrap_or_else(|_| uri.clone())
                    } else {
                        Url::from_file_path(target_path).unwrap_or_else(|_| uri.clone())
                    }
                } else {
                    Url::from_file_path(target_path).unwrap_or_else(|_| uri.clone())
                }
            } else {
                Url::from_file_path(target_path).unwrap_or_else(|_| uri.clone())
            };

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
                self.client.show_message(MessageType::INFO, "JLSP: Workspace cache has been cleaned.").await;
                Ok(Some(serde_json::json!({ "cleaned": cleaned })))
            }
            _ => Ok(None),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
