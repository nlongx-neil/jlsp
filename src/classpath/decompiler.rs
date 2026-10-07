use crate::model::{ClassInfo, ClassKind};

/// Generate human-readable Java source stub from binary ClassInfo
pub fn decompile_class_info(cls: &ClassInfo) -> String {
    let mut out = String::new();

    // 1. Package statement
    if !cls.package_name.is_empty() {
        out.push_str(&format!("package {};\n\n", cls.package_name));
    }

    out.push_str("// Decompiled by jlsp (pure Rust, JVM-free)\n");

    // 2. Class signature
    let kind_str = match cls.kind {
        ClassKind::Interface => "interface",
        ClassKind::Enum => "enum",
        ClassKind::Record => "record",
        ClassKind::Annotation => "@interface",
        ClassKind::Class => "class",
    };

    let visibility = if cls.is_public { "public " } else { "" };
    out.push_str(&format!("{}{} {}", visibility, kind_str, cls.simple_name));

    if let Some(super_cls) = &cls.super_class {
        if super_cls != "java.lang.Object" {
            let simple_super = super_cls.split('.').last().unwrap_or(super_cls);
            out.push_str(&format!(" extends {}", simple_super));
        }
    }

    if !cls.interfaces.is_empty() {
        let ifaces: Vec<String> = cls
            .interfaces
            .iter()
            .map(|i| i.split('.').last().unwrap_or(i).to_string())
            .collect();
        out.push_str(&format!(" implements {}", ifaces.join(", ")));
    }

    out.push_str(" {\n");

    // 3. Fields
    for field in &cls.fields {
        let vis = if field.is_public { "    public " } else { "    " };
        let stat = if field.is_static { "static " } else { "" };
        out.push_str(&format!(
            "{}{}{}{};\n",
            vis,
            stat,
            format!("{} ", field.field_type.simple_name()),
            field.name
        ));
    }

    if !cls.fields.is_empty() && !cls.methods.is_empty() {
        out.push('\n');
    }

    // 4. Methods
    for method in &cls.methods {
        let vis = if method.is_public { "    public " } else { "    " };
        let stat = if method.is_static { "static " } else { "" };
        let param_strs: Vec<String> = method
            .parameters
            .iter()
            .map(|(pname, pty)| format!("{} {}", pty.simple_name(), pname))
            .collect();

        if method.is_constructor {
            out.push_str(&format!(
                "{}{}({}) {{ /* compiled code */ }}\n",
                vis,
                cls.simple_name,
                param_strs.join(", ")
            ));
        } else {
            out.push_str(&format!(
                "{}{}{} {}({}) {{ /* compiled code */ }}\n",
                vis,
                stat,
                method.return_type.simple_name(),
                method.name,
                param_strs.join(", ")
            ));
        }
    }

    out.push_str("}\n");
    out
}

/// Try to locate and extract real source code for a class from local *-sources.jar
pub fn extract_source_from_sources_jar(jar_path: &std::path::Path, fqn: &str) -> Option<String> {
    let file_name = jar_path.file_name()?.to_str()?;
    if !file_name.ends_with(".jar") {
        return None;
    }

    // Try finding sibling or matching *-sources.jar in the same directory or parent tree
    let sources_jar_name = file_name.replace(".jar", "-sources.jar");
    let sources_jar_path = jar_path.with_file_name(sources_jar_name);

    let target_sources_jar = if sources_jar_path.exists() {
        Some(sources_jar_path)
    } else if let Some(parent) = jar_path.parent() {
        // In gradle cache, sources.jar might be in a sibling hash directory
        let mut found = None;
        if let Some(grandparent) = parent.parent() {
            for entry in walkdir::WalkDir::new(grandparent).max_depth(3).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                    if n.ends_with("-sources.jar") {
                        found = Some(p.to_path_buf());
                        break;
                    }
                }
            }
        }
        found
    } else {
        None
    };

    let sources_jar = target_sources_jar?;
    let file = std::fs::File::open(&sources_jar).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    let entry_name = format!("{}.java", fqn.replace('.', "/"));
    let mut zip_entry = archive.by_name(&entry_name).ok()?;

    let mut content = String::new();
    std::io::Read::read_to_string(&mut zip_entry, &mut content).ok()?;
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FieldInfo, MethodInfo, TypeSignature};

    #[test]
    fn test_decompile_simple_class() {
        let cls = ClassInfo {
            fqn: "java.lang.String".into(),
            simple_name: "String".into(),
            package_name: "java.lang".into(),
            kind: ClassKind::Class,
            super_class: Some("java.lang.Object".into()),
            interfaces: vec!["java.io.Serializable".into()],
            methods: vec![MethodInfo {
                name: "length".into(),
                return_type: TypeSignature::Int,
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: None,
            }],
            fields: vec![FieldInfo {
                name: "value".into(),
                field_type: TypeSignature::Array(Box::new(TypeSignature::Byte)),
                is_static: false,
                is_public: false,
            }],
            source_path: None,
            is_public: true,
        };

        let decompiled = decompile_class_info(&cls);
        assert!(decompiled.contains("package java.lang;"));
        assert!(decompiled.contains("public class String implements Serializable {"));
        assert!(decompiled.contains("public int length() { /* compiled code */ }"));
    }
}
