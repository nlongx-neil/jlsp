use std::fs::File;
use std::io::Read;
use std::path::Path;
use cafebabe::{parse_class, ClassAccessFlags, FieldAccessFlags, MethodAccessFlags};

use crate::classpath::descriptor::parse_method_descriptor;
use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo, TypeSignature};

pub fn parse_class_bytes(bytes: &[u8]) -> Result<ClassInfo, String> {
    let class_file = parse_class(bytes).map_err(|e| format!("Failed to parse class: {e:?}"))?;

    let raw_name = class_file.this_class.replace('/', ".");
    let fqn = raw_name.clone();
    let (pkg, simple_name) = match fqn.rfind('.') {
        Some(idx) => (fqn[..idx].to_string(), fqn[idx + 1..].to_string()),
        None => (String::new(), fqn.clone()),
    };

    let super_class = class_file.super_class.map(|s| s.replace('/', "."));
    let interfaces = class_file.interfaces.iter().map(|s| s.replace('/', ".")).collect();

    let is_interface = class_file.access_flags.contains(ClassAccessFlags::INTERFACE);
    let is_enum = class_file.access_flags.contains(ClassAccessFlags::ENUM);
    let is_annotation = class_file.access_flags.contains(ClassAccessFlags::ANNOTATION);

    let kind = if is_annotation {
        ClassKind::Annotation
    } else if is_interface {
        ClassKind::Interface
    } else if is_enum {
        ClassKind::Enum
    } else {
        ClassKind::Class
    };

    let mut methods = Vec::new();
    for m in &class_file.methods {
        let desc_str = format!("{}", m.descriptor);
        let (param_types, return_type) = parse_method_descriptor(&desc_str);
        let params: Vec<(String, TypeSignature)> = param_types
            .into_iter()
            .enumerate()
            .map(|(idx, ty)| (format!("arg{idx}"), ty))
            .collect();

        let is_static = m.access_flags.contains(MethodAccessFlags::STATIC);
        let is_public = m.access_flags.contains(MethodAccessFlags::PUBLIC);
        let is_constructor = m.name == "<init>";

        methods.push(MethodInfo {
            name: m.name.to_string(),
            return_type,
            parameters: params,
            is_static,
            is_public,
            is_constructor,
            javadoc: None,
        });
    }

    let mut fields = Vec::new();
    for f in &class_file.fields {
        let desc_str = format!("{}", f.descriptor);
        let (field_type, _) = crate::classpath::descriptor::parse_field_descriptor(&desc_str);
        let is_static = f.access_flags.contains(FieldAccessFlags::STATIC);
        let is_public = f.access_flags.contains(FieldAccessFlags::PUBLIC);

        fields.push(FieldInfo {
            name: f.name.to_string(),
            field_type,
            is_static,
            is_public,
        });
    }

    Ok(ClassInfo {
        fqn,
        simple_name,
        package_name: pkg,
        kind,
        super_class,
        interfaces,
        methods,
        fields,
        source_path: None,
        is_public: class_file.access_flags.contains(ClassAccessFlags::PUBLIC),
    })
}

pub fn parse_jar_file<P: AsRef<Path>>(jar_path: P) -> Result<Vec<ClassInfo>, String> {
    let file = File::open(&jar_path).map_err(|e| format!("Cannot open jar {}: {e}", jar_path.as_ref().display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Zip error: {e}"))?;

    let mut classes = Vec::new();

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(file) => file,
            Err(_) => continue,
        };

        let name = entry.name().to_string();
        if name.ends_with(".class") && !name.starts_with("META-INF/") {
            let mut buf = Vec::with_capacity(entry.size() as usize);
            if entry.read_to_end(&mut buf).is_ok()
                && let Ok(mut class_info) = parse_class_bytes(&buf) {
                    class_info.source_path = Some(jar_path.as_ref().to_string_lossy().to_string());
                    classes.push(class_info);
                }
        }
    }

    Ok(classes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sample_jar() {
        if std::path::Path::new("/tmp/java_test/sample.jar").exists() {
            let classes = parse_jar_file("/tmp/java_test/sample.jar").expect("parse jar");
            assert_eq!(classes.len(), 1);
            let cls = &classes[0];
            assert_eq!(cls.fqn, "com.demo.Sample");
            assert_eq!(cls.simple_name, "Sample");
            assert_eq!(cls.package_name, "com.demo");
            let greet = cls.methods.iter().find(|m| m.name == "greet").expect("greet method");
            assert_eq!(greet.return_type, TypeSignature::Class("java.lang.String".into()));
            assert_eq!(greet.parameters.len(), 1);
        }
    }
}
