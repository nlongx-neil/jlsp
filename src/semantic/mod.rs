pub mod lombok;
pub mod apt_jdwp;

use std::collections::HashMap;
use std::sync::Arc;
use tree_sitter::Node;

use crate::classpath::ClasspathRegistry;
use crate::model::{ClassInfo, MethodInfo, Range};
use crate::syntax::JavaAst;

pub struct SemanticContext<'a> {
    pub ast: &'a JavaAst,
    pub classpath: &'a ClasspathRegistry,
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub local_classes: HashMap<String, ClassInfo>,
}

impl<'a> SemanticContext<'a> {
    pub async fn new(ast: &'a JavaAst, classpath: &'a ClasspathRegistry) -> Self {
        let package = ast.extract_package();
        let imports = ast.extract_imports();
        let classes = ast.extract_classes(None);

        let mut local_classes = HashMap::new();
        for mut cls in classes {
            // Apply Lombok synthetic methods if present
            lombok::process_lombok_annotations(&mut cls, &ast.source);
            local_classes.insert(cls.simple_name.clone(), cls.clone());
            local_classes.insert(cls.fqn.clone(), cls);
        }

        Self {
            ast,
            classpath,
            package,
            imports,
            local_classes,
        }
    }

    /// Resolve simple type name (e.g. "String" or "List") into full ClassInfo
    pub async fn resolve_type(&self, type_name: &str) -> Option<Arc<ClassInfo>> {
        // 1. Primitive check
        if matches!(type_name, "int" | "long" | "boolean" | "byte" | "short" | "char" | "float" | "double" | "void") {
            return None;
        }

        // 2. Check local classes in current file
        if let Some(cls) = self.local_classes.get(type_name) {
            return Some(Arc::new(cls.clone()));
        }

        // 3. Check explicit imports (e.g., import java.util.List;)
        for imp in &self.imports {
            if (imp.ends_with(&format!(".{type_name}")) || imp == type_name)
                && let Some(cls) = self.classpath.get_class(imp).await {
                    return Some(cls);
                }
        }

        // 4. Check same package
        if let Some(pkg) = &self.package {
            let fqn = format!("{pkg}.{type_name}");
            if let Some(cls) = self.classpath.get_class(&fqn).await {
                return Some(cls);
            }
        }

        // 5. Check java.lang default package
        let java_lang_fqn = format!("java.lang.{type_name}");
        if let Some(cls) = self.classpath.get_class(&java_lang_fqn).await {
            return Some(cls);
        }

        // 6. Search registry by simple name
        let found = self.classpath.find_classes_by_simple_name(type_name).await;
        if let Some(first) = found.into_iter().next() {
            return Some(first);
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

        for cls in self.local_classes.values() {
            let mut required_methods = Vec::new();

            for iface in &cls.interfaces {
                if let Some(iface_info) = self.resolve_type(iface).await {
                    let inherited = self.get_all_methods_with_inheritance(&iface_info).await;
                    required_methods.extend(inherited);
                }
            }

            if let Some(super_name) = &cls.super_class {
                if let Some(super_info) = self.resolve_type(super_name).await {
                    for m in &super_info.methods {
                        // Check if method is abstract or not implemented
                        if !cls.methods.iter().any(|cm| cm.name == m.name) {
                            required_methods.push(m.clone());
                        }
                    }
                }
            }

            // Filter out methods already implemented in cls
            let mut missing = Vec::new();
            for req in required_methods {
                if !cls.methods.iter().any(|cm| cm.name == req.name) {
                    if !missing.iter().any(|m: &crate::model::MethodInfo| m.name == req.name) {
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

    /// Find AST node at specific line and column
    pub fn find_node_at_position(&self, line: u32, character: u32) -> Option<Node<'_>> {
        let root = self.ast.tree.root_node();
        let point = tree_sitter::Point {
            row: line as usize,
            column: character as usize,
        };
        root.descendant_for_point_range(point, point)
    }

    /// Resolve definition at given cursor location
    pub async fn goto_definition(&self, line: u32, character: u32) -> Option<(String, Range)> {
        let node = self.find_node_at_position(line, character)?;
        let ident_text = self.ast.node_text(&node);

        // 1. Check if cursor is at a method invocation (e.g. `obj.process()` or `process()`)
        if let Some((recv_opt, m_name)) = self.ast.find_method_call_receiver(line, character) {
            if m_name == ident_text {
                // Determine target class for receiver
                let mut target_class_name: Option<String> = None;

                if let Some(recv) = recv_opt {
                    if recv == "this" || recv == "super" {
                        target_class_name = self.ast.find_enclosing_class_at_position(line, character);
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
                                    return Some((String::new(), sym.selection_range));
                                }
                            }
                        }

                        // If not in local symbols or defined elsewhere (external file or JAR)
                        if let Some(src_path) = &cls_info.source_path {
                            let range = Range {
                                start: crate::model::Position { line: 0, character: 0 },
                                end: crate::model::Position { line: 0, character: 0 },
                            };
                            return Some((src_path.clone(), range));
                        }
                    }
                }
            }
        }

        // 2. Check if cursor is on a class identifier
        if let Some(cls) = self.resolve_type(ident_text).await {
            let path = cls.source_path.clone().unwrap_or_default();
            // Default to start of file
            let range = Range {
                start: crate::model::Position { line: 0, character: 0 },
                end: crate::model::Position { line: 0, character: 0 },
            };
            return Some((path, range));
        }

        // 3. Check if cursor is on a local symbol definition (method, field, etc.)
        // If cursor is inside a specific class, prefer symbols belonging to that class
        let enclosing_class = self.ast.find_enclosing_class_at_position(line, character);
        let symbols = self.ast.extract_symbols();

        if let Some(enc) = &enclosing_class {
            for sym in &symbols {
                if sym.name == ident_text && sym.container_name.as_ref() == Some(enc) {
                    return Some((String::new(), sym.selection_range.clone()));
                }
            }
        }

        for sym in &symbols {
            if sym.name == ident_text {
                return Some((String::new(), sym.selection_range.clone()));
            }
        }

        // 4. Check if cursor is on a synthetic Lombok getter/setter (e.g. getName / setName) -> jump to field 'name'
        if (ident_text.starts_with("get") || ident_text.starts_with("set")) && ident_text.len() > 3 {
            let field_cand = format!("{}{}", ident_text[3..4].to_lowercase(), &ident_text[4..]);
            if let Some(enc) = &enclosing_class {
                for sym in &symbols {
                    if sym.name == field_cand && sym.container_name.as_ref() == Some(enc) {
                        return Some((String::new(), sym.selection_range.clone()));
                    }
                }
            }
            for sym in &symbols {
                if sym.name == field_cand {
                    return Some((String::new(), sym.selection_range.clone()));
                }
            }
        } else if ident_text.starts_with("is") && ident_text.len() > 2 {
            let field_cand = format!("{}{}", ident_text[2..3].to_lowercase(), &ident_text[3..]);
            if let Some(enc) = &enclosing_class {
                for sym in &symbols {
                    if sym.name == field_cand && sym.container_name.as_ref() == Some(enc) {
                        return Some((String::new(), sym.selection_range.clone()));
                    }
                }
            }
            for sym in &symbols {
                if sym.name == field_cand {
                    return Some((String::new(), sym.selection_range.clone()));
                }
            }
        }

        None
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

                        // 3. Resolve target type methods with generics materialization
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
                                let inherited_methods = self.get_all_methods_with_inheritance(&target_cls).await;
                                for m in inherited_methods {
                                    let mut ret_display = m.return_type.simple_name();
                                    // Generics Materialization: if return type is generic placeholder (e.g. E, T, Object) and type argument is known
                                    if let Some(arg) = &type_arg {
                                        if matches!(ret_display.as_str(), "E" | "T" | "K" | "V" | "R" | "Object") {
                                            ret_display = arg.clone();
                                        }
                                    }

                                    let param_labels: Vec<String> = m.parameters.iter().map(|(p, t)| format!("{} {}", t.simple_name(), p)).collect();
                                    items.push(CompletionItem {
                                        label: m.name.clone(),
                                        kind: CompletionKind::Method,
                                        detail: Some(format!("({}) -> {}", param_labels.join(", "), ret_display)),
                                    });
                                }
                            }
                        }
                    }
                }
        }

        // If no dot member completion, suggest in-scope classes and keywords
        if items.is_empty() {
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
}

