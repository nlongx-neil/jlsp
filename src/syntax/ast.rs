use std::cell::RefCell;
use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};
use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo, Position, Range, SymbolDef, SymbolKind, TypeSignature};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiverStep {
    Identifier(String),
    MethodCall { name: String, arg_count: usize },
    FieldAccess(String),
    This,
    Super,
}

#[derive(Debug, Clone)]
pub struct MemberAccessInfo {
    pub receiver: Option<String>,
    pub receiver_steps: Vec<ReceiverStep>,
    pub member_name: String,
    pub is_method: bool,
    pub arg_count: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct InvocationArgInfo {
    pub receiver_steps: Vec<ReceiverStep>,
    pub receiver_raw: Option<String>,
    pub method_name: String,
    pub arg_index: usize,
    pub total_args: usize,
    pub position: Position,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticCode {
    SyntaxError,
    TypeMismatch,
    MissingReturn,
    UnexpectedReturn,
    FinalReassignment,
    DuplicateVariable,
    StaticContextViolation,
    UnhandledException,
    InaccessibleMember,
    UnreachableCode,
    NullDereference,
    SwitchMissingDefault,
    UnusedVariable,
    UnimplementedAbstractMethod,
    UnresolvedImport,
    PublicClassNameMismatch,
    UninitializedVariable,
    SpringTransactionalPrivate,
    SpringPathVariableMismatch,
    UnusedPrivateMember,
    MissingOverride,
    StaticMemberViaInstance,
    UnusedParameter,
    StringConcatInLoop,
    DeprecatedUsage,
    MissingSerialVersionUid,
}

impl DiagnosticCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagnosticCode::SyntaxError => "compiler.err.syntax",
            DiagnosticCode::TypeMismatch => "compiler.err.prob.found.req",
            DiagnosticCode::MissingReturn => "compiler.err.missing.ret.stmt",
            DiagnosticCode::UnexpectedReturn => "compiler.err.unexpected.ret",
            DiagnosticCode::FinalReassignment => "compiler.err.cant.assign.val.to.final.var",
            DiagnosticCode::DuplicateVariable => "compiler.err.already.defined",
            DiagnosticCode::StaticContextViolation => "compiler.err.non-static.cant.be.ref",
            DiagnosticCode::UnhandledException => "compiler.err.unreported.exception.need.to.catch.or.throw",
            DiagnosticCode::InaccessibleMember => "compiler.err.report.access",
            DiagnosticCode::UnreachableCode => "compiler.err.unreachable.stmt",
            DiagnosticCode::NullDereference => "compiler.err.null.deref",
            DiagnosticCode::SwitchMissingDefault => "compiler.warn.switch.incomplete",
            DiagnosticCode::UnusedVariable => "compiler.warn.unused",
            DiagnosticCode::UnusedPrivateMember => "compiler.warn.unused",
            DiagnosticCode::MissingOverride => "compiler.warn.missing.override",
            DiagnosticCode::StaticMemberViaInstance => "compiler.warn.static.access",
            DiagnosticCode::UnusedParameter => "compiler.warn.unused.parameter",
            DiagnosticCode::StringConcatInLoop => "performance.warn.string.concat.in.loop",
            DiagnosticCode::UnimplementedAbstractMethod => "compiler.err.does.not.override.abstract",
            DiagnosticCode::DeprecatedUsage => "compiler.warn.deprecated",
            DiagnosticCode::MissingSerialVersionUid => "compiler.warn.missing.serial.version.uid",
            DiagnosticCode::UnresolvedImport => "compiler.err.cant.resolve",
            DiagnosticCode::PublicClassNameMismatch => "compiler.err.public.class.must.be.in.file",
            DiagnosticCode::UninitializedVariable => "compiler.err.var.might.not.have.been.initialized",
            DiagnosticCode::SpringTransactionalPrivate => "spring.warn.transactional.private",
            DiagnosticCode::SpringPathVariableMismatch => "spring.warn.pathvariable.mismatch",
        }
    }

    pub fn is_error(&self) -> bool {
        !matches!(
            self,
            DiagnosticCode::UnusedVariable
                | DiagnosticCode::UnusedPrivateMember
                | DiagnosticCode::MissingOverride
                | DiagnosticCode::StaticMemberViaInstance
                | DiagnosticCode::UnusedParameter
                | DiagnosticCode::StringConcatInLoop
                | DiagnosticCode::SwitchMissingDefault
                | DiagnosticCode::SpringTransactionalPrivate
                | DiagnosticCode::SpringPathVariableMismatch
                | DiagnosticCode::DeprecatedUsage
                | DiagnosticCode::MissingSerialVersionUid
        )
    }

    pub fn is_unnecessary(&self) -> bool {
        matches!(
            self,
            DiagnosticCode::UnusedVariable
                | DiagnosticCode::UnusedPrivateMember
                | DiagnosticCode::UnusedParameter
                | DiagnosticCode::UnreachableCode
        )
    }

    pub fn is_deprecated(&self) -> bool {
        matches!(self, DiagnosticCode::DeprecatedUsage)
    }
}

#[derive(Debug, Clone)]
pub struct SemanticDiagnostic {
    pub range: Range,
    pub message: String,
    pub code: DiagnosticCode,
}

thread_local! {
    static TS_PARSER: RefCell<Parser> = RefCell::new({
        let mut parser = Parser::new();
        let _ = parser.set_language(&tree_sitter_java::LANGUAGE.into());
        parser
    });
}

#[derive(Clone)]
pub struct JavaAst {
    pub tree: Tree,
    pub source: Arc<String>,
}

impl JavaAst {
    pub fn parse(source: Arc<String>) -> Option<Self> {
        Self::parse_incremental(source, None)
    }

    pub fn parse_incremental(source: Arc<String>, _old_tree: Option<&Tree>) -> Option<Self> {
        TS_PARSER.with(|parser_cell| {
            let mut parser = parser_cell.borrow_mut();
            // In LSP FULL sync, the whole document text is replaced without Tree-sitter input_edit.
            // Passing an unedited old_tree causes Tree-sitter to reuse old node byte ranges that desync with new source.
            // Always parse cleanly with None to guarantee byte ranges match source.
            let tree = parser.parse(source.as_bytes(), None)?;
            Some(Self { tree, source })
        })
    }

    pub fn node_text<'a>(&'a self, node: &Node) -> &'a str {
        let start = node.start_byte();
        let end = node.end_byte();
        &self.source[start..end]
    }

    /// Convert a line's byte column offset to UTF-16 code unit offset (LSP specification compliance)
    pub fn byte_col_to_utf16(&self, line: u32, byte_col: usize) -> u32 {
        if let Some(line_str) = self.source.lines().nth(line as usize) {
            let mut current_byte = 0;
            let mut utf16_units = 0;
            for ch in line_str.chars() {
                if current_byte >= byte_col {
                    break;
                }
                current_byte += ch.len_utf8();
                utf16_units += ch.len_utf16() as u32;
            }
            utf16_units
        } else {
            byte_col as u32
        }
    }

    /// Convert a line's UTF-16 code unit offset to byte column offset (Tree-sitter Point compliance)
    pub fn utf16_col_to_byte(&self, line: u32, utf16_col: u32) -> usize {
        if let Some(line_str) = self.source.lines().nth(line as usize) {
            let mut current_utf16 = 0;
            let mut byte_offset = 0;
            for ch in line_str.chars() {
                if current_utf16 >= utf16_col {
                    break;
                }
                current_utf16 += ch.len_utf16() as u32;
                byte_offset += ch.len_utf8();
            }
            byte_offset
        } else {
            utf16_col as usize
        }
    }

    /// Convert Tree-sitter Point to LSP Position with UTF-16 code unit alignment
    pub fn point_to_position(&self, pt: tree_sitter::Point) -> Position {
        Position {
            line: pt.row as u32,
            character: self.byte_col_to_utf16(pt.row as u32, pt.column),
        }
    }

    pub fn node_range(&self, node: &Node) -> Range {
        Range {
            start: self.point_to_position(node.start_position()),
            end: self.point_to_position(node.end_position()),
        }
    }

    /// Find AST node at specific LSP position (line, UTF-16 character) with ERROR node recovery
    pub fn find_node_with_recovery(&self, line: u32, character: u32) -> Option<Node<'_>> {
        let root = self.tree.root_node();
        let byte_col = self.utf16_col_to_byte(line, character);
        let point = tree_sitter::Point {
            row: line as usize,
            column: byte_col,
        };
        
        let node = root.descendant_for_point_range(point, point)?;

        // If the matched node is an ERROR node, recover by finding the nearest preceding named token or leaf
        if node.kind() == "ERROR" {
            let mut cursor = node.walk();
            let mut best_child = None;
            for child in node.children(&mut cursor) {
                if child.start_position().row <= point.row && child.start_position().column <= point.column {
                    best_child = Some(child);
                }
            }
            if let Some(bc) = best_child {
                return Some(bc);
            }
        }

        Some(node)
    }

    /// Find AST node at specific LSP position (line, UTF-16 character)
    pub fn find_node_at_position(&self, line: u32, character: u32) -> Option<Node<'_>> {
        self.find_node_with_recovery(line, character)
    }

    /// Extract package declaration if any, e.g. "com.example.app"
    pub fn extract_package(&self) -> Option<String> {
        let root = self.tree.root_node();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "package_declaration"
                && let Some(id_node) = child.children(&mut child.walk()).find(|n| n.kind() == "scoped_identifier" || n.kind() == "identifier") {
                    return Some(self.node_text(&id_node).to_string());
                }
        }
        None
    }

    /// Extract package declaration with its Range
    pub fn extract_package_with_range(&self) -> Option<(tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "package_declaration"
                && let Some(id_node) = child.children(&mut child.walk()).find(|n| n.kind() == "scoped_identifier" || n.kind() == "identifier") {
                    return Some((self.node_lsp_range(&child), self.node_text(&id_node).to_string()));
                }
        }
        None
    }

    /// Extract import declarations with their exact source ranges (supporting static and wildcard imports)
    pub fn extract_imports_with_ranges(&self) -> Vec<(Range, String)> {
        let root = self.tree.root_node();
        let mut imports = Vec::new();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "import_declaration" {
                let is_static = child.children(&mut child.walk()).any(|c| c.kind() == "static");
                let has_asterisk = child.children(&mut child.walk()).any(|c| c.kind() == "asterisk");
                for node in child.children(&mut child.walk()) {
                    if node.kind() == "scoped_identifier" || node.kind() == "identifier" {
                        let base_text = self.node_text(&node);
                        let with_wildcard = if has_asterisk {
                            format!("{}.*", base_text)
                        } else {
                            base_text.to_string()
                        };
                        let full_import = if is_static {
                            format!("static {}", with_wildcard)
                        } else {
                            with_wildcard
                        };
                        imports.push((self.node_range(&node), full_import));
                    }
                }
            }
        }
        imports
    }

    /// Extract import declarations, e.g. ["java.util.List", "java.util.Map"]
    pub fn extract_imports(&self) -> Vec<String> {
        self.extract_imports_with_ranges().into_iter().map(|(_, s)| s).collect()
    }

    /// Calculate the line to insert a new import statement.
    /// Never insert before the package declaration.
    /// If there are existing imports, insert after the last import.
    /// If no imports but package exists, insert on the line after package.
    /// Otherwise, insert at line 0.
    pub fn find_import_insertion_position(&self) -> (Position, bool) {
        let root = self.tree.root_node();
        let mut last_import_end: Option<tree_sitter::Point> = None;
        let mut package_end: Option<tree_sitter::Point> = None;

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "package_declaration" {
                package_end = Some(child.end_position());
            } else if child.kind() == "import_declaration" {
                last_import_end = Some(child.end_position());
            }
        }

        if let Some(pt) = last_import_end {
            // Insert after the last import (on the line immediately following)
            (Position { line: pt.row as u32 + 1, character: 0 }, false)
        } else if let Some(pt) = package_end {
            // Insert after the package statement, adding an empty newline for clarity if needed
            (Position { line: pt.row as u32 + 1, character: 0 }, true)
        } else {
            (Position { line: 0, character: 0 }, false)
        }
    }

    /// Calculate the replacement range for all existing imports, or the insertion range if none exist.
    pub fn find_imports_replacement_range(&self) -> (Range, Option<String>) {
        let root = self.tree.root_node();
        let mut first_import: Option<tree_sitter::Point> = None;
        let mut last_import: Option<tree_sitter::Point> = None;
        let mut package_end: Option<tree_sitter::Point> = None;

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "package_declaration" {
                package_end = Some(child.end_position());
            } else if child.kind() == "import_declaration" {
                if first_import.is_none() {
                    first_import = Some(child.start_position());
                }
                last_import = Some(child.end_position());
            }
        }

        if let (Some(start_pt), Some(end_pt)) = (first_import, last_import) {
            (
                Range {
                    start: Position { line: start_pt.row as u32, character: 0 },
                    end: Position { line: end_pt.row as u32 + 1, character: 0 },
                },
                None,
            )
        } else if let Some(pkg_pt) = package_end {
            (
                Range {
                    start: Position { line: pkg_pt.row as u32 + 1, character: 0 },
                    end: Position { line: pkg_pt.row as u32 + 1, character: 0 },
                },
                Some("\n".to_string()),
            )
        } else {
            (
                Range {
                    start: Position { line: 0, character: 0 },
                    end: Position { line: 0, character: 0 },
                },
                None,
            )
        }
    }

    /// Find the declaration line of a target class, interface, method, or field in Java source code
    pub fn find_target_line_in_source(content: &str, target_name: &str) -> u32 {
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

    /// Collect all identifier names and type names referenced in code (outside import and package declarations),
    /// as well as identifier symbols referenced inside Javadoc comments (e.g. {@link Foo}, @see Foo).
    pub fn collect_used_identifier_names(&self) -> std::collections::HashSet<String> {
        let mut used = std::collections::HashSet::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, used: &mut std::collections::HashSet<String>) {
            let kind = node.kind();
            if kind == "import_declaration" {
                return;
            }
            if kind == "package_declaration" {
                // Traverse package declaration's modifiers/annotations (e.g. @NonNullApi in package-info.java),
                // but skip the package name itself
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "modifiers" || child.kind() == "annotation" || child.kind() == "marker_annotation" {
                        traverse(child, ast, used);
                    }
                }
                return;
            }

            if kind == "identifier" || kind == "type_identifier" {
                used.insert(ast.node_text(&node).to_string());
            } else if kind == "comment" || kind == "block_comment" || kind == "line_comment" {
                let text = ast.node_text(&node);
                // Extract Javadoc links, e.g. {@link ClassName} or @see ClassName
                for word in text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '$') {
                    if !word.is_empty() && word.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false) {
                        used.insert(word.to_string());
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, used);
            }
        }

        traverse(root, self, &mut used);
        used
    }

    /// Analyze used identifier tokens and clean up imports:
    /// Returns sorted, unique, and referenced imports (preserving wildcards and static imports)
    pub fn organize_imports(&self) -> Vec<String> {
        let current_imports = self.extract_imports();
        let used_identifiers = self.collect_used_identifier_names();
        let mut used_static = Vec::new();
        let mut used_normal = Vec::new();

        for imp in current_imports {
            if imp.contains('*') {
                // Wildcard imports must always be preserved
                if imp.starts_with("static ") {
                    used_static.push(imp);
                } else {
                    used_normal.push(imp);
                }
                continue;
            }

            let is_static = imp.starts_with("static ");
            let clean_imp = imp.strip_prefix("static ").unwrap_or(&imp);
            let simple_name = clean_imp.split('.').last().unwrap_or(clean_imp);

            if used_identifiers.contains(simple_name) {
                if is_static {
                    used_static.push(imp);
                } else {
                    used_normal.push(imp);
                }
            }
        }

        // Deduplicate and sort static imports
        used_static.sort();
        used_static.dedup();

        // Sort normal imports with standard Java package hierarchy:
        // java.*, javax.*, then all other packages alphabetically
        used_normal.sort_by(|a, b| {
            fn package_group(pkg: &str) -> u8 {
                if pkg.starts_with("java.") {
                    0
                } else if pkg.starts_with("javax.") {
                    1
                } else {
                    2
                }
            }
            let group_a = package_group(a);
            let group_b = package_group(b);
            if group_a != group_b {
                group_a.cmp(&group_b)
            } else {
                a.cmp(b)
            }
        });
        used_normal.dedup();

        let mut result = used_static;
        result.extend(used_normal);
        result
    }

    /// Find unused import declarations with their exact ranges (including duplicates)
    pub fn find_unused_imports(&self) -> Vec<(String, tower_lsp::lsp_types::Range)> {
        let root = self.tree.root_node();
        let used_identifiers = self.collect_used_identifier_names();
        let mut unused = Vec::new();
        let mut seen_imports = std::collections::HashSet::new();

        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "import_declaration" {
                let import_text = self.node_text(&child);
                let clean = import_text.trim_start_matches("import ").trim_end_matches(';').trim().to_string();

                // Detect duplicate imports
                if !seen_imports.insert(clean.clone()) {
                    unused.push((clean, self.node_lsp_range(&child)));
                    continue;
                }

                if import_text.contains('*') {
                    // Wildcard imports preserved
                    continue;
                }

                let clean_no_static = clean.strip_prefix("static ").unwrap_or(&clean);
                let simple_name = clean_no_static.split('.').last().unwrap_or(clean_no_static);

                if !used_identifiers.contains(simple_name) {
                    unused.push((clean, self.node_lsp_range(&child)));
                }
            }
        }
        unused
    }

    /// Extract document symbols (Classes, Interfaces, Enums, Methods, Fields)
    pub fn extract_symbols(&self) -> Vec<SymbolDef> {
        let mut symbols = Vec::new();
        let root = self.tree.root_node();
        self.collect_symbols(root, None, &mut symbols);
        symbols
    }

    fn collect_symbols(&self, node: Node, container: Option<&str>, out: &mut Vec<SymbolDef>) {
        let kind = node.kind();
        match kind {
            "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "annotation_type_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_string();
                    let sym_kind = match kind {
                        "class_declaration" => SymbolKind::Class,
                        "interface_declaration" | "annotation_type_declaration" => SymbolKind::Interface,
                        "enum_declaration" => SymbolKind::Enum,
                        _ => SymbolKind::Class,
                    };
                    out.push(SymbolDef {
                        name: name.clone(),
                        kind: sym_kind,
                        range: self.node_range(&node),
                        selection_range: self.node_range(&name_node),
                        detail: Some(kind.replace("_declaration", "")),
                        container_name: container.map(|s| s.to_string()),
                    });

                    // Visit children inside body
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.collect_symbols(child, Some(&name), out);
                        }
                    }
                }
            }
            "method_declaration" | "constructor_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_string();
                    let ret_type = node.child_by_field_name("type").map(|n| self.node_text(&n)).unwrap_or("void");
                    out.push(SymbolDef {
                        name: name.clone(),
                        kind: SymbolKind::Method,
                        range: self.node_range(&node),
                        selection_range: self.node_range(&name_node),
                        detail: Some(ret_type.to_string()),
                        container_name: container.map(|s| s.to_string()),
                    });
                }
            }
            "field_declaration" => {
                let ty = node.child_by_field_name("type").map(|n| self.node_text(&n).to_string()).unwrap_or_default();
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "variable_declarator"
                        && let Some(name_node) = child.child_by_field_name("name") {
                            let name = self.node_text(&name_node).to_string();
                            out.push(SymbolDef {
                                name,
                                kind: SymbolKind::Field,
                                range: self.node_range(&node),
                                selection_range: self.node_range(&name_node),
                                detail: Some(ty.clone()),
                                container_name: container.map(|s| s.to_string()),
                            });
                        }
                }
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.collect_symbols(child, container, out);
                }
            }
        }
    }

    /// Convert AST node Range into tower_lsp::lsp_types::Range with UTF-16 column positioning
    pub fn node_lsp_range(&self, node: &tree_sitter::Node) -> tower_lsp::lsp_types::Range {
        let r = self.node_range(node);
        tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: r.start.line,
                character: r.start.character,
            },
            end: tower_lsp::lsp_types::Position {
                line: r.end.line,
                character: r.end.character,
            },
        }
    }

    /// Extract nested hierarchical document symbols (Package -> Class -> Methods / Fields)
    pub fn extract_hierarchical_symbols(&self) -> Vec<tower_lsp::lsp_types::DocumentSymbol> {
        let mut symbols = Vec::new();
        let root = self.tree.root_node();
        self.collect_hierarchical_symbols(root, &mut symbols);
        symbols
    }

    fn collect_hierarchical_symbols(&self, node: Node, out: &mut Vec<tower_lsp::lsp_types::DocumentSymbol>) {
        let kind = node.kind();
        match kind {
            "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration" | "annotation_type_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_string();
                    let sym_kind = match kind {
                        "class_declaration" => tower_lsp::lsp_types::SymbolKind::CLASS,
                        "interface_declaration" | "annotation_type_declaration" => tower_lsp::lsp_types::SymbolKind::INTERFACE,
                        "enum_declaration" => tower_lsp::lsp_types::SymbolKind::ENUM,
                        _ => tower_lsp::lsp_types::SymbolKind::CLASS,
                    };

                    let mut children = Vec::new();
                    if let Some(body) = node.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            self.collect_hierarchical_symbols(child, &mut children);
                        }
                    }

                    #[allow(deprecated)]
                    out.push(tower_lsp::lsp_types::DocumentSymbol {
                        name,
                        detail: Some(kind.replace("_declaration", "")),
                        kind: sym_kind,
                        tags: None,
                        deprecated: None,
                        range: self.node_lsp_range(&node),
                        selection_range: self.node_lsp_range(&name_node),
                        children: if children.is_empty() { None } else { Some(children) },
                    });
                }
            }
            "constructor_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_string();
                    #[allow(deprecated)]
                    out.push(tower_lsp::lsp_types::DocumentSymbol {
                        name,
                        detail: Some("constructor".to_string()),
                        kind: tower_lsp::lsp_types::SymbolKind::CONSTRUCTOR,
                        tags: None,
                        deprecated: None,
                        range: self.node_lsp_range(&node),
                        selection_range: self.node_lsp_range(&name_node),
                        children: None,
                    });
                }
            }
            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node).to_string();
                    let ret_type = node.child_by_field_name("type").map(|n| self.node_text(&n)).unwrap_or("void");
                    #[allow(deprecated)]
                    out.push(tower_lsp::lsp_types::DocumentSymbol {
                        name,
                        detail: Some(ret_type.to_string()),
                        kind: tower_lsp::lsp_types::SymbolKind::METHOD,
                        tags: None,
                        deprecated: None,
                        range: self.node_lsp_range(&node),
                        selection_range: self.node_lsp_range(&name_node),
                        children: None,
                    });
                }
            }
            "field_declaration" => {
                let ty = node.child_by_field_name("type").map(|n| self.node_text(&n).to_string()).unwrap_or_default();
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "variable_declarator"
                        && let Some(name_node) = child.child_by_field_name("name") {
                            let name = self.node_text(&name_node).to_string();
                            #[allow(deprecated)]
                            out.push(tower_lsp::lsp_types::DocumentSymbol {
                                name,
                                detail: Some(ty.clone()),
                                kind: tower_lsp::lsp_types::SymbolKind::FIELD,
                                tags: None,
                                deprecated: None,
                                range: self.node_lsp_range(&node),
                                selection_range: self.node_lsp_range(&name_node),
                                children: None,
                            });
                        }
                }
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    self.collect_hierarchical_symbols(child, out);
                }
            }
        }
    }

    /// Convert parsed AST class into ClassInfo representation for semantic indexing
    pub fn extract_classes(&self, file_path: Option<String>) -> Vec<ClassInfo> {
        let pkg = self.extract_package().unwrap_or_default();
        let mut classes = Vec::new();

        let root = self.tree.root_node();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if (child.kind() == "class_declaration" || child.kind() == "interface_declaration" || child.kind() == "enum_declaration" || child.kind() == "record_declaration" || child.kind() == "annotation_type_declaration")
                && let Some(name_node) = child.child_by_field_name("name") {
                    let simple_name = self.node_text(&name_node).to_string();
                    let fqn = if pkg.is_empty() {
                        simple_name.clone()
                    } else {
                        format!("{}.{}", pkg, simple_name)
                    };

                    let kind = match child.kind() {
                        "interface_declaration" => ClassKind::Interface,
                        "enum_declaration" => ClassKind::Enum,
                        "record_declaration" => ClassKind::Record,
                        "annotation_type_declaration" => ClassKind::Annotation,
                        _ => ClassKind::Class,
                    };

                    // Extract superclass and interfaces
                    let mut super_class = None;
                    let mut interfaces = Vec::new();

                    let mut c_cursor = child.walk();
                    for c_child in child.children(&mut c_cursor) {
                        if c_child.kind() == "superclass" {
                            let mut sc_cursor = c_child.walk();
                            for sc in c_child.children(&mut sc_cursor) {
                                if sc.kind() == "type_identifier" || sc.kind() == "scoped_type_identifier" || sc.kind() == "generic_type" {
                                    let raw = self.node_text(&sc);
                                    let clean = raw.split('<').next().unwrap_or(raw).trim();
                                    super_class = Some(clean.to_string());
                                }
                            }
                        } else if c_child.kind() == "super_interfaces" || c_child.kind() == "extends_interfaces" {
                            let mut if_cursor = c_child.walk();
                            for if_child in c_child.children(&mut if_cursor) {
                                if if_child.kind() == "type_list" || if_child.kind() == "interface_type_list" {
                                    let mut tl_cursor = if_child.walk();
                                    for t in if_child.children(&mut tl_cursor) {
                                        if t.kind() == "type_identifier" || t.kind() == "scoped_type_identifier" || t.kind() == "generic_type" {
                                            let raw = self.node_text(&t);
                                            let clean = raw.split('<').next().unwrap_or(raw).trim();
                                            interfaces.push(clean.to_string());
                                        }
                                    }
                                } else if if_child.kind() == "type_identifier" || if_child.kind() == "scoped_type_identifier" || if_child.kind() == "generic_type" {
                                    let raw = self.node_text(&if_child);
                                    let clean = raw.split('<').next().unwrap_or(raw).trim();
                                    interfaces.push(clean.to_string());
                                }
                            }
                        }
                    }

                    // Extract methods and fields
                    let mut methods = Vec::new();
                    let mut fields = Vec::new();

                    // For Java 14+ record_declaration, extract parameters as canonical fields and accessors
                    if child.kind() == "record_declaration" {
                        if let Some(params_node) = child.child_by_field_name("parameters") {
                            let mut p_cursor = params_node.walk();
                            for p_child in params_node.children(&mut p_cursor) {
                                if p_child.kind() == "formal_parameter" || p_child.kind() == "record_component" {
                                    let p_type = p_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object");
                                    if let Some(name_node) = p_child.child_by_field_name("name") {
                                        let field_name = self.node_text(&name_node).to_string();
                                        fields.push(FieldInfo {
                                            name: field_name.clone(),
                                            field_type: TypeSignature::Class(p_type.to_string()),
                                            is_static: false,
                                            is_public: false,
                                            is_final: true,
                                        });

                                        // Record components generate canonical accessor: e.g. id() -> Long
                                        methods.push(MethodInfo {
                                            name: field_name,
                                            return_type: TypeSignature::Class(p_type.to_string()),
                                            parameters: vec![],
                                            is_static: false,
                                            is_public: true,
                                            is_constructor: false,
                                            javadoc: Some("Generated canonical record accessor".into()),
                                            body: None,
                                        });
                                    }
                                }
                            }
                        }
                    }

                    if let Some(body) = child.child_by_field_name("body") {
                        let mut b_cursor = body.walk();
                        for m_child in body.children(&mut b_cursor) {
                            if m_child.kind() == "method_declaration" || m_child.kind() == "constructor_declaration" {
                                if let Some(m_name_node) = m_child.child_by_field_name("name") {
                                    let m_name = self.node_text(&m_name_node).to_string();
                                    let is_ctor = m_child.kind() == "constructor_declaration";
                                    let ret_type = if is_ctor {
                                        TypeSignature::Void
                                    } else {
                                        let ret_str = m_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("void");
                                        TypeSignature::Class(ret_str.to_string())
                                    };

                                    let mut parameters = Vec::new();
                                    if let Some(params_node) = m_child.child_by_field_name("parameters") {
                                        let mut p_cursor = params_node.walk();
                                        for p_child in params_node.children(&mut p_cursor) {
                                            if p_child.kind() == "formal_parameter" || p_child.kind() == "spread_parameter" {
                                                let p_type = p_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object");
                                                let p_name = p_child.child_by_field_name("name").map(|n| self.node_text(&n)).unwrap_or("arg");
                                                parameters.push((p_name.to_string(), TypeSignature::Class(p_type.to_string())));
                                            }
                                        }
                                    }

                                    let mut is_static = false;
                                    let mut is_public = false;
                                    for m_mod in m_child.children(&mut m_child.walk()) {
                                        let txt = self.node_text(&m_mod);
                                        if m_mod.kind() == "modifiers" || m_mod.kind() == "modifier" {
                                            for w in txt.split_whitespace() {
                                                if w == "static" { is_static = true; }
                                                if w == "public" { is_public = true; }
                                            }
                                        } else if m_mod.kind() == "static" || txt == "static" {
                                            is_static = true;
                                        } else if m_mod.kind() == "public" || txt == "public" {
                                            is_public = true;
                                        }
                                    }
                                    
                                    // Only remember whether a body exists; the text itself is never
                                    // consumed by the workspace index and would otherwise duplicate
                                    // every source file in memory and in the on-disk index cache.
                                    let has_body = m_child.child_by_field_name("body").is_some();
                                    methods.push(MethodInfo {
                                        name: m_name,
                                        return_type: ret_type,
                                        parameters,
                                        is_static,
                                        is_public,
                                        is_constructor: is_ctor,
                                        javadoc: None,
                                        body: if has_body { Some(String::new()) } else { None },
                                    });
                                }
                            } else if m_child.kind() == "annotation_type_element_declaration" {
                                if let Some(m_name_node) = m_child.child_by_field_name("name") {
                                    let m_name = self.node_text(&m_name_node).to_string();
                                    let ret_str = m_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object");
                                    let ret_type = TypeSignature::Class(ret_str.to_string());
                                    methods.push(MethodInfo {
                                        name: m_name,
                                        return_type: ret_type,
                                        parameters: Vec::new(),
                                        is_static: false,
                                        is_public: true,
                                        is_constructor: false,
                                        javadoc: None,
                                        body: None,
                                    });
                                }
                            } else if m_child.kind() == "field_declaration" {
                                let f_type_str = m_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object");
                                let mut is_static = false;
                                let mut is_public = false;
                                let mut is_final = false;
                                for f_mod in m_child.children(&mut m_child.walk()) {
                                    let txt = self.node_text(&f_mod);
                                    if f_mod.kind() == "modifiers" || f_mod.kind() == "modifier" {
                                        for w in txt.split_whitespace() {
                                            if w == "static" { is_static = true; }
                                            if w == "public" { is_public = true; }
                                            if w == "final" { is_final = true; }
                                        }
                                    } else if f_mod.kind() == "static" || txt == "static" {
                                        is_static = true;
                                    } else if f_mod.kind() == "public" || txt == "public" {
                                        is_public = true;
                                    } else if f_mod.kind() == "final" || txt == "final" {
                                        is_final = true;
                                    }
                                }
                                let mut f_cursor = m_child.walk();
                                for dec in m_child.children(&mut f_cursor) {
                                    if dec.kind() == "variable_declarator"
                                        && let Some(f_name_node) = dec.child_by_field_name("name") {
                                            fields.push(FieldInfo {
                                                name: self.node_text(&f_name_node).to_string(),
                                                field_type: TypeSignature::Class(f_type_str.to_string()),
                                                is_static,
                                                is_public,
                                                is_final,
                                            });
                                        }
                                }
                            }
                        }
                    }

                    classes.push(ClassInfo {
                        fqn,
                        simple_name,
                        package_name: pkg.clone(),
                        kind,
                        super_class,
                        interfaces,
                        methods,
                        fields,
                        source_path: file_path.clone(),
                        is_public: true,
                    });
                }
        }

        if classes.is_empty() {
            // Fault-tolerant text fallback: extract class name and method declarations from source
            let mut curr_class_name = None;
            let mut curr_kind = ClassKind::Class;
            let mut methods = Vec::new();
            let fields = Vec::new();

            for line in self.source.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                    continue;
                }
                for kw in &["@interface ", "interface ", "class ", "enum ", "record "] {
                    if let Some(idx) = trimmed.find(kw) {
                        let after = &trimmed[idx + kw.len()..];
                        let name = after
                            .split(|c: char| !c.is_alphanumeric() && c != '_')
                            .next()
                            .unwrap_or("")
                            .trim();
                        if !name.is_empty() {
                            curr_class_name = Some(name.to_string());
                            curr_kind = match *kw {
                                "@interface " => ClassKind::Annotation,
                                "interface " => ClassKind::Interface,
                                "enum " => ClassKind::Enum,
                                "record " => ClassKind::Record,
                                _ => ClassKind::Class,
                            };
                            break;
                        }
                    }
                }

                // If inside a class, detect method signatures: e.g. "public void process(User user) {"
                if let (Some(open), Some(close)) = (trimmed.find('('), trimmed.find(')')) {
                    if open < close && !trimmed.starts_with("if") && !trimmed.starts_with("while") && !trimmed.starts_with("for") && !trimmed.starts_with("switch") {
                        let before_paren = trimmed[..open].trim();
                        let parts: Vec<&str> = before_paren.split_whitespace().collect();
                        if parts.len() >= 2 {
                            let m_name = parts[parts.len() - 1].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                            let ret_type_str = parts[parts.len() - 2];
                            if !m_name.is_empty() && !ret_type_str.is_empty() {
                                let mut params = Vec::new();
                                let inside = &trimmed[open + 1..close];
                                for param in inside.split(',') {
                                    let p_parts: Vec<&str> = param.split_whitespace().collect();
                                    if p_parts.len() >= 2 {
                                        let p_type = p_parts[p_parts.len() - 2];
                                        let p_name = p_parts[p_parts.len() - 1].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                                        if !p_name.is_empty() {
                                            params.push((p_name.to_string(), TypeSignature::Class(p_type.to_string())));
                                        }
                                    }
                                }
                                let ret_sig = if ret_type_str == "void" {
                                    TypeSignature::Void
                                } else {
                                    TypeSignature::Class(ret_type_str.to_string())
                                };
                                methods.push(MethodInfo {
                                    name: m_name.to_string(),
                                    return_type: ret_sig,
                                    parameters: params,
                                    is_static: parts.contains(&"static"),
                                    is_public: parts.contains(&"public"),
                                    is_constructor: false,
                                    javadoc: None,
                                    body: None,
                                });
                            }
                        }
                    }
                }
            }

            if let Some(simple_name) = curr_class_name {
                let fqn = if pkg.is_empty() {
                    simple_name.clone()
                } else {
                    format!("{}.{}", pkg, simple_name)
                };
                classes.push(ClassInfo {
                    fqn,
                    simple_name,
                    package_name: pkg.clone(),
                    kind: curr_kind,
                    super_class: None,
                    interfaces: Vec::new(),
                    methods,
                    fields,
                    source_path: file_path,
                    is_public: true,
                });
            }
        }

        classes
    }

    /// Walk AST and report syntax errors or missing nodes
    pub fn collect_syntax_errors(&self) -> Vec<(Range, String)> {
        let mut errors = Vec::new();
        let root = self.tree.root_node();
        let _cursor = root.walk();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, errs: &mut Vec<(Range, String)>) {
            if node.is_error() {
                errs.push((
                    ast.node_range(&node),
                    format!("Syntax error near '{}'", ast.node_text(&node)),
                ));
            } else if node.is_missing() {
                errs.push((
                    ast.node_range(&node),
                    format!("Missing expected token: {}", node.kind()),
                ));
            }

            let mut child_cursor = node.walk();
            for child in node.children(&mut child_cursor) {
                traverse(child, ast, errs);
            }
        }

        traverse(root, self, &mut errors);
        errors
    }

    /// Perform compiler-grade AST-level semantic checks (Type mismatches, missing returns, final reassignment, static violations, etc.)
    pub fn collect_semantic_diagnostics(&self) -> Vec<(Range, String)> {
        self.collect_semantic_diagnostics_detailed(None)
            .into_iter()
            .map(|d| (d.range, d.message))
            .collect()
    }

    /// Perform exhaustive compiler-grade AST semantic checks returning full SemanticDiagnostic items
    pub fn collect_semantic_diagnostics_detailed(&self, file_name: Option<&str>) -> Vec<SemanticDiagnostic> {
        let mut diags = Vec::new();
        let root = self.tree.root_node();

        // 1. Check public class name vs file name
        if let Some(fname) = file_name {
            let expected_cls = fname.strip_suffix(".java").unwrap_or(fname);
            let mut cursor = root.walk();
            for child in root.children(&mut cursor) {
                if matches!(child.kind(), "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration") {
                    let is_public = child.children(&mut child.walk()).any(|c| (c.kind() == "modifiers" && self.node_text(&c).contains("public")) || self.node_text(&c) == "public");
                    if is_public {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            let cls_name = self.node_text(&name_node);
                            if cls_name != expected_cls {
                                diags.push(SemanticDiagnostic {
                                    range: self.node_range(&name_node),
                                    message: format!("Class '{}' is public, should be declared in a file named '{}.java'", cls_name, cls_name),
                                    code: DiagnosticCode::PublicClassNameMismatch,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Helper to check if a node has `final` modifier
        fn has_final_modifier(node: &tree_sitter::Node, ast: &JavaAst) -> bool {
            let mut cursor = node.walk();
            for c in node.children(&mut cursor) {
                if c.kind() == "modifiers" {
                    let mut m_cursor = c.walk();
                    for m_child in c.children(&mut m_cursor) {
                        if m_child.kind() == "modifier" && ast.node_text(&m_child) == "final" {
                            return true;
                        }
                    }
                    if ast.node_text(&c).split_whitespace().any(|w| w == "final") {
                        return true;
                    }
                } else if c.kind() == "modifier" {
                    if ast.node_text(&c) == "final" {
                        return true;
                    }
                } else if ast.node_text(&c) == "final" {
                    return true;
                }
            }
            false
        }

        // Scope-aware final variable check
        fn is_target_final(target_ident: &tree_sitter::Node, ast: &JavaAst) -> bool {
            let target_name = ast.node_text(target_ident);
            let mut curr = target_ident.parent();
            let mut enclosing_callable_is_constructor = false;
            let mut enclosing_is_initializer = false;
            let mut callable_found = false;

            while let Some(n) = curr {
                let k = n.kind();

                // Track whether the immediate enclosing callable is a constructor or initializer
                if !callable_found {
                    if matches!(k, "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" | "lambda_expression") {
                        callable_found = true;
                        enclosing_callable_is_constructor = k == "constructor_declaration";
                    } else if k == "static_initializer" {
                        callable_found = true;
                        enclosing_is_initializer = true;
                    } else if k == "block" && n.parent().map(|p| p.kind() == "class_body").unwrap_or(false) {
                        callable_found = true;
                        enclosing_is_initializer = true;
                    }
                }

                // 1. Check local variable declarations in a block
                if k == "block" {
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        if child.start_byte() < target_ident.start_byte() && child.kind() == "local_variable_declaration" {
                            let mut v_cursor = child.walk();
                            for v_child in child.children(&mut v_cursor) {
                                if v_child.kind() == "variable_declarator" {
                                    if let Some(name_node) = v_child.child_by_field_name("name") {
                                        if ast.node_text(&name_node) == target_name {
                                            return has_final_modifier(&child, ast);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // 2. Check traditional for statement initializer: for (int i = 0; ...)
                else if k == "for_statement" {
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        if child.kind() == "local_variable_declaration" {
                            let mut v_cursor = child.walk();
                            for v_child in child.children(&mut v_cursor) {
                                if v_child.kind() == "variable_declarator" {
                                    if let Some(name_node) = v_child.child_by_field_name("name") {
                                        if ast.node_text(&name_node) == target_name {
                                            return has_final_modifier(&child, ast);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // 3. Check enhanced for statement: for (FileStatus fs : files)
                else if k == "enhanced_for_statement" {
                    if let Some(name_node) = n.child_by_field_name("name") {
                        if ast.node_text(&name_node) == target_name {
                            return has_final_modifier(&n, ast);
                        }
                    }
                }
                // 4. Check catch clause: catch (IOException e)
                else if k == "catch_clause" {
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        if child.kind() == "catch_formal_parameter" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                if ast.node_text(&name_node) == target_name {
                                    return has_final_modifier(&child, ast);
                                }
                            }
                        }
                    }
                }
                // 5. Check try-with-resources
                else if k == "try_with_resources_statement" || k == "resource_specification" {
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        if child.kind() == "resource" || child.kind() == "local_variable_declaration" {
                            let mut v_cursor = child.walk();
                            for v_child in child.children(&mut v_cursor) {
                                if v_child.kind() == "variable_declarator" {
                                    if let Some(name_node) = v_child.child_by_field_name("name") {
                                        if ast.node_text(&name_node) == target_name {
                                            return has_final_modifier(&child, ast);
                                        }
                                    }
                                }
                            }
                            if let Some(name_node) = child.child_by_field_name("name") {
                                if ast.node_text(&name_node) == target_name {
                                    return true;
                                }
                            }
                        }
                    }
                }
                // 6. Check method / constructor / lambda parameters
                else if matches!(k, "method_declaration" | "constructor_declaration" | "compact_constructor_declaration" | "lambda_expression") {
                    if let Some(params) = n.child_by_field_name("parameters") {
                        let mut cursor = params.walk();
                        for child in params.children(&mut cursor) {
                            if matches!(child.kind(), "formal_parameter" | "spread_parameter") {
                                if let Some(name_node) = child.child_by_field_name("name") {
                                    if ast.node_text(&name_node) == target_name {
                                        return has_final_modifier(&child, ast);
                                    }
                                }
                            }
                        }
                    }
                }
                // 7. Check enclosing class / record / enum / interface fields
                else if matches!(k, "class_declaration" | "record_declaration" | "enum_declaration" | "interface_declaration") {
                    if let Some(body) = n.child_by_field_name("body") {
                        let mut cursor = body.walk();
                        for child in body.children(&mut cursor) {
                            if child.kind() == "field_declaration" {
                                let mut v_cursor = child.walk();
                                for v_child in child.children(&mut v_cursor) {
                                    if v_child.kind() == "variable_declarator" {
                                        if let Some(name_node) = v_child.child_by_field_name("name") {
                                            if ast.node_text(&name_node) == target_name {
                                                if !has_final_modifier(&child, ast) {
                                                    return false;
                                                }
                                                // If in constructor or initializer and field has no initializer, it is blank final initialization
                                                if (enclosing_callable_is_constructor || enclosing_is_initializer) && v_child.child_by_field_name("value").is_none() {
                                                    return false;
                                                }
                                                return true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if k == "record_declaration" {
                        if let Some(header) = n.child_by_field_name("parameters") {
                            let mut cursor = header.walk();
                            for child in header.children(&mut cursor) {
                                if child.kind() == "formal_parameter" || child.kind() == "record_pattern_component" {
                                    if let Some(name_node) = child.child_by_field_name("name") {
                                        if ast.node_text(&name_node) == target_name {
                                            return true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                curr = n.parent();
            }

            false
        }

        // Helper to determine if an exception class is a checked exception
        fn is_checked_exception(type_name: &str) -> bool {
            let clean = type_name.split('<').next().unwrap_or(type_name).trim();
            let simple_name = clean.split('.').last().unwrap_or(clean);

            // 1. Explicit unchecked exceptions: RuntimeException and Error subclasses
            if simple_name == "RuntimeException" || simple_name.ends_with("RuntimeException") {
                return false;
            }
            if simple_name == "Error" || simple_name.ends_with("Error") {
                return false;
            }
            if matches!(
                simple_name,
                "IllegalArgumentException"
                    | "IllegalStateException"
                    | "NullPointerException"
                    | "IndexOutOfBoundsException"
                    | "ArrayIndexOutOfBoundsException"
                    | "StringIndexOutOfBoundsException"
                    | "UnsupportedOperationException"
                    | "NoSuchElementException"
                    | "ConcurrentModificationException"
                    | "SecurityException"
                    | "ArithmeticException"
                    | "ClassCastException"
                    | "NumberFormatException"
                    | "ArrayStoreException"
                    | "NegativeArraySizeException"
                    | "TypeNotPresentException"
                    | "BufferOverflowException"
                    | "BufferUnderflowException"
                    | "EmptyStackException"
                    | "MissingResourceException"
                    | "IllegalMonitorStateException"
                    | "IllegalThreadStateException"
                    | "InputMismatchException"
                    | "MalformedParameterizedTypeException"
                    | "UncheckedIOException"
                    | "DateTimeException"
            ) {
                return false;
            }

            // 2. Explicit checked exceptions: Throwable, Exception (not RuntimeException), and well-known checked exceptions
            if simple_name == "Throwable" || simple_name == "Exception" || simple_name.ends_with("CheckedException") {
                return true;
            }

            matches!(
                simple_name,
                "IOException"
                    | "SQLException"
                    | "ClassNotFoundException"
                    | "NoSuchMethodException"
                    | "NoSuchFieldException"
                    | "IllegalAccessException"
                    | "InstantiationException"
                    | "InvocationTargetException"
                    | "InterruptedException"
                    | "ParseException"
                    | "TimeoutException"
                    | "ExecutionException"
                    | "CloneNotSupportedException"
                    | "CertificateException"
                    | "URISyntaxException"
                    | "MalformedURLException"
                    | "GeneralSecurityException"
                    | "KeyException"
                    | "NoSuchAlgorithmException"
                    | "SignatureException"
                    | "DigestException"
                    | "InvalidKeySpecException"
                    | "BadPaddingException"
                    | "IllegalBlockSizeException"
                    | "NoSuchPaddingException"
                    | "SAXException"
                    | "ParserConfigurationException"
                    | "NamingException"
                    | "JMSException"
                    | "ServletException"
                    | "FileNotFoundException"
                    | "SocketException"
                    | "UnknownHostException"
                    | "EOFException"
                    | "ZipException"
                    | "JarException"
                    | "BadLocationException"
                    | "UnsupportedAudioFileException"
                    | "LineUnavailableException"
                    | "MimeTypeParseException"
                    | "PrinterException"
                    | "AWTException"
                    | "FontFormatException"
                    | "DataFormatException"
                    | "BrokenBarrierException"
                    | "ReflectiveOperationException"
                    | "InvalidPropertiesFormatException"
            )
        }

        // Helper to extract the thrown exception type name from a throw_statement
        fn get_thrown_exception_type(throw_node: &tree_sitter::Node, ast: &JavaAst) -> Option<String> {
            let mut cursor = throw_node.walk();
            for child in throw_node.children(&mut cursor) {
                if child.kind() == "object_creation_expression" {
                    if let Some(ty) = child.child_by_field_name("type") {
                        let raw_text = ast.node_text(&ty);
                        let clean = raw_text.split('<').next().unwrap_or(raw_text).trim();
                        let simple = clean.split('.').last().unwrap_or(clean);
                        return Some(simple.to_string());
                    }
                } else if child.kind() == "identifier" {
                    let var_name = ast.node_text(&child);
                    let mut curr = child.parent();
                    while let Some(n) = curr {
                        if n.kind() == "catch_clause" {
                            let mut c_cursor = n.walk();
                            for c_child in n.children(&mut c_cursor) {
                                if c_child.kind() == "catch_formal_parameter" {
                                    if let Some(name_node) = c_child.child_by_field_name("name") {
                                        if ast.node_text(&name_node) == var_name {
                                            if let Some(ty_node) = c_child.child_by_field_name("type") {
                                                let raw_text = ast.node_text(&ty_node);
                                                let clean = raw_text.split('<').next().unwrap_or(raw_text).trim();
                                                let simple = clean.split('.').last().unwrap_or(clean);
                                                return Some(simple.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        } else if n.kind() == "block" {
                            let mut b_cursor = n.walk();
                            for b_child in n.children(&mut b_cursor) {
                                if b_child.start_byte() < child.start_byte() && b_child.kind() == "local_variable_declaration" {
                                    let mut v_cursor = b_child.walk();
                                    for v_child in b_child.children(&mut v_cursor) {
                                        if v_child.kind() == "variable_declarator" {
                                            if let Some(name_node) = v_child.child_by_field_name("name") {
                                                if ast.node_text(&name_node) == var_name {
                                                    if let Some(ty_node) = b_child.child_by_field_name("type") {
                                                        let raw_text = ast.node_text(&ty_node);
                                                        let clean = raw_text.split('<').next().unwrap_or(raw_text).trim();
                                                        let simple = clean.split('.').last().unwrap_or(clean);
                                                        return Some(simple.to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        curr = n.parent();
                    }
                }
            }
            None
        }

        fn traverse(
            node: tree_sitter::Node,
            ast: &JavaAst,
            out: &mut Vec<SemanticDiagnostic>,
        ) {
            // Check local variable declaration type mismatches, e.g. int x = "hello";
            if node.kind() == "local_variable_declaration" {
                if let Some(ty_node) = node.child_by_field_name("type") {
                    let declared_type = ast.node_text(&ty_node);
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "variable_declarator" {
                            if let Some(val_node) = child.child_by_field_name("value") {
                                let val_kind = val_node.kind();
                                let incompatible = match declared_type {
                                    "int" | "long" | "short" | "byte" => {
                                        matches!(val_kind, "string_literal" | "true" | "false")
                                    }
                                    "boolean" => {
                                        matches!(val_kind, "string_literal" | "decimal_integer_literal" | "decimal_floating_point_literal")
                                    }
                                    "String" => {
                                        matches!(val_kind, "decimal_integer_literal" | "decimal_floating_point_literal" | "true" | "false")
                                    }
                                    _ => false,
                                };

                                if incompatible {
                                    out.push(SemanticDiagnostic {
                                        range: ast.node_range(&val_node),
                                        message: format!(
                                            "Type mismatch: cannot convert from {} literal to '{}'",
                                            val_kind.replace("_literal", ""),
                                            declared_type
                                        ),
                                        code: DiagnosticCode::TypeMismatch,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Check final variable reassignment
            if node.kind() == "assignment_expression" {
                if let Some(left) = node.child_by_field_name("left") {
                    if left.kind() == "identifier" {
                        let name = ast.node_text(&left);
                        if is_target_final(&left, ast) {
                            out.push(SemanticDiagnostic {
                                range: ast.node_range(&left),
                                message: format!("Cannot assign a value to final variable '{}'", name),
                                code: DiagnosticCode::FinalReassignment,
                            });
                        }
                    }
                }
            } else if node.kind() == "update_expression" {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "identifier" {
                        let name = ast.node_text(&child);
                        if is_target_final(&child, ast) {
                            out.push(SemanticDiagnostic {
                                range: ast.node_range(&child),
                                message: format!("Cannot assign a value to final variable '{}'", name),
                                code: DiagnosticCode::FinalReassignment,
                            });
                        }
                    }
                }
            }

            // Helper to collect variable names assigned in any subtree
            fn collect_assigned_names(n: tree_sitter::Node, ast: &JavaAst, assigned: &mut Vec<String>) {
                if n.kind() == "assignment_expression" {
                    if let Some(left) = n.child_by_field_name("left") {
                        if left.kind() == "identifier" {
                            assigned.push(ast.node_text(&left).to_string());
                        }
                    }
                }
                let mut cursor = n.walk();
                for ch in n.children(&mut cursor) {
                    collect_assigned_names(ch, ast, assigned);
                }
            }

            // Helper to check for uninitialized variable reads with control-flow awareness
            fn check_reads(
                n: tree_sitter::Node,
                ast: &JavaAst,
                unassigned: &std::collections::HashMap<String, Range>,
                out: &mut Vec<SemanticDiagnostic>,
            ) {
                if unassigned.is_empty() {
                    return;
                }

                match n.kind() {
                    "assignment_expression" => {
                        // Right-hand side is evaluated first
                        if let Some(right) = n.child_by_field_name("right") {
                            check_reads(right, ast, unassigned, out);
                        }
                        // Left-hand side is only checked if it's not a simple variable identifier being assigned
                        if let Some(left) = n.child_by_field_name("left") {
                            if left.kind() != "identifier" {
                                check_reads(left, ast, unassigned, out);
                            }
                        }
                    }
                    "while_statement" => {
                        let mut curr_unassigned = unassigned.clone();
                        if let Some(cond) = n.child_by_field_name("condition") {
                            check_reads(cond, ast, &curr_unassigned, out);
                            let mut cond_assigned = Vec::new();
                            collect_assigned_names(cond, ast, &mut cond_assigned);
                            for aname in &cond_assigned {
                                curr_unassigned.remove(aname);
                            }
                        }
                        if let Some(body) = n.child_by_field_name("body") {
                            check_reads(body, ast, &curr_unassigned, out);
                        }
                    }
                    "for_statement" => {
                        let mut curr_unassigned = unassigned.clone();
                        if let Some(init) = n.child_by_field_name("init") {
                            check_reads(init, ast, &curr_unassigned, out);
                            let mut init_assigned = Vec::new();
                            collect_assigned_names(init, ast, &mut init_assigned);
                            for aname in &init_assigned {
                                curr_unassigned.remove(aname);
                            }
                        }
                        if let Some(cond) = n.child_by_field_name("condition") {
                            check_reads(cond, ast, &curr_unassigned, out);
                            let mut cond_assigned = Vec::new();
                            collect_assigned_names(cond, ast, &mut cond_assigned);
                            for aname in &cond_assigned {
                                curr_unassigned.remove(aname);
                            }
                        }
                        if let Some(body) = n.child_by_field_name("body") {
                            check_reads(body, ast, &curr_unassigned, out);
                        }
                        if let Some(update) = n.child_by_field_name("update") {
                            check_reads(update, ast, &curr_unassigned, out);
                        }
                    }
                    "do_statement" => {
                        let mut body_assigned = Vec::new();
                        if let Some(body) = n.child_by_field_name("body") {
                            check_reads(body, ast, unassigned, out);
                            collect_assigned_names(body, ast, &mut body_assigned);
                        }
                        let mut cond_unassigned = unassigned.clone();
                        for aname in &body_assigned {
                            cond_unassigned.remove(aname);
                        }
                        if let Some(cond) = n.child_by_field_name("condition") {
                            check_reads(cond, ast, &cond_unassigned, out);
                        }
                    }
                    "if_statement" => {
                        let mut cond_assigned = Vec::new();
                        if let Some(cond) = n.child_by_field_name("condition") {
                            check_reads(cond, ast, unassigned, out);
                            collect_assigned_names(cond, ast, &mut cond_assigned);
                        }
                        let mut branch_unassigned = unassigned.clone();
                        for aname in &cond_assigned {
                            branch_unassigned.remove(aname);
                        }
                        if let Some(cons) = n.child_by_field_name("consequence") {
                            check_reads(cons, ast, &branch_unassigned, out);
                        }
                        if let Some(alt) = n.child_by_field_name("alternative") {
                            check_reads(alt, ast, &branch_unassigned, out);
                        }
                    }
                    "block" => {
                        let mut curr_unassigned = unassigned.clone();
                        let mut cursor = n.walk();
                        for child in n.children(&mut cursor) {
                            if child.kind() == "local_variable_declaration" {
                                let mut v_cursor = child.walk();
                                for v_child in child.children(&mut v_cursor) {
                                    if v_child.kind() == "variable_declarator" {
                                        if let Some(val_node) = v_child.child_by_field_name("value") {
                                            check_reads(val_node, ast, &curr_unassigned, out);
                                        }
                                    }
                                }
                                continue;
                            }
                            check_reads(child, ast, &curr_unassigned, out);
                            let mut st_assigned = Vec::new();
                            collect_assigned_names(child, ast, &mut st_assigned);
                            for aname in &st_assigned {
                                curr_unassigned.remove(aname);
                            }
                        }
                    }
                    "identifier" => {
                        let id = ast.node_text(&n);
                        if unassigned.contains_key(id) {
                            let p = n.parent().map(|p| p.kind()).unwrap_or("");
                            let is_assign_lhs = n.parent().map(|p| {
                                p.kind() == "assignment_expression" && p.child_by_field_name("left").map(|l| l.id() == n.id()).unwrap_or(false)
                            }).unwrap_or(false);
                            if p != "variable_declarator" && !is_assign_lhs {
                                out.push(SemanticDiagnostic {
                                    range: ast.node_range(&n),
                                    message: format!("variable '{}' might not have been initialized", id),
                                    code: DiagnosticCode::UninitializedVariable,
                                });
                            }
                        }
                    }
                    _ => {
                        let mut cursor = n.walk();
                        for ch in n.children(&mut cursor) {
                            check_reads(ch, ast, unassigned, out);
                        }
                    }
                }
            }

            // Check duplicate variable declaration and uninitialized local variable reads in same block
            if node.kind() == "block" {
                let mut block_vars = std::collections::HashSet::new();
                let mut unassigned_vars: std::collections::HashMap<String, Range> = std::collections::HashMap::new();

                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "local_variable_declaration" {
                        let mut v_cursor = child.walk();
                        for v_child in child.children(&mut v_cursor) {
                            if v_child.kind() == "variable_declarator" {
                                if let Some(name_node) = v_child.child_by_field_name("name") {
                                    let name = ast.node_text(&name_node).to_string();
                                    if !block_vars.insert(name.clone()) {
                                        out.push(SemanticDiagnostic {
                                            range: ast.node_range(&name_node),
                                            message: format!("Variable '{}' is already defined in scope", name),
                                            code: DiagnosticCode::DuplicateVariable,
                                        });
                                    } else if let Some(val_node) = v_child.child_by_field_name("value") {
                                        // Check if initial value reads any unassigned variable
                                        check_reads(val_node, ast, &unassigned_vars, out);
                                    } else {
                                        // Variable declared without initializer: e.g. int x;
                                        unassigned_vars.insert(name, ast.node_range(&name_node));
                                    }
                                }
                            }
                        }
                    } else {
                        // Check reads with control-flow aware tracking (while, for, if, try, do)
                        check_reads(child, ast, &unassigned_vars, out);

                        let mut st_assigned = Vec::new();
                        collect_assigned_names(child, ast, &mut st_assigned);
                        for aname in &st_assigned {
                            unassigned_vars.remove(aname);
                        }
                    }
                }
            }

            // Check for method declarations: return statements, missing return, and static context
            if node.kind() == "method_declaration" {
                let is_static = node.children(&mut node.walk()).any(|c| (c.kind() == "modifiers" && ast.node_text(&c).contains("static")) || ast.node_text(&c) == "static");
                let ret_type_opt = node.child_by_field_name("type").map(|t| ast.node_text(&t).trim()).filter(|s| !s.is_empty());
                let is_abstract = node.children(&mut node.walk()).any(|c| (c.kind() == "modifiers" && ast.node_text(&c).contains("abstract")) || ast.node_text(&c) == "abstract");
                let is_native = node.children(&mut node.walk()).any(|c| (c.kind() == "modifiers" && ast.node_text(&c).contains("native")) || ast.node_text(&c) == "native");
                let in_interface = node.parent().map(|p| p.kind() == "interface_declaration" || p.kind() == "annotation_type_declaration").unwrap_or(false);

                if let Some(body) = node.child_by_field_name("body") {
                    // Static context violation: cannot use this or super in static method
                    if is_static {
                        fn check_static_context(n: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<SemanticDiagnostic>) {
                            if matches!(
                                n.kind(),
                                "class_declaration"
                                    | "interface_declaration"
                                    | "record_declaration"
                                    | "enum_declaration"
                                    | "class_body"
                                    | "method_declaration"
                                    | "constructor_declaration"
                            ) {
                                return;
                            }
                            if n.kind() == "this" || n.kind() == "super" {
                                out.push(SemanticDiagnostic {
                                    range: ast.node_range(&n),
                                    message: format!("Cannot use '{}' in a static context", ast.node_text(&n)),
                                    code: DiagnosticCode::StaticContextViolation,
                                });
                            }
                            let mut cursor = n.walk();
                            for child in n.children(&mut cursor) {
                                check_static_context(child, ast, out);
                            }
                        }
                        check_static_context(body, ast, out);
                    }

                    // Check return statements compatibility
                    if let Some(ret_type) = ret_type_opt {
                        let is_void = ret_type == "void";

                        fn check_returns(n: tree_sitter::Node, ast: &JavaAst, m_ret: &str, is_v: bool, out: &mut Vec<SemanticDiagnostic>) {
                            if matches!(
                                n.kind(),
                                "lambda_expression"
                                    | "class_declaration"
                                    | "interface_declaration"
                                    | "record_declaration"
                                    | "enum_declaration"
                                    | "class_body"
                                    | "method_declaration"
                                    | "constructor_declaration"
                            ) {
                                return;
                            }
                            if n.kind() == "return_statement" {
                                let mut cursor = n.walk();
                                let expr_child = n.children(&mut cursor).find(|c| c.kind() != "return" && c.kind() != ";" && c.is_named());
                                if is_v {
                                    if let Some(expr) = expr_child {
                                        out.push(SemanticDiagnostic {
                                            range: ast.node_range(&expr),
                                            message: "Cannot return a value from a method with void result type".to_string(),
                                            code: DiagnosticCode::UnexpectedReturn,
                                        });
                                    }
                                } else if let Some(expr) = expr_child {
                                    let val_kind = expr.kind();
                                    let incompatible = match m_ret {
                                        "int" | "long" | "short" | "byte" => matches!(val_kind, "string_literal" | "true" | "false"),
                                        "boolean" => matches!(val_kind, "string_literal" | "decimal_integer_literal" | "decimal_floating_point_literal"),
                                        "String" => matches!(val_kind, "decimal_integer_literal" | "decimal_floating_point_literal" | "true" | "false"),
                                        _ => false,
                                    };
                                    if incompatible {
                                        out.push(SemanticDiagnostic {
                                            range: ast.node_range(&expr),
                                            message: format!(
                                                "Type mismatch: cannot convert from {} literal to '{}'",
                                                val_kind.replace("_literal", ""),
                                                m_ret
                                            ),
                                            code: DiagnosticCode::TypeMismatch,
                                        });
                                    }
                                }
                            }
                            let mut cursor = n.walk();
                            for child in n.children(&mut cursor) {
                                check_returns(child, ast, m_ret, is_v, out);
                            }
                        }
                        check_returns(body, ast, ret_type, is_void, out);

                        // Missing return statement check in non-void concrete methods
                        if !is_void && !is_abstract && !is_native && !in_interface {
                            fn definitely_returns(stmt: tree_sitter::Node, ast: &JavaAst) -> bool {
                                match stmt.kind() {
                                    "return_statement" | "throw_statement" => true,
                                    "block" => {
                                        let mut cursor = stmt.walk();
                                        let children: Vec<_> = stmt.children(&mut cursor).filter(|c| c.kind() != "{" && c.kind() != "}").collect();
                                        children.iter().any(|s| definitely_returns(*s, ast))
                                    }
                                    "if_statement" => {
                                        let cons = stmt.child_by_field_name("consequence");
                                        let alt = stmt.child_by_field_name("alternative");
                                        if let (Some(c), Some(a)) = (cons, alt) {
                                            definitely_returns(c, ast) && definitely_returns(a, ast)
                                        } else {
                                            false
                                        }
                                    }
                                    "switch_expression" | "switch_statement" => {
                                        let block_opt = stmt.child_by_field_name("body")
                                            .or_else(|| stmt.children(&mut stmt.walk()).find(|c| c.kind() == "switch_block"));
                                        if let Some(sb) = block_opt {
                                            let mut cursor = sb.walk();
                                            let groups: Vec<_> = sb.children(&mut cursor)
                                                .filter(|c| c.kind() == "switch_block_statement_group" || c.kind() == "switch_rule")
                                                .collect();

                                            if groups.is_empty() {
                                                return false;
                                            }

                                            let mut has_default = false;
                                            for group in &groups {
                                                if group.kind() == "switch_block_statement_group" {
                                                    let group_has_default = group.children(&mut group.walk()).any(|c| {
                                                        c.kind() == "switch_label" && (
                                                            c.children(&mut c.walk()).any(|k| k.kind() == "default")
                                                            || ast.node_text(&c).trim().starts_with("default")
                                                        )
                                                    });
                                                    if group_has_default {
                                                        has_default = true;
                                                    }
                                                    let stmts: Vec<_> = group.children(&mut group.walk())
                                                        .filter(|c| c.kind() != "switch_label" && c.kind() != ":" && c.is_named())
                                                        .collect();
                                                    let group_returns = stmts.iter().any(|s| definitely_returns(*s, ast));
                                                    if !group_returns {
                                                        return false;
                                                    }
                                                } else if group.kind() == "switch_rule" {
                                                    let rule_has_default = group.children(&mut group.walk()).any(|c| {
                                                        c.kind() == "switch_label" && (
                                                            c.children(&mut c.walk()).any(|k| k.kind() == "default")
                                                            || ast.node_text(&c).trim().starts_with("default")
                                                        )
                                                    });
                                                    if rule_has_default {
                                                        has_default = true;
                                                    }
                                                    let rule_body = group.children(&mut group.walk())
                                                        .find(|c| c.kind() != "switch_label" && c.kind() != "->" && c.is_named());
                                                    if let Some(rb) = rule_body {
                                                        if !definitely_returns(rb, ast) {
                                                            return false;
                                                        }
                                                    } else {
                                                        return false;
                                                    }
                                                }
                                            }
                                            has_default
                                        } else {
                                            false
                                        }
                                    }
                                    "try_statement" | "try_with_resources_statement" => {
                                        let finally_clause = stmt.children(&mut stmt.walk()).find(|c| c.kind() == "finally_clause");
                                        if let Some(fc) = finally_clause {
                                            if let Some(f_body) = fc.child_by_field_name("body").or_else(|| fc.children(&mut fc.walk()).find(|c| c.kind() == "block")) {
                                                if definitely_returns(f_body, ast) {
                                                    return true;
                                                }
                                            }
                                        }
                                        let try_body = stmt.child_by_field_name("body")
                                            .or_else(|| stmt.children(&mut stmt.walk()).find(|c| c.kind() == "block"));
                                        let try_returns = try_body.map(|b| definitely_returns(b, ast)).unwrap_or(false);
                                        if !try_returns {
                                            return false;
                                        }
                                        let catch_clauses: Vec<_> = stmt.children(&mut stmt.walk()).filter(|c| c.kind() == "catch_clause").collect();
                                        for cc in catch_clauses {
                                            let c_body = cc.child_by_field_name("body")
                                                .or_else(|| cc.children(&mut cc.walk()).find(|c| c.kind() == "block"));
                                            if let Some(cb) = c_body {
                                                if !definitely_returns(cb, ast) {
                                                    return false;
                                                }
                                            } else {
                                                return false;
                                            }
                                        }
                                        true
                                    }
                                    "synchronized_statement" => {
                                        stmt.child_by_field_name("body")
                                            .or_else(|| stmt.children(&mut stmt.walk()).find(|c| c.kind() == "block"))
                                            .map(|b| definitely_returns(b, ast))
                                            .unwrap_or(false)
                                    }
                                    "labeled_statement" => {
                                        stmt.child_by_field_name("statement")
                                            .map(|s| definitely_returns(s, ast))
                                            .unwrap_or(false)
                                    }
                                    "do_statement" => {
                                        stmt.child_by_field_name("body")
                                            .map(|b| definitely_returns(b, ast))
                                            .unwrap_or(false)
                                    }
                                    "while_statement" => {
                                        let cond = stmt.child_by_field_name("condition").map(|c| ast.node_text(&c)).unwrap_or_default();
                                        let cond_clean = cond.trim_matches(|c| c == '(' || c == ')' || c == ' ');
                                        if cond_clean == "true" {
                                            fn has_unlabelled_break(n: tree_sitter::Node) -> bool {
                                                if n.kind() == "break_statement" {
                                                    return true;
                                                }
                                                if matches!(n.kind(), "while_statement" | "for_statement" | "do_statement" | "switch_statement") {
                                                    return false;
                                                }
                                                let mut cursor = n.walk();
                                                for child in n.children(&mut cursor) {
                                                    if has_unlabelled_break(child) {
                                                        return true;
                                                    }
                                                }
                                                false
                                            }
                                            stmt.child_by_field_name("body")
                                                .map(|b| !has_unlabelled_break(b))
                                                .unwrap_or(false)
                                        } else {
                                            false
                                        }
                                    }
                                    "for_statement" => {
                                        let has_cond = stmt.child_by_field_name("condition").is_some();
                                        if !has_cond {
                                            fn has_unlabelled_break(n: tree_sitter::Node) -> bool {
                                                if n.kind() == "break_statement" {
                                                    return true;
                                                }
                                                if matches!(n.kind(), "while_statement" | "for_statement" | "do_statement" | "switch_statement") {
                                                    return false;
                                                }
                                                let mut cursor = n.walk();
                                                for child in n.children(&mut cursor) {
                                                    if has_unlabelled_break(child) {
                                                        return true;
                                                    }
                                                }
                                                false
                                            }
                                            stmt.child_by_field_name("body")
                                                .map(|b| !has_unlabelled_break(b))
                                                .unwrap_or(false)
                                        } else {
                                            false
                                        }
                                    }
                                    _ => false,
                                }
                            }

                            if !definitely_returns(body, ast) {
                                let name_node = node.child_by_field_name("name").unwrap_or(node);
                                out.push(SemanticDiagnostic {
                                    range: ast.node_range(&name_node),
                                    message: format!("Missing return statement in method returning '{}'", ret_type),
                                    code: DiagnosticCode::MissingReturn,
                                });
                            }
                        }
                    }
                }

                // Check for unused local variables in methods
                let mut declared_vars = Vec::new();
                let mut all_identifiers = Vec::new();

                fn find_declarations_and_reads(
                    n: tree_sitter::Node,
                    ast: &JavaAst,
                    decls: &mut Vec<(String, Range)>,
                    reads: &mut Vec<String>,
                ) {
                    if matches!(
                        n.kind(),
                        "class_declaration"
                            | "interface_declaration"
                            | "record_declaration"
                            | "enum_declaration"
                            | "class_body"
                            | "method_declaration"
                            | "constructor_declaration"
                    ) {
                        fn collect_reads_only(n: tree_sitter::Node, ast: &JavaAst, reads: &mut Vec<String>) {
                            if n.kind() == "identifier" {
                                reads.push(ast.node_text(&n).to_string());
                            }
                            let mut cursor = n.walk();
                            for child in n.children(&mut cursor) {
                                collect_reads_only(child, ast, reads);
                            }
                        }
                        collect_reads_only(n, ast, reads);
                        return;
                    }

                    if n.kind() == "variable_declarator" {
                        if let Some(name_node) = n.child_by_field_name("name") {
                            decls.push((ast.node_text(&name_node).to_string(), ast.node_range(&name_node)));
                        }
                    } else if n.kind() == "identifier" {
                        reads.push(ast.node_text(&n).to_string());
                    }

                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        find_declarations_and_reads(child, ast, decls, reads);
                    }
                }

                if let Some(body) = node.child_by_field_name("body") {
                    find_declarations_and_reads(body, ast, &mut declared_vars, &mut all_identifiers);
                }

                if !declared_vars.is_empty() {
                    let mut ident_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::with_capacity(all_identifiers.len());
                    for id in &all_identifiers {
                        *ident_counts.entry(id.as_str()).or_default() += 1;
                    }

                    for (var_name, range) in declared_vars {
                        if var_name == "_" || var_name.starts_with("ignored") || var_name.starts_with("unused") {
                            continue;
                        }
                        let count = ident_counts.get(var_name.as_str()).copied().unwrap_or(0);
                        if count <= 1 {
                            out.push(SemanticDiagnostic {
                                range,
                                message: format!("The value of the local variable '{}' is not used", var_name),
                                code: DiagnosticCode::UnusedVariable,
                            });
                        }
                    }
                }

                // Check for unused parameters in private methods
                let is_private = node.children(&mut node.walk()).any(|c| {
                    (c.kind() == "modifiers" && ast.node_text(&c).split_whitespace().any(|w| w == "private"))
                        || ast.node_text(&c) == "private"
                });
                if is_private && !in_interface && !is_abstract {
                    if let Some(params_node) = node.child_by_field_name("parameters") {
                        let mut p_cursor = params_node.walk();
                        for param in params_node.children(&mut p_cursor) {
                            if param.kind() == "formal_parameter" {
                                if let Some(p_name_node) = param.child_by_field_name("name") {
                                    let p_name = ast.node_text(&p_name_node);
                                    if p_name == "_" || p_name.starts_with("ignored") || p_name.starts_with("unused") {
                                        continue;
                                    }
                                    let count = all_identifiers.iter().filter(|id| id.as_str() == p_name).count();
                                    if count == 0 {
                                        out.push(SemanticDiagnostic {
                                            range: ast.node_range(&p_name_node),
                                            message: format!("The parameter '{}' is never read", p_name),
                                            code: DiagnosticCode::UnusedParameter,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

                // Spring Framework Check 1: @Transactional on private method warning
                let has_transactional = node.children(&mut node.walk()).any(|c| {
                    (c.kind() == "modifiers" || c.kind() == "marker_annotation" || c.kind() == "annotation")
                        && ast.node_text(&c).contains("Transactional")
                });
                let is_private = node.children(&mut node.walk()).any(|c| {
                    (c.kind() == "modifiers" && ast.node_text(&c).split_whitespace().any(|w| w == "private"))
                        || ast.node_text(&c) == "private"
                });
                if has_transactional && is_private {
                    let name_node = node.child_by_field_name("name").unwrap_or(node);
                    out.push(SemanticDiagnostic {
                        range: ast.node_range(&name_node),
                        message: "Methods annotated with '@Transactional' should be public. Spring AOP proxying ignores private methods.".to_string(),
                        code: DiagnosticCode::SpringTransactionalPrivate,
                    });
                }

                // Spring Framework Check 2: @PathVariable URI template variable consistency
                let mut uri_vars = Vec::new();
                for c in node.children(&mut node.walk()) {
                    let text = ast.node_text(&c);
                    if (c.kind() == "annotation" || c.kind() == "modifiers") && text.contains("Mapping") {
                        let mut curr_str = text;
                        while let Some(open_brace) = curr_str.find('{') {
                            if let Some(close_brace) = curr_str[open_brace + 1..].find('}') {
                                let var_name = &curr_str[open_brace + 1..open_brace + 1 + close_brace];
                                let clean_var = var_name.trim();
                                let clean_name = clean_var.split(':').next().unwrap_or(clean_var).trim();
                                if !clean_name.is_empty() {
                                    uri_vars.push(clean_name.to_string());
                                }
                                curr_str = &curr_str[open_brace + 1 + close_brace + 1..];
                            } else {
                                break;
                            }
                        }
                    }
                }
                // Also check enclosing class/record @RequestMapping for path variables
                let mut p_enc = node.parent();
                while let Some(parent_node) = p_enc {
                    if parent_node.kind() == "class_declaration" || parent_node.kind() == "record_declaration" {
                        for c in parent_node.children(&mut parent_node.walk()) {
                            let text = ast.node_text(&c);
                            if (c.kind() == "annotation" || c.kind() == "modifiers") && text.contains("Mapping") {
                                let mut curr_str = text;
                                while let Some(open_brace) = curr_str.find('{') {
                                    if let Some(close_brace) = curr_str[open_brace + 1..].find('}') {
                                        let var_name = &curr_str[open_brace + 1..open_brace + 1 + close_brace];
                                        let clean_var = var_name.trim();
                                        let clean_name = clean_var.split(':').next().unwrap_or(clean_var).trim();
                                        if !clean_name.is_empty() {
                                            uri_vars.push(clean_name.to_string());
                                        }
                                        curr_str = &curr_str[open_brace + 1 + close_brace + 1..];
                                    } else {
                                        break;
                                    }
                                }
                            }
                        }
                        break;
                    }
                    p_enc = parent_node.parent();
                }

                if !uri_vars.is_empty() {
                    if let Some(params_node) = node.child_by_field_name("parameters") {
                        let mut p_cursor = params_node.walk();
                        for param in params_node.children(&mut p_cursor) {
                            let p_text = ast.node_text(&param);
                            if p_text.contains("PathVariable") {
                                let explicit_name = if let Some(pv_idx) = p_text.find("PathVariable") {
                                    let after = &p_text[pv_idx..];
                                    if let Some(q1) = after.find('"') {
                                        after[q1 + 1..].find('"').map(|q2| after[q1 + 1..q1 + 1 + q2].trim().to_string())
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                let target_var = explicit_name.unwrap_or_else(|| {
                                    param.child_by_field_name("name")
                                        .map(|n| ast.node_text(&n).to_string())
                                        .unwrap_or_default()
                                });

                                if !target_var.is_empty() && !uri_vars.contains(&target_var) {
                                    let err_node = param.child_by_field_name("name").unwrap_or(param);
                                    out.push(SemanticDiagnostic {
                                        range: ast.node_range(&err_node),
                                        message: format!(
                                            "Path variable '{}' does not match any URI template variable {{{}}} in mapping annotation",
                                            target_var,
                                            uri_vars.join(", ")
                                        ),
                                        code: DiagnosticCode::SpringPathVariableMismatch,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // Check for unhandled throw statement outside try-catch (checked exceptions only)
            if node.kind() == "throw_statement" {
                if let Some(exc_name) = get_thrown_exception_type(&node, ast) {
                    if is_checked_exception(&exc_name) {
                        let mut in_try = false;
                        let mut p = node.parent();
                        while let Some(parent_node) = p {
                            if parent_node.kind() == "try_statement" || parent_node.kind() == "try_with_resources_statement" {
                                let text = ast.node_text(&parent_node);
                                if text.contains(&exc_name) || text.contains("Exception") || text.contains("Throwable") {
                                    in_try = true;
                                    break;
                                }
                            }
                            if parent_node.kind() == "method_declaration" || parent_node.kind() == "constructor_declaration" {
                                let p_text = ast.node_text(&parent_node);
                                if p_text.contains("SneakyThrows") {
                                    in_try = true;
                                    break;
                                }
                                let mut enc = parent_node.parent();
                                while let Some(e) = enc {
                                    if e.kind() == "class_declaration" || e.kind() == "record_declaration" {
                                        if let Some(mods) = e.child_by_field_name("modifiers") {
                                            if ast.node_text(&mods).contains("SneakyThrows") {
                                                in_try = true;
                                            }
                                        }
                                        break;
                                    }
                                    enc = e.parent();
                                }
                                if in_try {
                                    break;
                                }
                                let throws_node = parent_node.child_by_field_name("throws")
                                    .or_else(|| parent_node.children(&mut parent_node.walk()).find(|c| c.kind() == "throws"));
                                if let Some(throws_node) = throws_node {
                                    let t_text = ast.node_text(&throws_node);
                                    if t_text.contains(&exc_name) || t_text.contains("Exception") || t_text.contains("Throwable") {
                                        in_try = true;
                                    }
                                }
                                break;
                            }
                            if parent_node.kind() == "lambda_expression" {
                                break;
                            }
                            p = parent_node.parent();
                        }

                        if !in_try {
                            out.push(SemanticDiagnostic {
                                range: ast.node_range(&node),
                                message: format!("Unhandled exception: {}; must be caught or declared to be thrown", exc_name),
                                code: DiagnosticCode::UnhandledException,
                            });
                        }
                    }
                }
            }

            // Check for dead code after return/throw/break statements
            if node.kind() == "block" {
                let mut cursor = node.walk();
                let mut has_terminated = false;
                for child in node.children(&mut cursor) {
                    let k = child.kind();
                    if has_terminated {
                        if child.is_named() && !k.ends_with("_comment") && k != "line_comment" && k != "block_comment" && k != ";" && k != "}" && k != "{" {
                            out.push(SemanticDiagnostic {
                                range: ast.node_range(&child),
                                message: "Unreachable code: statement will never be executed".to_string(),
                                code: DiagnosticCode::UnreachableCode,
                            });
                            break;
                        }
                    }
                    if k == "return_statement"
                        || k == "throw_statement"
                        || k == "break_statement"
                        || k == "continue_statement"
                    {
                        has_terminated = true;
                    }
                }
            }

            // Check for explicit null dereference: e.g. null.foo() or x.foo() when x is known to be null
            if node.kind() == "method_invocation" {
                if let Some(obj) = node.child_by_field_name("object") {
                    let obj_name = ast.node_text(&obj);
                    if obj_name == "null" {
                        out.push(SemanticDiagnostic {
                            range: ast.node_range(&node),
                            message: "Null pointer dereference: invoking method on null literal".to_string(),
                            code: DiagnosticCode::NullDereference,
                        });
                    } else if obj.kind() == "identifier" {
                        let mut curr_p = node.parent();
                        while let Some(p) = curr_p {
                            if p.kind() == "if_statement" {
                                if let Some(cond) = p.child_by_field_name("condition") {
                                    let cond_text = ast.node_text(&cond);
                                    // Condition must not contain '||' disjunction (which makes null state non-deterministic)
                                    // And method invocation must be strictly within the consequence body (not in the if condition)
                                    if !cond_text.contains("||")
                                        && (cond_text.contains(&format!("{} == null", obj_name))
                                            || cond_text.contains(&format!("null == {}", obj_name)))
                                        && let Some(conseq) = p.child_by_field_name("consequence") {
                                            if node.start_byte() >= conseq.start_byte()
                                                && node.end_byte() <= conseq.end_byte() {
                                                // If variable was assigned a non-null value before this invocation, it is not null
                                                fn is_var_assigned_before(scope: tree_sitter::Node, var: &str, before: usize, ast: &JavaAst) -> bool {
                                                    let mut stack = vec![scope];
                                                    while let Some(n) = stack.pop() {
                                                        if n.start_byte() >= before {
                                                            continue;
                                                        }
                                                        if n.kind() == "assignment_expression" {
                                                            if let Some(left) = n.child_by_field_name("left") {
                                                                let left_text = ast.node_text(&left);
                                                                let clean = left_text.trim();
                                                                if clean == var || clean.ends_with(&format!(".{}", var)) {
                                                                    if let Some(right) = n.child_by_field_name("right") {
                                                                        if ast.node_text(&right).trim() != "null" {
                                                                            return true;
                                                                        }
                                                                    } else {
                                                                        return true;
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        let mut cursor = n.walk();
                                                        for child in n.children(&mut cursor) {
                                                            if child.start_byte() < before {
                                                                stack.push(child);
                                                            }
                                                        }
                                                    }
                                                    false
                                                }

                                                if is_var_assigned_before(conseq, &obj_name, node.start_byte(), ast) {
                                                    break;
                                                }

                                                out.push(SemanticDiagnostic {
                                                    range: ast.node_range(&node),
                                                    message: format!("Potential null pointer access: '{}' is known to be null at this location", obj_name),
                                                    code: DiagnosticCode::NullDereference,
                                                });
                                                break;
                                            }
                                        }
                                }
                            }
                            if p.kind() == "method_declaration" {
                                break;
                            }
                            curr_p = p.parent();
                        }
                    }
                }

                // Check for method invocations throwing well-known Checked Exceptions outside try-catch
                let m_name = node.child_by_field_name("name").map(|n| ast.node_text(&n)).unwrap_or("");
                let obj_text = node.child_by_field_name("object").map(|o| ast.node_text(&o)).unwrap_or("");

                // Check for deprecated method invocations
                let is_known_deprecated = (obj_text.contains("Thread") && (m_name == "stop" || m_name == "suspend" || m_name == "resume"))
                    || (obj_text.contains("Date") && (m_name == "getYear" || m_name == "setYear" || m_name == "getMonth" || m_name == "getHours"))
                    || (obj_text == "System" && m_name == "runFinalizersOnExit");
                if is_known_deprecated {
                    out.push(SemanticDiagnostic {
                        range: ast.node_range(&node),
                        message: format!("The method '{}' from the type '{}' is deprecated", m_name, obj_text),
                        code: DiagnosticCode::DeprecatedUsage,
                    });
                }

                let checked_exc = if (obj_text == "Thread" && m_name == "sleep") || m_name == "wait" {
                    Some("InterruptedException")
                } else if (obj_text == "Files" && (m_name.starts_with("read") || m_name.starts_with("write") || m_name == "copy" || m_name == "lines"))
                    || (obj_text.contains("Reader") && m_name == "readLine")
                    || (obj_text.contains("Stream") && (m_name == "read" || m_name == "write" || m_name == "close")) {
                    Some("IOException")
                } else if obj_text == "Class" && m_name == "forName" {
                    Some("ClassNotFoundException")
                } else {
                    None
                };

                if let Some(exc_name) = checked_exc {
                    let mut in_try = false;
                    let mut p = node.parent();
                    while let Some(parent_node) = p {
                        if parent_node.kind() == "try_statement" || parent_node.kind() == "try_with_resources_statement" {
                            let text = ast.node_text(&parent_node);
                            if text.contains(exc_name) || text.contains("Exception") || text.contains("Throwable") {
                                in_try = true;
                                break;
                            }
                        }
                        if parent_node.kind() == "method_declaration" || parent_node.kind() == "constructor_declaration" {
                            let p_text = ast.node_text(&parent_node);
                            if p_text.contains("SneakyThrows") {
                                in_try = true;
                                break;
                            }
                            let mut enc = parent_node.parent();
                            while let Some(e) = enc {
                                if e.kind() == "class_declaration" || e.kind() == "record_declaration" {
                                    if let Some(mods) = e.child_by_field_name("modifiers") {
                                        if ast.node_text(&mods).contains("SneakyThrows") {
                                            in_try = true;
                                        }
                                    }
                                    break;
                                }
                                enc = e.parent();
                            }
                            if in_try {
                                break;
                            }
                            let throws_node = parent_node.child_by_field_name("throws")
                                .or_else(|| parent_node.children(&mut parent_node.walk()).find(|c| c.kind() == "throws"));
                            if let Some(throws_node) = throws_node {
                                let t_text = ast.node_text(&throws_node);
                                if t_text.contains(exc_name) || t_text.contains("Exception") || t_text.contains("Throwable") {
                                    in_try = true;
                                }
                            }
                            break;
                        }
                        if parent_node.kind() == "lambda_expression" {
                            break;
                        }
                        p = parent_node.parent();
                    }

                    if !in_try {
                        let name_node = node.child_by_field_name("name").unwrap_or(node);
                        out.push(SemanticDiagnostic {
                            range: ast.node_range(&name_node),
                            message: format!("Unhandled exception: {}; must be caught or declared to be thrown", exc_name),
                            code: DiagnosticCode::UnhandledException,
                        });
                    }
                }

                // Check for static method accessed via instance (compiler.warn.static.access)
                if let (Some(obj), Some(name_node)) = (node.child_by_field_name("object"), node.child_by_field_name("name")) {
                    let m_name = ast.node_text(&name_node);
                    let obj_text = ast.node_text(&obj);
                    let static_owner = match m_name {
                        "sleep" | "yield" | "currentThread" | "interrupted" | "holdsLock" => Some("Thread"),
                        "currentTimeMillis" | "nanoTime" | "gc" | "exit" | "getenv" | "lineSeparator" => Some("System"),
                        "emptyList" | "emptyMap" | "emptySet" | "singletonList" | "unmodifiableList" => Some("Collections"),
                        "requireNonNull" | "checkIndex" => Some("Objects"),
                        _ => None,
                    };
                    if let Some(owner) = static_owner {
                        if obj_text != owner && !obj_text.ends_with(&format!(".{}", owner)) && obj_text != "super" && obj_text != "this" {
                            let start = node.start_position();
                            let locals = ast.extract_local_variables_at_position(start.row as u32, start.column as u32);
                            let is_matching_type = if obj.kind() == "identifier" {
                                if let Some((_, var_type)) = locals.iter().find(|(name, _)| name == &obj_text) {
                                    var_type == owner || var_type.ends_with(&format!(".{}", owner))
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                            if is_matching_type {
                                out.push(SemanticDiagnostic {
                                    range: ast.node_range(&node),
                                    message: format!("The static method '{}' from the type '{}' should be accessed in a static way", m_name, owner),
                                    code: DiagnosticCode::StaticMemberViaInstance,
                                });
                            }
                        }
                    }
                }
            }

            // Check for string concatenation in loops (performance warning)
            if node.kind() == "for_statement"
                || node.kind() == "enhanced_for_statement"
                || node.kind() == "while_statement"
                || node.kind() == "do_statement"
            {
                fn check_loop_string_concat(n: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<SemanticDiagnostic>) {
                    if n.kind() == "assignment_expression" {
                        let text = ast.node_text(&n);
                        if text.contains("+=") {
                            // Only warn if the variable is declared as String or the RHS contains a string literal or string method call,
                            // avoiding false positives on numeric accumulators (e.g. sum += i)
                            let is_string_rhs = if let Some(right) = n.child_by_field_name("right") {
                                let rtext = ast.node_text(&right);
                                right.kind() == "string_literal" || rtext.contains('"') || rtext.contains(".toString()") || rtext.contains("String.valueOf(")
                            } else {
                                false
                            };
                            let is_string_target = if let Some(left) = n.child_by_field_name("left") {
                                let var_name = ast.node_text(&left);
                                let mut p = n.parent();
                                let mut found_str = false;
                                while let Some(parent_node) = p {
                                    if matches!(parent_node.kind(), "method_declaration" | "constructor_declaration" | "class_declaration") {
                                        let p_text = ast.node_text(&parent_node);
                                        for line in p_text.lines() {
                                            let trimmed = line.trim();
                                            if (trimmed.starts_with("String ") || trimmed.starts_with("String\t") || trimmed.contains(" String "))
                                                && trimmed.contains(var_name) {
                                                found_str = true;
                                                break;
                                            }
                                        }
                                        break;
                                    }
                                    p = parent_node.parent();
                                }
                                found_str
                            } else {
                                false
                            };
                            if is_string_rhs || is_string_target {
                                out.push(SemanticDiagnostic {
                                    range: ast.node_range(&n),
                                    message: "String concatenation in loop: consider using 'StringBuilder' for better performance".to_string(),
                                    code: DiagnosticCode::StringConcatInLoop,
                                });
                            }
                        }
                    }
                    let mut cursor = n.walk();
                    for child in n.children(&mut cursor) {
                        check_loop_string_concat(child, ast, out);
                    }
                }
                if let Some(body) = node.child_by_field_name("body") {
                    check_loop_string_concat(body, ast, out);
                }
            }

            // Check for uninitialized blank final fields in class constructors
            if node.kind() == "class_declaration" {
                let class_mods_text = {
                    let mut text = String::new();
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "modifiers" || child.kind() == "annotation" || child.kind() == "marker_annotation" {
                            text.push_str(ast.node_text(&child));
                            text.push(' ');
                        } else if matches!(child.kind(), "class" | "interface" | "enum" | "record") {
                            break;
                        }
                    }
                    text
                };
                let has_lombok_ctors = class_mods_text.contains("RequiredArgsConstructor")
                    || class_mods_text.contains("AllArgsConstructor")
                    || class_mods_text.contains("Data")
                    || class_mods_text.contains("Value")
                    || class_mods_text.contains("Builder");
                let has_lombok_data_or_getters = has_lombok_ctors
                    || class_mods_text.contains("Getter")
                    || class_mods_text.contains("Setter")
                    || class_mods_text.contains("ToString")
                    || class_mods_text.contains("EqualsAndHashCode")
                    || class_mods_text.contains("Entity")
                    || class_mods_text.contains("Table")
                    || class_mods_text.contains("ConfigurationProperties");

                let mut blank_final_fields = Vec::new();
                let mut constructors = Vec::new();
                let mut has_instance_init = false;

                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for member in body.children(&mut cursor) {
                        if member.kind() == "field_declaration" {
                            let is_final = member.children(&mut member.walk()).any(|c| {
                                (c.kind() == "modifiers" && ast.node_text(&c).split_whitespace().any(|w| w == "final"))
                                    || ast.node_text(&c) == "final"
                            });
                            let is_static = member.children(&mut member.walk()).any(|c| {
                                (c.kind() == "modifiers" && ast.node_text(&c).split_whitespace().any(|w| w == "static"))
                                    || ast.node_text(&c) == "static"
                            });
                            if is_final && !is_static {
                                let mut f_cursor = member.walk();
                                for dec in member.children(&mut f_cursor) {
                                    if dec.kind() == "variable_declarator" && dec.child_by_field_name("value").is_none() {
                                        if let Some(name_node) = dec.child_by_field_name("name") {
                                            blank_final_fields.push((ast.node_text(&name_node).to_string(), ast.node_range(&name_node)));
                                        }
                                    }
                                }
                            }
                        } else if member.kind() == "constructor_declaration" {
                            constructors.push(member);
                        } else if member.kind() == "block" {
                            has_instance_init = true;
                        }
                    }
                }

                if !has_lombok_ctors && !has_instance_init && !blank_final_fields.is_empty() && !constructors.is_empty() {
                    for ctor in &constructors {
                        let ctor_text = ast.node_text(ctor);
                        if ctor_text.contains("this(") {
                            continue;
                        }
                        for (fname, frange) in &blank_final_fields {
                            let assigned = ctor_text.contains(&format!("this.{} =", fname))
                                || ctor_text.contains(&format!("this.{}=", fname))
                                || ctor_text.contains(&format!("{} =", fname))
                                || ctor_text.contains(&format!("{}=", fname));
                            if !assigned {
                                out.push(SemanticDiagnostic {
                                    range: frange.clone(),
                                    message: format!("Variable '{}' might not have been initialized in constructor", fname),
                                    code: DiagnosticCode::UninitializedVariable,
                                });
                            }
                        }
                    }
                }

                // Check for unused private fields and methods in class
                let class_name = node.child_by_field_name("name").map(|n| ast.node_text(&n)).unwrap_or("");
                if let Some(body) = node.child_by_field_name("body") {
                    let mut private_fields = Vec::new();
                    let mut private_methods = Vec::new();
                    let mut methods_to_check_override = Vec::new();

                    let mut iface_names = Vec::new();
                    if let Some(interfaces_node) = node.child_by_field_name("interfaces") {
                        let text = ast.node_text(&interfaces_node);
                        iface_names.push(text);
                    }
                    for ch in node.children(&mut node.walk()) {
                        if ch.kind() == "super_interfaces" {
                            iface_names.push(ast.node_text(&ch));
                        }
                    }

                    let iface_has = |target: &str| -> bool {
                        iface_names.iter().any(|iface| {
                            iface.split(|c: char| !c.is_alphanumeric()).any(|part| part == target)
                        })
                    };

                    let mut cursor = body.walk();
                    for member in body.children(&mut cursor) {
                        if member.kind() == "field_declaration" {
                            let is_private = member.children(&mut member.walk()).any(|c| {
                                (c.kind() == "modifiers" && ast.node_text(&c).split_whitespace().any(|w| w == "private"))
                                    || ast.node_text(&c) == "private"
                            });
                            let is_annotated = member.children(&mut member.walk()).any(|c| c.kind() == "marker_annotation" || c.kind() == "annotation");
                            if is_private && !is_annotated && !has_lombok_data_or_getters {
                                for dec in member.children(&mut member.walk()) {
                                    if dec.kind() == "variable_declarator" {
                                        if let Some(name_node) = dec.child_by_field_name("name") {
                                            let fname = ast.node_text(&name_node);
                                            if fname != "serialVersionUID" && !fname.starts_with('_') {
                                                private_fields.push((fname.to_string(), ast.node_range(&name_node)));
                                            }
                                        }
                                    }
                                }
                            }
                        } else if member.kind() == "method_declaration" {
                            let is_private = member.children(&mut member.walk()).any(|c| {
                                (c.kind() == "modifiers" && ast.node_text(&c).split_whitespace().any(|w| w == "private"))
                                    || ast.node_text(&c) == "private"
                            });
                            let is_annotated = member.children(&mut member.walk()).any(|c| c.kind() == "marker_annotation" || c.kind() == "annotation");
                            if is_private && !is_annotated {
                                if let Some(name_node) = member.child_by_field_name("name") {
                                    let m_name = ast.node_text(&name_node);
                                    if !matches!(m_name, "readObject" | "writeObject" | "readObjectNoData" | "writeReplace" | "readResolve") {
                                        private_methods.push((m_name.to_string(), ast.node_range(&name_node)));
                                    }
                                }
                            }

                            // Check missing @Override
                            let has_override = member.children(&mut member.walk()).any(|c| {
                                (c.kind() == "marker_annotation" || c.kind() == "annotation") && ast.node_text(&c).contains("Override")
                            });
                            if !has_override {
                                if let Some(m_name_node) = member.child_by_field_name("name") {
                                    let m_name = ast.node_text(&m_name_node);
                                    let p_count = member.child_by_field_name("parameters").map(|p| {
                                        p.children(&mut p.walk()).filter(|c| c.kind() == "formal_parameter").count()
                                    }).unwrap_or(0);
                                    methods_to_check_override.push((m_name.to_string(), p_count, ast.node_range(&m_name_node)));
                                }
                            }
                        }
                    }

                    if !private_fields.is_empty() || !private_methods.is_empty() {
                        let mut all_idents = Vec::new();
                        fn collect_body_idents(n: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<String>) {
                            if n.kind() == "identifier" {
                                out.push(ast.node_text(&n).to_string());
                            }
                            let mut c = n.walk();
                            for child in n.children(&mut c) {
                                collect_body_idents(child, ast, out);
                            }
                        }
                        collect_body_idents(body, ast, &mut all_idents);

                        let mut ident_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::with_capacity(all_idents.len());
                        for id in &all_idents {
                            *ident_counts.entry(id.as_str()).or_default() += 1;
                        }

                        for (fname, frange) in private_fields {
                            let count = ident_counts.get(fname.as_str()).copied().unwrap_or(0);
                            if count <= 1 {
                                out.push(SemanticDiagnostic {
                                    range: frange,
                                    message: format!("The value of the field '{}.{}' is not used", class_name, fname),
                                    code: DiagnosticCode::UnusedPrivateMember,
                                });
                            }
                        }

                        for (mname, mrange) in private_methods {
                            let count = ident_counts.get(mname.as_str()).copied().unwrap_or(0);
                            if count <= 1 {
                                out.push(SemanticDiagnostic {
                                    range: mrange,
                                    message: format!("The method '{}' from the type '{}' is never used locally", mname, class_name),
                                    code: DiagnosticCode::UnusedPrivateMember,
                                });
                            }
                        }
                    }

                    // Check if methods match standard interfaces
                    if !iface_names.is_empty() && !methods_to_check_override.is_empty() {
                        for (mname, pcount, mrange) in methods_to_check_override {
                            let matched_iface = if iface_has("Runnable") && mname == "run" && pcount == 0 {
                                Some("Runnable")
                            } else if iface_has("Comparable") && mname == "compareTo" && pcount == 1 {
                                Some("Comparable")
                            } else if (iface_has("Closeable") || iface_has("AutoCloseable")) && mname == "close" && pcount == 0 {
                                Some("AutoCloseable")
                            } else if iface_has("Callable") && mname == "call" && pcount == 0 {
                                Some("Callable")
                            } else if iface_has("Supplier") && mname == "get" && pcount == 0 {
                                Some("Supplier")
                            } else if iface_has("Consumer") && mname == "accept" && pcount == 1 {
                                Some("Consumer")
                            } else {
                                None
                            };

                            if let Some(iface) = matched_iface {
                                out.push(SemanticDiagnostic {
                                    range: mrange,
                                    message: format!("Method '{}' overrides or implements a method in '{}', but is missing '@Override' annotation", mname, iface),
                                    code: DiagnosticCode::MissingOverride,
                                });
                            }
                        }
                    }

                    // Check for unimplemented abstract methods if the class is concrete and has no superclass
                    let is_abstract_class = node.child_by_field_name("modifiers")
                        .map(|m| ast.node_text(&m).contains("abstract"))
                        .unwrap_or(false);
                    let has_superclass = node.child_by_field_name("superclass").is_some();
                    if !is_abstract_class && !has_superclass && !iface_names.is_empty() {
                        let all_methods: Vec<String> = if let Some(body) = node.child_by_field_name("body") {
                            let mut cursor = body.walk();
                            body.children(&mut cursor)
                                .filter(|c| c.kind() == "method_declaration")
                                .filter_map(|c| c.child_by_field_name("name").map(|n| ast.node_text(&n).to_string()))
                                .collect()
                        } else {
                            vec![]
                        };
                        let expected_methods: Vec<(&str, &str)> = [
                            ("Runnable", "run"),
                            ("Callable", "call"),
                            ("AutoCloseable", "close"),
                            ("Closeable", "close"),
                            ("Comparable", "compareTo"),
                            ("Supplier", "get"),
                            ("Consumer", "accept"),
                        ].iter().filter(|(iface, _)| iface_has(iface)).copied().collect();

                        for (iface, required_method) in expected_methods {
                            if !all_methods.iter().any(|m| m == required_method) {
                                let class_name = node.child_by_field_name("name").map(|n| ast.node_text(&n)).unwrap_or("Class");
                                let class_range = node.child_by_field_name("name").map(|n| ast.node_range(&n)).unwrap_or_else(|| ast.node_range(&node));
                                out.push(SemanticDiagnostic {
                                    range: class_range,
                                    message: format!("The type '{}' must implement the inherited abstract method '{}()' from '{}'", class_name, required_method, iface),
                                    code: DiagnosticCode::UnimplementedAbstractMethod,
                                });
                            }
                        }

                        if iface_has("Serializable") {
                            let mut has_serial_version_uid = false;
                            if let Some(body) = node.child_by_field_name("body") {
                                let mut b_cursor = body.walk();
                                for b_child in body.children(&mut b_cursor) {
                                    if b_child.kind() == "field_declaration" {
                                        let text = ast.node_text(&b_child);
                                        if text.contains("serialVersionUID") {
                                            has_serial_version_uid = true;
                                            break;
                                        }
                                    }
                                }
                            }
                            if !has_serial_version_uid {
                                let class_name = node.child_by_field_name("name").map(|n| ast.node_text(&n)).unwrap_or("Class");
                                let class_range = node.child_by_field_name("name").map(|n| ast.node_range(&n)).unwrap_or_else(|| ast.node_range(&node));
                                out.push(SemanticDiagnostic {
                                    range: class_range,
                                    message: format!("The serializable class '{}' does not declare a static final serialVersionUID field of type long", class_name),
                                    code: DiagnosticCode::MissingSerialVersionUid,
                                });
                            }
                        }
                    }
                }
            }

            // Check for switch expression or statement missing default case (Switch Exhaustiveness)
            if node.kind() == "switch_expression" || node.kind() == "switch_statement" {
                if let Some(body) = node.child_by_field_name("body") {
                    let mut has_default = false;
                    let mut has_cases = false;
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        if child.kind() == "switch_block_statement_group" || child.kind() == "switch_rule" {
                            has_cases = true;
                            let text = ast.node_text(&child);
                            if text.contains("default") {
                                has_default = true;
                                break;
                            }
                        }
                    }
                    if has_cases && !has_default {
                        let msg = if node.kind() == "switch_expression" {
                            "Switch expression does not cover all possible input values and lacks a 'default' branch"
                        } else {
                            "Switch statement does not cover all possible values and lacks a 'default' branch"
                        };
                        out.push(SemanticDiagnostic {
                            range: ast.node_range(&node),
                            message: msg.to_string(),
                            code: DiagnosticCode::SwitchMissingDefault,
                        });
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, out);
            }
        }

        traverse(root, self, &mut diags);
        diags
    }

    /// Find all occurrences of an identifier name in this AST
    pub fn find_all_occurrences(&self, ident: &str) -> Vec<Range> {
        if !self.source.contains(ident) {
            return Vec::new();
        }
        let map = self.find_all_occurrences_multiple(&[ident]);
        map.into_values().next().unwrap_or_default()
    }

    /// Find all occurrences of multiple identifier names in a single AST traversal pass
    pub fn find_all_occurrences_multiple(&self, idents: &[&str]) -> std::collections::HashMap<String, Vec<Range>> {
        let mut result: std::collections::HashMap<String, Vec<Range>> = std::collections::HashMap::new();
        let target_set: std::collections::HashSet<&str> = idents
            .iter()
            .copied()
            .filter(|id| self.source.contains(*id))
            .collect();

        if target_set.is_empty() {
            return result;
        }

        let root = self.tree.root_node();

        fn traverse(
            node: tree_sitter::Node,
            ast: &JavaAst,
            target_set: &std::collections::HashSet<&str>,
            out: &mut std::collections::HashMap<String, Vec<Range>>,
        ) {
            if node.kind() == "identifier" || node.kind() == "type_identifier" {
                let text = ast.node_text(&node);
                if target_set.contains(text) {
                    out.entry(text.to_string())
                        .or_default()
                        .push(ast.node_range(&node));
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, target_set, out);
            }
        }

        traverse(root, self, &target_set, &mut result);
        result
    }

    /// Locate method/constructor call context at cursor: returns (target_name, active_parameter_index, is_constructor)
    pub fn find_method_call_at_position(&self, line: u32, character: u32) -> Option<(String, u32, bool)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let mut node = root.descendant_for_point_range(point, point)?;

        // Walk up to find argument_list, method_invocation, or object_creation_expression
        while node.kind() != "method_invocation"
            && node.kind() != "argument_list"
            && node.kind() != "object_creation_expression"
        {
            let parent = node.parent()?;
            node = parent;
        }

        let invocation_node = if node.kind() == "argument_list" {
            node.parent()?
        } else {
            node
        };

        let (target_name, is_constructor) = if invocation_node.kind() == "method_invocation" {
            let name_node = invocation_node.child_by_field_name("name")?;
            (self.node_text(&name_node).to_string(), false)
        } else if invocation_node.kind() == "object_creation_expression" {
            let type_node = invocation_node.child_by_field_name("type")?;
            let raw_text = self.node_text(&type_node);
            let clean = raw_text.split('<').next().unwrap_or(raw_text).trim();
            let simple = clean.split('.').last().unwrap_or(clean);
            (simple.to_string(), true)
        } else {
            return None;
        };

        // Calculate active parameter index by counting commas before cursor
        let mut active_param = 0;
        if let Some(args_node) = invocation_node.child_by_field_name("arguments") {
            let mut cursor = args_node.walk();
            for child in args_node.children(&mut cursor) {
                if child.start_position().row > point.row
                    || (child.start_position().row == point.row && child.start_position().column > point.column)
                {
                    break;
                }
                if child.kind() == "," {
                    active_param += 1;
                }
            }
        }

        Some((target_name, active_param, is_constructor))
    }

    /// Extract code folding ranges (Imports block, Classes, Methods, Comments)
    pub fn extract_folding_ranges(&self) -> Vec<(u32, u32, &str)> {
        let mut ranges = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ranges: &mut Vec<(u32, u32, &'static str)>) {
            let start = node.start_position().row as u32;
            let end = node.end_position().row as u32;

            if start < end {
                match node.kind() {
                    "class_declaration" | "interface_declaration" | "enum_declaration" | "annotation_type_declaration" => {
                        ranges.push((start, end, "region"));
                    }
                    "method_declaration" | "constructor_declaration" => {
                        ranges.push((start, end, "region"));
                    }
                    "block" | "annotation_type_body" => {
                        ranges.push((start, end, "region"));
                    }
                    "comment" | "block_comment" => {
                        ranges.push((start, end, "comment"));
                    }
                    _ => {}
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ranges);
            }
        }

        traverse(root, &mut ranges);
        ranges
    }

    /// Generate Getters and Setters for fields of the enclosing class at cursor
    pub fn generate_getters_and_setters(&self, _line: u32) -> Vec<(String, String)> {
        let mut generated = Vec::new();
        for cls in self.extract_classes(None) {
            for f in &cls.fields {
                let capitalized = {
                    let mut chars = f.name.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                        None => f.name.clone(),
                    }
                };
                let ty_name = f.field_type.simple_name();

                let getter = format!(
                    "    public {} get{}() {{\n        return this.{};\n    }}\n",
                    ty_name, capitalized, f.name
                );

                let setter = format!(
                    "    public void set{}({} {}) {{\n        this.{} = {};\n    }}\n",
                    capitalized, ty_name, f.name, f.name, f.name
                );

                generated.push((format!("Generate Getter for '{}'", f.name), getter));
                generated.push((format!("Generate Setter for '{}'", f.name), setter));
            }

            // Generate full-arguments constructor
            if !cls.fields.is_empty() {
                let params_str = cls
                    .fields
                    .iter()
                    .map(|f| format!("{} {}", f.field_type.simple_name(), f.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let inits_str = cls
                    .fields
                    .iter()
                    .map(|f| format!("        this.{} = {};\n", f.name, f.name))
                    .collect::<Vec<_>>()
                    .concat();

                let constructor_code = format!(
                    "    public {}({}) {{\n{}    }}\n",
                    cls.simple_name, params_str, inits_str
                );
                generated.push((format!("Generate Constructor using fields for '{}'", cls.simple_name), constructor_code));
            }

            // Generate default no-arg constructor if not already present
            let has_no_arg_ctor = cls.methods.iter().any(|m| m.name == cls.simple_name && m.parameters.is_empty());
            if !has_no_arg_ctor {
                let default_ctor = format!("    public {}() {{\n    }}\n", cls.simple_name);
                generated.push((format!("Generate default constructor for '{}'", cls.simple_name), default_ctor));
            }
        }
        generated
    }

    /// Generate equals, hashCode, and toString methods for enclosing class
    pub fn generate_equals_hashcode_and_tostring(&self) -> Vec<(String, String)> {
        let mut results = Vec::new();
        for cls in self.extract_classes(None) {
            if cls.fields.is_empty() {
                continue;
            }

            // 1. Generate equals()
            let mut equals_checks = Vec::new();
            for f in &cls.fields {
                if matches!(f.field_type, crate::model::TypeSignature::Int | crate::model::TypeSignature::Long | crate::model::TypeSignature::Boolean | crate::model::TypeSignature::Double | crate::model::TypeSignature::Float | crate::model::TypeSignature::Byte | crate::model::TypeSignature::Short | crate::model::TypeSignature::Char) {
                    equals_checks.push(format!("this.{} == other.{}", f.name, f.name));
                } else {
                    equals_checks.push(format!("java.util.Objects.equals(this.{}, other.{})", f.name, f.name));
                }
            }
            let equals_code = format!(
                "    @Override\n    public boolean equals(Object o) {{\n        if (this == o) return true;\n        if (o == null || getClass() != o.getClass()) return false;\n        {} other = ({}) o;\n        return {};\n    }}\n",
                cls.simple_name, cls.simple_name, equals_checks.join(" &&\n               ")
            );
            results.push((format!("Generate equals() for '{}'", cls.simple_name), equals_code));

            // 2. Generate hashCode()
            let field_names: Vec<String> = cls.fields.iter().map(|f| format!("this.{}", f.name)).collect();
            let hashcode_code = format!(
                "    @Override\n    public int hashCode() {{\n        return java.util.Objects.hash({});\n    }}\n",
                field_names.join(", ")
            );
            results.push((format!("Generate hashCode() for '{}'", cls.simple_name), hashcode_code));

            // 3. Generate toString()
            let field_prints: Vec<String> = cls.fields.iter().map(|f| format!("\"{}\" + \"='\" + this.{} + '\\\''", f.name, f.name)).collect();
            let tostring_code = format!(
                "    @Override\n    public String toString() {{\n        return \"{}\" + \"{{\" +\n               {} +\n               '}}';\n    }}\n",
                cls.simple_name, field_prints.join(" + \", \" +\n               ")
            );
            results.push((format!("Generate toString() for '{}'", cls.simple_name), tostring_code));
        }
        results
    }

    /// Generate Extract Variable refactoring edit if selection spans an expression
    pub fn generate_extract_variable_refactoring(
        &self,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Option<(String, Range, String)> {
        if start_line == end_line && start_col == end_col {
            return None;
        }

        let root = self.tree.root_node();
        let p_start = tree_sitter::Point {
            row: start_line as usize,
            column: start_col as usize,
        };
        let p_end = tree_sitter::Point {
            row: end_line as usize,
            column: end_col as usize,
        };

        let node = root.descendant_for_point_range(p_start, p_end)?;
        let expr_text = self.node_text(&node).trim();
        if expr_text.is_empty() || expr_text.contains('\n') || expr_text.len() < 2 {
            return None;
        }

        // Avoid extracting whole statements or declarations
        if node.kind().ends_with("_statement") || node.kind().ends_with("_declaration") {
            return None;
        }

        let var_name = "extractedVar";
        let (inferred_type, range) = {
            let ty = if node.kind() == "string_literal" {
                "String"
            } else if node.kind() == "decimal_integer_literal" || node.kind() == "binary_integer_literal" {
                "int"
            } else if node.kind() == "true" || node.kind() == "false" {
                "boolean"
            } else if node.kind() == "object_creation_expression" {
                node.child_by_field_name("type")
                    .map(|t| self.node_text(&t))
                    .unwrap_or("var")
            } else {
                "var"
            };
            (ty.to_string(), self.node_range(&node))
        };

        let title = format!("Extract variable '{}' for expression", var_name);
        let definition_statement = format!("{} {} = {};\n", inferred_type, var_name, expr_text);
        Some((title, range, definition_statement))
    }

    /// Generate Extract Constant refactoring edit if selection spans a literal
    pub fn generate_extract_constant_refactoring(
        &self,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Option<(String, Range, String, String)> {
        if start_line == end_line && start_col == end_col {
            return None;
        }

        let root = self.tree.root_node();
        let p_start = tree_sitter::Point {
            row: start_line as usize,
            column: start_col as usize,
        };
        let p_end = tree_sitter::Point {
            row: end_line as usize,
            column: end_col as usize,
        };

        let node = root.descendant_for_point_range(p_start, p_end)?;
        let raw = self.node_text(&node).trim();
        if raw.is_empty() || raw.contains('\n') {
            return None;
        }

        let clean_str = raw.trim_matches('"').trim_matches('\'');
        let derived_name = clean_str
            .chars()
            .map(|c| if c.is_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
            .collect::<String>();
        let const_name = if derived_name.trim_matches('_').is_empty() {
            "CONSTANT_VALUE".to_string()
        } else {
            derived_name.trim_matches('_').to_string()
        };

        let const_type = match node.kind() {
            "string_literal" => "String",
            "decimal_integer_literal" => "int",
            "decimal_floating_point_literal" => "double",
            "true" | "false" => "boolean",
            _ => "Object",
        };

        let title = format!("Extract constant '{}'", const_name);
        let const_decl = format!("    public static final {} {} = {};\n", const_type, const_name, raw);
        Some((title, self.node_range(&node), const_name, const_decl))
    }

    /// Generate QuickFix to create method stub if invoking an unrecognised method on current class
    pub fn generate_create_method_stub_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            if n.kind() == "method_invocation" {
                if let Some(name_n) = n.child_by_field_name("name") {
                    let m_name = self.node_text(&name_n);

                    // If invocation has an explicit receiver object that is not 'this',
                    // do not suggest creating method in enclosing class (e.g. b.bMethod() calls B, not enclosing A)
                    if let Some(obj_n) = n.child_by_field_name("object") {
                        let obj_text = self.node_text(&obj_n);
                        if obj_text != "this" {
                            return None;
                        }
                    }

                    let title = format!("Create method '{}()' in enclosing class", m_name);
                    let stub = format!(
                        "    private void {}() {{\n        // TODO: auto-generated method stub\n    }}\n",
                        m_name
                    );
                    return Some((title, stub));
                }
            }
            curr = n.parent();
        }
        None
    }

    /// Generate Extract Method refactoring edit for selected range of statements
    pub fn generate_extract_method_refactoring(
        &self,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Option<(String, Range, String, String)> {
        if start_line >= end_line && start_col >= end_col {
            return None;
        }

        let root = self.tree.root_node();
        let p_start = tree_sitter::Point {
            row: start_line as usize,
            column: start_col as usize,
        };
        let p_end = tree_sitter::Point {
            row: end_line as usize,
            column: end_col as usize,
        };

        let node = root.descendant_for_point_range(p_start, p_end)?;
        let raw_selected = self.node_text(&node).trim();
        if raw_selected.is_empty() {
            return None;
        }

        let method_name = "extractedMethod";
        let title = format!("Extract method '{}()'", method_name);
        let call_stmt = format!("{}();", method_name);

        let new_method_body = format!(
            "    private void {}() {{\n        {}\n    }}\n",
            method_name,
            raw_selected.replace('\n', "\n    ")
        );

        Some((title, self.node_range(&node), call_stmt, new_method_body))
    }

    /// Generate Inline Variable refactoring edit
    pub fn generate_inline_variable_refactoring(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, Range, Vec<(Range, String)>)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point)?;

        // Find enclosing variable_declarator
        let mut curr = Some(node);
        let mut decl_node = None;
        while let Some(n) = curr {
            if n.kind() == "variable_declarator" {
                decl_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let declarator = decl_node?;
        let name_n = declarator.child_by_field_name("name")?;
        let val_n = declarator.child_by_field_name("value")?;

        let var_name = self.node_text(&name_n).to_string();
        let init_val = self.node_text(&val_n).to_string();

        let parent_stmt = declarator.parent()?;
        let decl_stmt_range = self.node_range(&parent_stmt);

        // Find occurrences to replace in enclosing method
        let mut occurrences = Vec::new();
        for r in self.find_all_occurrences(&var_name) {
            // Exclude declaration itself
            if r.start.line != name_n.start_position().row as u32
                || r.start.character != name_n.start_position().column as u32
            {
                occurrences.push((r, init_val.clone()));
            }
        }

        if occurrences.is_empty() {
            return None;
        }

        let title = format!("Inline local variable '{}'", var_name);
        Some((title, decl_stmt_range, occurrences))
    }

    /// Generate Change Method Signature (add parameter) refactoring edit
    pub fn generate_change_signature_refactoring(
        &self,
        line: u32,
        col: u32,
        new_param_type: &str,
        new_param_name: &str,
    ) -> Option<(String, Range, String)> {
        let (method_name, _) = self.find_method_declaration_at_position(line, col)?;
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut node = root.descendant_for_point_range(point, point)?;

        while node.kind() != "method_declaration" && node.kind() != "constructor_declaration" {
            node = node.parent()?;
        }

        let params_n = node.child_by_field_name("parameters")?;
        let params_text = self.node_text(&params_n);
        let updated_params = if params_text == "()" {
            format!("({} {})", new_param_type, new_param_name)
        } else {
            let inner = params_text.trim_start_matches('(').trim_end_matches(')');
            format!("({}, {} {})", inner, new_param_type, new_param_name)
        };

        let title = format!("Change signature of '{}': add parameter '{} {}'", method_name, new_param_type, new_param_name);
        Some((title, self.node_range(&params_n), updated_params))
    }

    /// Generate "Surround with try-catch" refactoring
    pub fn generate_surround_with_try_catch(
        &self,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let (s_line, e_line) = if start_line == end_line && start_col == end_col {
            let point = tree_sitter::Point {
                row: start_line as usize,
                column: start_col as usize,
            };
            let mut node = root.descendant_for_point_range(point, point)?;
            while !node.kind().ends_with("_statement") && node.kind() != "local_variable_declaration" {
                if let Some(p) = node.parent() {
                    if matches!(p.kind(), "class_body" | "compilation_unit") {
                        return None;
                    }
                    node = p;
                } else {
                    return None;
                }
            }
            let s = node.start_position().row as u32;
            let e = node.end_position().row as u32;
            (s, e)
        } else {
            let s = start_line.min(end_line);
            let e = start_line.max(end_line);
            (s, e)
        };

        let lines: Vec<&str> = self.source.lines().collect();
        if s_line as usize >= lines.len() {
            return None;
        }
        let e_line = e_line.min(lines.len().saturating_sub(1) as u32);

        let first_line = lines[s_line as usize];
        let base_indent = first_line.chars().take_while(|c| c.is_whitespace()).collect::<String>();
        let body_indent = format!("{}    ", base_indent);

        let mut wrapped = String::new();
        wrapped.push_str(&format!("{}try {{\n", base_indent));
        for i in s_line..=e_line {
            let line = lines[i as usize];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                wrapped.push('\n');
            } else {
                wrapped.push_str(&body_indent);
                wrapped.push_str(trimmed);
                wrapped.push('\n');
            }
        }
        wrapped.push_str(&format!("{}}} catch (Exception e) {{\n", base_indent));
        wrapped.push_str(&format!("{}    e.printStackTrace();\n", base_indent));
        wrapped.push_str(&format!("{}}}\n", base_indent));

        let has_next_line = (e_line as usize + 1) < lines.len();
        let range = if has_next_line {
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: s_line, character: 0 },
                end: tower_lsp::lsp_types::Position { line: e_line + 1, character: 0 },
            }
        } else {
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: s_line, character: 0 },
                end: tower_lsp::lsp_types::Position { line: e_line, character: lines[e_line as usize].len() as u32 },
            }
        };

        Some(("Surround with try-catch".to_string(), range, wrapped))
    }

    /// Generate "Surround with if" refactoring
    pub fn generate_surround_with_if(
        &self,
        start_line: u32,
        _start_col: u32,
        end_line: u32,
        _end_col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let (s_line, e_line) = (start_line.min(end_line), start_line.max(end_line));
        let lines: Vec<&str> = self.source.lines().collect();
        if s_line as usize >= lines.len() {
            return None;
        }

        let first_line = lines[s_line as usize];
        let base_indent: String = first_line.chars().take_while(|c| c.is_whitespace()).collect();
        let body_indent = format!("{}    ", base_indent);

        let mut wrapped = String::new();
        wrapped.push_str(&format!("{}if (condition) {{\n", base_indent));
        for i in s_line..=e_line.min(lines.len().saturating_sub(1) as u32) {
            let line = lines[i as usize];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                wrapped.push('\n');
            } else {
                wrapped.push_str(&body_indent);
                wrapped.push_str(trimmed);
                wrapped.push('\n');
            }
        }
        wrapped.push_str(&format!("{}}}\n", base_indent));

        let has_next_line = (e_line as usize + 1) < lines.len();
        let range = if has_next_line {
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: s_line, character: 0 },
                end: tower_lsp::lsp_types::Position { line: e_line + 1, character: 0 },
            }
        } else {
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: s_line, character: 0 },
                end: tower_lsp::lsp_types::Position { line: e_line, character: lines[e_line as usize].len() as u32 },
            }
        };

        Some(("Surround with 'if (condition)'".to_string(), range, wrapped))
    }

    /// Generate "Surround with while" refactoring
    pub fn generate_surround_with_while(
        &self,
        start_line: u32,
        _start_col: u32,
        end_line: u32,
        _end_col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let (s_line, e_line) = (start_line.min(end_line), start_line.max(end_line));
        let lines: Vec<&str> = self.source.lines().collect();
        if s_line as usize >= lines.len() {
            return None;
        }

        let first_line = lines[s_line as usize];
        let base_indent: String = first_line.chars().take_while(|c| c.is_whitespace()).collect();
        let body_indent = format!("{}    ", base_indent);

        let mut wrapped = String::new();
        wrapped.push_str(&format!("{}while (condition) {{\n", base_indent));
        for i in s_line..=e_line.min(lines.len().saturating_sub(1) as u32) {
            let line = lines[i as usize];
            let trimmed = line.trim();
            if trimmed.is_empty() {
                wrapped.push('\n');
            } else {
                wrapped.push_str(&body_indent);
                wrapped.push_str(trimmed);
                wrapped.push('\n');
            }
        }
        wrapped.push_str(&format!("{}}}\n", base_indent));

        let has_next_line = (e_line as usize + 1) < lines.len();
        let range = if has_next_line {
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: s_line, character: 0 },
                end: tower_lsp::lsp_types::Position { line: e_line + 1, character: 0 },
            }
        } else {
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: s_line, character: 0 },
                end: tower_lsp::lsp_types::Position { line: e_line, character: lines[e_line as usize].len() as u32 },
            }
        };

        Some(("Surround with 'while (condition)'".to_string(), range, wrapped))
    }

    /// Generate "Generate Delegate Methods" refactoring for a field
    pub fn generate_delegate_methods(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut field_node = None;
        while let Some(n) = curr {
            if n.kind() == "field_declaration" {
                field_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let field = field_node?;
        let type_node = field.child_by_field_name("type")?;
        let type_name = self.node_text(&type_node);

        let mut declarator = None;
        let mut cursor = field.walk();
        for child in field.children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                declarator = Some(child);
                break;
            }
        }
        let decl = declarator?;
        let name_node = decl.child_by_field_name("name")?;
        let field_name = self.node_text(&name_node);

        let delegate_code = format!(
            "\n    public void executeOn{}() {{\n        this.{}.run();\n    }}\n",
            field_name, field_name
        );

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: field.end_position().row as u32 + 1,
                character: 0,
            },
            end: tower_lsp::lsp_types::Position {
                line: field.end_position().row as u32 + 1,
                character: 0,
            },
        };

        Some((
            format!("Generate delegate methods for '{}' ({})", field_name, type_name),
            range,
            delegate_code,
        ))
    }

    /// Generate "Generate Element Javadoc" code action
    pub fn generate_javadoc_comment(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);
        let mut curr = node;
        let mut target_node = None;
        while let Some(n) = curr {
            if matches!(n.kind(), "method_declaration" | "constructor_declaration" | "class_declaration" | "interface_declaration") {
                target_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let target = target_node?;
        let start_pt = target.start_position();
        let insert_line = start_pt.row as u32;

        let lines: Vec<&str> = self.source.lines().collect();
        // Check if previous non-empty line already ends with */
        if insert_line > 0 {
            let prev_line = lines.get((insert_line - 1) as usize).unwrap_or(&"").trim();
            if prev_line.ends_with("*/") {
                return None;
            }
        }

        let base_indent = if (insert_line as usize) < lines.len() {
            lines[insert_line as usize].chars().take_while(|c| c.is_whitespace()).collect::<String>()
        } else {
            String::new()
        };

        let mut doc = String::new();
        doc.push_str(&format!("{}/**\n", base_indent));

        if target.kind() == "method_declaration" || target.kind() == "constructor_declaration" {
            let name = target.child_by_field_name("name").map(|n| self.node_text(&n)).unwrap_or("method");
            doc.push_str(&format!("{} * {}.\n", base_indent, name));
            doc.push_str(&format!("{} *\n", base_indent));

            if let Some(params_node) = target.child_by_field_name("parameters") {
                let mut cursor = params_node.walk();
                for child in params_node.children(&mut cursor) {
                    if child.kind() == "formal_parameter" {
                        if let Some(p_name_node) = child.child_by_field_name("name") {
                            let p_name = self.node_text(&p_name_node);
                            doc.push_str(&format!("{} * @param {} the {}\n", base_indent, p_name, p_name));
                        }
                    }
                }
            }

            if let Some(type_node) = target.child_by_field_name("type") {
                let ret_type = self.node_text(&type_node);
                if ret_type != "void" {
                    doc.push_str(&format!("{} * @return {}\n", base_indent, ret_type));
                }
            }
        } else {
            let name = target.child_by_field_name("name").map(|n| self.node_text(&n)).unwrap_or("Type");
            doc.push_str(&format!("{} * The type {}.\n", base_indent, name));
            doc.push_str(&format!("{} *\n", base_indent));
            doc.push_str(&format!("{} * @author developer\n", base_indent));
        }

        doc.push_str(&format!("{} */\n", base_indent));

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
            end: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
        };

        Some(("Generate Element Javadoc".to_string(), range, doc))
    }

    /// Generate pattern matching instanceof refactoring (Java 16+)
    pub fn generate_pattern_instanceof_refactoring(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut inst_node = None;
        while let Some(n) = curr {
            if n.kind() == "instanceof_expression" {
                inst_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let expr = inst_node?;
        let left = expr.child(0)?;
        let right = expr.child(2)?;

        // If it already has pattern variable (more than 3 children or right is a pattern), skip
        if expr.child_count() > 3 {
            return None;
        }

        let left_text = self.node_text(&left);
        let right_text = self.node_text(&right);
        let var_name = right_text.chars().next().map(|c| c.to_lowercase().to_string()).unwrap_or_else(|| "val".to_string());

        let new_text = format!("{} instanceof {} {}", left_text, right_text, var_name);
        let r = self.node_range(&expr);
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: r.start.line, character: r.start.character },
            end: tower_lsp::lsp_types::Position { line: r.end.line, character: r.end.character },
        };

        Some(("Use pattern matching for 'instanceof'".to_string(), range, new_text))
    }

    /// Generate "Assign parameter to field" refactoring in constructors
    pub fn generate_assign_param_to_field(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut param_node = None;
        let mut ctor_node = None;
        while let Some(n) = curr {
            if n.kind() == "formal_parameter" && param_node.is_none() {
                param_node = Some(n);
            }
            if n.kind() == "constructor_declaration" {
                ctor_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let param = param_node?;
        let ctor = ctor_node?;
        let _p_type = param.child_by_field_name("type")?;
        let p_name = param.child_by_field_name("name")?;
        let name_str = self.node_text(&p_name);

        let body = ctor.child_by_field_name("body")?;
        let body_start = body.start_position();
        let insert_line = body_start.row as u32 + 1;
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
            end: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
        };
        let code = format!("        this.{} = {};\n", name_str, name_str);

        Some((format!("Assign parameter '{}' to field", name_str), range, code))
    }

    /// Generate refactoring to convert switch statement with return/break to enhanced switch rules (->)
    pub fn generate_convert_to_switch_rule(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut switch_node = None;
        while let Some(n) = curr {
            if n.kind() == "switch_statement" || n.kind() == "switch_expression" {
                switch_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let sw = switch_node?;
        let body = sw.child_by_field_name("body")?;
        let body_text = self.node_text(&body);

        if body_text.contains("->") {
            return None;
        }

        let lines: Vec<&str> = body_text.lines().collect();
        let mut new_lines = Vec::new();
        let mut converted_any = false;

        for l in lines {
            let trimmed = l.trim();
            if trimmed.starts_with("case ") && trimmed.ends_with(':') {
                let case_val = trimmed.trim_end_matches(':');
                new_lines.push(format!("    {} ->", case_val));
                converted_any = true;
            } else if trimmed == "default:" {
                new_lines.push("    default ->".to_string());
                converted_any = true;
            } else if trimmed == "break;" {
                // omit redundant break
            } else {
                new_lines.push(l.to_string());
            }
        }

        if !converted_any {
            return None;
        }

        let r = self.node_range(&body);
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: r.start.line, character: r.start.character },
            end: tower_lsp::lsp_types::Position { line: r.end.line, character: r.end.character },
        };

        Some(("Convert to enhanced switch rule ('->')".to_string(), range, new_lines.join("\n")))
    }

    /// Generate refactoring to convert string literal to Java 15+ text block (""")
    pub fn generate_convert_to_text_block(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut str_node = None;
        while let Some(n) = curr {
            if n.kind() == "string_literal" {
                str_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let s = str_node?;
        let raw = self.node_text(&s);
        if !raw.contains("\\n") && !raw.contains("\\\"") {
            return None;
        }

        let unescaped = raw
            .trim_matches('"')
            .replace("\\n", "\n")
            .replace("\\\"", "\"");

        let text_block = format!("\"\"\"\n{}\"\"\"", unescaped);
        let r = self.node_range(&s);
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: r.start.line, character: r.start.character },
            end: tower_lsp::lsp_types::Position { line: r.end.line, character: r.end.character },
        };

        Some(("Convert to text block (Java 15+)".to_string(), range, text_block))
    }

    /// Generate "Add return statement" quickfix for methods with missing return
    pub fn generate_missing_return_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut node = root.descendant_for_point_range(point, point)?;
        while node.kind() != "method_declaration" {
            node = node.parent()?;
        }

        let body = node.child_by_field_name("body")?;
        let type_node = node.child_by_field_name("type")?;
        let ret_type = self.node_text(&type_node).trim();
        if ret_type == "void" {
            return None;
        }

        let default_val = match ret_type {
            "boolean" => "false",
            "int" | "short" | "byte" | "char" => "0",
            "long" => "0L",
            "float" => "0.0f",
            "double" => "0.0",
            _ if ret_type.starts_with("Optional") => "Optional.empty()",
            _ => "null",
        };

        let close_brace_row = body.end_position().row as u32;
        let insertion_line = close_brace_row;
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: insertion_line, character: 0 },
            end: tower_lsp::lsp_types::Position { line: insertion_line, character: 0 },
        };

        let lines: Vec<&str> = self.source.lines().collect();
        let close_line = lines.get(close_brace_row as usize).unwrap_or(&"");
        let close_indent = close_line.chars().take_while(|c| c.is_whitespace()).collect::<String>();
        let return_stmt = format!("{}    return {};\n", close_indent, default_val);

        let title = format!("Add 'return {};'", default_val);
        Some((title, range, return_stmt))
    }

    /// Generate "Add 'throws ...' to method signature" quickfix
    pub fn generate_add_throws_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut target_exception = None;
        let mut anchor_node = root.descendant_for_point_range(point, point);

        let mut curr = anchor_node;
        while let Some(n) = curr {
            if n.kind() == "throw_statement" {
                if let Some(expr) = n.child_by_field_name("expression") {
                    let text = self.node_text(&expr);
                    if let Some(open_p) = text.find('(') {
                        let name_part = text[..open_p].trim();
                        let simple = name_part.split_whitespace().last().unwrap_or("Exception");
                        let simple = simple.split('.').last().unwrap_or("Exception");
                        target_exception = Some(simple.to_string());
                    } else {
                        let simple = text.split_whitespace().last().unwrap_or("Exception");
                        target_exception = Some(simple.to_string());
                    }
                } else {
                    target_exception = Some("Exception".to_string());
                }
                break;
            } else if n.kind() == "method_invocation" {
                let m_name = n.child_by_field_name("name").map(|name_n| self.node_text(&name_n)).unwrap_or("");
                let exc = match m_name {
                    "sleep" => Some("InterruptedException"),
                    "read" | "readAllBytes" | "readAllLines" | "write" | "writeString" | "readLine" | "close" => Some("IOException"),
                    "forName" => Some("ClassNotFoundException"),
                    _ => None,
                };
                if let Some(e) = exc {
                    target_exception = Some(e.to_string());
                    break;
                }
            }
            if matches!(n.kind(), "method_declaration" | "constructor_declaration" | "class_declaration") {
                break;
            }
            curr = n.parent();
        }

        if target_exception.is_none() {
            fn find_exc_on_line(n: tree_sitter::Node, ast: &JavaAst, target_line: u32) -> Option<(String, tree_sitter::Node<'static>)> {
                if n.start_position().row as u32 <= target_line && n.end_position().row as u32 >= target_line {
                    if n.kind() == "throw_statement" {
                        let text = ast.node_text(&n);
                        let name = if let Some(open_p) = text.find('(') {
                            let prefix = &text[..open_p];
                            let simple = prefix.split_whitespace().last().unwrap_or("Exception");
                            simple.split('.').last().unwrap_or("Exception").to_string()
                        } else {
                            "Exception".to_string()
                        };
                        return Some((name, unsafe { std::mem::transmute(n) }));
                    } else if n.kind() == "method_invocation" {
                        let m_name = n.child_by_field_name("name").map(|name_n| ast.node_text(&name_n)).unwrap_or("");
                        let exc = match m_name {
                            "sleep" => Some("InterruptedException"),
                            "read" | "readAllBytes" | "readAllLines" | "write" | "writeString" | "readLine" | "close" => Some("IOException"),
                            "forName" => Some("ClassNotFoundException"),
                            _ => None,
                        };
                        if let Some(e) = exc {
                            return Some((e.to_string(), unsafe { std::mem::transmute(n) }));
                        }
                    }
                    let mut cursor = n.walk();
                    for ch in n.children(&mut cursor) {
                        if let Some(res) = find_exc_on_line(ch, ast, target_line) {
                            return Some(res);
                        }
                    }
                }
                None
            }
            if let Some((e, found_node)) = find_exc_on_line(root, self, line) {
                target_exception = Some(e);
                anchor_node = Some(found_node);
            }
        }

        let exc_name = target_exception?;
        let node = anchor_node?;

        // Find enclosing method_declaration or constructor_declaration
        let mut cur = Some(node);
        let mut method_opt = None;
        while let Some(n) = cur {
            if n.kind() == "method_declaration" || n.kind() == "constructor_declaration" {
                method_opt = Some(n);
                break;
            }
            cur = n.parent();
        }
        let method_node = method_opt?;

        let throws_n = method_node.child_by_field_name("throws")
            .or_else(|| method_node.children(&mut method_node.walk()).find(|c| c.kind() == "throws"));
        if let Some(throws_n) = throws_n {
            let throws_text = self.node_text(&throws_n);
            if throws_text.contains(&exc_name) || throws_text.contains("Exception") || throws_text.contains("Throwable") {
                return None;
            }
            let end_p = throws_n.end_position();
            let range = tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: end_p.row as u32, character: end_p.column as u32 },
                end: tower_lsp::lsp_types::Position { line: end_p.row as u32, character: end_p.column as u32 },
            };
            let title = format!("Add 'throws {}' to method signature", exc_name);
            Some((title, range, format!(", {}", exc_name)))
        } else {
            let params_n = method_node.child_by_field_name("parameters")?;
            let end_p = params_n.end_position();
            let range = tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position { line: end_p.row as u32, character: end_p.column as u32 },
                end: tower_lsp::lsp_types::Position { line: end_p.row as u32, character: end_p.column as u32 },
            };
            let title = format!("Add 'throws {}' to method signature", exc_name);
            Some((title, range, format!(" throws {}", exc_name)))
        }
    }

    /// Generate "Add 'default' branch to switch" quickfix
    pub fn generate_switch_add_default_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut switch_opt = None;
        let mut curr = node;
        while let Some(n) = curr {
            if n.kind() == "switch_expression" || n.kind() == "switch_statement" {
                switch_opt = Some(n);
                break;
            }
            curr = n.parent();
        }

        if switch_opt.is_none() {
            fn find_switch_on_line(n: tree_sitter::Node, target_line: u32) -> Option<tree_sitter::Node<'static>> {
                if n.start_position().row as u32 <= target_line && n.end_position().row as u32 >= target_line {
                    if n.kind() == "switch_expression" || n.kind() == "switch_statement" {
                        return Some(unsafe { std::mem::transmute(n) });
                    }
                    let mut cursor = n.walk();
                    for ch in n.children(&mut cursor) {
                        if let Some(res) = find_switch_on_line(ch, target_line) {
                            return Some(res);
                        }
                    }
                }
                None
            }
            switch_opt = find_switch_on_line(root, line);
        }

        let switch_node = switch_opt?;
        let body = switch_node.child_by_field_name("body")?;

        let mut has_default = false;
        let mut uses_arrow = false;
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            let text = self.node_text(&child);
            if text.contains("default") {
                has_default = true;
                break;
            }
            if child.kind() == "switch_rule" || text.contains("->") {
                uses_arrow = true;
            }
        }
        if has_default {
            return None;
        }

        let is_expr = switch_node.kind() == "switch_expression";
        let lines: Vec<&str> = self.source.lines().collect();

        let close_p = body.end_position();
        let close_row = close_p.row;
        let close_col = close_p.column.saturating_sub(1);
        let switch_row = switch_node.start_position().row;

        let switch_line = lines.get(switch_row).unwrap_or(&"");
        let switch_indent = switch_line.chars().take_while(|c| c.is_whitespace()).collect::<String>();
        let case_indent = format!("{}    ", switch_indent);

        let (range, default_snippet) = if close_row == switch_row {
            let branch = if uses_arrow {
                " default -> throw new IllegalStateException(); "
            } else {
                " default: break; "
            };
            (
                tower_lsp::lsp_types::Range {
                    start: tower_lsp::lsp_types::Position { line: close_row as u32, character: close_col as u32 },
                    end: tower_lsp::lsp_types::Position { line: close_row as u32, character: close_col as u32 },
                },
                branch.to_string(),
            )
        } else {
            let snippet = if uses_arrow {
                if is_expr {
                    format!("{}default -> throw new IllegalStateException(\"Unexpected value\");\n", case_indent)
                } else {
                    format!("{}default -> {{}}\n", case_indent)
                }
            } else {
                format!("{}default:\n{}    break;\n", case_indent, case_indent)
            };
            (
                tower_lsp::lsp_types::Range {
                    start: tower_lsp::lsp_types::Position { line: close_row as u32, character: 0 },
                    end: tower_lsp::lsp_types::Position { line: close_row as u32, character: 0 },
                },
                snippet,
            )
        };

        Some(("Add 'default' branch to switch".to_string(), range, default_snippet))
    }

    /// Generate "Initialize field" quickfix for blank final fields
    pub fn generate_initialize_field_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut field_decl = None;
        let mut curr = node;
        while let Some(n) = curr {
            if n.kind() == "field_declaration" {
                field_decl = Some(n);
                break;
            }
            curr = n.parent();
        }

        if field_decl.is_none() {
            fn find_field_on_line(n: tree_sitter::Node, target_line: u32) -> Option<tree_sitter::Node<'static>> {
                if n.start_position().row as u32 <= target_line && n.end_position().row as u32 >= target_line {
                    if n.kind() == "field_declaration" {
                        return Some(unsafe { std::mem::transmute(n) });
                    }
                    let mut cursor = n.walk();
                    for ch in n.children(&mut cursor) {
                        if let Some(res) = find_field_on_line(ch, target_line) {
                            return Some(res);
                        }
                    }
                }
                None
            }
            field_decl = find_field_on_line(root, line);
        }

        let decl_node = field_decl?;
        let is_final = decl_node.children(&mut decl_node.walk()).any(|c| {
            (c.kind() == "modifiers" && self.node_text(&c).contains("final")) || self.node_text(&c) == "final"
        });
        if !is_final {
            return None;
        }

        let type_node = decl_node.child_by_field_name("type")?;
        let ftype = self.node_text(&type_node).trim();

        let mut cursor = decl_node.walk();
        for child in decl_node.children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if child.child_by_field_name("value").is_none() {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let fname = self.node_text(&name_node);
                        let default_val = match ftype {
                            "int" | "short" | "byte" | "char" => "0",
                            "long" => "0L",
                            "float" => "0.0f",
                            "double" => "0.0",
                            "boolean" => "false",
                            "String" => "\"\"",
                            _ => "null",
                        };
                        let end_p = name_node.end_position();
                        let range = tower_lsp::lsp_types::Range {
                            start: tower_lsp::lsp_types::Position { line: end_p.row as u32, character: end_p.column as u32 },
                            end: tower_lsp::lsp_types::Position { line: end_p.row as u32, character: end_p.column as u32 },
                        };
                        let title = format!("Initialize field '{}'", fname);
                        return Some((title, range, format!(" = {}", default_val)));
                    }
                }
            }
        }

        None
    }

    /// Generate "Invert equals to 'literal'.equals(var)" refactoring to prevent NPE
    pub fn generate_invert_equals_refactoring(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut method_inv = None;
        while let Some(n) = curr {
            if n.kind() == "method_invocation" {
                method_inv = Some(n);
                break;
            }
            curr = n.parent();
        }

        let inv_node = method_inv?;
        let m_name = inv_node.child_by_field_name("name").map(|n| self.node_text(&n))?;
        if m_name != "equals" && m_name != "equalsIgnoreCase" {
            return None;
        }

        let obj_node = inv_node.child_by_field_name("object")?;
        let args_node = inv_node.child_by_field_name("arguments")?;

        let mut cursor = args_node.walk();
        let arg_expr = args_node.children(&mut cursor).find(|c| c.kind() != "(" && c.kind() != ")" && c.kind() != "," && c.is_named())?;

        let obj_text = self.node_text(&obj_node).trim();
        let arg_text = self.node_text(&arg_expr).trim();

        let is_arg_literal = arg_expr.kind() == "string_literal";
        let is_obj_literal = obj_node.kind() == "string_literal";

        if is_arg_literal || is_obj_literal {
            let inverted = format!("{}.{}({})", arg_text, m_name, obj_text);
            let title = format!("Invert equals to {}.{}({})", arg_text, m_name, obj_text);
            Some((title, self.node_lsp_range(&inv_node), inverted))
        } else {
            None
        }
    }

    /// Generate "Convert anonymous class to lambda expression" refactoring
    pub fn generate_convert_anonymous_to_lambda(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut obj_creation = None;
        while let Some(n) = curr {
            if n.kind() == "object_creation_expression" {
                obj_creation = Some(n);
                break;
            }
            curr = n.parent();
        }

        let creation_node = obj_creation?;
        let mut class_body = None;
        let mut cursor = creation_node.walk();
        for child in creation_node.children(&mut cursor) {
            if child.kind() == "class_body" {
                class_body = Some(child);
                break;
            }
        }

        let body = class_body?;
        let mut methods = Vec::new();
        let mut c = body.walk();
        for child in body.children(&mut c) {
            if child.kind() == "method_declaration" {
                methods.push(child);
            }
        }

        if methods.len() != 1 {
            return None;
        }

        let method = methods[0];
        let params_node = method.child_by_field_name("parameters")?;
        let body_node = method.child_by_field_name("body")?;

        let mut param_names = Vec::new();
        let mut p_cursor = params_node.walk();
        for p in params_node.children(&mut p_cursor) {
            if p.kind() == "formal_parameter" {
                if let Some(pname) = p.child_by_field_name("name") {
                    param_names.push(self.node_text(&pname).to_string());
                }
            }
        }

        let params_text = if param_names.is_empty() {
            "()".to_string()
        } else if param_names.len() == 1 {
            param_names[0].clone()
        } else {
            format!("({})", param_names.join(", "))
        };

        let body_text = self.node_text(&body_node);
        let lambda_text = format!("{} -> {}", params_text, body_text);
        let title = "Convert anonymous class to lambda expression".to_string();
        Some((title, self.node_lsp_range(&creation_node), lambda_text))
    }

    /// Generate "Add '@Override' annotation" quickfix
    pub fn generate_add_override_annotation_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut method_opt = None;
        while let Some(n) = curr {
            if n.kind() == "method_declaration" {
                method_opt = Some(n);
                break;
            }
            curr = n.parent();
        }

        let method_node = method_opt?;
        let has_override = method_node.children(&mut method_node.walk()).any(|c| {
            (c.kind() == "marker_annotation" || c.kind() == "annotation") && self.node_text(&c).contains("Override")
        });
        if has_override {
            return None;
        }

        let s_pos = method_node.start_position();
        let lines: Vec<&str> = self.source.lines().collect();
        let line_text = lines.get(s_pos.row).unwrap_or(&"");
        let indent = line_text.chars().take_while(|c| c.is_whitespace()).collect::<String>();

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: s_pos.row as u32, character: s_pos.column as u32 },
            end: tower_lsp::lsp_types::Position { line: s_pos.row as u32, character: s_pos.column as u32 },
        };

        let new_text = format!("@Override\n{}", indent);
        Some(("Add '@Override' annotation".to_string(), range, new_text))
    }

    /// Generate "Add unimplemented methods" quickfix
    pub fn generate_unimplemented_methods_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut class_node = None;
        while let Some(n) = curr {
            if n.kind() == "class_declaration" {
                class_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let cls = class_node?;
        let body = cls.child_by_field_name("body")?;
        let mut iface_names = Vec::new();
        if let Some(interfaces_node) = cls.child_by_field_name("interfaces") {
            let mut cursor = interfaces_node.walk();
            for child in interfaces_node.children(&mut cursor) {
                if child.kind() == "type_identifier" || child.kind().ends_with("_type") {
                    iface_names.push(self.node_text(&child).to_string());
                }
            }
        }
        for ch in cls.children(&mut cls.walk()) {
            if ch.kind() == "super_interfaces" || ch.kind() == "extends_interfaces" {
                iface_names.push(self.node_text(&ch).to_string());
            }
        }

        let joined = iface_names.join(" ");
        let method_stub = if joined.contains("Runnable") {
            "\n    @Override\n    public void run() {\n        // TODO: Auto-generated method stub\n    }\n"
        } else if joined.contains("Callable") {
            "\n    @Override\n    public Object call() throws Exception {\n        return null;\n    }\n"
        } else if joined.contains("AutoCloseable") || joined.contains("Closeable") {
            "\n    @Override\n    public void close() throws Exception {\n        // TODO: Auto-generated method stub\n    }\n"
        } else if joined.contains("Comparable") {
            "\n    @Override\n    public int compareTo(Object o) {\n        return 0;\n    }\n"
        } else if joined.contains("Supplier") {
            "\n    @Override\n    public Object get() {\n        return null;\n    }\n"
        } else if joined.contains("Consumer") {
            "\n    @Override\n    public void accept(Object t) {\n        // TODO: Auto-generated method stub\n    }\n"
        } else {
            return None;
        };

        let end_pt = body.end_position();
        let insert_line = end_pt.row.saturating_sub(1) as u32;
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
            end: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
        };

        Some(("Add unimplemented methods".to_string(), range, method_stub.to_string()))
    }

    /// Generate QuickFix to add default serialVersionUID to Serializable class
    pub fn generate_serial_version_uid_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut class_node = None;
        while let Some(n) = curr {
            if n.kind() == "class_declaration" {
                class_node = Some(n);
                break;
            }
            curr = n.parent();
        }
        let cls = class_node?;
        let body = cls.child_by_field_name("body")?;
        let start_pt = body.start_position();
        let insert_line = start_pt.row as u32 + 1;
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
            end: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
        };
        let code = "    private static final long serialVersionUID = 1L;\n\n";
        Some(("Add default serial version ID".to_string(), range, code.to_string()))
    }

    /// Generate QuickFix to add @SuppressWarnings("...") annotation
    pub fn generate_suppress_warnings_quickfix(
        &self,
        line: u32,
        col: u32,
        warning_type: &str,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        let mut target_node = None;
        while let Some(n) = curr {
            if matches!(n.kind(), "method_declaration" | "field_declaration" | "class_declaration" | "local_variable_declaration" | "constructor_declaration") {
                target_node = Some(n);
                break;
            }
            curr = n.parent();
        }
        let target = target_node?;
        let start_pt = target.start_position();
        let insert_line = start_pt.row as u32;
        let lines: Vec<&str> = self.source.lines().collect();
        let base_indent = if (insert_line as usize) < lines.len() {
            lines[insert_line as usize].chars().take_while(|c| c.is_whitespace()).collect::<String>()
        } else {
            String::new()
        };
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
            end: tower_lsp::lsp_types::Position { line: insert_line, character: 0 },
        };
        let code = format!("{}@SuppressWarnings(\"{}\")\n", base_indent, warning_type);
        Some((format!("Add @SuppressWarnings(\"{}\")", warning_type), range, code))
    }

    /// Generate QuickFix to change instance method/field access to static type access
    pub fn generate_static_access_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);
        let mut method_opt = None;
        let mut curr = node;
        while let Some(n) = curr {
            if n.kind() == "method_invocation" {
                method_opt = Some(n);
                break;
            }
            curr = n.parent();
        }

        let m_node = method_opt?;
        let obj_node = m_node.child_by_field_name("object")?;
        let name_node = m_node.child_by_field_name("name")?;
        let m_name = self.node_text(&name_node);
        let static_owner = match m_name {
            "sleep" | "yield" | "currentThread" | "interrupted" | "holdsLock" => Some("Thread"),
            "currentTimeMillis" | "nanoTime" | "gc" | "exit" | "getenv" | "lineSeparator" => Some("System"),
            "emptyList" | "emptyMap" | "emptySet" | "singletonList" | "unmodifiableList" => Some("Collections"),
            "min" | "max" | "abs" | "sqrt" | "pow" | "random" | "round" | "ceil" | "floor" => Some("Math"),
            "requireNonNull" | "checkIndex" => Some("Objects"),
            _ => None,
        }?;

        let obj_text = self.node_text(&obj_node);
        if obj_text == static_owner || obj_text.ends_with(&format!(".{}", static_owner)) || obj_text == "super" || obj_text == "this" {
            return None;
        }

        let start = m_node.start_position();
        let locals = self.extract_local_variables_at_position(start.row as u32, start.column as u32);
        let is_matching_type = if obj_node.kind() == "identifier" {
            if let Some((_, var_type)) = locals.iter().find(|(name, _)| name == &obj_text) {
                var_type == static_owner || var_type.ends_with(&format!(".{}", static_owner))
            } else {
                false
            }
        } else {
            false
        };

        if !is_matching_type {
            return None;
        }

        let range = self.node_lsp_range(&obj_node);
        let title = format!("Change access to static '{}.{}'", static_owner, m_name);
        Some((title, range, static_owner.to_string()))
    }

    /// Generate QuickFix to remove unused parameter from method declaration
    pub fn generate_remove_unused_parameter_quickfix(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut formal_param = None;
        while let Some(n) = curr {
            if n.kind() == "formal_parameter" {
                formal_param = Some(n);
                break;
            }
            curr = n.parent();
        }

        if formal_param.is_none() {
            let mut best = None;
            fn search_params<'a>(n: tree_sitter::Node<'a>, l: u32, c: u32, best: &mut Option<tree_sitter::Node<'a>>) {
                if n.start_position().row as u32 == l {
                    if n.kind() == "formal_parameter" {
                        if (n.start_position().column as u32) <= c && c <= (n.end_position().column as u32 + 2) {
                            *best = Some(n);
                            return;
                        } else if best.is_none() {
                            *best = Some(n);
                        }
                    }
                    let mut cursor = n.walk();
                    for ch in n.children(&mut cursor) {
                        search_params(ch, l, c, best);
                    }
                } else if n.start_position().row as u32 <= l && n.end_position().row as u32 >= l {
                    let mut cursor = n.walk();
                    for ch in n.children(&mut cursor) {
                        search_params(ch, l, c, best);
                    }
                }
            }
            search_params(root, line, col, &mut best);
            formal_param = best;
        }

        let param_node = formal_param?;
        let params_parent = param_node.parent()?;
        let p_name = param_node.child_by_field_name("name").map(|n| self.node_text(&n))?;

        let mut cursor = params_parent.walk();
        let all_params: Vec<_> = params_parent.children(&mut cursor).filter(|c| c.kind() == "formal_parameter").collect();
        let param_idx = all_params.iter().position(|p| p.id() == param_node.id())?;

        let range = if all_params.len() == 1 {
            self.node_lsp_range(&param_node)
        } else if param_idx < all_params.len() - 1 {
            let next_param = all_params[param_idx + 1];
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position {
                    line: param_node.start_position().row as u32,
                    character: param_node.start_position().column as u32,
                },
                end: tower_lsp::lsp_types::Position {
                    line: next_param.start_position().row as u32,
                    character: next_param.start_position().column as u32,
                },
            }
        } else {
            let prev_param = all_params[param_idx - 1];
            tower_lsp::lsp_types::Range {
                start: tower_lsp::lsp_types::Position {
                    line: prev_param.end_position().row as u32,
                    character: prev_param.end_position().column as u32,
                },
                end: tower_lsp::lsp_types::Position {
                    line: param_node.end_position().row as u32,
                    character: param_node.end_position().column as u32,
                },
            }
        };

        let title = format!("Remove unused parameter '{}'", p_name);
        Some((title, range, String::new()))
    }

    /// Generate "Convert to try-with-resources" refactoring
    pub fn generate_convert_to_try_with_resources(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut try_node = None;
        while let Some(n) = curr {
            if n.kind() == "try_statement" {
                try_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let try_stmt = try_node?;
        let parent = try_stmt.parent()?;
        let mut cursor = parent.walk();
        let siblings: Vec<_> = parent.children(&mut cursor).collect();
        let try_idx = siblings.iter().position(|s| s.id() == try_stmt.id())?;
        if try_idx == 0 {
            return None;
        }

        let prev_sibling = siblings[try_idx - 1];
        if prev_sibling.kind() != "local_variable_declaration" {
            return None;
        }

        let prev_text = self.node_text(&prev_sibling).trim().trim_end_matches(';').trim();
        let is_autocloseable = prev_text.contains("Stream")
            || prev_text.contains("Reader")
            || prev_text.contains("Writer")
            || prev_text.contains("Connection")
            || prev_text.contains("Scanner");

        if !is_autocloseable {
            return None;
        }

        let body = try_stmt.child_by_field_name("body")?;
        let body_text = self.node_text(&body);

        let lines: Vec<&str> = self.source.lines().collect();
        let s_row = prev_sibling.start_position().row;
        let prev_line = lines.get(s_row).unwrap_or(&"");
        let indent = prev_line.chars().take_while(|c| c.is_whitespace()).collect::<String>();

        let new_code = format!("{}try ({}) {}", indent, prev_text, body_text);
        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position { line: prev_sibling.start_position().row as u32, character: 0 },
            end: tower_lsp::lsp_types::Position {
                line: try_stmt.end_position().row as u32,
                character: try_stmt.end_position().column as u32,
            },
        };

        Some(("Convert to try-with-resources".to_string(), range, new_code))
    }

    /// Generate "Extract Interface" refactoring
    pub fn generate_extract_interface(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut class_node = None;
        while let Some(n) = curr {
            if n.kind() == "class_declaration" {
                class_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let cls = class_node?;
        let name_node = cls.child_by_field_name("name")?;
        let class_name = self.node_text(&name_node);
        let interface_name = format!("I{}", class_name);

        let body = cls.child_by_field_name("body")?;
        let mut method_sigs = Vec::new();
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "method_declaration" {
                let _text = self.node_text(&child);
                let is_public = child
                    .child_by_field_name("modifiers")
                    .map(|m| self.node_text(&m).contains("public"))
                    .unwrap_or(true);
                let is_static = child
                    .child_by_field_name("modifiers")
                    .map(|m| self.node_text(&m).contains("static"))
                    .unwrap_or(false);

                if is_public && !is_static {
                    if let Some(body_n) = child.child_by_field_name("body") {
                        let sig_end = body_n.start_byte();
                        let sig_start = child.start_byte();
                        if sig_start < sig_end {
                            let sig = self.source[sig_start..sig_end].trim();
                            method_sigs.push(format!("    {};", sig));
                        }
                    }
                }
            }
        }

        let interface_code = format!(
            "\npublic interface {} {{\n{}\n}}\n",
            interface_name,
            method_sigs.join("\n")
        );

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: cls.end_position().row as u32 + 1,
                character: 0,
            },
            end: tower_lsp::lsp_types::Position {
                line: cls.end_position().row as u32 + 1,
                character: 0,
            },
        };

        Some((
            format!("Extract interface '{}'", interface_name),
            range,
            interface_code,
        ))
    }

    /// Generate "Encapsulate Field" refactoring
    pub fn generate_encapsulate_field(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut field_node = None;
        while let Some(n) = curr {
            if n.kind() == "field_declaration" {
                field_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let field = field_node?;
        let modifiers_node = field.child_by_field_name("modifiers");
        let mod_text = modifiers_node.map(|m| self.node_text(&m)).unwrap_or_default();
        if mod_text.contains("private") {
            return None;
        }

        let type_node = field.child_by_field_name("type")?;
        let type_text = self.node_text(&type_node);

        let mut declarator = None;
        let mut cursor = field.walk();
        for child in field.children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                declarator = Some(child);
                break;
            }
        }
        let decl = declarator?;
        let name_node = decl.child_by_field_name("name")?;
        let field_name = self.node_text(&name_node);

        let capitalized = {
            let mut chars = field_name.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => field_name.to_string(),
            }
        };

        let is_bool = type_text == "boolean" || type_text == "Boolean";
        let getter_name = if is_bool {
            format!("is{}", capitalized)
        } else {
            format!("get{}", capitalized)
        };
        let setter_name = format!("set{}", capitalized);

        let new_field_decl = format!("private {} {};\n\n    public {} {}() {{\n        return this.{};\n    }}\n\n    public void {}({} {}) {{\n        this.{} = {};\n    }}",
            type_text, field_name, type_text, getter_name, field_name, setter_name, type_text, field_name, field_name, field_name);

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: field.start_position().row as u32,
                character: field.start_position().column as u32,
            },
            end: tower_lsp::lsp_types::Position {
                line: field.end_position().row as u32,
                character: field.end_position().column as u32,
            },
        };

        Some((
            format!("Encapsulate field '{}'", field_name),
            range,
            new_field_decl,
        ))
    }

    /// Generate "Convert for-loop to forEach()" refactoring
    pub fn generate_convert_for_to_foreach(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut for_node = None;
        while let Some(n) = curr {
            if n.kind() == "enhanced_for_statement" {
                for_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let for_stmt = for_node?;
        let name_node = for_stmt.child_by_field_name("name")?;
        let var_name = self.node_text(&name_node);
        let val_node = for_stmt.child_by_field_name("value")?;
        let collection_name = self.node_text(&val_node);
        let body_node = for_stmt.child_by_field_name("body")?;
        let body_text = self.node_text(&body_node);

        let new_code = format!("{}.forEach({} -> {});", collection_name, var_name, body_text);

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: for_stmt.start_position().row as u32,
                character: for_stmt.start_position().column as u32,
            },
            end: tower_lsp::lsp_types::Position {
                line: for_stmt.end_position().row as u32,
                character: for_stmt.end_position().column as u32,
            },
        };

        Some(("Convert for-loop to forEach()".to_string(), range, new_code))
    }

    /// Generate "Use 'var'" or "Replace 'var' with explicit type" refactoring
    pub fn generate_use_var_refactoring(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut var_decl_node = None;
        while let Some(n) = curr {
            if n.kind() == "local_variable_declaration" {
                var_decl_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let var_decl = var_decl_node?;
        let type_node = var_decl.child_by_field_name("type")?;
        let type_text = self.node_text(&type_node);

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: type_node.start_position().row as u32,
                character: type_node.start_position().column as u32,
            },
            end: tower_lsp::lsp_types::Position {
                line: type_node.end_position().row as u32,
                character: type_node.end_position().column as u32,
            },
        };

        if type_text == "var" {
            // Find initializer value to infer explicit type
            let mut val_type = "Object";
            let mut cursor = var_decl.walk();
            for child in var_decl.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(val_n) = child.child_by_field_name("value") {
                        val_type = match val_n.kind() {
                            "object_creation_expression" => {
                                val_n.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object")
                            }
                            "string_literal" => "String",
                            "decimal_integer_literal" => "int",
                            "decimal_floating_point_literal" => "double",
                            "true" | "false" => "boolean",
                            _ => "Object",
                        };
                    }
                }
            }
            Some((
                format!("Replace 'var' with explicit type '{}'", val_type),
                range,
                val_type.to_string(),
            ))
        } else {
            Some(("Use 'var' for local variable declaration".to_string(), range, "var".to_string()))
        }
    }

    /// Generate "Remove redundant cast" QuickFix
    pub fn generate_remove_redundant_cast(
        &self,
        line: u32,
        col: u32,
    ) -> Option<(String, tower_lsp::lsp_types::Range, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: col as usize,
        };
        let node = root.descendant_for_point_range(point, point);

        let mut curr = node;
        let mut cast_node = None;
        while let Some(n) = curr {
            if n.kind() == "cast_expression" {
                cast_node = Some(n);
                break;
            }
            curr = n.parent();
        }

        let cast = cast_node?;
        let value_node = cast.child_by_field_name("value")?;
        let value_text = self.node_text(&value_node);

        let range = tower_lsp::lsp_types::Range {
            start: tower_lsp::lsp_types::Position {
                line: cast.start_position().row as u32,
                character: cast.start_position().column as u32,
            },
            end: tower_lsp::lsp_types::Position {
                line: cast.end_position().row as u32,
                character: cast.end_position().column as u32,
            },
        };

        Some(("Remove redundant cast".to_string(), range, value_text.to_string()))
    }

    /// Extract Inlay Hints for variable type hints in this AST (e.g. Java 10+ var x = ...): returns (Position, label)
    pub fn find_inlay_hints(&self) -> Vec<(Position, String)> {
        self.find_inlay_hints_in_range(None)
    }

    /// Find type hints (e.g. for Java 10+ 'var') optionally restricted to a specific viewport range
    pub fn find_inlay_hints_in_range(&self, range: Option<&Range>) -> Vec<(Position, String)> {
        let mut hints = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, range: Option<&Range>, out: &mut Vec<(Position, String)>) {
            if let Some(r) = range {
                let start_row = node.start_position().row as u32;
                let end_row = node.end_position().row as u32;
                if end_row < r.start.line || start_row > r.end.line {
                    return;
                }
            }

            // Type hints for Java 10+ 'var' declarations: e.g. var x = new ArrayList<>(); -> x: ArrayList
            if node.kind() == "local_variable_declaration" {
                if let Some(type_n) = node.child_by_field_name("type") {
                    if ast.node_text(&type_n) == "var" {
                        let mut cursor = node.walk();
                        for child in node.children(&mut cursor) {
                            if child.kind() == "variable_declarator" {
                                if let (Some(name_n), Some(val_n)) = (
                                    child.child_by_field_name("name"),
                                    child.child_by_field_name("value"),
                                ) {
                                    let inferred = match val_n.kind() {
                                        "object_creation_expression" => {
                                            val_n.child_by_field_name("type").map(|t| ast.node_text(&t)).unwrap_or("Object")
                                        }
                                        "string_literal" => "String",
                                        "decimal_integer_literal" => "int",
                                        "decimal_floating_point_literal" => "double",
                                        "true" | "false" => "boolean",
                                        _ => "Object",
                                    };
                                    let end = name_n.end_position();
                                    let pos = Position {
                                        line: end.row as u32,
                                        character: ast.byte_col_to_utf16(end.row as u32, end.column),
                                    };
                                    let in_range = if let Some(r) = range {
                                        pos.line >= r.start.line && pos.line <= r.end.line
                                    } else {
                                        true
                                    };
                                    if in_range {
                                        out.push((pos, format!(": {}", inferred)));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, range, out);
            }
        }

        traverse(root, self, range, &mut hints);
        hints
    }

    /// Find method invocations and the positions of their arguments to resolve real parameter inlay hints
    pub fn find_method_invocation_arg_positions(&self) -> Vec<InvocationArgInfo> {
        self.find_method_invocation_arg_positions_in_range(None)
    }

    /// Find method invocations and the positions of their arguments optionally restricted to a specific viewport range
    pub fn find_method_invocation_arg_positions_in_range(&self, range: Option<&Range>) -> Vec<InvocationArgInfo> {
        let mut results = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, range: Option<&Range>, out: &mut Vec<InvocationArgInfo>) {
            if let Some(r) = range {
                let start_row = node.start_position().row as u32;
                let end_row = node.end_position().row as u32;
                if end_row < r.start.line || start_row > r.end.line {
                    return;
                }
            }

            if node.kind() == "method_invocation" {
                if let (Some(name_n), Some(args_node)) = (
                    node.child_by_field_name("name"),
                    node.child_by_field_name("arguments"),
                ) {
                    let m_name = ast.node_text(&name_n).to_string();
                    let recv_raw = node.child_by_field_name("object").map(|obj| ast.node_text(&obj).to_string());
                    let mut steps = Vec::new();
                    if let Some(obj) = node.child_by_field_name("object") {
                        JavaAst::collect_receiver_steps(obj, ast, &mut steps);
                    }
                    let mut arg_nodes = Vec::new();
                    let mut cursor = args_node.walk();
                    for child in args_node.children(&mut cursor) {
                        if child.is_named() {
                            arg_nodes.push(child);
                        }
                    }
                    let total = arg_nodes.len();
                    for (idx, child) in arg_nodes.into_iter().enumerate() {
                        let start = child.start_position();
                        let pos = Position {
                            line: start.row as u32,
                            character: ast.byte_col_to_utf16(start.row as u32, start.column),
                        };
                        let in_range = if let Some(r) = range {
                            pos.line >= r.start.line && pos.line <= r.end.line
                        } else {
                            true
                        };
                        if in_range {
                            out.push(InvocationArgInfo {
                                receiver_steps: steps.clone(),
                                receiver_raw: recv_raw.clone(),
                                method_name: m_name.clone(),
                                arg_index: idx,
                                total_args: total,
                                position: pos,
                            });
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, range, out);
            }
        }

        traverse(root, self, range, &mut results);
        results
    }

    /// Locate method declaration node at line/character
    pub fn find_method_declaration_at_position(&self, line: u32, character: u32) -> Option<(String, Range)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let mut node = root.descendant_for_point_range(point, point)?;

        while node.kind() != "method_declaration" && node.kind() != "constructor_declaration" {
            if let Some(parent) = node.parent() {
                node = parent;
            } else {
                return None;
            }
        }

        let name_node = node.child_by_field_name("name")?;
        let name = self.node_text(&name_node).to_string();
        let range = self.node_range(&name_node);
        Some((name, range))
    }

    /// Extract local variables and parameters visible at the specified position
    /// Returns vector of (var_name, type_name_or_inferred)
    pub fn extract_local_variables_at_position(&self, line: u32, character: u32) -> Vec<(String, String)> {
        let mut vars = Vec::new();
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let target_node = match root.descendant_for_point_range(point, point) {
            Some(n) => n,
            None => return vars,
        };

        // Find enclosing method_declaration or constructor_declaration
        let mut curr = Some(target_node);
        let mut enclosing_method = None;
        while let Some(n) = curr {
            if n.kind() == "method_declaration" || n.kind() == "constructor_declaration" {
                enclosing_method = Some(n);
                break;
            }
            curr = n.parent();
        }

        if let Some(method_node) = enclosing_method {
            // 1. Extract formal parameters
            if let Some(params_node) = method_node.child_by_field_name("parameters") {
                let mut p_cursor = params_node.walk();
                for p_child in params_node.children(&mut p_cursor) {
                    if p_child.kind() == "formal_parameter" || p_child.kind() == "spread_parameter" {
                        let p_type = p_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object");
                        if let Some(name_n) = p_child.child_by_field_name("name") {
                            vars.push((self.node_text(&name_n).to_string(), p_type.to_string()));
                        }
                    }
                }
            }
        }

        // 2. Extract local variable declarations preceding the cursor
        fn collect_locals(node: tree_sitter::Node, ast: &JavaAst, target_point: tree_sitter::Point, out: &mut Vec<(String, String)>) {
            if node.start_position().row > target_point.row {
                return;
            }

            if node.kind() == "local_variable_declaration" {
                let ty_node_opt = node.child_by_field_name("type");
                let explicit_type = ty_node_opt.map(|t| ast.node_text(&t).to_string()).unwrap_or_else(|| "var".to_string());

                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "variable_declarator" {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            let var_name = ast.node_text(&name_node).to_string();
                            let mut resolved_type = explicit_type.clone();

                            // Java 10+ var type inference
                            if resolved_type == "var" {
                                if let Some(init_node) = child.child_by_field_name("value") {
                                    if init_node.kind() == "object_creation_expression" {
                                        if let Some(ty_n) = init_node.child_by_field_name("type") {
                                            resolved_type = ast.node_text(&ty_n).to_string();
                                        }
                                    } else if init_node.kind() == "string_literal" {
                                        resolved_type = "String".to_string();
                                    } else if init_node.kind() == "decimal_integer_literal" || init_node.kind() == "hex_integer_literal" {
                                        resolved_type = "int".to_string();
                                    } else if init_node.kind() == "true" || init_node.kind() == "false" {
                                        resolved_type = "boolean".to_string();
                                    }
                                }
                            }

                            out.push((var_name, resolved_type));
                        }
                    }
                }
            } else if node.kind() == "instanceof_expression" {
                // Java 21 Record Pattern: instanceof Point(int x, int y)
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "record_pattern" {
                        let mut r_cursor = child.walk();
                        for r_child in child.children(&mut r_cursor) {
                            if r_child.kind() == "record_pattern_body" {
                                let mut b_cursor = r_child.walk();
                                for b_child in r_child.children(&mut b_cursor) {
                                    if b_child.kind() == "record_pattern_component" {
                                        let c_type = b_child.child_by_field_name("type")
                                            .map(|t| ast.node_text(&t).to_string())
                                            .unwrap_or_else(|| {
                                                b_child.children(&mut b_child.walk())
                                                    .find(|c| c.kind().ends_with("_type") || c.kind() == "type_identifier")
                                                    .map(|t| ast.node_text(&t).to_string())
                                                    .unwrap_or_else(|| "Object".to_string())
                                            });
                                        if let Some(id_n) = b_child.child_by_field_name("name")
                                            .or_else(|| b_child.children(&mut b_child.walk()).find(|c| c.kind() == "identifier")) {
                                            out.push((ast.node_text(&id_n).to_string(), c_type));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Java 16 Type Pattern: instanceof String s
                let children: Vec<_> = node.children(&mut node.walk()).collect();
                if children.len() >= 4 {
                    let maybe_type = &children[2];
                    let maybe_name = &children[3];
                    if (maybe_type.kind() == "type_identifier" || maybe_type.kind().ends_with("_type"))
                        && maybe_name.kind() == "identifier" {
                        out.push((ast.node_text(maybe_name).to_string(), ast.node_text(maybe_type).to_string()));
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_locals(child, ast, target_point, out);
            }
        }

        if let Some(m_node) = enclosing_method {
            if let Some(body_node) = m_node.child_by_field_name("body") {
                collect_locals(body_node, self, point, &mut vars);
            }
        }

        // Fault-tolerant fallback 1: search preceding AST nodes for formal_parameters if vars is empty
        if vars.is_empty() {
            fn find_nearest_params(node: tree_sitter::Node, target_row: usize, ast: &JavaAst, out: &mut Vec<(String, String)>) {
                if node.kind() == "formal_parameters" && node.start_position().row <= target_row {
                    let mut p_cursor = node.walk();
                    for p_child in node.children(&mut p_cursor) {
                        if p_child.kind() == "formal_parameter" || p_child.kind() == "spread_parameter" {
                            let p_type = p_child.child_by_field_name("type").map(|t| ast.node_text(&t)).unwrap_or("Object");
                            if let Some(name_n) = p_child.child_by_field_name("name") {
                                out.push((ast.node_text(&name_n).to_string(), p_type.to_string()));
                            }
                        }
                    }
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.start_position().row <= target_row {
                        find_nearest_params(child, target_row, ast, out);
                    }
                }
            }
            find_nearest_params(root, line as usize, self, &mut vars);
        }

        // Fault-tolerant fallback 2: text-based backward parameter scan if still empty
        if vars.is_empty() {
            for l in self.source.lines().take(line as usize + 1).collect::<Vec<_>>().into_iter().rev() {
                let trimmed = l.trim();
                if let (Some(open), Some(close)) = (trimmed.find('('), trimmed.find(')')) {
                    if open < close {
                        let inside = &trimmed[open + 1..close];
                        for param in inside.split(',') {
                            let parts: Vec<&str> = param.split_whitespace().collect();
                            if parts.len() >= 2 {
                                let p_type = parts[parts.len() - 2];
                                let p_name = parts[parts.len() - 1].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
                                if !p_name.is_empty() && !p_type.is_empty() {
                                    vars.push((p_name.to_string(), p_type.to_string()));
                                }
                            }
                        }
                        if !vars.is_empty() {
                            break;
                        }
                    }
                }
            }
        }

        vars
    }

    /// Find outgoing method invocations within a given method name
    pub fn find_outgoing_calls(&self, caller_method_name: &str) -> Vec<(String, Range)> {
        let mut calls = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, target_m: &str, in_target: bool, out: &mut Vec<(String, Range)>) {
            let is_target_method = if node.kind() == "method_declaration" || node.kind() == "constructor_declaration" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    ast.node_text(&name_node) == target_m
                } else {
                    false
                }
            } else {
                in_target
            };

            if is_target_method && node.kind() == "method_invocation" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    out.push((ast.node_text(&name_node).to_string(), ast.node_range(&name_node)));
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, target_m, is_target_method, out);
            }
        }

        traverse(root, self, caller_method_name, false, &mut calls);
        calls
    }

    /// Check if identifier at position is a local variable, parameter, or catch parameter,
    /// and if so, return all occurrences of that variable strictly within its enclosing scope (method or lambda).
    pub fn find_local_scope_occurrences(&self, line: u32, character: u32) -> Option<(String, Vec<tower_lsp::lsp_types::Range>)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let target_node = root.descendant_for_point_range(point, point)?;

        let ident_node = if target_node.kind() == "identifier" {
            target_node
        } else if let Some(id) = target_node.child_by_field_name("name") {
            id
        } else {
            return None;
        };

        let var_name = self.node_text(&ident_node);

        // Find enclosing scope: method_declaration, constructor_declaration, or lambda_expression
        let mut curr = Some(ident_node);
        let mut enclosing_scope = None;
        let mut is_local = false;

        while let Some(n) = curr {
            let k = n.kind();
            if k == "variable_declarator" {
                if let Some(p) = n.parent() {
                    if p.kind() == "local_variable_declaration" {
                        is_local = true;
                    }
                }
            } else if matches!(k, "formal_parameter" | "spread_parameter" | "catch_formal_parameter" | "enhanced_for_statement" | "record_pattern_component") {
                is_local = true;
            } else if k == "identifier" && n.parent().map(|p| p.kind() == "instanceof_expression").unwrap_or(false) {
                is_local = true;
            }

            if matches!(k, "method_declaration" | "constructor_declaration" | "lambda_expression") {
                enclosing_scope = Some(n);
                break;
            }
            curr = n.parent();
        }

        let scope_node = enclosing_scope?;

        if !is_local {
            fn is_declared_in_scope(n: tree_sitter::Node, ast: &JavaAst, target_name: &str) -> bool {
                let k = n.kind();
                if k == "variable_declarator" {
                    if let Some(name_n) = n.child_by_field_name("name") {
                        if ast.node_text(&name_n) == target_name {
                            if let Some(p) = n.parent() {
                                if p.kind() == "local_variable_declaration" {
                                    return true;
                                }
                            }
                        }
                    }
                } else if matches!(k, "formal_parameter" | "spread_parameter" | "catch_formal_parameter") {
                    if let Some(name_n) = n.child_by_field_name("name") {
                        if ast.node_text(&name_n) == target_name {
                            return true;
                        }
                    }
                } else if k == "enhanced_for_statement" {
                    if let Some(name_n) = n.child_by_field_name("name") {
                        if ast.node_text(&name_n) == target_name {
                            return true;
                        }
                    }
                } else if k == "record_pattern_component" {
                    if let Some(name_n) = n.child_by_field_name("name")
                        .or_else(|| n.children(&mut n.walk()).find(|c| c.kind() == "identifier")) {
                        if ast.node_text(&name_n) == target_name {
                            return true;
                        }
                    }
                } else if k == "instanceof_expression" {
                    let children: Vec<_> = n.children(&mut n.walk()).collect();
                    if children.len() >= 4 {
                        if let Some(last_child) = children.last() {
                            if last_child.kind() == "identifier" && ast.node_text(last_child) == target_name {
                                return true;
                            }
                        }
                    }
                }

                let mut cursor = n.walk();
                for child in n.children(&mut cursor) {
                    if !matches!(child.kind(), "class_declaration" | "interface_declaration" | "record_declaration" | "enum_declaration") {
                        if is_declared_in_scope(child, ast, target_name) {
                            return true;
                        }
                    }
                }
                false
            }

            if !is_declared_in_scope(scope_node, self, var_name) {
                return None;
            }
        }

        let mut occs = Vec::new();
        fn collect_ident_occurrences(n: tree_sitter::Node, ast: &JavaAst, target_name: &str, out: &mut Vec<tower_lsp::lsp_types::Range>) {
            if n.kind() == "identifier" && ast.node_text(&n) == target_name {
                out.push(ast.node_lsp_range(&n));
            }
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if !matches!(child.kind(), "class_declaration" | "interface_declaration" | "record_declaration" | "enum_declaration") {
                    collect_ident_occurrences(child, ast, target_name, out);
                }
            }
        }

        collect_ident_occurrences(scope_node, self, var_name, &mut occs);
        if occs.is_empty() {
            None
        } else {
            Some((var_name.to_string(), occs))
        }
    }

    /// Extract semantic tokens: returns relative tokens (delta_line, delta_start, length, token_type, token_modifiers)
    /// Token types: 0 = class, 1 = method, 2 = property/field, 3 = variable, 4 = parameter, 5 = keyword
    pub fn extract_semantic_tokens(&self) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut raw_tokens = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<(u32, u32, u32, u32)>) {
            let kind = node.kind();
            let token_type_opt = match kind {
                "type_identifier" => Some(0), // class / interface / annotation
                "identifier" => {
                    let parent_kind = node.parent().map(|p| p.kind()).unwrap_or("");
                    match parent_kind {
                        "annotation_type_declaration" => Some(0), // annotation name
                        "annotation_type_element_declaration" => Some(1), // annotation member element
                        "method_declaration" | "method_invocation" => Some(1), // method
                        "field_declaration" | "variable_declarator" => Some(2), // field/prop
                        "formal_parameter" => Some(4), // parameter
                        _ => Some(3), // variable
                    }
                }
                "@interface" => Some(5), // keyword
                _ => None,
            };

            if let Some(t_type) = token_type_opt {
                let start = node.start_position();
                let len = node.end_byte().saturating_sub(node.start_byte()) as u32;
                out.push((start.row as u32, start.column as u32, len, t_type));
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, out);
            }
        }

        traverse(root, self, &mut raw_tokens);
        raw_tokens.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        // Convert to LSP relative delta encoding
        let mut delta_tokens = Vec::new();
        let mut prev_line = 0;
        let mut prev_col = 0;

        for (line, col, len, t_type) in raw_tokens {
            let delta_line = line.saturating_sub(prev_line);
            let delta_col = if delta_line == 0 {
                col.saturating_sub(prev_col)
            } else {
                col
            };

            delta_tokens.push((delta_line, delta_col, len, t_type, 0));
            prev_line = line;
            prev_col = col;
        }

        delta_tokens
    }

    /// Extract hierarchical selection ranges from innermost node to root
    pub fn find_selection_ranges(&self, line: u32, character: u32) -> Vec<Range> {
        let mut ranges = Vec::new();
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };

        if let Some(mut node) = root.descendant_for_point_range(point, point) {
            loop {
                ranges.push(self.node_range(&node));
                if let Some(parent) = node.parent() {
                    node = parent;
                } else {
                    break;
                }
            }
        }

        ranges
    }

    /// Find URL links in comments and text
    pub fn find_links(&self) -> Vec<(Range, String)> {
        let mut links = Vec::new();
        for (line_idx, line) in self.source.lines().enumerate() {
            for prefix in &["http://", "https://"] {
                let mut search_from = 0;
                while let Some(start_idx) = line[search_from..].find(prefix) {
                    let abs_start = search_from + start_idx;
                    let rest = &line[abs_start..];
                    let len = rest
                        .find(|c: char| c.is_whitespace() || c == '*' || c == '"' || c == '>' || c == ')')
                        .unwrap_or(rest.len());
                    let url = rest[..len].to_string();

                    links.push((
                        Range {
                            start: Position { line: line_idx as u32, character: abs_start as u32 },
                            end: Position { line: line_idx as u32, character: (abs_start + len) as u32 },
                        },
                        url,
                    ));
                    search_from = abs_start + len;
                }
            }
        }
        links
    }

    /// Identify runnable main methods and JUnit test methods: returns (MethodName, Range, is_test)
    pub fn find_test_and_main_methods(&self) -> Vec<(String, Range, bool)> {
        let mut results = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<(String, Range, bool)>) {
            if node.kind() == "method_declaration" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let m_name = ast.node_text(&name_node);
                    let m_text = ast.node_text(&node);

                    // Check for JUnit / TestNG annotations
                    let is_test = m_text.contains("@Test")
                        || m_text.contains("@ParameterizedTest")
                        || m_text.contains("@RepeatedTest");

                    // Check for main method
                    let is_main = m_name == "main"
                        && m_text.contains("public")
                        && m_text.contains("static")
                        && m_text.contains("void")
                        && m_text.contains("String[]");

                    if is_test || is_main {
                        out.push((m_name.to_string(), ast.node_range(&name_node), is_test));
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, out);
            }
        }

        traverse(root, self, &mut results);
        results
    }

    /// Find test classes (classes with test methods or annotated with @SpringBootTest / @TestInstance)
    pub fn find_test_classes(&self) -> Vec<(String, Range)> {
        let mut results = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<(String, Range)>) {
            if node.kind() == "class_declaration" {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let c_name = ast.node_text(&name_node);
                    let c_text = ast.node_text(&node);

                    let has_test_annotation = c_text.contains("@SpringBootTest")
                        || c_text.contains("@TestInstance")
                        || c_text.contains("@Nested");
                    let name_ends_test = c_name.ends_with("Test")
                        || c_name.ends_with("Tests")
                        || c_name.ends_with("TestCase")
                        || c_name.ends_with("IT");
                    let contains_test_method = c_text.contains("@Test")
                        || c_text.contains("@ParameterizedTest")
                        || c_text.contains("@RepeatedTest");

                    if has_test_annotation || (name_ends_test && contains_test_method) || contains_test_method {
                        out.push((c_name.to_string(), ast.node_range(&name_node)));
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, out);
            }
        }

        traverse(root, self, &mut results);
        results
    }

    /// Extract Spring HTTP route endpoints (e.g. ("GET", "/api/users", range))
    pub fn find_spring_endpoints(&self) -> Vec<(String, String, Range)> {
        let mut results = Vec::new();
        let root = self.tree.root_node();

        fn extract_annotation_path(text: &str) -> String {
            if let Some(start) = text.find('"') {
                if let Some(end) = text[start + 1..].find('"') {
                    return text[start + 1..start + 1 + end].to_string();
                }
            }
            String::new()
        }

        fn combine_paths(base: &str, path: &str) -> String {
            let clean_base = base.trim().trim_matches('"');
            let clean_path = path.trim().trim_matches('"');
            let b = clean_base.trim_end_matches('/');
            let p = if clean_path.starts_with('/') {
                clean_path.to_string()
            } else if !clean_path.is_empty() {
                format!("/{}", clean_path)
            } else {
                String::new()
            };
            let mut combined = format!("{}{}", b, p);
            if combined.is_empty() {
                combined = "/".to_string();
            }
            if !combined.starts_with('/') {
                combined = format!("/{}", combined);
            }
            combined
        }

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<(String, String, Range)>) {
            if node.kind() == "class_declaration" {
                let mut class_base_path = String::new();
                let class_text = ast.node_text(&node);
                if let Some(rm_idx) = class_text.find("@RequestMapping") {
                    let rm_sub = &class_text[rm_idx..];
                    class_base_path = extract_annotation_path(rm_sub.lines().next().unwrap_or(""));
                }

                if let Some(body) = node.child_by_field_name("body") {
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        if child.kind() == "method_declaration" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                let m_text = ast.node_text(&child);
                                let mut endpoint_info: Option<(&str, String)> = None;

                                let mapping_specs = [
                                    ("@GetMapping", "GET"),
                                    ("@PostMapping", "POST"),
                                    ("@PutMapping", "PUT"),
                                    ("@DeleteMapping", "DELETE"),
                                    ("@PatchMapping", "PATCH"),
                                    ("@RequestMapping", "REQUEST"),
                                ];

                                for (annot, method) in mapping_specs {
                                    if let Some(idx) = m_text.find(annot) {
                                        let annot_sub = &m_text[idx..];
                                        let line = annot_sub.lines().next().unwrap_or("");
                                        let m_path = extract_annotation_path(line);
                                        let full_path = combine_paths(&class_base_path, &m_path);
                                        endpoint_info = Some((method, full_path));
                                        break;
                                    }
                                }

                                if let Some((http_m, path)) = endpoint_info {
                                    out.push((http_m.to_string(), path, ast.node_range(&name_node)));
                                }
                            }
                        }
                    }
                }
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, out);
            }
        }

        traverse(root, self, &mut results);
        results
    }

    /// Find the simple name of the enclosing class at a given position
    pub fn find_enclosing_class_at_position(&self, line: u32, character: u32) -> Option<String> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            if matches!(
                n.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "annotation_type_declaration"
            ) {
                if let Some(name_n) = n.child_by_field_name("name") {
                    return Some(self.node_text(&name_n).to_string());
                }
            }
            curr = n.parent();
        }

        // Fault-tolerant text fallback: scan backwards for class/interface/enum/record declaration
        for l in self.source.lines().take(line as usize + 1).collect::<Vec<_>>().into_iter().rev() {
            let trimmed = l.trim();
            for kw in &["class ", "interface ", "enum ", "record "] {
                if let Some(idx) = trimmed.find(kw) {
                    let after = &trimmed[idx + kw.len()..];
                    let name = after
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }

        None
    }

    /// Extract declaration line of a member (method or field) in this AST, with method overload resolution
    pub fn find_member_declaration_line(&self, member_name: &str, is_method: bool, arg_count: Option<usize>) -> Option<u32> {
        let root = self.tree.root_node();
        let mut fallback_line = None;

        fn search_node(node: Node, ast: &JavaAst, member_name: &str, is_method: bool, arg_count: Option<usize>, fallback: &mut Option<u32>) -> Option<u32> {
            if matches!(node.kind(), "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration") {
                if let Some(body) = node.child_by_field_name("body") {
                    let mut b_cursor = body.walk();
                    for member in body.children(&mut b_cursor) {
                        if is_method && member.kind() == "method_declaration" {
                            if let Some(name_node) = member.child_by_field_name("name") {
                                if ast.node_text(&name_node) == member_name {
                                    if fallback.is_none() {
                                        *fallback = Some(name_node.start_position().row as u32);
                                    }
                                    if let Some(target_args) = arg_count {
                                        let param_count = member.child_by_field_name("parameters").map(|p_node| {
                                            let mut p_cursor = p_node.walk();
                                            p_node.children(&mut p_cursor)
                                                .filter(|c| c.kind() == "formal_parameter" || c.kind() == "spread_parameter")
                                                .count()
                                        }).unwrap_or(0);
                                        if param_count == target_args {
                                            return Some(name_node.start_position().row as u32);
                                        }
                                    } else {
                                        return Some(name_node.start_position().row as u32);
                                    }
                                }
                            }
                        } else if !is_method && member.kind() == "field_declaration" {
                            let mut f_cursor = member.walk();
                            for dec in member.children(&mut f_cursor) {
                                if dec.kind() == "variable_declarator" {
                                    if let Some(name_node) = dec.child_by_field_name("name") {
                                        if ast.node_text(&name_node) == member_name {
                                            return Some(name_node.start_position().row as u32);
                                        }
                                    }
                                }
                            }
                        } else if matches!(member.kind(), "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration") {
                            if let Some(l) = search_node(member, ast, member_name, is_method, arg_count, fallback) {
                                return Some(l);
                            }
                        }
                    }
                }
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if let Some(l) = search_node(child, ast, member_name, is_method, arg_count, fallback) {
                        return Some(l);
                    }
                }
            }
            None
        }

        if let Some(l) = search_node(root, self, member_name, is_method, arg_count, &mut fallback_line) {
            Some(l)
        } else {
            fallback_line
        }
    }

    pub(crate) fn collect_receiver_steps(node: Node, ast: &JavaAst, steps: &mut Vec<ReceiverStep>) {
        match node.kind() {
            "method_invocation" => {
                if let Some(inner_obj) = node.child_by_field_name("object") {
                    Self::collect_receiver_steps(inner_obj, ast, steps);
                }
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = ast.node_text(&name_node).to_string();
                    let arg_count = node.child_by_field_name("arguments").map(|args| {
                        let mut cursor = args.walk();
                        args.children(&mut cursor).filter(|c| c.is_named()).count()
                    }).unwrap_or(0);
                    steps.push(ReceiverStep::MethodCall { name, arg_count });
                }
            }
            "field_access" => {
                if let Some(inner_obj) = node.child_by_field_name("object") {
                    Self::collect_receiver_steps(inner_obj, ast, steps);
                }
                if let Some(field_node) = node.child_by_field_name("field") {
                    let name = ast.node_text(&field_node).to_string();
                    steps.push(ReceiverStep::FieldAccess(name));
                }
            }
            "this" => {
                steps.push(ReceiverStep::This);
            }
            "super" => {
                steps.push(ReceiverStep::Super);
            }
            _ => {
                let text = ast.node_text(&node).to_string();
                steps.push(ReceiverStep::Identifier(text));
            }
        }
    }

    /// If cursor is at or on a method invocation or field access member name,
    /// return detailed MemberAccessInfo including receiver expression chain steps and method argument count.
    pub fn find_member_access_info(&self, line: u32, character: u32) -> Option<MemberAccessInfo> {
        let root = self.tree.root_node();
        let byte_col = self.utf16_col_to_byte(line, character);
        let point = tree_sitter::Point {
            row: line as usize,
            column: byte_col,
        };

        fn point_in_node(pt: tree_sitter::Point, node: &Node) -> bool {
            let start = node.start_position();
            let end = node.end_position();
            (pt.row > start.row || (pt.row == start.row && pt.column >= start.column))
                && (pt.row < end.row || (pt.row == end.row && pt.column <= end.column))
        }

        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            if n.kind() == "method_invocation" {
                if let Some(name_n) = n.child_by_field_name("name") {
                    if point_in_node(point, &name_n) {
                        let m_name = self.node_text(&name_n).to_string();
                        let recv = n.child_by_field_name("object").map(|obj| self.node_text(&obj).to_string());
                        let mut steps = Vec::new();
                        if let Some(obj) = n.child_by_field_name("object") {
                            Self::collect_receiver_steps(obj, self, &mut steps);
                        }
                        let arg_count = n.child_by_field_name("arguments").map(|args| {
                            let mut cursor = args.walk();
                            args.children(&mut cursor).filter(|c| c.is_named()).count()
                        });
                        return Some(MemberAccessInfo {
                            receiver: recv,
                            receiver_steps: steps,
                            member_name: m_name,
                            is_method: true,
                            arg_count,
                        });
                    }
                }
            } else if n.kind() == "field_access" {
                if let Some(field_n) = n.child_by_field_name("field") {
                    if point_in_node(point, &field_n) {
                        let f_name = self.node_text(&field_n).to_string();
                        let recv = n.child_by_field_name("object").map(|obj| self.node_text(&obj).to_string());
                        let mut steps = Vec::new();
                        if let Some(obj) = n.child_by_field_name("object") {
                            Self::collect_receiver_steps(obj, self, &mut steps);
                        }
                        return Some(MemberAccessInfo {
                            receiver: recv,
                            receiver_steps: steps,
                            member_name: f_name,
                            is_method: false,
                            arg_count: None,
                        });
                    }
                }
            } else if n.kind() == "scoped_identifier" {
                // e.g. Config.TIMEOUT where Config is scope and TIMEOUT is name
                if let Some(name_n) = n.child_by_field_name("name") {
                    if point_in_node(point, &name_n) {
                        let f_name = self.node_text(&name_n).to_string();
                        let recv = n.child_by_field_name("scope").map(|s| self.node_text(&s).to_string());
                        let mut steps = Vec::new();
                        if let Some(scope_node) = n.child_by_field_name("scope") {
                            Self::collect_receiver_steps(scope_node, self, &mut steps);
                        }
                        return Some(MemberAccessInfo {
                            receiver: recv,
                            receiver_steps: steps,
                            member_name: f_name,
                            is_method: false,
                            arg_count: None,
                        });
                    }
                }
            }
            curr = n.parent();
        }

        // If cursor is on a plain identifier, check if it's not a definition or declaration
        let curr_node = root.descendant_for_point_range(point, point)?;
        if curr_node.kind() == "identifier" {
            let p_kind = curr_node.parent().map(|p| p.kind()).unwrap_or("");
            if p_kind != "variable_declarator" && p_kind != "method_declaration" && p_kind != "class_declaration" {
                let name = self.node_text(&curr_node).to_string();
                return Some(MemberAccessInfo {
                    receiver: None,
                    receiver_steps: Vec::new(),
                    member_name: name,
                    is_method: false,
                    arg_count: None,
                });
            }
        }

        None
    }

    /// If cursor is at or on a method invocation or field access member name,
    /// return (receiver_object_text, member_name, is_method).
    /// receiver_object_text is None for implicit 'this' or same-class member calls/references like `foo()` or `MY_CONST`
    pub fn find_member_access_receiver(&self, line: u32, character: u32) -> Option<(Option<String>, String, bool)> {
        self.find_member_access_info(line, character)
            .map(|info| (info.receiver, info.member_name, info.is_method))
    }

    /// If cursor is at or on a method invocation's name, return (receiver_object_text, method_name)
    /// receiver_object_text is None for implicit 'this' calls like `foo()`
    pub fn find_method_call_receiver(&self, line: u32, character: u32) -> Option<(Option<String>, String)> {
        self.find_member_access_receiver(line, character).and_then(|(recv, name, is_method)| {
            if is_method {
                Some((recv, name))
            } else {
                None
            }
        })
    }

    /// Check if cursor position is right after a dot (e.g. `obj.` or `foo.bar.`) or typing a member identifier after dot
    pub fn is_after_dot_access(&self, line: u32, character: u32) -> bool {
        let root = self.tree.root_node();
        let byte_col = self.utf16_col_to_byte(line, character);
        let point = tree_sitter::Point {
            row: line as usize,
            column: byte_col,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            let k = n.kind();
            if k == "field_access" || k == "method_invocation" {
                return true;
            }
            curr = n.parent();
        }

        // Fast text inspection fallback with UTF-16 awareness
        if let Some(line_str) = self.source.lines().nth(line as usize) {
            let byte_limit = self.utf16_col_to_byte(line, character).min(line_str.len());
            let prefix = line_str[..byte_limit].trim_end();
            if prefix.ends_with('.') {
                return true;
            }
            let before_ident = prefix.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_');
            return before_ident.ends_with('.');
        }

        false
    }

    /// Check if cursor position is preceded by `new ` keyword (for constructor instantiation completion)
    pub fn is_after_new_expression(&self, line: u32, character: u32) -> bool {
        let root = self.tree.root_node();
        let byte_col = self.utf16_col_to_byte(line, character);
        let point = tree_sitter::Point {
            row: line as usize,
            column: byte_col,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            if n.kind() == "object_creation_expression" {
                return true;
            }
            curr = n.parent();
        }

        // Fast text inspection fallback with UTF-16 awareness
        if let Some(line_str) = self.source.lines().nth(line as usize) {
            let byte_limit = self.utf16_col_to_byte(line, character).min(line_str.len());
            let prefix = line_str[..byte_limit].trim_end();
            if prefix.ends_with("new") {
                return true;
            }
            let before_ident = prefix.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == '.');
            let trimmed = before_ident.trim_end();
            return trimmed.ends_with("new");
        }

        false
    }

    /// Extract receiver expression before dot when Tree-sitter collapses into an ERROR node (Fault Tolerance)
    pub fn extract_receiver_before_dot_fallback(&self, line: u32, character: u32) -> Option<String> {
        if let Some(line_str) = self.source.lines().nth(line as usize) {
            let byte_limit = self.utf16_col_to_byte(line, character).min(line_str.len());
            let prefix = line_str[..byte_limit].trim_end();
            if let Some(dot_idx) = prefix.rfind('.') {
                let expr_before_dot = prefix[..dot_idx].trim_end();
                // Extract last identifier or chain, e.g. "user" in "user." or "order.getItem()"
                let last_token = expr_before_dot
                    .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
                    .filter(|s| !s.is_empty())
                    .last();
                return last_token.map(|s| s.to_string());
            }
        }
        None
    }

    /// Extract receiver chain steps before the current cursor position (for completion and resolution).
    /// Works both when AST node is valid and during incomplete typing right after a dot (Fault Tolerant).
    pub fn extract_receiver_steps_before_position(&self, line: u32, character: u32) -> Vec<ReceiverStep> {
        // 1. Try AST-based member access info if cursor is on an existing member token
        if let Some(info) = self.find_member_access_info(line, character) {
            if !info.receiver_steps.is_empty() {
                return info.receiver_steps;
            }
        }

        // 2. Parse text immediately before cursor
        let line_str = match self.source.lines().nth(line as usize) {
            Some(l) => l,
            None => return Vec::new(),
        };
        let byte_limit = self.utf16_col_to_byte(line, character).min(line_str.len());
        let prefix = &line_str[..byte_limit];

        // Find the dot: strip any identifier currently being typed after dot
        let dot_idx = match prefix.rfind('.') {
            Some(idx) => idx,
            None => return Vec::new(),
        };

        // Everything after the dot up to byte_limit must be a valid identifier prefix (or empty)
        let suffix = &prefix[dot_idx + 1..];
        if !suffix.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Vec::new();
        }

        let before_dot = prefix[..dot_idx].trim_end();
        if before_dot.is_empty() {
            return Vec::new();
        }

        // Backward scan to find start of receiver expression
        let bytes = before_dot.as_bytes();
        let mut i = bytes.len();
        let mut paren_depth: i32 = 0;
        let mut bracket_depth: i32 = 0;

        while i > 0 {
            let b = bytes[i - 1];
            if b == b')' {
                paren_depth += 1;
                i -= 1;
            } else if b == b'(' {
                if paren_depth > 0 {
                    paren_depth -= 1;
                    i -= 1;
                } else {
                    break;
                }
            } else if b == b']' {
                bracket_depth += 1;
                i -= 1;
            } else if b == b'[' {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                    i -= 1;
                } else {
                    break;
                }
            } else if paren_depth == 0 && bracket_depth == 0 {
                if b.is_ascii_whitespace()
                    || b == b';'
                    || b == b','
                    || b == b'='
                    || b == b'{'
                    || b == b'}'
                    || b == b'?'
                    || b == b':'
                    || b == b'+'
                    || b == b'-'
                    || b == b'*'
                    || b == b'/'
                    || b == b'%'
                    || b == b'&'
                    || b == b'|'
                    || b == b'^'
                    || b == b'!'
                    || b == b'<'
                    || b == b'>'
                {
                    break;
                }
                i -= 1;
            } else {
                i -= 1;
            }
        }

        let expr = before_dot[i..].trim();
        if expr.is_empty() {
            return Vec::new();
        }

        // Tokenize expr into ReceiverSteps by splitting on '.' with paren_depth == 0
        let mut steps = Vec::new();
        let mut start = 0;
        let mut p_depth = 0;
        let expr_bytes = expr.as_bytes();

        for j in 0..expr_bytes.len() {
            let b = expr_bytes[j];
            if b == b'(' {
                p_depth += 1;
            } else if b == b')' {
                if p_depth > 0 {
                    p_depth -= 1;
                }
            } else if b == b'.' && p_depth == 0 {
                let part = expr[start..j].trim();
                if !part.is_empty() {
                    Self::append_receiver_part(&mut steps, part);
                }
                start = j + 1;
            }
        }

        if start < expr.len() {
            let part = expr[start..].trim();
            if !part.is_empty() {
                Self::append_receiver_part(&mut steps, part);
            }
        }

        steps
    }

    fn append_receiver_part(steps: &mut Vec<ReceiverStep>, part: &str) {
        if part == "this" {
            steps.push(ReceiverStep::This);
        } else if part == "super" {
            steps.push(ReceiverStep::Super);
        } else if part.ends_with(')') {
            if let Some(open_paren) = part.find('(') {
                let name = part[..open_paren].trim().to_string();
                let inner = part[open_paren + 1..part.len() - 1].trim();
                let arg_count = if inner.is_empty() {
                    0
                } else {
                    let mut count = 1;
                    let mut depth = 0;
                    for c in inner.chars() {
                        if c == '(' || c == '[' || c == '{' {
                            depth += 1;
                        } else if c == ')' || c == ']' || c == '}' {
                            if depth > 0 { depth -= 1; }
                        } else if c == ',' && depth == 0 {
                            count += 1;
                        }
                    }
                    count
                };
                steps.push(ReceiverStep::MethodCall { name, arg_count });
            } else {
                steps.push(ReceiverStep::Identifier(part.to_string()));
            }
        } else {
            if steps.is_empty() {
                steps.push(ReceiverStep::Identifier(part.to_string()));
            } else {
                steps.push(ReceiverStep::FieldAccess(part.to_string()));
            }
        }
    }

    /// Extract receiver expression and replacement range for IntelliJ-style Postfix Completion
    /// (e.g., `user.getName().sout` -> ("user.getName()", Range replacing `user.getName().sout`))
    pub fn extract_postfix_receiver_and_range(&self, line: u32, character: u32) -> Option<(String, Range)> {
        let line_str = self.source.lines().nth(line as usize)?;
        let byte_limit = self.utf16_col_to_byte(line, character).min(line_str.len());
        let prefix = &line_str[..byte_limit];

        let dot_idx = prefix.rfind('.')?;
        let suffix = &prefix[dot_idx + 1..];
        // Suffix must be a valid identifier or empty
        if !suffix.is_empty() && !suffix.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }

        let before_dot = prefix[..dot_idx].trim_end();
        if before_dot.is_empty() {
            return None;
        }

        let bytes = before_dot.as_bytes();
        let mut i = bytes.len();
        let mut paren_depth: i32 = 0;
        let mut bracket_depth: i32 = 0;

        while i > 0 {
            let b = bytes[i - 1];
            if b == b')' {
                paren_depth += 1;
                i -= 1;
            } else if b == b'(' {
                if paren_depth > 0 {
                    paren_depth -= 1;
                    i -= 1;
                } else {
                    break;
                }
            } else if b == b']' {
                bracket_depth += 1;
                i -= 1;
            } else if b == b'[' {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                    i -= 1;
                } else {
                    break;
                }
            } else if paren_depth == 0 && bracket_depth == 0 {
                if b.is_ascii_whitespace()
                    || b == b';'
                    || b == b','
                    || b == b'='
                    || b == b'{'
                    || b == b'}'
                    || b == b'?'
                    || b == b':'
                    || b == b'+'
                    || b == b'-'
                    || b == b'*'
                    || b == b'/'
                    || b == b'%'
                    || b == b'&'
                    || b == b'|'
                    || b == b'^'
                    || b == b'!'
                    || b == b'<'
                    || b == b'>'
                {
                    break;
                }
                i -= 1;
            } else {
                i -= 1;
            }
        }

        let expr = before_dot[i..].trim();
        if expr.is_empty() {
            return None;
        }

        let expr_start_col = self.byte_col_to_utf16(line, i);
        let remaining = &line_str[byte_limit..];
        let extra_ident_len = remaining
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .map(|c| c.len_utf8())
            .sum::<usize>();
        let end_byte = byte_limit + extra_ident_len;
        let end_col = self.byte_col_to_utf16(line, end_byte);

        Some((
            expr.to_string(),
            Range {
                start: Position { line, character: expr_start_col },
                end: Position { line, character: end_col },
            },
        ))
    }

    /// Check if cursor position is at a method, constructor, or field definition identifier,
    /// returning the exact Range of the definition identifier if so.
    pub fn get_definition_identifier_range(&self, line: u32, character: u32) -> Option<tower_lsp::lsp_types::Range> {
        let root = self.tree.root_node();
        let byte_col = self.utf16_col_to_byte(line, character);
        let point = tree_sitter::Point {
            row: line as usize,
            column: byte_col,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            let k = n.kind();
            if k == "method_declaration" || k == "constructor_declaration" {
                if let Some(name_n) = n.child_by_field_name("name") {
                    let start = name_n.start_position();
                    let end = name_n.end_position();
                    if point.row >= start.row && point.row <= end.row && point.column >= start.column && point.column <= end.column {
                        return Some(tower_lsp::lsp_types::Range {
                            start: tower_lsp::lsp_types::Position {
                                line: start.row as u32,
                                character: self.byte_col_to_utf16(start.row as u32, start.column),
                            },
                            end: tower_lsp::lsp_types::Position {
                                line: end.row as u32,
                                character: self.byte_col_to_utf16(end.row as u32, end.column),
                            },
                        });
                    }
                }
                return None;
            }
            if k == "variable_declarator" {
                if let Some(parent) = n.parent() {
                    if parent.kind() == "field_declaration" {
                        if let Some(name_n) = n.child_by_field_name("name") {
                            let start = name_n.start_position();
                            let end = name_n.end_position();
                            if point.row >= start.row && point.row <= end.row && point.column >= start.column && point.column <= end.column {
                                return Some(tower_lsp::lsp_types::Range {
                                    start: tower_lsp::lsp_types::Position {
                                        line: start.row as u32,
                                        character: self.byte_col_to_utf16(start.row as u32, start.column),
                                    },
                                    end: tower_lsp::lsp_types::Position {
                                        line: end.row as u32,
                                        character: self.byte_col_to_utf16(end.row as u32, end.column),
                                    },
                                });
                            }
                        }
                    }
                }
                return None;
            }
            if k == "class_declaration" || k == "interface_declaration" || k == "enum_declaration" {
                return None;
            }
            curr = n.parent();
        }
        None
    }

    /// Check if cursor position is at a method, constructor, or field definition identifier
    pub fn is_definition_node(&self, line: u32, character: u32) -> bool {
        self.get_definition_identifier_range(line, character).is_some()
    }
}

/// Free function version of find_target_line_in_source
pub fn find_target_line_in_source(content: &str, target_name: &str) -> u32 {
    JavaAst::find_target_line_in_source(content, target_name)
}

/// Check if a string is a valid Java identifier (and not a Java keyword or literal)
pub fn is_valid_java_identifier(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_alphabetic() && first != '_' && first != '$' {
        return false;
    }
    if !chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    !is_java_keyword(s)
}

/// Check if a string is a reserved Java keyword or boolean/null literal
pub fn is_java_keyword(s: &str) -> bool {
    matches!(
        s,
        "abstract"
            | "assert"
            | "boolean"
            | "break"
            | "byte"
            | "case"
            | "catch"
            | "char"
            | "class"
            | "const"
            | "continue"
            | "default"
            | "do"
            | "double"
            | "else"
            | "enum"
            | "extends"
            | "final"
            | "finally"
            | "float"
            | "for"
            | "goto"
            | "if"
            | "implements"
            | "import"
            | "instanceof"
            | "int"
            | "interface"
            | "long"
            | "native"
            | "new"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "return"
            | "short"
            | "static"
            | "strictfp"
            | "super"
            | "switch"
            | "synchronized"
            | "this"
            | "throw"
            | "throws"
            | "transient"
            | "try"
            | "void"
            | "volatile"
            | "while"
            | "true"
            | "false"
            | "null"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ast_parsing() {
        let code = r#"
            package com.example;

            import java.util.List;

            public class UserService extends BaseService implements IService, Comparable<UserService> {
                private String name;

                public String getName(int id, String prefix) {
                    return this.name;
                }
            }
        "#;

        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        let classes = ast.extract_classes(None);
        assert_eq!(classes.len(), 1);
        let user_service = &classes[0];
        assert_eq!(user_service.super_class, Some("BaseService".to_string()));
        assert_eq!(user_service.interfaces, vec!["IService".to_string(), "Comparable".to_string()]);
        assert_eq!(user_service.methods.len(), 1);
        let get_name = &user_service.methods[0];
        assert_eq!(get_name.name, "getName");
        assert_eq!(get_name.parameters.len(), 2);
        assert_eq!(get_name.parameters[0].0, "id");
        assert_eq!(get_name.parameters[1].0, "prefix");

        assert_eq!(ast.extract_package(), Some("com.example".to_string()));
        let imports = ast.extract_imports();
        assert!(imports.contains(&"java.util.List".to_string()));

        let symbols = ast.extract_symbols();
        assert_eq!(symbols.len(), 3); // UserService, name, getName
        assert_eq!(symbols[0].name, "UserService");
    }

    #[test]
    fn test_find_method_call() {
        let code = r#"
            public class Test {
                public void run() {
                    System.out.println("hello", 42);
                    User u = new User("admin", 123);
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        // Row 3, column 40 (inside println arguments)
        let call = ast.find_method_call_at_position(3, 40);
        assert!(call.is_some());
        let (method_name, param_idx, is_constructor) = call.unwrap();
        assert_eq!(method_name, "println");
        assert_eq!(param_idx, 0);
        assert_eq!(is_constructor, false);

        // Row 4, column 48 (inside new User arguments after comma)
        let ctor_call = ast.find_method_call_at_position(4, 48);
        assert!(ctor_call.is_some());
        let (cls_name, ctor_param_idx, is_ctor) = ctor_call.unwrap();
        assert_eq!(cls_name, "User");
        assert_eq!(ctor_param_idx, 1);
        assert_eq!(is_ctor, true);
    }

    #[test]
    fn test_extract_variable_refactoring() {
        let code = r#"
            public class Test {
                public void run() {
                    String str = "hello world";
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        // Select "hello world" expression (row 3, col 33 to 46)
        let refactor = ast.generate_extract_variable_refactoring(3, 33, 3, 46);
        assert!(refactor.is_some());
        let (title, range, def_stmt) = refactor.unwrap();
        assert!(title.contains("extractedVar"));
        assert_eq!(range.start.line, 3);
        assert!(def_stmt.contains("String extractedVar = \"hello world\";"));
    }

    #[test]
    fn test_semantic_diagnostics_type_mismatch() {
        let code = r#"
            public class Test {
                public void run() {
                    int x = "incompatible";
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        let diags = ast.collect_semantic_diagnostics();
        assert_eq!(diags.len(), 2); // 1. Type mismatch, 2. Unused local variable 'x'
        assert!(diags.iter().any(|d| d.1.contains("Type mismatch: cannot convert from string literal to 'int'")));
        assert!(diags.iter().any(|d| d.1.contains("The value of the local variable 'x' is not used")));
    }

    #[test]
    fn test_extract_method_refactoring() {
        let code = r#"
            public class Test {
                public void run() {
                    int a = 1;
                    int b = 2;
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        // Select lines 3 to 4
        let refactor = ast.generate_extract_method_refactoring(3, 20, 4, 30);
        assert!(refactor.is_some());
        let (title, _range, call_stmt, method_def) = refactor.unwrap();
        assert!(title.contains("extractedMethod"));
        assert_eq!(call_stmt, "extractedMethod();");
        assert!(method_def.contains("private void extractedMethod()"));
    }

    #[test]
    fn test_generate_equals_hashcode_tostring() {
        let code = r#"
            public class User {
                private String username;
                private int age;
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        let gens = ast.generate_equals_hashcode_and_tostring();
        assert_eq!(gens.len(), 3);
        assert!(gens[0].1.contains("public boolean equals(Object o)"));
        assert!(gens[1].1.contains("public int hashCode()"));
        assert!(gens[2].1.contains("public String toString()"));
    }

    #[test]
    fn test_inline_variable_refactoring() {
        let code = r#"
            public class Test {
                public void run() {
                    String msg = "hello";
                    System.out.println(msg);
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        // Cursor on 'msg' declarator (row 3, col 28)
        let refactor = ast.generate_inline_variable_refactoring(3, 28);
        assert!(refactor.is_some());
        let (title, _decl_range, occs) = refactor.unwrap();
        assert!(title.contains("Inline local variable 'msg'"));
        assert_eq!(occs.len(), 1);
        assert_eq!(occs[0].1, "\"hello\"");
    }

    #[test]
    fn test_change_signature_refactoring() {
        let code = r#"
            public class Test {
                public void run(int x) {
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        // Cursor on 'run' method (row 2, col 28)
        let refactor = ast.generate_change_signature_refactoring(2, 28, "String", "name");
        assert!(refactor.is_some());
        let (title, _range, new_params) = refactor.unwrap();
        assert!(title.contains("Change signature"));
        assert_eq!(new_params, "(int x, String name)");
    }

    #[test]
    fn test_annotation_declaration_diagnostics() {
        let snippets = [
            "public @interface MyAnnotation {}",
            "@interface Simple {}",
            "@Retention(RetentionPolicy.RUNTIME)\npublic @interface Custom { String value() default \"\"; }",
            "@interface Test { int id(); String[] tags() default {}; }",
            "package a.b;\npublic @interface Meta {\n  Class<?> type() default Void.class;\n}",
        ];

        for (idx, code) in snippets.iter().enumerate() {
            let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
            let errs = ast.collect_syntax_errors();
            assert!(errs.is_empty(), "Snippet {} has syntax errors: {:?}", idx, errs);
            let sem_errs = ast.collect_semantic_diagnostics();
            assert!(sem_errs.is_empty(), "Snippet {} has semantic errors: {:?}", idx, sem_errs);
            let classes = ast.extract_classes(None);
            println!("Snippet {} extracted classes: {:?}", idx, classes.iter().map(|c| (&c.simple_name, &c.fqn, &c.kind)).collect::<Vec<_>>());
            assert!(!classes.is_empty(), "Snippet {} should extract class!", idx);
        }

        let user_code = r#"package com.example;
@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
@Documented
@Controller
@ResponseBody
public @interface RestController {
}
"#;
        let ast = JavaAst::parse(Arc::new(user_code.to_string())).expect("Parse user code");
        let classes = ast.extract_classes(None);
        println!("User RestController extracted classes: {:?}", classes.iter().map(|c| (&c.simple_name, &c.fqn, &c.kind)).collect::<Vec<_>>());
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].simple_name, "RestController");
        assert_eq!(classes[0].fqn, "com.example.RestController");
    }

    #[test]
    fn test_dead_code_and_inlay_hints() {
        let code = r#"
            public class Test {
                public void run() {
                    var x = "hello";
                    var n = 123;
                    return;
                    int unreachable = 999;
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Parse code");

        // Verify Inlay Hints include var type hints
        let hints = ast.find_inlay_hints();
        let var_x_hint = hints.iter().find(|(_, label)| label.contains("String"));
        let var_n_hint = hints.iter().find(|(_, label)| label.contains("int"));
        assert!(var_x_hint.is_some(), "Expected Inlay Hint ': String' for var x");
        assert!(var_n_hint.is_some(), "Expected Inlay Hint ': int' for var n");

        // Verify dead code diagnostic
        let sem_diags = ast.collect_semantic_diagnostics();
        let dead_code = sem_diags.iter().find(|(_, msg)| msg.contains("Unreachable code"));
        assert!(dead_code.is_some(), "Expected Unreachable code diagnostic after return");
    }

    #[test]
    fn test_extract_constant_and_method_stub() {
        let code = r#"
            public class Test {
                public void calc() {
                    System.out.println("Hello World");
                    this.doProcess(123);
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Parse code");

        // 1. Test Extract Constant on string literal "Hello World" (row 3, col 39)
        let constant_refactor = ast.generate_extract_constant_refactoring(3, 39, 3, 52);
        assert!(constant_refactor.is_some(), "Expected Extract Constant refactoring");
        let (title, _, const_name, decl) = constant_refactor.unwrap();
        assert!(title.contains("Extract constant"));
        assert!(decl.contains("public static final String"));
        assert_eq!(const_name, "HELLO_WORLD");

        // 2. Test Create Method Stub on unresolved call `this.doProcess(123)` (row 4, col 25)
        let stub_fix = ast.generate_create_method_stub_quickfix(4, 25);
        assert!(stub_fix.is_some(), "Expected Create Method Stub quickfix");
        let (fix_title, stub_code) = stub_fix.unwrap();
        assert!(fix_title.contains("doProcess"), "Expected title to contain method name, got: {}", fix_title);
        assert!(stub_code.contains("void doProcess()"), "Expected stub to contain method signature, got: {}", stub_code);

        // 3. Test that calling a method on another object (e.g. `b.bMethod()`) does NOT prompt Create method in enclosing class
        let code_b = r#"
            public class ClassA {
                public void test() {
                    ClassB b = new ClassB();
                    b.bMethod();
                }
            }
        "#;
        let ast_b = JavaAst::parse(Arc::new(code_b.to_string())).expect("Parse code");
        let lines_b: Vec<&str> = code_b.lines().collect();
        let b_line = lines_b.iter().position(|l| l.contains("b.bMethod()")).unwrap() as u32;
        let b_col = lines_b[b_line as usize].find("bMethod").unwrap() as u32;
        let no_stub = ast_b.generate_create_method_stub_quickfix(b_line, b_col);
        assert!(no_stub.is_none(), "Calling b.bMethod() should NOT suggest creating method in enclosing class");
    }

    #[test]
    fn test_record_and_switch_exhaustiveness() {
        let code = r#"
            public record Point(int x, int y) {}
            class SwitchTest {
                void check(int v) {
                    var s = switch (v) {
                        case 1 -> "one";
                    };
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Parse code");

        // 1. Verify record extraction
        let classes = ast.extract_classes(None);
        let point_cls = classes.iter().find(|c| c.simple_name == "Point");
        assert!(point_cls.is_some(), "Expected Point record to be extracted");
        let point = point_cls.unwrap();
        assert!(point.methods.iter().any(|m| m.name == "x"), "Expected canonical accessor x() on record");
        assert!(point.methods.iter().any(|m| m.name == "y"), "Expected canonical accessor y() on record");

        // 2. Verify switch expression exhaustiveness diagnostic (missing default branch)
        let diags = ast.collect_semantic_diagnostics();
        let switch_diag = diags.iter().find(|(_, msg)| msg.contains("Switch expression does not cover all possible input values"));
        assert!(switch_diag.is_some(), "Expected missing default branch warning for switch expression");
    }

    #[test]
    fn test_import_insertion_position_and_replacement() {
        // Case 1: File with package and existing imports
        let code1 = "package com.example.demo;\n\nimport java.util.List;\n\npublic class Demo {}";
        let ast1 = JavaAst::parse(Arc::new(code1.to_string())).unwrap();
        let (pos1, need_nl1) = ast1.find_import_insertion_position();
        // Existing import is at line 2 -> new import should be at line 3, never before package (line 0)
        assert!(pos1.line > 0, "Import must never be inserted before package");
        assert_eq!(pos1.line, 3);
        assert!(!need_nl1);

        // Case 2: File with package but NO imports
        let code2 = "package com.example.demo;\n\npublic class Demo {}";
        let ast2 = JavaAst::parse(Arc::new(code2.to_string())).unwrap();
        let (pos2, need_nl2) = ast2.find_import_insertion_position();
        // Must insert after package line (line 0)
        assert_eq!(pos2.line, 1);
        assert!(need_nl2);

        // Case 3: File with no package and no imports
        let code3 = "public class Demo {}";
        let ast3 = JavaAst::parse(Arc::new(code3.to_string())).unwrap();
        let (pos3, _) = ast3.find_import_insertion_position();
        assert_eq!(pos3.line, 0);
    }

    #[test]
    fn test_organize_imports_with_static_and_wildcard() {
        let code = r#"package com.example;

import java.util.List;
import java.util.Map;
import java.io.*;
import static java.util.Collections.emptyList;
import static org.junit.Assert.*;
import com.unused.UnusedClass;

public class Demo {
    private List<String> items;
    public void run() {
        emptyList();
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let organized = ast.organize_imports();
        // 1. Used imports must be present: List, emptyList
        assert!(organized.contains(&"java.util.List".to_string()));
        assert!(organized.contains(&"static java.util.Collections.emptyList".to_string()));
        // 2. Wildcard imports must be preserved: java.io.*, static org.junit.Assert.*
        assert!(organized.contains(&"java.io.*".to_string()));
        assert!(organized.contains(&"static org.junit.Assert.*".to_string()));
        // 3. Unused imports must be eliminated: Map, UnusedClass
        assert!(!organized.contains(&"java.util.Map".to_string()));
        assert!(!organized.contains(&"com.unused.UnusedClass".to_string()));
    }

    #[test]
    fn test_find_unused_imports_and_duplicates() {
        let code = r#"package com.example;

import java.util.List;
import java.util.List;
import java.util.Map;
import com.example.JavadocDoc;

/**
 * See {@link JavadocDoc}
 */
public class Demo {
    private List<String> items;
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let unused = ast.find_unused_imports();
        let unused_names: Vec<_> = unused.iter().map(|(n, _)| n.as_str()).collect();
        // Duplicate List should be flagged as unused
        assert!(unused_names.contains(&"java.util.List"));
        // Unused Map should be flagged as unused
        assert!(unused_names.contains(&"java.util.Map"));
        // JavadocDoc is referenced in Javadoc comment, so it should NOT be flagged as unused
        assert!(!unused_names.contains(&"com.example.JavadocDoc"));
    }

    #[test]
    fn test_is_definition_node_and_range() {
        let code = r#"public class Person {
    private String name;
    public void doWork() {}
    public Person() {}
}"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        // Line 1: '    private String name;' -> 'name' is at col 19-23
        assert!(ast.is_definition_node(1, 19));
        assert!(ast.is_definition_node(1, 22));
        assert!(!ast.is_definition_node(1, 10)); // 'String' type is not definition
        let field_range = ast.get_definition_identifier_range(1, 19).unwrap();
        assert_eq!(field_range.start.line, 1);
        assert_eq!(field_range.start.character, 19);
        assert_eq!(field_range.end.character, 23);

        // Line 2: '    public void doWork() {}' -> 'doWork' is at col 16-22
        assert!(ast.is_definition_node(2, 16));
        assert!(ast.is_definition_node(2, 20));
        assert!(!ast.is_definition_node(2, 8)); // 'void' is not definition

        // Line 3: '    public Person() {}' -> 'Person' constructor is at col 11-17
        assert!(ast.is_definition_node(3, 11));
        assert!(ast.is_definition_node(3, 16));
    }

    #[test]
    fn test_compiler_grade_static_diagnostics() {
        // 1. Missing return statement in non-void method & returning value in void method
        let code1 = r#"
public class TestReturn {
    public int missingReturn() {
        int x = 10;
    }
    public void unexpectedReturn() {
        return 42;
    }
    public int validReturn() {
        if (true) {
            return 1;
        } else {
            return 2;
        }
    }
}
"#;
        let ast1 = JavaAst::parse(Arc::new(code1.to_string())).unwrap();
        let diags1 = ast1.collect_semantic_diagnostics_detailed(Some("TestReturn"));
        println!("DIAGS1 = {:#?}", diags1);
        assert!(
            diags1.iter().any(|d| d.code == DiagnosticCode::MissingReturn && d.message.to_lowercase().contains("missing return statement")),
            "Expected missing return statement diagnostic for missingReturn(), got: {:?}",
            diags1
        );
        assert!(
            diags1.iter().any(|d| d.code == DiagnosticCode::UnexpectedReturn && d.message.to_lowercase().contains("cannot return a value from a method with void")),
            "Expected unexpected return diagnostic for unexpectedReturn(), got: {:?}",
            diags1
        );
        // validReturn should not trigger missing return
        assert!(
            !diags1.iter().any(|d| d.code == DiagnosticCode::MissingReturn && d.range.start.line >= 9),
            "validReturn should not trigger missing return diagnostic"
        );

        // 2. Final variable reassignment
        let code2 = r#"
public class TestFinal {
    public void test() {
        final int x = 100;
        x = 200;
        x++;
    }
}
"#;
        let ast2 = JavaAst::parse(Arc::new(code2.to_string())).unwrap();
        let diags2 = ast2.collect_semantic_diagnostics_detailed(Some("TestFinal"));
        let final_errs: Vec<_> = diags2.iter().filter(|d| d.code == DiagnosticCode::FinalReassignment).collect();
        assert_eq!(final_errs.len(), 2, "Expected 2 final reassignment errors (x = 200 and x++)");

        // 2b. Non-final local variable in method with inner class declaring final field with same name
        let code2b = r#"
public class FilePackager {
    public static void packageFiles() {
        long totalBytes = 0;
        for (FileStatus fs : files) totalBytes += fs.getLen();
    }

    private static class LZWProgress {
        private final long totalBytes;
        LZWProgress(long totalBytes) {
            this.totalBytes = totalBytes;
        }
    }
}
"#;
        let ast2b = JavaAst::parse(Arc::new(code2b.to_string())).unwrap();
        let diags2b = ast2b.collect_semantic_diagnostics_detailed(Some("FilePackager"));
        let final_errs2b: Vec<_> = diags2b.iter().filter(|d| d.code == DiagnosticCode::FinalReassignment).collect();
        assert_eq!(final_errs2b.len(), 0, "Non-final local totalBytes must not produce FinalReassignment error");

        // 3. Duplicate variable declaration in same block
        let code3 = r#"
public class TestDuplicate {
    public void test() {
        int count = 1;
        String count = "dup";
    }
}
"#;
        let ast3 = JavaAst::parse(Arc::new(code3.to_string())).unwrap();
        let diags3 = ast3.collect_semantic_diagnostics_detailed(Some("TestDuplicate"));
        assert!(
            diags3.iter().any(|d| d.code == DiagnosticCode::DuplicateVariable && d.message.to_lowercase().contains("is already defined")),
            "Expected duplicate variable error for count, got: {:?}",
            diags3
        );

        // 4. Static context violation (this/super in static method)
        let code4 = r#"
public class TestStatic {
    private int val;
    public static void run() {
        int x = this.val;
    }
}
"#;
        let ast4 = JavaAst::parse(Arc::new(code4.to_string())).unwrap();
        let diags4 = ast4.collect_semantic_diagnostics_detailed(Some("TestStatic"));
        assert!(
            diags4.iter().any(|d| d.code == DiagnosticCode::StaticContextViolation && d.message.to_lowercase().contains("static context")),
            "Expected static context violation diagnostic, got: {:?}",
            diags4
        );

        // 5. Public class name vs file name mismatch
        let code5 = r#"
public class FooService {
}
"#;
        let ast5 = JavaAst::parse(Arc::new(code5.to_string())).unwrap();
        let diags5 = ast5.collect_semantic_diagnostics_detailed(Some("BarService"));
        assert!(
            diags5.iter().any(|d| d.code == DiagnosticCode::PublicClassNameMismatch && d.message.contains("FooService.java")),
            "Expected class name vs file name mismatch diagnostic, got: {:?}",
            diags5
        );
    }

    #[test]
    fn test_tree_sitter_utf16_and_fault_tolerance() {
        // 1. UTF-16 column vs UTF-8 byte conversion
        // "String greeting = \"" is 19 ASCII chars (19 bytes, 19 UTF-16 units)
        // "你好" is 2 chars, 6 bytes in UTF-8, 2 code units in UTF-16
        let code = "String greeting = \"你好\";";
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        // Index right before "你好": line 0, 19 bytes
        let byte_col = 19;
        let utf16_col = ast.byte_col_to_utf16(0, byte_col);
        assert_eq!(utf16_col, 19);
        // After "你好" (19 + 6 = 25 bytes in UTF-8)
        let byte_after = 25;
        let utf16_after = ast.byte_col_to_utf16(0, byte_after);
        // In UTF-16: 19 + 2 code units = 21
        assert_eq!(utf16_after, 21);
        // Convert back from UTF-16 col 21 -> byte 25
        assert_eq!(ast.utf16_col_to_byte(0, 21), 25);

        // 2. Fault tolerance for incomplete dot access (syntax error when typing)
        let incomplete_code = r#"
public class TestIncomplete {
    public void run() {
        User user = new User();
        user.
    }
}
"#;
        let ast_inc = JavaAst::parse(Arc::new(incomplete_code.to_string())).unwrap();
        // Line 4 is '        user.'
        assert!(ast_inc.is_after_dot_access(4, 13));
        let recv = ast_inc.extract_receiver_before_dot_fallback(4, 13);
        assert_eq!(recv, Some("user".to_string()));
    }

    #[test]
    fn test_instanceof_pattern_ast() {
        let code = r#"
class Test {
    void foo(Object obj) {
        if (obj instanceof Point(int x, int y)) {
            System.out.println(x + y);
        }
        if (obj instanceof String s) {
            System.out.println(s);
        }
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        fn dump_nodes(n: tree_sitter::Node, ast: &JavaAst, depth: usize) {
            let indent = "  ".repeat(depth);
            println!("{}{} [{}]", indent, n.kind(), ast.node_text(&n));
            let mut cursor = n.walk();
            for c in n.children(&mut cursor) {
                dump_nodes(c, ast, depth + 1);
            }
        }
        dump_nodes(ast.tree.root_node(), &ast, 0);
    }

    #[test]
    fn test_annotation_parsing() {
        let code1 = r#"
@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
@Documented
@Controller
@ResponseBody
public @interface RestController {
}
"#;
        let ast1 = JavaAst::parse(Arc::new(code1.to_string())).unwrap();
        let classes1 = ast1.extract_classes(None);
        println!("classes1 = {:?}", classes1);
        assert_eq!(classes1.len(), 1);
        assert_eq!(classes1[0].simple_name, "RestController");
        assert_eq!(classes1[0].kind, ClassKind::Annotation);

        let code2 = r#"
@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
@Documented
@Controller
@ResponseBody
public @interface RestController
"#;
        let ast2 = JavaAst::parse(Arc::new(code2.to_string())).unwrap();
        fn dump(n: tree_sitter::Node, ast: &JavaAst, depth: usize) {
            let indent = "  ".repeat(depth);
            println!("{}{} [{}]", indent, n.kind(), ast.node_text(&n));
            let mut cursor = n.walk();
            for c in n.children(&mut cursor) {
                dump(c, ast, depth + 1);
            }
        }
        dump(ast2.tree.root_node(), &ast2, 0);
        let classes2 = ast2.extract_classes(None);
        println!("classes2 = {:?}", classes2);
        let errors2 = ast2.collect_syntax_errors();
        println!("errors2 = {:?}", errors2);
    }

    #[test]
    fn test_anonymous_class_return_inside_void_method() {
        let code = r#"
class Demo {
    public void deleteDir(Path path) throws IOException {
        Files.walkFileTree(path, new SimpleFileVisitor<Path>() {
            @Override
            public FileVisitResult postVisitDirectory(Path dir, IOException exc) throws IOException {
                Files.delete(dir);
                return FileVisitResult.CONTINUE;
            }
        });
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("Demo"));
        let unexpected_return: Vec<_> = diags.iter().filter(|d| d.code == DiagnosticCode::UnexpectedReturn).collect();
        assert!(unexpected_return.is_empty(), "Should NOT flag return in anonymous class as returning from void method! Found: {:?}", unexpected_return);
    }

    #[test]
    fn test_unhandled_exception_with_throws_declaration() {
        let code = r#"
class Demo {
    public static void copyFile(File src, File dest) throws IOException {
        ensureDirectory(dest.getParent());
        Files.copy(src.toPath(), dest.toPath(), StandardCopyOption.REPLACE_EXISTING);
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("Demo"));
        for d in &diags {
            eprintln!("DIAG: {:?} - {}", d.code, d.message);
        }
        let unhandled: Vec<_> = diags.iter().filter(|d| d.code == DiagnosticCode::UnhandledException).collect();
        assert!(unhandled.is_empty(), "Should NOT flag Files.copy when method declares throws IOException! Found: {:?}", unhandled);
    }

    #[test]
    fn test_throw_runtime_exception_not_flagged() {
        // 1. Unchecked exception (RuntimeException) should not be flagged as unhandled
        let code1 = r#"
class Demo {
    public void validate() {
        StringBuilder sb = new StringBuilder("error");
        throw new RuntimeException(sb.toString());
    }
}
"#;
        let ast1 = JavaAst::parse(Arc::new(code1.to_string())).unwrap();
        let diags1 = ast1.collect_semantic_diagnostics_detailed(Some("Demo"));
        let unhandled1: Vec<_> = diags1.iter().filter(|d| d.code == DiagnosticCode::UnhandledException).collect();
        assert!(unhandled1.is_empty(), "throw new RuntimeException must NOT be flagged as unhandled exception! Found: {:?}", unhandled1);

        // 2. Checked exception without throws must be flagged with exact exception name
        let code2 = r#"
class Demo {
    public void check() {
        throw new IOException("error");
    }
}
"#;
        let ast2 = JavaAst::parse(Arc::new(code2.to_string())).unwrap();
        let diags2 = ast2.collect_semantic_diagnostics_detailed(Some("Demo"));
        let unhandled2: Vec<_> = diags2.iter().filter(|d| d.code == DiagnosticCode::UnhandledException).collect();
        assert_eq!(unhandled2.len(), 1, "throw new IOException without throws must be flagged");
        assert!(unhandled2[0].message.contains("IOException"), "Message must contain IOException, got: {}", unhandled2[0].message);
    }

    #[test]
    fn test_datasource_config_final() {
        let code = r#"
public class DataSourceConfig {
    private final DruidDataSource dataSource;
    private static final String DRIVER;

    static {
        DRIVER = "com.mysql.cj.jdbc.Driver";
    }

    private DataSourceConfig() {
        AppConfiguration config = AppConfiguration.getInstance();
        dataSource = new DruidDataSource();
    }

    public void test() {
        // Assigning final field outside constructor must still be flagged!
        dataSource = null;
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("DataSourceConfig"));
        let final_errs: Vec<_> = diags.iter().filter(|d| d.code == DiagnosticCode::FinalReassignment).collect();
        assert_eq!(final_errs.len(), 1, "Only dataSource = null in test() should be flagged as final reassignment, got: {:?}", final_errs);
        assert_eq!(final_errs[0].range.start.line, 16);
    }

    #[test]
    fn test_void_method_type_node() {
        let code = r#"
class Demo {
    public static void copyFile(File src, File dest) throws IOException {
        Files.copy(src.toPath(), dest.toPath(), StandardCopyOption.REPLACE_EXISTING);
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let method = ast.tree.root_node().named_child(0).unwrap().named_child(1).unwrap().named_child(0).unwrap();
        println!("method kind: {}", method.kind());
        println!("method type field: {:?}", method.child_by_field_name("type"));
        for child in method.children(&mut method.walk()) {
            println!("child kind: {}, text: {:?}", child.kind(), ast.node_text(&child));
        }
    }

    #[test]
    fn test_incremental_parse_desync() {
        let code1 = r#"
class Demo {
    public static void copyFile(File src, File dest) throws IOException {
        ensureDirectory(dest.getParent());
        Files.copy(src.toPath(), dest.toPath(), StandardCopyOption.REPLACE_EXISTING);
    }
}
"#;
        let ast1 = JavaAst::parse(Arc::new(code1.to_string())).unwrap();

        let code2 = r#"
class Demo {
    public static void copyFile(File src, File dest) throws IOException {
        Files.copy(src.toPath(), dest.toPath(), StandardCopyOption.REPLACE_EXISTING);
    }
}
"#;
        let ast2 = JavaAst::parse_incremental(Arc::new(code2.to_string()), Some(&ast1.tree)).unwrap();
        let diags2 = ast2.collect_semantic_diagnostics_detailed(Some("Demo"));
        let missing_ret: Vec<_> = diags2.iter().filter(|d| d.code == DiagnosticCode::MissingReturn).collect();
        assert!(missing_ret.is_empty(), "Should NOT report missing return on void method after incremental edit! Found: {:?}", missing_ret);
    }

    #[test]
    fn test_switch_definitely_returns() {
        let code = r#"
class Demo {
    private static String formatLabel(String compressType) {
        switch (compressType) {
            case "gz":   return "gzip(.gz/.dat.gz)";
            case "Z":    return "compress(.Z/.dat.Z)";
            case "zstd": return "zstd(.zst/.zstd)";
            default:     return compressType;
        }
    }

    private int parseWithTry(String s) {
        try {
            return Integer.parseInt(s);
        } catch (NumberFormatException e) {
            return -1;
        }
    }

    private String arrowSwitch(int x) {
        return switch (x) {
            case 1 -> "one";
            default -> "other";
        };
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();
        let diags = ast.collect_semantic_diagnostics_detailed(Some("Demo"));
        let missing_ret: Vec<_> = diags.iter().filter(|d| d.code == DiagnosticCode::MissingReturn).collect();
        assert!(missing_ret.is_empty(), "Should NOT report missing return when switch or try covers all cases! Found: {:?}", missing_ret);
    }

    #[test]
    fn test_new_refactorings_and_quickfixes() {
        let code = r#"
public class UserService {
    public String role = "ADMIN";

    public void process(java.util.List<String> items) {
        String greeting = (String) "Hello";
        var count = 42;
        for (String item : items) {
            System.out.println(item);
        }
    }
}
"#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).unwrap();

        // 1. Extract Interface
        let iface = ast.generate_extract_interface(1, 14);
        assert!(iface.is_some());
        let (title, _, code_gen) = iface.unwrap();
        assert!(title.contains("IUserService"));
        assert!(code_gen.contains("public interface IUserService"));

        // 2. Encapsulate Field
        let encap = ast.generate_encapsulate_field(2, 18);
        assert!(encap.is_some());
        let (title, _, code_gen) = encap.unwrap();
        assert!(title.contains("role"));
        assert!(code_gen.contains("private String role"));
        assert!(code_gen.contains("getRole()"));
        assert!(code_gen.contains("setRole("));

        // 3. Remove Redundant Cast
        let cast = ast.generate_remove_redundant_cast(5, 27);
        assert!(cast.is_some());
        let (title, _, code_gen) = cast.unwrap();
        assert_eq!(title, "Remove redundant cast");
        assert_eq!(code_gen, "\"Hello\"");

        // 4. Use var / Replace var with explicit type
        let use_var = ast.generate_use_var_refactoring(5, 9);
        assert!(use_var.is_some());
        assert_eq!(use_var.unwrap().2, "var");

        let replace_var = ast.generate_use_var_refactoring(6, 9);
        assert!(replace_var.is_some());
        assert_eq!(replace_var.unwrap().2, "int");

        // 5. Convert for-loop to forEach()
        let for_each = ast.generate_convert_for_to_foreach(7, 9);
        assert!(for_each.is_some());
        let (_, _, code_gen) = for_each.unwrap();
        assert!(code_gen.contains("items.forEach(item ->"));
    }
}



