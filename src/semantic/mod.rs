pub mod lombok;
pub mod apt_jdwp;

use std::collections::HashMap;
use std::sync::Arc;
use tree_sitter::Node;

use crate::classpath::ClasspathRegistry;
use crate::model::{ClassInfo, ClassKind, MethodInfo, Range};
use crate::syntax::JavaAst;

pub struct SemanticContext<'a> {
    pub ast: &'a JavaAst,
    pub classpath: &'a ClasspathRegistry,
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub local_classes: HashMap<String, ClassInfo>,
    pub indexer: Option<&'a crate::indexer::WorkspaceIndex>,
}

#[derive(Debug, Clone)]
pub enum GotoTarget {
    /// Local definition in current file at specific range
    Local(Range),
    /// External class (from JDK, library JAR, or classpath) with optional member/target name
    External {
        cls: Arc<ClassInfo>,
        target_name: Option<String>,
    },
    /// Known workspace source file on disk
    SourceFile {
        path: String,
        range: Range,
    },
}

impl<'a> SemanticContext<'a> {
    pub async fn new(ast: &'a JavaAst, classpath: &'a ClasspathRegistry) -> Self {
        Self::with_indexer(ast, classpath, None).await
    }

    pub async fn with_indexer(
        ast: &'a JavaAst,
        classpath: &'a ClasspathRegistry,
        indexer: Option<&'a crate::indexer::WorkspaceIndex>,
    ) -> Self {
        let package = ast.extract_package();
        let imports = ast.extract_imports();
        let classes = ast.extract_classes(None);

        let mut local_classes = HashMap::new();
        for mut cls in classes {
            // Apply Lombok synthetic methods if present
            let lombok_generated = lombok::process_lombok_annotations(&mut cls, &ast.source);
            let apt_generated = apt_jdwp::AnnotationProcessorRunner::process_apt_annotations(&mut cls, &ast.source);
            local_classes.insert(cls.simple_name.clone(), cls.clone());
            local_classes.insert(cls.fqn.clone(), cls);
            for gen_cls in apt_generated {
                local_classes.insert(gen_cls.simple_name.clone(), gen_cls.clone());
                local_classes.insert(gen_cls.fqn.clone(), gen_cls);
            }
            for gen_cls in lombok_generated {
                local_classes.insert(gen_cls.simple_name.clone(), gen_cls.clone());
                local_classes.insert(gen_cls.fqn.clone(), gen_cls);
            }
        }

        Self {
            ast,
            classpath,
            indexer,
            package,
            imports,
            local_classes,
        }
    }

    /// Resolve simple type name (e.g. "String" or "List") into full ClassInfo
    pub async fn resolve_type(&self, raw_type_name: &str) -> Option<Arc<ClassInfo>> {
        let type_name = raw_type_name.trim();
        // 1. Primitive check
        if matches!(type_name, "int" | "long" | "boolean" | "byte" | "short" | "char" | "float" | "double" | "void") {
            return None;
        }

        // Extract simple name for fallback (handles "pkg.Foo" as well as syntax recovery "Foo.\n Foo")
        let simple_name = type_name.split_whitespace().last().unwrap_or(type_name).split('.').last().unwrap_or(type_name);
        if !crate::syntax::is_valid_java_identifier(simple_name) {
            return None;
        }

        // 2. Check local classes in current file
        if let Some(cls) = self.local_classes.get(type_name).or_else(|| self.local_classes.get(simple_name)) {
            return Some(Arc::new(cls.clone()));
        }

        // 2.5 Check exact FQN in classpath
        if type_name.contains('.') {
            if let Some(cls) = self.classpath.get_class(type_name).await {
                return Some(cls);
            }
            if let Some(idx) = self.indexer {
                if let Some(sym) = idx.find_by_fqn(type_name).await {
                    return Some(Arc::new(sym.class_info));
                }
            }
        }

        // 3. Check explicit imports (e.g., import java.util.List;)
        for imp in &self.imports {
            if imp.ends_with(&format!(".{simple_name}")) || imp == simple_name {
                if let Some(cls) = self.classpath.get_class(imp).await {
                    return Some(cls);
                }
                if let Some(idx) = self.indexer {
                    if let Some(sym) = idx.find_by_fqn(imp).await {
                        return Some(Arc::new(sym.class_info));
                    }
                }
                if crate::classpath::is_standard_jdk_package(imp) {
                    if let Some(cls) = crate::classpath::get_builtin_jdk_class(imp) {
                        return Some(Arc::new(cls));
                    }
                }
                if let Some(cls) = crate::classpath::get_builtin_framework_class(imp) {
                    return Some(Arc::new(cls));
                }
                if imp.contains('.') {
                    let s_name = imp.rsplit('.').next().unwrap_or(imp);
                    if s_name.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                        let pkg = imp.rsplitn(2, '.').nth(1).unwrap_or("");
                        let info = ClassInfo {
                            fqn: imp.clone(),
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
                        return Some(Arc::new(info));
                    }
                }
            }
        }

        // 3.5 Check wildcard imports (e.g., import java.util.concurrent.*;)
        for imp in &self.imports {
            if imp.ends_with(".*") {
                let pkg = imp.trim_end_matches(".*");
                let candidate_fqn = format!("{pkg}.{simple_name}");
                if let Some(cls) = self.classpath.get_class(&candidate_fqn).await {
                    return Some(cls);
                }
                if let Some(idx) = self.indexer {
                    if let Some(sym) = idx.find_by_fqn(&candidate_fqn).await {
                        return Some(Arc::new(sym.class_info));
                    }
                }
                if crate::classpath::is_standard_jdk_package(&candidate_fqn) {
                    if let Some(cls) = crate::classpath::get_builtin_jdk_class(&candidate_fqn) {
                        return Some(Arc::new(cls));
                    }
                }
            }
        }

        // 4. Check same package
        if let Some(pkg) = &self.package {
            let fqn = format!("{pkg}.{simple_name}");
            if let Some(cls) = self.classpath.get_class(&fqn).await {
                return Some(cls);
            }
            if let Some(idx) = self.indexer {
                if let Some(sym) = idx.find_by_fqn(&fqn).await {
                    return Some(Arc::new(sym.class_info));
                }
            }
        }

        // 5. Check java.lang default package
        let java_lang_fqn = format!("java.lang.{simple_name}");
        if let Some(cls) = self.classpath.get_class(&java_lang_fqn).await {
            return Some(cls);
        }
        if let Some(cls) = crate::classpath::get_builtin_jdk_class(&java_lang_fqn) {
            return Some(Arc::new(cls));
        }

        // 6. Search registry by simple name
        let found = self.classpath.find_classes_by_simple_name(simple_name).await;
        if let Some(first) = found.into_iter().next() {
            return Some(first);
        }

        // 7. Search workspace index candidates by simple name and package context
        if let Some(idx) = self.indexer {
            if let Some(sym) = idx.find_candidate(simple_name, self.package.as_deref(), &self.imports).await {
                return Some(Arc::new(sym.class_info));
            }
        }

        // 8. Fallback to common JDK classes if still not found
        let common = crate::classpath::find_common_jdk_classes_by_name(simple_name);
        for fqn in common {
            if let Some(cls) = crate::classpath::get_builtin_jdk_class(&fqn) {
                return Some(Arc::new(cls));
            }
        }
        if let Some(cls) = crate::classpath::get_builtin_jdk_class_by_simple_name(simple_name) {
            return Some(Arc::new(cls));
        }
        if let Some(cls) = crate::classpath::get_builtin_framework_class_by_simple_name(simple_name) {
            return Some(Arc::new(cls));
        }

        None
    }

    /// Search for method signatures by name across classpath and local classes
    pub async fn find_methods_by_name(&self, method_name: &str) -> Vec<MethodInfo> {
        let mut results = Vec::new();

        // 1. Search in local classes
        for cls in self.local_classes.values() {
            for m in &cls.methods {
                if m.name == method_name {
                    results.push(m.clone());
                }
            }
        }

        // 2. Search in classpath
        for cls in self.classpath.all_classes().await {
            for m in &cls.methods {
                if m.name == method_name {
                    results.push(m.clone());
                }
            }
        }

        results
    }

    /// Recursively collect all methods for a given class including superclasses and interfaces
    pub async fn get_all_methods_with_inheritance(&self, cls: &ClassInfo) -> Vec<crate::model::MethodInfo> {
        let mut methods = cls.methods.clone();
        let mut visited = std::collections::HashSet::new();
        visited.insert(cls.fqn.clone());

        let mut queue = std::collections::VecDeque::new();
        if let Some(super_cls) = &cls.super_class {
            queue.push_back(super_cls.clone());
        }
        for iface in &cls.interfaces {
            queue.push_back(iface.clone());
        }

        while let Some(parent_name) = queue.pop_front() {
            if !visited.insert(parent_name.clone()) {
                continue;
            }

            if let Some(parent_info) = self.resolve_type(&parent_name).await {
                methods.extend(parent_info.methods.clone());
                if let Some(s) = &parent_info.super_class {
                    queue.push_back(s.clone());
                }
                for iface in &parent_info.interfaces {
                    queue.push_back(iface.clone());
                }
            }
        }

        // Deduplicate methods by name and signature
        let mut unique = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for m in methods {
            let sig = format!("{}:{:?}", m.name, m.parameters);
            if seen.insert(sig) {
                unique.push(m);
            }
        }

        unique
    }

    /// Recursively collect all fields for a given class including superclasses
    pub async fn get_all_fields_with_inheritance(&self, cls: &ClassInfo) -> Vec<crate::model::FieldInfo> {
        let mut fields = cls.fields.clone();
        let mut visited = std::collections::HashSet::new();
        visited.insert(cls.fqn.clone());

        let mut curr_super = cls.super_class.clone();
        while let Some(super_name) = curr_super {
            if !visited.insert(super_name.clone()) {
                break;
            }
            if let Some(super_info) = self.resolve_type(&super_name).await {
                fields.extend(super_info.fields.clone());
                curr_super = super_info.super_class.clone();
            } else {
                break;
            }
        }

        // Deduplicate fields by name
        let mut unique = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for f in fields {
            if seen.insert(f.name.clone()) {
                unique.push(f);
            }
        }
        unique
    }

    /// Find implementations/subclasses of a given class or interface
    pub async fn find_implementations(&self, target_name: &str) -> Vec<ClassInfo> {
        let mut impls = Vec::new();

        // 1. Check local classes
        for cls in self.local_classes.values() {
            if cls.super_class.as_deref() == Some(target_name)
                || cls.interfaces.iter().any(|i| i == target_name || i.ends_with(&format!(".{target_name}")))
            {
                impls.push(cls.clone());
            }
        }

        // 2. Check classpath
        for cls in self.classpath.all_classes().await {
            if cls.super_class.as_deref() == Some(target_name)
                || cls.interfaces.iter().any(|i| i == target_name || i.ends_with(&format!(".{target_name}")))
            {
                impls.push((*cls).clone());
            }
        }

        impls
    }

    /// Find missing interface/abstract methods that need to be implemented for local classes
    pub async fn find_missing_abstract_methods(&self) -> Vec<(String, Vec<crate::model::MethodInfo>)> {
        let mut missing_by_class = Vec::new();
        let mut seen_fqns = std::collections::HashSet::new();

        let is_object_method = |name: &str| -> bool {
            matches!(
                name,
                "<init>"
                    | "<clinit>"
                    | "registerNatives"
                    | "getClass"
                    | "hashCode"
                    | "equals"
                    | "clone"
                    | "toString"
                    | "notify"
                    | "notifyAll"
                    | "wait"
                    | "finalize"
            )
        };

        for cls in self.local_classes.values() {
            // Deduplicate local classes (since local_classes maps both simple_name and fqn)
            if !seen_fqns.insert(cls.fqn.clone()) {
                continue;
            }

            // Interfaces, annotations, and enums don't need to implement abstract methods
            if cls.kind == ClassKind::Interface || cls.kind == ClassKind::Annotation || cls.kind == ClassKind::Enum {
                continue;
            }

            // Skip classes explicitly declared abstract in source AST (including nested/inner classes)
            fn is_class_abstract(node: tree_sitter::Node, ast: &crate::syntax::JavaAst, target: &str) -> bool {
                if matches!(node.kind(), "class_declaration" | "record_declaration" | "interface_declaration" | "enum_declaration") {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        if ast.node_text(&name_node) == target {
                            return node.child_by_field_name("modifiers")
                                .map(|m| ast.node_text(&m).contains("abstract"))
                                .unwrap_or(false);
                        }
                    }
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if is_class_abstract(child, ast, target) {
                        return true;
                    }
                }
                false
            }
            if is_class_abstract(self.ast.tree.root_node(), self.ast, &cls.simple_name) {
                continue;
            }

            // If super class is external and unresolved, avoid false positives on interface methods
            // that the unknown superclass may already implement
            let mut super_class_unresolved = false;
            if let Some(super_name) = &cls.super_class {
                if super_name != "java.lang.Object" && super_name != "java/lang/Object" && super_name != "Object" {
                    if self.resolve_type(super_name).await.is_none() {
                        super_class_unresolved = true;
                    }
                }
            }
            if super_class_unresolved {
                continue;
            }

            let mut required_methods = Vec::new();

            for iface in &cls.interfaces {
                if let Some(iface_info) = self.resolve_type(iface).await {
                    let inherited = self.get_all_methods_with_inheritance(&iface_info).await;
                    for m in inherited {
                        // Skip static methods, default methods (with body), constructors, and Object methods
                        if m.is_static || m.is_constructor || is_object_method(&m.name) || m.has_body() {
                            continue;
                        }
                        required_methods.push(m);
                    }
                }
            }

            if let Some(super_name) = &cls.super_class {
                if super_name != "java.lang.Object" && super_name != "java/lang/Object" && super_name != "Object" {
                    if let Some(super_info) = self.resolve_type(super_name).await {
                        // Only abstract methods without bodies in abstract superclasses are required
                        for m in &super_info.methods {
                            if !m.is_static && !m.is_constructor && !is_object_method(&m.name) && !m.has_body() {
                                required_methods.push(m.clone());
                            }
                        }
                    }
                }
            }

            // Collect all available methods from cls and its superclass hierarchy
            let mut available_methods = cls.methods.clone();
            let mut curr_super = cls.super_class.clone();
            while let Some(super_name) = curr_super {
                if super_name == "java.lang.Object" || super_name == "java/lang/Object" || super_name == "Object" {
                    break;
                }
                if let Some(super_info) = self.resolve_type(&super_name).await {
                    for m in &super_info.methods {
                        if !m.is_static && !m.is_constructor && m.has_body() {
                            available_methods.push(m.clone());
                        }
                    }
                    curr_super = super_info.super_class.clone();
                } else {
                    break;
                }
            }

            // Filter out methods already implemented in cls or superclasses (comparing name and parameter count)
            let mut missing = Vec::new();
            for req in required_methods {
                if !available_methods.iter().any(|cm| cm.name == req.name && cm.parameters.len() == req.parameters.len()) {
                    if !missing.iter().any(|m: &crate::model::MethodInfo| m.name == req.name && m.parameters.len() == req.parameters.len()) {
                        missing.push(req);
                    }
                }
            }

            if !missing.is_empty() {
                missing_by_class.push((cls.simple_name.clone(), missing));
            }
        }

        missing_by_class
    }

    /// Find AST node at specific line and column (aligned with UTF-16)
    pub fn find_node_at_position(&self, line: u32, character: u32) -> Option<Node<'_>> {
        self.ast.find_node_with_recovery(line, character)
    }

    /// Resolve semantic definition target (Local, SourceFile, or External JDK/Jar Class)
    pub async fn resolve_goto_target(&self, line: u32, character: u32) -> Option<GotoTarget> {
        let node = self.find_node_at_position(line, character)?;
        let ident_text = self.ast.node_text(&node);

        // 0. Check if cursor is inside an import declaration
        let mut is_import = false;
        let mut curr = Some(node);
        while let Some(n) = curr {
            if n.kind() == "import_declaration" {
                is_import = true;
                break;
            }
            curr = n.parent();
        }

        if is_import {
            // Check direct type resolution on the identifier under cursor
            if let Some(cls) = self.resolve_type(ident_text).await {
                if self.local_classes.contains_key(&cls.simple_name) {
                    if let Some(sym) = self.ast.extract_symbols().into_iter().find(|s| s.name == cls.simple_name && s.kind == crate::model::SymbolKind::Class) {
                        return Some(GotoTarget::Local(sym.selection_range));
                    }
                }
                if let Some(ref sp) = cls.source_path {
                    if sp.ends_with(".java") && std::path::Path::new(sp).exists() {
                        let mut line = 0;
                        if let Ok(content) = std::fs::read_to_string(sp) {
                            line = crate::syntax::find_target_line_in_source(&content, &cls.simple_name);
                        }
                        return Some(GotoTarget::SourceFile {
                            path: sp.clone(),
                            range: Range {
                                start: crate::model::Position { line, character: 0 },
                                end: crate::model::Position { line, character: cls.simple_name.len() as u32 },
                            },
                        });
                    }
                }
                return Some(GotoTarget::External {
                    cls,
                    target_name: Some(ident_text.to_string()),
                });
            }

            // Check matching import statement on this line
            for (imp_range, imp_str) in self.ast.extract_imports_with_ranges() {
                if imp_range.start.line <= line && line <= imp_range.end.line {
                    let is_static = imp_str.starts_with("static ");
                    let clean = imp_str.trim_start_matches("static ").trim_end_matches(".*").trim();
                    let (cls_lookup, member_target) = if is_static {
                        let parts: Vec<&str> = clean.rsplitn(2, '.').collect();
                        if parts.len() == 2 {
                            (parts[1], Some(parts[0].to_string()))
                        } else {
                            (clean, None)
                        }
                    } else {
                        (clean, None)
                    };
                    if let Some(cls) = self.classpath.get_class(cls_lookup).await {
                        return Some(GotoTarget::External {
                            cls,
                            target_name: member_target,
                        });
                    }
                    if let Some(cls) = self.resolve_type(cls_lookup).await {
                        if let Some(ref sp) = cls.source_path {
                            if sp.ends_with(".java") && std::path::Path::new(sp).exists() {
                                let target_sym = member_target.as_deref().unwrap_or(&cls.simple_name);
                                let mut line = 0;
                                if let Ok(content) = std::fs::read_to_string(sp) {
                                    line = crate::syntax::find_target_line_in_source(&content, target_sym);
                                }
                                return Some(GotoTarget::SourceFile {
                                    path: sp.clone(),
                                    range: Range {
                                        start: crate::model::Position { line, character: 0 },
                                        end: crate::model::Position { line, character: target_sym.len() as u32 },
                                    },
                                });
                            }
                        }
                        return Some(GotoTarget::External {
                            cls,
                            target_name: member_target,
                        });
                    }
                }
            }
        }

        // 1. Check if cursor is at a method invocation (e.g. `obj.process()` or `process()`)
        if let Some((recv_opt, m_name)) = self.ast.find_method_call_receiver(line, character) {
            if m_name == ident_text {
                // Determine target class for receiver
                let mut target_class_name: Option<String> = None;

                if let Some(recv) = recv_opt {
                    if recv == "this" {
                        target_class_name = self.ast.find_enclosing_class_at_position(line, character);
                    } else if recv == "super" {
                        let enc = self.ast.find_enclosing_class_at_position(line, character);
                        target_class_name = enc.and_then(|e| self.local_classes.get(&e).and_then(|c| c.super_class.clone()));
                    } else {
                        // Check local variables / parameters
                        let locals = self.ast.extract_local_variables_at_position(line, character);
                        for (v_name, v_type) in &locals {
                            if v_name == &recv {
                                let clean = v_type.split('<').next().unwrap_or(v_type).trim();
                                target_class_name = Some(clean.to_string());
                                break;
                            }
                        }

                        // Check fields of enclosing class
                        if target_class_name.is_none() {
                            let enclosing = self.ast.find_enclosing_class_at_position(line, character);
                            if let Some(enc_name) = enclosing {
                                if let Some(enc_cls) = self.local_classes.get(&enc_name) {
                                    for f in &enc_cls.fields {
                                        if f.name == recv {
                                            let f_ty = f.field_type.simple_name();
                                            let clean = f_ty.split('<').next().unwrap_or(&f_ty).trim();
                                            target_class_name = Some(clean.to_string());
                                            break;
                                        }
                                    }
                                }
                            }
                        }

                        // Check if receiver is a class name (static method call)
                        if target_class_name.is_none() {
                            if self.local_classes.contains_key(&recv) || self.resolve_type(&recv).await.is_some() {
                                target_class_name = Some(recv);
                            }
                        }
                    }
                } else {
                    // Implicit this method call: target is enclosing class
                    target_class_name = self.ast.find_enclosing_class_at_position(line, character);
                }

                if let Some(target_type) = target_class_name {
                    // Try to resolve target type
                    if let Some(cls_info) = self.resolve_type(&target_type).await {
                        // Check if target class is defined in the current file
                        if let Some(local_cls) = self.local_classes.get(&cls_info.simple_name) {
                            for sym in self.ast.extract_symbols() {
                                if sym.name == m_name
                                    && sym.kind == crate::model::SymbolKind::Method
                                    && sym.container_name.as_deref() == Some(&local_cls.simple_name)
                                {
                                    return Some(GotoTarget::Local(sym.selection_range));
                                }
                            }

                            // Check Lombok synthetic getter/setter on local class
                            if (m_name.starts_with("get") || m_name.starts_with("set")) && m_name.len() > 3 {
                                let field_cand = format!("{}{}", m_name[3..4].to_lowercase(), &m_name[4..]);
                                for sym in self.ast.extract_symbols() {
                                    if sym.name == field_cand && sym.container_name.as_deref() == Some(&local_cls.simple_name) {
                                        return Some(GotoTarget::Local(sym.selection_range));
                                    }
                                }
                            } else if m_name.starts_with("is") && m_name.len() > 2 {
                                let field_cand = format!("{}{}", m_name[2..3].to_lowercase(), &m_name[3..]);
                                for sym in self.ast.extract_symbols() {
                                    if sym.name == field_cand && sym.container_name.as_deref() == Some(&local_cls.simple_name) {
                                        return Some(GotoTarget::Local(sym.selection_range));
                                    }
                                }
                            }

                            // If not found directly in local class, search inherited methods from superclasses/interfaces
                            let mut queue = Vec::new();
                            if let Some(ref sc) = local_cls.super_class {
                                queue.push(sc.clone());
                            }
                            queue.extend(local_cls.interfaces.clone());

                            let mut visited = std::collections::HashSet::new();
                            while let Some(ancestor_name) = queue.pop() {
                                if !visited.insert(ancestor_name.clone()) {
                                    continue;
                                }
                                if let Some(ancestor_info) = self.resolve_type(&ancestor_name).await {
                                    if ancestor_info.methods.iter().any(|m| m.name == m_name) {
                                        if let Some(ref sp) = ancestor_info.source_path {
                                            if sp.ends_with(".java") && std::path::Path::new(sp).exists() {
                                                let mut line = 0;
                                                if let Ok(content) = std::fs::read_to_string(sp) {
                                                    line = crate::syntax::find_target_line_in_source(&content, &m_name);
                                                }
                                                return Some(GotoTarget::SourceFile {
                                                    path: sp.clone(),
                                                    range: Range {
                                                        start: crate::model::Position { line, character: 0 },
                                                        end: crate::model::Position { line, character: m_name.len() as u32 },
                                                    },
                                                });
                                            }
                                        }
                                        return Some(GotoTarget::External {
                                            cls: ancestor_info,
                                            target_name: Some(m_name.clone()),
                                        });
                                    }
                                    if let Some(ref sc) = ancestor_info.super_class {
                                        queue.push(sc.clone());
                                    }
                                    queue.extend(ancestor_info.interfaces.clone());
                                }
                            }
                        } else {
                            // If defined in a project workspace source file
                            if let Some(ref sp) = cls_info.source_path {
                                if sp.ends_with(".java") && std::path::Path::new(sp).exists() {
                                    let mut line = 0;
                                    if let Ok(content) = std::fs::read_to_string(sp) {
                                        line = crate::syntax::find_target_line_in_source(&content, &m_name);
                                    }
                                    return Some(GotoTarget::SourceFile {
                                        path: sp.clone(),
                                        range: Range {
                                            start: crate::model::Position { line, character: 0 },
                                            end: crate::model::Position { line, character: m_name.len() as u32 },
                                        },
                                    });
                                }
                            }

                            // External class (JDK or JAR library)
                            return Some(GotoTarget::External {
                                cls: cls_info,
                                target_name: Some(m_name),
                            });
                        }
                    }
                }
            }
        }

        // 2. Check if cursor is on a class/type identifier
        if let Some(cls) = self.resolve_type(ident_text).await {
            // Defined in current local file?
            if let Some(local_cls) = self.local_classes.get(&cls.simple_name) {
                for sym in self.ast.extract_symbols() {
                    if sym.name == local_cls.simple_name && sym.kind == crate::model::SymbolKind::Class {
                        return Some(GotoTarget::Local(sym.selection_range));
                    }
                }
            }

            // Defined in a project workspace source file?
            if let Some(ref sp) = cls.source_path {
                if sp.ends_with(".java") && std::path::Path::new(sp).exists() {
                    let mut line = 0;
                    if let Ok(content) = std::fs::read_to_string(sp) {
                        line = crate::syntax::find_target_line_in_source(&content, &cls.simple_name);
                    }
                    return Some(GotoTarget::SourceFile {
                        path: sp.clone(),
                        range: Range {
                            start: crate::model::Position { line, character: 0 },
                            end: crate::model::Position { line, character: cls.simple_name.len() as u32 },
                        },
                    });
                }
            }

            // External class (JDK or JAR library)
            return Some(GotoTarget::External {
                cls,
                target_name: None,
            });
        }

        // 3. Check if cursor is on a local symbol definition (method, field, etc.)
        let enclosing_class = self.ast.find_enclosing_class_at_position(line, character);
        let symbols = self.ast.extract_symbols();

        if let Some(enc) = &enclosing_class {
            for sym in &symbols {
                if sym.name == ident_text && sym.container_name.as_ref() == Some(enc) {
                    return Some(GotoTarget::Local(sym.selection_range.clone()));
                }
            }
        }

        for sym in &symbols {
            if sym.name == ident_text {
                return Some(GotoTarget::Local(sym.selection_range.clone()));
            }
        }

        // 4. Check if cursor is on a synthetic Lombok getter/setter (e.g. getName / setName) -> jump to field 'name'
        if (ident_text.starts_with("get") || ident_text.starts_with("set")) && ident_text.len() > 3 {
            let field_cand = format!("{}{}", ident_text[3..4].to_lowercase(), &ident_text[4..]);
            if let Some(enc) = &enclosing_class {
                for sym in &symbols {
                    if sym.name == field_cand && sym.container_name.as_ref() == Some(enc) {
                        return Some(GotoTarget::Local(sym.selection_range.clone()));
                    }
                }
            }
            for sym in &symbols {
                if sym.name == field_cand {
                    return Some(GotoTarget::Local(sym.selection_range.clone()));
                }
            }
        } else if ident_text.starts_with("is") && ident_text.len() > 2 {
            let field_cand = format!("{}{}", ident_text[2..3].to_lowercase(), &ident_text[3..]);
            if let Some(enc) = &enclosing_class {
                for sym in &symbols {
                    if sym.name == field_cand && sym.container_name.as_ref() == Some(enc) {
                        return Some(GotoTarget::Local(sym.selection_range.clone()));
                    }
                }
            }
            for sym in &symbols {
                if sym.name == field_cand {
                    return Some(GotoTarget::Local(sym.selection_range.clone()));
                }
            }
        }

        None
    }

    /// Resolve definition at given cursor location
    pub async fn goto_definition(&self, line: u32, character: u32) -> Option<(String, Range)> {
        match self.resolve_goto_target(line, character).await? {
            GotoTarget::Local(r) => Some((String::new(), r)),
            GotoTarget::SourceFile { path, range } => Some((path, range)),
            GotoTarget::External { cls, .. } => {
                let p = cls.source_path.clone().unwrap_or_default();
                Some((p, Range::default()))
            }
        }
    }

    /// Resolve type definition for a variable/field/expression at cursor location
    pub async fn goto_type_definition(&self, line: u32, character: u32) -> Option<(String, Range)> {
        let node = self.find_node_at_position(line, character)?;
        let ident_text = self.ast.node_text(&node);

        // 1. If cursor is already on a class/type name, jump to that type
        if let Some(cls) = self.resolve_type(ident_text).await {
            let path = cls.source_path.clone().unwrap_or_default();
            let range = Range {
                start: crate::model::Position { line: 0, character: 0 },
                end: crate::model::Position { line: 0, character: 0 },
            };
            return Some((path, range));
        }

        // 2. If cursor is on a local variable or parameter, look up its declared type
        let local_vars = self.ast.extract_local_variables_at_position(line, character);
        for (v_name, v_type) in &local_vars {
            if v_name == ident_text {
                let clean_type = v_type.split('<').next().unwrap_or(v_type).trim();
                if let Some(target_cls) = self.resolve_type(clean_type).await {
                    let path = target_cls.source_path.clone().unwrap_or_default();
                    let range = Range {
                        start: crate::model::Position { line: 0, character: 0 },
                        end: crate::model::Position { line: 0, character: 0 },
                    };
                    return Some((path, range));
                }
            }
        }

        // 3. If cursor is on a field of enclosing or local class, look up its type
        for cls in self.local_classes.values() {
            for f in &cls.fields {
                if f.name == ident_text {
                    let type_str = f.field_type.simple_name();
                    let clean_type = type_str.split('<').next().unwrap_or(&type_str).trim();
                    if let Some(target_cls) = self.resolve_type(clean_type).await {
                        let path = target_cls.source_path.clone().unwrap_or_default();
                        let range = Range {
                            start: crate::model::Position { line: 0, character: 0 },
                            end: crate::model::Position { line: 0, character: 0 },
                        };
                        return Some((path, range));
                    }
                }
            }
        }

        None
    }

    pub async fn populate_member_completions(
        &self,
        target_cls: &ClassInfo,
        type_arg: Option<&String>,
        is_static_access: bool,
        line: u32,
        character: u32,
        items: &mut Vec<CompletionItem>,
    ) {
        let current_enclosing = self.ast.find_enclosing_class_at_position(line, character);
        let is_same_class = current_enclosing.as_deref() == Some(&target_cls.simple_name);
        let current_pkg = self.ast.extract_package().unwrap_or_default();
        let is_same_package = !target_cls.package_name.is_empty() && current_pkg == target_cls.package_name;

        // 1. Inherited & local methods with access control visibility & static/instance filtering
        let inherited_methods = self.get_all_methods_with_inheritance(target_cls).await;
        for m in inherited_methods {
            if m.is_constructor {
                continue;
            }
            if is_static_access && !m.is_static {
                continue;
            }
            if !is_static_access && m.is_static {
                continue;
            }
            // Visibility check: private methods only visible in same class, package-private only in same package
            if !is_same_class && !m.is_public && !is_same_package {
                continue;
            }
            let mut ret_display = m.return_type.simple_name();
            // Generics Materialization: if return type is generic placeholder (e.g. E, T, Object) and type argument is known
            if let Some(arg) = type_arg {
                if matches!(ret_display.as_str(), "E" | "T" | "K" | "V" | "R" | "Object") {
                    ret_display = arg.clone();
                }
            }
            let param_labels = m.parameter_display_strings();
            items.push(CompletionItem {
                label: m.name.clone(),
                kind: CompletionKind::Method,
                detail: Some(format!("({}) -> {}", param_labels.join(", "), ret_display)),
            });
        }

        // 2. Inherited & local fields with access control visibility & static/instance filtering
        let inherited_fields = self.get_all_fields_with_inheritance(target_cls).await;
        for f in inherited_fields {
            if is_static_access && !f.is_static {
                continue;
            }
            if !is_static_access && f.is_static {
                continue;
            }
            if !is_same_class && !f.is_public && !is_same_package {
                continue;
            }
            items.push(CompletionItem {
                label: f.name.clone(),
                kind: CompletionKind::Field,
                detail: Some(format!("{} : {}", f.field_type.simple_name(), f.name)),
            });
        }
    }

    /// Provide code completion candidates
    pub async fn get_completions(&self, line: u32, character: u32) -> Vec<CompletionItem> {
        let mut items = Vec::new();
        let node_opt = self.find_node_at_position(line, character);

        // If after a dot (e.g. `obj.`)
        if let Some(node) = node_opt {
            let parent = node.parent();
            if let Some(p) = parent
                && (p.kind() == "field_access" || p.kind() == "method_invocation") {
                    // Try to resolve target object type
                    if let Some(target) = p.child_by_field_name("object") {
                        let target_name = self.ast.node_text(&target);
                        let mut target_type_opt = None;
                        let mut is_static_access = false;

                        // 1. Search local variables & parameters (including Java 10+ var)
                        let local_vars = self.ast.extract_local_variables_at_position(line, character);
                        for (v_name, v_type) in &local_vars {
                            if v_name == target_name {
                                target_type_opt = Some(v_type.clone());
                                break;
                            }
                        }

                        // 2. Search local fields in classes
                        if target_type_opt.is_none() {
                            for cls in self.local_classes.values() {
                                for f in &cls.fields {
                                    if f.name == target_name {
                                        target_type_opt = Some(f.field_type.simple_name());
                                        break;
                                    }
                                }
                            }
                        }

                        // 3. Fallback to static class access (e.g. Math. or StringUtils.)
                        if target_type_opt.is_none() {
                            target_type_opt = Some(target_name.to_string());
                            is_static_access = true;
                        }

                        // 4. Resolve target type methods with generics materialization
                        if let Some(ty_str) = target_type_opt {
                            // Extract base type and type argument, e.g. "List<String>" -> base="List", arg="String"
                            let (base_type, type_arg) = if let Some(idx) = ty_str.find('<') {
                                let base = ty_str[..idx].trim();
                                let arg = ty_str[idx + 1..].trim_end_matches('>').trim();
                                (base, Some(arg.to_string()))
                            } else {
                                (ty_str.as_str(), None)
                            };

                            if let Some(target_cls) = self.resolve_type(base_type).await {
                                self.populate_member_completions(&target_cls, type_arg.as_ref(), is_static_access, line, character, &mut items).await;
                            }
                        }
                    }
                }
        }

        // Fault-tolerance: If user is typing right after a dot (e.g. `obj.`) but Tree-sitter AST collapsed into ERROR node
        if items.is_empty() && self.ast.is_after_dot_access(line, character) {
            if let Some(target_name) = self.ast.extract_receiver_before_dot_fallback(line, character) {
                let mut target_type_opt = None;
                let mut is_static_access = false;

                // 1. Search local variables & parameters
                let local_vars = self.ast.extract_local_variables_at_position(line, character);
                for (v_name, v_type) in &local_vars {
                    if v_name == &target_name {
                        target_type_opt = Some(v_type.clone());
                        break;
                    }
                }

                // 2. Search local fields in classes
                if target_type_opt.is_none() {
                    for cls in self.local_classes.values() {
                        for f in &cls.fields {
                            if f.name == target_name {
                                target_type_opt = Some(f.field_type.simple_name());
                                break;
                            }
                        }
                    }
                }

                // 3. Fallback to static class access
                if target_type_opt.is_none() {
                    target_type_opt = Some(target_name.clone());
                    is_static_access = true;
                }

                // 4. Resolve target type methods & fields with generics materialization and visibility filtering
                if let Some(ty_str) = target_type_opt {
                    let (base_type, type_arg) = if let Some(idx) = ty_str.find('<') {
                        let base = ty_str[..idx].trim();
                        let arg = ty_str[idx + 1..].trim_end_matches('>').trim();
                        (base, Some(arg.to_string()))
                    } else {
                        (ty_str.as_str(), None)
                    };

                    if let Some(target_cls) = self.resolve_type(base_type).await {
                        self.populate_member_completions(&target_cls, type_arg.as_ref(), is_static_access, line, character, &mut items).await;
                    }
                }
            }
        }

        // If no dot member completion, suggest in-scope classes and keywords
        if items.is_empty() && !self.ast.is_after_dot_access(line, character) {
            for cls in self.classpath.all_classes().await {
                items.push(CompletionItem {
                    label: cls.simple_name.clone(),
                    kind: CompletionKind::Class,
                    detail: Some(cls.fqn.clone()),
                });
            }

            for cls in self.local_classes.values() {
                items.push(CompletionItem {
                    label: cls.simple_name.clone(),
                    kind: CompletionKind::Class,
                    detail: Some(cls.fqn.clone()),
                });
            }

            // Java keywords
            let keywords = ["public", "private", "protected", "class", "interface", "void", "return", "import", "package", "new", "this"];
            for kw in keywords {
                items.push(CompletionItem {
                    label: kw.to_string(),
                    kind: CompletionKind::Keyword,
                    detail: None,
                });
            }
        }

        items
    }
}

#[derive(Debug, Clone)]
pub enum CompletionKind {
    Class,
    Method,
    Field,
    Keyword,
}

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_goto_definition_method_receiver_disambiguation() {
        let code = r#"
            class ClassA {
                public void process() {
                    // Implementation A
                }
            }

            class ClassB {
                public void process() {
                    // Implementation B
                }
            }

            public class Caller {
                public void test() {
                    ClassA a = new ClassA();
                    ClassB b = new ClassB();
                    a.process();
                    b.process();
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Parse java code");
        let cp = ClasspathRegistry::new();
        let sem = SemanticContext::new(&ast, &cp).await;

        // Find symbols to get exact line numbers for ClassA.process and ClassB.process
        let symbols = ast.extract_symbols();
        let method_a = symbols
            .iter()
            .find(|s| s.name == "process" && s.container_name.as_deref() == Some("ClassA"))
            .expect("ClassA.process symbol");
        let method_b = symbols
            .iter()
            .find(|s| s.name == "process" && s.container_name.as_deref() == Some("ClassB"))
            .expect("ClassB.process symbol");

        // Row 18: `a.process();` (0-indexed line: 18)
        // Find line number of `a.process();`
        let lines: Vec<&str> = code.lines().collect();
        let a_call_line = lines
            .iter()
            .position(|l| l.contains("a.process()"))
            .expect("find a.process line") as u32;
        let a_call_col = lines[a_call_line as usize]
            .find("process")
            .expect("find col of process") as u32;

        let b_call_line = lines
            .iter()
            .position(|l| l.contains("b.process()"))
            .expect("find b.process line") as u32;
        let b_call_col = lines[b_call_line as usize]
            .find("process")
            .expect("find col of process") as u32;

        // Calling `a.process()` MUST jump to ClassA's method, NOT ClassB!
        let def_a = sem.goto_definition(a_call_line, a_call_col).await;
        assert!(def_a.is_some(), "Definition for a.process() should be found");
        let (_, range_a) = def_a.unwrap();
        assert_eq!(
            range_a.start.line, method_a.selection_range.start.line,
            "a.process() must jump to ClassA.process"
        );

        // Calling `b.process()` MUST jump to ClassB's method, NOT ClassA!
        let def_b = sem.goto_definition(b_call_line, b_call_col).await;
        assert!(def_b.is_some(), "Definition for b.process() should be found");
        let (_, range_b) = def_b.unwrap();
        assert_eq!(
            range_b.start.line, method_b.selection_range.start.line,
            "b.process() must jump to ClassB.process"
        );
    }

    #[tokio::test]
    async fn test_static_vs_instance_member_completion_filtering() {
        let code = r#"
            public class MathUtil {
                public static int max(int a, int b) { return a > b ? a : b; }
                public void instanceHelper() {}
            }

            public class Caller {
                public void test() {
                    MathUtil.
                    MathUtil u = new MathUtil();
                    u.
                }
            }
        "#;
        let ast = JavaAst::parse(Arc::new(code.to_string())).expect("Parse code");
        let cp = ClasspathRegistry::new();
        let sem = SemanticContext::new(&ast, &cp).await;

        // Line 8: 'MathUtil.' -> static access
        let static_completions = sem.get_completions(8, 29).await;
        let static_labels: Vec<String> = static_completions.iter().map(|c| c.label.clone()).collect();
        assert!(static_labels.contains(&"max".to_string()), "MathUtil. should suggest static method max, got: {:?}", static_labels);
        assert!(!static_labels.contains(&"instanceHelper".to_string()), "MathUtil. must NOT suggest instanceHelper");

        // Line 10: 'u.' -> instance access
        let instance_completions = sem.get_completions(10, 22).await;
        let instance_labels: Vec<String> = instance_completions.iter().map(|c| c.label.clone()).collect();
        assert!(instance_labels.contains(&"instanceHelper".to_string()), "u. should suggest instanceHelper, got: {:?}", instance_labels);
        assert!(!instance_labels.contains(&"max".to_string()), "u. should not suggest static method max");
    }
}

