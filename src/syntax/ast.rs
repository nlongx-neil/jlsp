use std::sync::Arc;
use tree_sitter::{Node, Parser, Tree};
use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo, Position, Range, SymbolDef, SymbolKind, TypeSignature};

pub struct JavaAst {
    pub tree: Tree,
    pub source: Arc<String>,
}

impl JavaAst {
    pub fn parse(source: Arc<String>) -> Option<Self> {
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_java::LANGUAGE.into()).ok()?;
        let tree = parser.parse(source.as_bytes(), None)?;
        Some(Self { tree, source })
    }

    pub fn node_text<'a>(&'a self, node: &Node) -> &'a str {
        let start = node.start_byte();
        let end = node.end_byte();
        &self.source[start..end]
    }

    pub fn node_range(&self, node: &Node) -> Range {
        let start = node.start_position();
        let end = node.end_position();
        Range {
            start: Position {
                line: start.row as u32,
                character: start.column as u32,
            },
            end: Position {
                line: end.row as u32,
                character: end.column as u32,
            },
        }
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

    /// Extract import declarations, e.g. ["java.util.List", "java.util.Map"]
    pub fn extract_imports(&self) -> Vec<String> {
        let root = self.tree.root_node();
        let mut imports = Vec::new();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "import_declaration" {
                for node in child.children(&mut child.walk()) {
                    if node.kind() == "scoped_identifier" || node.kind() == "identifier" {
                        imports.push(self.node_text(&node).to_string());
                    }
                }
            }
        }
        imports
    }

    /// Analyze used identifier tokens and clean up imports:
    /// Returns sorted, unique, and referenced imports
    pub fn organize_imports(&self) -> Vec<String> {
        let current_imports = self.extract_imports();
        let mut used_imports = Vec::new();

        for imp in current_imports {
            let simple_name = imp.split('.').last().unwrap_or(&imp);
            // Check if simple_name appears in file body
            let occs = self.find_all_occurrences(simple_name);
            // If it appears more than once (once is the import statement itself), it's used
            if occs.len() > 1 {
                used_imports.push(imp);
            }
        }

        used_imports.sort();
        used_imports.dedup();
        used_imports
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
                        } else if c_child.kind() == "super_interfaces" {
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
                                        });
                                    }
                                }
                            }
                        }
                    }

                    if let Some(body) = child.child_by_field_name("body") {
                        let mut b_cursor = body.walk();
                        for m_child in body.children(&mut b_cursor) {
                            if m_child.kind() == "method_declaration" {
                                if let Some(m_name_node) = m_child.child_by_field_name("name") {
                                    let m_name = self.node_text(&m_name_node).to_string();
                                    let ret_str = m_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("void");
                                    let ret_type = TypeSignature::Class(ret_str.to_string());

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

                                    let is_static = m_child.children(&mut m_child.walk()).any(|m| self.node_text(&m) == "static");
                                    let is_public = m_child.children(&mut m_child.walk()).any(|m| self.node_text(&m) == "public");
                                    
                                    methods.push(MethodInfo {
                                        name: m_name,
                                        return_type: ret_type,
                                        parameters,
                                        is_static,
                                        is_public,
                                        is_constructor: false,
                                        javadoc: None,
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
                                    });
                                }
                            } else if m_child.kind() == "field_declaration" {
                                let f_type_str = m_child.child_by_field_name("type").map(|t| self.node_text(&t)).unwrap_or("Object");
                                let is_static = m_child.children(&mut m_child.walk()).any(|m| self.node_text(&m) == "static");
                                let is_public = m_child.children(&mut m_child.walk()).any(|m| self.node_text(&m) == "public");
                                let mut f_cursor = m_child.walk();
                                for dec in m_child.children(&mut f_cursor) {
                                    if dec.kind() == "variable_declarator"
                                        && let Some(f_name_node) = dec.child_by_field_name("name") {
                                            fields.push(FieldInfo {
                                                name: self.node_text(&f_name_node).to_string(),
                                                field_type: TypeSignature::Class(f_type_str.to_string()),
                                                is_static,
                                                is_public,
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

    /// Perform fast AST-level semantic checks (Type mismatches, invalid assignments)
    pub fn collect_semantic_diagnostics(&self) -> Vec<(Range, String)> {
        let mut diags = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<(Range, String)>) {
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
                                    out.push((
                                        ast.node_range(&val_node),
                                        format!(
                                            "Type mismatch: cannot convert from {} literal to '{}'",
                                            val_kind.replace("_literal", ""),
                                            declared_type
                                        ),
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            // Check for unused local variables in methods
            if node.kind() == "method_declaration" {
                let mut declared_vars = Vec::new();
                let mut all_identifiers = Vec::new();

                fn find_declarations_and_reads(
                    n: tree_sitter::Node,
                    ast: &JavaAst,
                    decls: &mut Vec<(String, Range)>,
                    reads: &mut Vec<String>,
                ) {
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

                for (var_name, range) in declared_vars {
                    // Count how many times var_name appears in method body (1 = only declaration itself)
                    let count = all_identifiers.iter().filter(|id| *id == &var_name).count();
                    if count <= 1 {
                        out.push((
                            range,
                            format!("The value of the local variable '{}' is not used", var_name),
                        ));
                    }
                }
            }

            // Check for unhandled throw statement outside try-catch
            if node.kind() == "throw_statement" {
                let mut in_try = false;
                let mut p = node.parent();
                while let Some(parent_node) = p {
                    if parent_node.kind() == "try_statement" || parent_node.kind() == "try_with_resources_statement" {
                        in_try = true;
                        break;
                    }
                    if parent_node.kind() == "method_declaration" {
                        // Check if method declares throws
                        if parent_node.child_by_field_name("throws").is_some() {
                            in_try = true;
                        }
                        break;
                    }
                    p = parent_node.parent();
                }

                if !in_try {
                    out.push((
                        ast.node_range(&node),
                        "Unhandled exception: type must be caught or declared to be thrown".to_string(),
                    ));
                }
            }

            // Check for private field/method direct access from other class declarations
            if node.kind() == "field_access" {
                if let Some(field_n) = node.child_by_field_name("field") {
                    let field_name = ast.node_text(&field_n);
                    if field_name.starts_with('_') || field_name.contains("private") {
                        out.push((
                            ast.node_range(&field_n),
                            format!("The field '{}' is not visible", field_name),
                        ));
                    }
                }
            }

            // Check for dead code after return/throw/break statements
            if node.kind() == "block" {
                let mut cursor = node.walk();
                let mut has_terminated = false;
                for child in node.children(&mut cursor) {
                    if has_terminated && child.kind() != "}" {
                        out.push((
                            ast.node_range(&child),
                            "Unreachable code: statement will never be executed".to_string(),
                        ));
                        break;
                    }
                    if child.kind() == "return_statement"
                        || child.kind() == "throw_statement"
                        || child.kind() == "break_statement"
                        || child.kind() == "continue_statement"
                    {
                        has_terminated = true;
                    }
                }
            }

            // Check for explicit null dereference: e.g. null.foo() or Type x = null; x.foo()
            if node.kind() == "method_invocation" {
                if let Some(obj) = node.child_by_field_name("object") {
                    if ast.node_text(&obj) == "null" {
                        out.push((
                            ast.node_range(&node),
                            "Null pointer dereference: invoking method on null literal".to_string(),
                        ));
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
                    if has_cases && !has_default && node.kind() == "switch_expression" {
                        out.push((
                            ast.node_range(&node),
                            "Switch expression does not cover all possible input values and lacks a 'default' branch".to_string(),
                        ));
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
        let mut occurrences = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, ident: &str, out: &mut Vec<Range>) {
            if (node.kind() == "identifier" || node.kind() == "type_identifier")
                && ast.node_text(&node) == ident {
                    out.push(ast.node_range(&node));
                }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                traverse(child, ast, ident, out);
            }
        }

        traverse(root, self, ident, &mut occurrences);
        occurrences
    }

    /// Locate method call context at cursor: returns (method_name, active_parameter_index)
    pub fn find_method_call_at_position(&self, line: u32, character: u32) -> Option<(String, u32)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let mut node = root.descendant_for_point_range(point, point)?;

        // Walk up to find argument_list or method_invocation
        while node.kind() != "method_invocation" && node.kind() != "argument_list" {
            {
                let parent = node.parent()?;
                node = parent;
            }
        }

        let invocation_node = if node.kind() == "argument_list" {
            node.parent()?
        } else {
            node
        };

        if invocation_node.kind() != "method_invocation" {
            return None;
        }

        let name_node = invocation_node.child_by_field_name("name")?;
        let method_name = self.node_text(&name_node).to_string();

        // Calculate active parameter index by counting commas before cursor
        let mut active_param = 0;
        if let Some(args_node) = invocation_node.child_by_field_name("arguments") {
            let _cursor_byte = point.row; // approximate or count siblings
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

        Some((method_name, active_param))
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

    /// Generate QuickFix to create method stub if invoking an unrecognised method
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

    /// Extract Inlay Hints for method arguments & variable type hints in this AST: returns (Position, label)
    pub fn find_inlay_hints(&self) -> Vec<(Position, String)> {
        let mut hints = Vec::new();
        let root = self.tree.root_node();

        fn traverse(node: tree_sitter::Node, ast: &JavaAst, out: &mut Vec<(Position, String)>) {
            // 1. Argument name hints
            if node.kind() == "method_invocation" {
                if let Some(args_node) = node.child_by_field_name("arguments") {
                    let mut arg_idx = 0;
                    let mut cursor = args_node.walk();
                    for child in args_node.children(&mut cursor) {
                        if child.kind() != "(" && child.kind() != ")" && child.kind() != "," {
                            let start = child.start_position();
                            let pos = Position {
                                line: start.row as u32,
                                character: start.column as u32,
                            };
                            out.push((pos, format!("arg{}: ", arg_idx)));
                            arg_idx += 1;
                        }
                    }
                }
            }

            // 2. Type hints for Java 10+ 'var' declarations: e.g. var x = new ArrayList<>(); -> x: ArrayList
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
                                        character: end.column as u32,
                                    };
                                    out.push((pos, format!(": {}", inferred)));
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

        traverse(root, self, &mut hints);
        hints
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

        let method_node = match enclosing_method {
            Some(m) => m,
            None => return vars,
        };

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
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_locals(child, ast, target_point, out);
            }
        }

        if let Some(body_node) = method_node.child_by_field_name("body") {
            collect_locals(body_node, self, point, &mut vars);
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
        None
    }

    /// If cursor is at or on a method invocation's name, return (receiver_object_text, method_name)
    /// receiver_object_text is None for implicit 'this' calls like `foo()`
    pub fn find_method_call_receiver(&self, line: u32, character: u32) -> Option<(Option<String>, String)> {
        let root = self.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        let mut curr = root.descendant_for_point_range(point, point);
        while let Some(n) = curr {
            if n.kind() == "method_invocation" {
                if let Some(name_n) = n.child_by_field_name("name") {
                    let m_name = self.node_text(&name_n).to_string();
                    let recv = n.child_by_field_name("object").map(|obj| self.node_text(&obj).to_string());
                    return Some((recv, m_name));
                }
            }
            curr = n.parent();
        }
        None
    }
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
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Should parse");
        // Row 3, column 40 (inside println arguments)
        let call = ast.find_method_call_at_position(3, 40);
        assert!(call.is_some());
        let (method_name, param_idx) = call.unwrap();
        assert_eq!(method_name, "println");
        assert_eq!(param_idx, 0);
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
            println!("Snippet {} parsed with 0 errors.", idx);
        }
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
}



