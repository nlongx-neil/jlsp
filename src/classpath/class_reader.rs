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
            .iter()
            .enumerate()
            .map(|(idx, ty)| (format!("arg{idx}"), ty.clone()))
            .collect();

        let is_static = m.access_flags.contains(MethodAccessFlags::STATIC);
        let is_public = m.access_flags.contains(MethodAccessFlags::PUBLIC);
        let is_constructor = m.name == "<init>";

        let mut decompiled_body = None;
        let mut real_params = Vec::new();

        for attr in &m.attributes {
            if let cafebabe::attributes::AttributeData::Code(ref code_data) = attr.data {
                decompiled_body = crate::classpath::decompiler::decompile_code_data(
                    &m.name,
                    &desc_str,
                    is_static,
                    is_constructor,
                    &simple_name,
                    code_data,
                );

                // Extract real parameter names from LocalVariableTable if present
                for c_attr in &code_data.attributes {
                    if let cafebabe::attributes::AttributeData::LocalVariableTable(ref entries) = c_attr.data {
                        let mut slot = if is_static { 0 } else { 1 };
                        for (_idx, pty) in param_types.iter().enumerate() {
                            if let Some(entry) = entries.iter().find(|e| e.index == slot && e.start_pc == 0) {
                                real_params.push((entry.name.to_string(), pty.clone()));
                            }
                            if matches!(pty, TypeSignature::Long | TypeSignature::Double) {
                                slot += 2;
                            } else {
                                slot += 1;
                            }
                        }
                    }
                }
                break;
            }
        }

        let final_params = if real_params.len() == params.len() {
            real_params
        } else {
            params
        };

        methods.push(MethodInfo {
            name: m.name.to_string(),
            return_type,
            parameters: final_params,
            is_static,
            is_public,
            is_constructor,
            javadoc: None,
            body: decompiled_body,
        });
    }

    let mut fields = Vec::new();
    for f in &class_file.fields {
        let desc_str = format!("{}", f.descriptor);
        let (field_type, _) = crate::classpath::descriptor::parse_field_descriptor(&desc_str);
        let is_static = f.access_flags.contains(FieldAccessFlags::STATIC);
        let is_public = f.access_flags.contains(FieldAccessFlags::PUBLIC);
        let is_final = f.access_flags.contains(FieldAccessFlags::FINAL);

        fields.push(FieldInfo {
            name: f.name.to_string(),
            field_type,
            is_static,
            is_public,
            is_final,
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

pub fn parse_class_dir<P: AsRef<Path>>(dir_path: P) -> Result<Vec<ClassInfo>, String> {
    let mut classes = Vec::new();
    for entry in walkdir::WalkDir::new(dir_path.as_ref())
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("class") {
            let path_str = path.to_string_lossy();
            if path_str.contains("META-INF") {
                continue;
            }
            if let Ok(buf) = std::fs::read(path) {
                if let Ok(mut class_info) = parse_class_bytes(&buf) {
                    class_info.source_path = Some(path_str.to_string());
                    classes.push(class_info);
                }
            }
        }
    }
    Ok(classes)
}

pub fn parse_src_zip<P: AsRef<Path>>(zip_path: P) -> Result<Vec<ClassInfo>, String> {
    let file = File::open(&zip_path).map_err(|e| format!("Cannot open src.zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Zip error: {e}"))?;
    let mut classes = Vec::new();
    for i in 0..archive.len() {
        let entry_name = match archive.by_index(i) {
            Ok(file) => file.name().to_string(),
            Err(_) => continue,
        };
        if entry_name.ends_with(".java")
            && !entry_name.ends_with("module-info.java")
            && !entry_name.ends_with("package-info.java")
        {
            let path_clean = if let Some(idx) = entry_name.find('/') {
                let first_seg = &entry_name[..idx];
                if first_seg.starts_with("java.") || first_seg.starts_with("jdk.") {
                    &entry_name[idx + 1..]
                } else {
                    &entry_name
                }
            } else {
                &entry_name
            };
            if let Some(without_ext) = path_clean.strip_suffix(".java") {
                let fqn = without_ext.replace('/', ".");
                let (pkg, simple_name) = match fqn.rfind('.') {
                    Some(idx) => (fqn[..idx].to_string(), fqn[idx + 1..].to_string()),
                    None => (String::new(), fqn.clone()),
                };
                classes.push(ClassInfo {
                    fqn,
                    simple_name,
                    package_name: pkg,
                    kind: ClassKind::Class,
                    super_class: None,
                    interfaces: Vec::new(),
                    methods: Vec::new(),
                    fields: Vec::new(),
                    source_path: Some(zip_path.as_ref().to_string_lossy().to_string()),
                    is_public: true,
                });
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

    #[test]
    fn test_parse_java_21_class() {
        let p = std::path::Path::new("/tmp/test_al/java.base/java/util/ArrayList.class");
        if p.exists() {
            let bytes = std::fs::read(p).unwrap();
            let parsed = parse_class_bytes(&bytes);
            println!("Parsed result: {:?}", parsed.as_ref().map(|c| (&c.fqn, &c.simple_name, c.methods.len(), c.fields.len())));
            assert!(parsed.is_ok());
            let cls = parsed.unwrap();
            assert_eq!(cls.fqn, "java.util.ArrayList");
            assert_eq!(cls.simple_name, "ArrayList");
            assert_eq!(cls.package_name, "java.util");
        }
    }
}
