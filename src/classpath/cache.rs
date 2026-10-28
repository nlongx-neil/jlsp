use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo};

/// Compact class archive representation for disk cache.
/// Optimizations applied:
/// 1. String Interning: All unique `source_path` strings (JAR paths) are collected into `paths`.
///    Each class references its path via a 4-byte `path_idx`, eliminating thousands of duplicated path strings.
/// 2. Redundancy elimination: `simple_name` and `package_name` are stripped out; they are derived on the fly from `fqn`.
/// 3. Compact binary encoding via `bincode`.
/// 4. Block compression via `zstd` (level 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactClassArchive {
    pub version: u32,
    pub paths: Vec<String>,
    pub classes: Vec<CompactClassInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactClassInfo {
    pub fqn: String,
    pub kind: ClassKind,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub methods: Vec<MethodInfo>,
    pub fields: Vec<FieldInfo>,
    pub path_idx: Option<u32>,
    pub is_public: bool,
}

impl CompactClassArchive {
    pub fn from_class_refs<'a>(classes: impl IntoIterator<Item = &'a ClassInfo>, count_hint: usize) -> Self {
        let mut paths = Vec::new();
        let mut path_map = HashMap::new();
        let mut compact_classes = Vec::with_capacity(count_hint);

        for cls in classes {
            let path_idx = if let Some(ref path) = cls.source_path {
                if let Some(&idx) = path_map.get(path) {
                    Some(idx)
                } else {
                    let idx = paths.len() as u32;
                    paths.push(path.clone());
                    path_map.insert(path.clone(), idx);
                    Some(idx)
                }
            } else {
                None
            };

            compact_classes.push(CompactClassInfo {
                fqn: cls.fqn.clone(),
                kind: cls.kind.clone(),
                super_class: cls.super_class.clone(),
                interfaces: cls.interfaces.clone(),
                methods: cls.methods.clone(),
                fields: cls.fields.clone(),
                path_idx,
                is_public: cls.is_public,
            });
        }

        Self {
            version: 1,
            paths,
            classes: compact_classes,
        }
    }

    pub fn from_classes(classes: &[ClassInfo]) -> Self {
        Self::from_class_refs(classes.iter(), classes.len())
    }

    pub fn into_classes(self) -> Vec<ClassInfo> {
        let paths = self.paths;
        self.classes
            .into_iter()
            .map(|c| {
                let (package_name, simple_name) = match c.fqn.rfind('.') {
                    Some(idx) => (c.fqn[..idx].to_string(), c.fqn[idx + 1..].to_string()),
                    None => (String::new(), c.fqn.clone()),
                };
                let source_path = c.path_idx.and_then(|idx| paths.get(idx as usize).cloned());

                ClassInfo {
                    fqn: c.fqn,
                    simple_name,
                    package_name,
                    kind: c.kind,
                    super_class: c.super_class,
                    interfaces: c.interfaces,
                    methods: c.methods,
                    fields: c.fields,
                    source_path,
                    is_public: c.is_public,
                }
            })
            .collect()
    }
}

/// Serialize classes to compact binary bytes compressed with zstd
pub fn serialize_archive(classes: &[ClassInfo]) -> Result<Vec<u8>, String> {
    let archive = CompactClassArchive::from_classes(classes);
    let bin = bincode::serialize(&archive).map_err(|e| format!("Bincode serialize error: {e}"))?;
    zstd::encode_all(&bin[..], 3).map_err(|e| format!("Zstd encode error: {e}"))
}

/// Deserialize classes from bytes (supports compact zstd, raw bincode, or legacy JSON)
pub fn deserialize_archive(data: &[u8]) -> Result<Vec<ClassInfo>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    // 1. Check if data is compressed with zstd (magic header: 0x28, 0xB5, 0x2F, 0xFD)
    let decompressed = if data.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::decode_all(data).map_err(|e| format!("Zstd decode error: {e}"))?
    } else {
        data.to_vec()
    };

    // 2. Try deserializing as CompactClassArchive (P1 compact format)
    if let Ok(archive) = bincode::deserialize::<CompactClassArchive>(&decompressed) {
        return Ok(archive.into_classes());
    }

    // 3. Fallback: try raw bincode Vec<ClassInfo> (P0 format)
    if let Ok(list) = bincode::deserialize::<Vec<ClassInfo>>(&decompressed) {
        return Ok(list);
    }

    // 4. Fallback: try JSON format from decompressed stream
    if let Ok(list) = serde_json::from_slice::<Vec<ClassInfo>>(&decompressed) {
        return Ok(list);
    }

    // 5. Fallback: try JSON format from raw uncompressed stream
    if let Ok(list) = serde_json::from_slice::<Vec<ClassInfo>>(data) {
        return Ok(list);
    }

    Err("Unrecognized index cache format or corrupted data".to_string())
}

/// Serialize classes from iterator of references without cloning
pub fn serialize_archive_from_refs<'a>(classes: impl IntoIterator<Item = &'a ClassInfo>, count_hint: usize) -> Result<Vec<u8>, String> {
    let archive = CompactClassArchive::from_class_refs(classes, count_hint);
    let bin = bincode::serialize(&archive).map_err(|e| format!("Bincode serialize error: {e}"))?;
    zstd::encode_all(&bin[..], 3).map_err(|e| format!("Zstd encode error: {e}"))
}

/// Save classes from iterator of references without cloning
pub fn save_archive_file_from_refs<'a, P: AsRef<Path>>(path: P, classes: impl IntoIterator<Item = &'a ClassInfo>, count_hint: usize) -> Result<(), String> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let bytes = serialize_archive_from_refs(classes, count_hint)?;
    std::fs::write(path, bytes).map_err(|e| format!("Disk write error: {e}"))
}

/// Save classes to an archive file
pub fn save_archive_file<P: AsRef<Path>>(path: P, classes: &[ClassInfo]) -> Result<(), String> {
    save_archive_file_from_refs(path, classes.iter(), classes.len())
}

/// Load classes from an archive file
pub fn load_archive_file<P: AsRef<Path>>(path: P) -> Result<Vec<ClassInfo>, String> {
    let p = path.as_ref();
    if !p.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read(p).map_err(|e| format!("Disk read error: {e}"))?;
    deserialize_archive(&data)
}

/// Compute a persistent, content-sensitive cache key for a jar file
pub fn compute_jar_cache_key(jar_path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(jar_path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();

    jar_path.to_string_lossy().hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    if let Ok(mtime) = metadata.modified() {
        mtime.hash(&mut hasher);
    }

    let stem = jar_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("jar")
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect::<String>();

    Some(format!("{}_{:016x}.bin.zst", stem, hasher.finish()))
}

/// Return global jar cache directory: ~/.jlsp/cache/jars
pub fn global_jar_cache_dir() -> PathBuf {
    crate::classpath::dirs_home()
        .unwrap_or_else(std::env::temp_dir)
        .join(".jlsp")
        .join("cache")
        .join("jars")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClassKind, TypeSignature};

    #[test]
    fn test_compact_archive_roundtrip() {
        let shared_path = Some("/path/to/shared/spring-beans-5.3.20.jar".to_string());
        let classes = vec![
            ClassInfo {
                fqn: "org.springframework.beans.factory.BeanFactory".to_string(),
                simple_name: "BeanFactory".to_string(),
                package_name: "org.springframework.beans.factory".to_string(),
                kind: ClassKind::Interface,
                super_class: None,
                interfaces: vec![],
                methods: vec![MethodInfo {
                    name: "getBean".to_string(),
                    return_type: TypeSignature::Class("java.lang.Object".to_string()),
                    parameters: vec![("name".to_string(), TypeSignature::Class("java.lang.String".to_string()))],
                    is_static: false,
                    is_public: true,
                    is_constructor: false,
                    javadoc: None,
                    body: None,
                }],
                fields: vec![],
                source_path: shared_path.clone(),
                is_public: true,
            },
            ClassInfo {
                fqn: "org.springframework.beans.factory.ListableBeanFactory".to_string(),
                simple_name: "ListableBeanFactory".to_string(),
                package_name: "org.springframework.beans.factory".to_string(),
                kind: ClassKind::Interface,
                super_class: None,
                interfaces: vec!["org.springframework.beans.factory.BeanFactory".to_string()],
                methods: vec![],
                fields: vec![],
                source_path: shared_path.clone(),
                is_public: true,
            },
        ];

        let archive = CompactClassArchive::from_classes(&classes);
        // Verify path was interned into a single entry
        assert_eq!(archive.paths.len(), 1);
        assert_eq!(archive.paths[0], "/path/to/shared/spring-beans-5.3.20.jar");
        assert_eq!(archive.classes[0].path_idx, Some(0));
        assert_eq!(archive.classes[1].path_idx, Some(0));

        // Roundtrip serialization with zstd
        let bytes = serialize_archive(&classes).expect("serialization should succeed");
        let restored = deserialize_archive(&bytes).expect("deserialization should succeed");

        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].fqn, "org.springframework.beans.factory.BeanFactory");
        assert_eq!(restored[0].simple_name, "BeanFactory");
        assert_eq!(restored[0].package_name, "org.springframework.beans.factory");
        assert_eq!(restored[0].source_path, shared_path);
        assert_eq!(restored[1].simple_name, "ListableBeanFactory");
        assert_eq!(restored[1].package_name, "org.springframework.beans.factory");
    }

    #[test]
    fn test_legacy_json_backward_compatibility() {
        let classes = vec![ClassInfo {
            fqn: "com.example.Demo".to_string(),
            simple_name: "Demo".to_string(),
            package_name: "com.example".to_string(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: None,
            is_public: true,
        }];

        // Old format was uncompressed serde_json
        let json_bytes = serde_json::to_vec(&classes).unwrap();
        let loaded = deserialize_archive(&json_bytes).expect("should load legacy json");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].fqn, "com.example.Demo");
        assert_eq!(loaded[0].simple_name, "Demo");
    }

    #[test]
    fn test_compression_ratio_bench() {
        let mut classes = Vec::new();
        let path = Some("/Users/username/.m2/repository/org/springframework/spring-webmvc/5.3.20/spring-webmvc-5.3.20.jar".to_string());
        for i in 0..500 {
            classes.push(ClassInfo {
                fqn: format!("org.springframework.web.servlet.HandlerAdapter{}", i),
                simple_name: format!("HandlerAdapter{}", i),
                package_name: "org.springframework.web.servlet".to_string(),
                kind: ClassKind::Class,
                super_class: Some("java.lang.Object".to_string()),
                interfaces: vec!["org.springframework.web.servlet.Handler".to_string()],
                methods: vec![
                    MethodInfo {
                        name: "supports".to_string(),
                        return_type: TypeSignature::Boolean,
                        parameters: vec![("handler".to_string(), TypeSignature::Class("java.lang.Object".to_string()))],
                        is_static: false,
                        is_public: true,
                        is_constructor: false,
                        javadoc: None,
                        body: None,
                    },
                    MethodInfo {
                        name: "handle".to_string(),
                        return_type: TypeSignature::Void,
                        parameters: vec![
                            ("request".to_string(), TypeSignature::Class("javax.servlet.http.HttpServletRequest".to_string())),
                            ("response".to_string(), TypeSignature::Class("javax.servlet.http.HttpServletResponse".to_string())),
                        ],
                        is_static: false,
                        is_public: true,
                        is_constructor: false,
                        javadoc: None,
                        body: None,
                    },
                ],
                fields: vec![
                    crate::model::FieldInfo {
                        name: "order".to_string(),
                        field_type: TypeSignature::Int,
                        is_static: false,
                        is_public: true,
                        is_final: false,
                    }
                ],
                source_path: path.clone(),
                is_public: true,
            });
        }

        let json_bytes = serde_json::to_vec(&classes).unwrap();
        let archive_bytes = serialize_archive(&classes).unwrap();
        println!("JSON size: {} bytes, Compressed Archive size: {} bytes (Ratio: {:.2}%)", 
            json_bytes.len(), archive_bytes.len(), (archive_bytes.len() as f64 / json_bytes.len() as f64) * 100.0);

        // Verify that P0+P1 achieves > 80% size reduction over uncompressed JSON
        assert!(
            archive_bytes.len() < json_bytes.len() / 5,
            "JSON size: {}, Archive size: {}",
            json_bytes.len(),
            archive_bytes.len()
        );
    }
}
