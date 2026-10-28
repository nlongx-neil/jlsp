use std::collections::HashMap;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use bincode::Options;
use serde::{Deserialize, Serialize};

use crate::classpath::store::ArchivedStore;
use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo};

/// Upper bound on decoded cache size, so a corrupted file cannot trigger a huge allocation.
const CACHE_DECODE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;

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

/// Borrowed view of [`CompactClassInfo`] so saving a cache never deep-clones
/// methods/fields/interfaces of the whole classpath.
#[derive(Serialize)]
struct CompactClassInfoRef<'a> {
    fqn: &'a str,
    kind: &'a ClassKind,
    super_class: Option<&'a str>,
    interfaces: &'a [String],
    methods: &'a [MethodInfo],
    fields: &'a [FieldInfo],
    path_idx: Option<u32>,
    is_public: bool,
}

/// Borrowed view of [`CompactClassArchive`]; serializes to the exact same
/// bincode layout, so old caches remain readable and new caches stay compatible.
#[derive(Serialize)]
struct CompactClassArchiveRef<'a> {
    version: u32,
    paths: Vec<&'a str>,
    classes: Vec<CompactClassInfoRef<'a>>,
}

impl<'a> CompactClassArchiveRef<'a> {
    fn from_class_refs(
        classes: impl IntoIterator<Item = &'a ClassInfo>,
        count_hint: usize,
    ) -> Self {
        let mut paths: Vec<&'a str> = Vec::new();
        let mut path_map: HashMap<&'a str, u32> = HashMap::new();
        let mut compact_classes = Vec::with_capacity(count_hint);

        for cls in classes {
            let path_idx = cls.source_path.as_deref().map(|path| {
                if let Some(&idx) = path_map.get(path) {
                    idx
                } else {
                    let idx = paths.len() as u32;
                    paths.push(path);
                    path_map.insert(path, idx);
                    idx
                }
            });

            compact_classes.push(CompactClassInfoRef {
                fqn: &cls.fqn,
                kind: &cls.kind,
                super_class: cls.super_class.as_deref(),
                interfaces: &cls.interfaces,
                methods: &cls.methods,
                fields: &cls.fields,
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
}

/// Serialize classes to compact binary bytes compressed with zstd
pub fn serialize_archive(classes: &[ClassInfo]) -> Result<Vec<u8>, String> {
    serialize_archive_from_refs(classes.iter(), classes.len())
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
    let archive = CompactClassArchiveRef::from_class_refs(classes, count_hint);
    let mut out = Vec::new();
    {
        let mut encoder = zstd::Encoder::new(&mut out, 3).map_err(|e| format!("Zstd encode error: {e}"))?;
        bincode::serialize_into(&mut encoder, &archive).map_err(|e| format!("Bincode serialize error: {e}"))?;
        encoder.finish().map_err(|e| format!("Zstd encode error: {e}"))?;
    }
    Ok(out)
}

/// Save classes from iterator of references without cloning, streaming straight to disk
pub fn save_archive_file_from_refs<'a, P: AsRef<Path>>(path: P, classes: impl IntoIterator<Item = &'a ClassInfo>, count_hint: usize) -> Result<(), String> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let archive = CompactClassArchiveRef::from_class_refs(classes, count_hint);
    let file = File::create(path).map_err(|e| format!("Disk write error: {e}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = zstd::Encoder::new(writer, 3).map_err(|e| format!("Zstd encode error: {e}"))?;
    bincode::serialize_into(&mut encoder, &archive).map_err(|e| format!("Bincode serialize error: {e}"))?;
    let mut writer = encoder.finish().map_err(|e| format!("Zstd encode error: {e}"))?;
    writer.flush().map_err(|e| format!("Disk write error: {e}"))?;
    Ok(())
}

/// Save classes to an archive file
pub fn save_archive_file<P: AsRef<Path>>(path: P, classes: &[ClassInfo]) -> Result<(), String> {
    save_archive_file_from_refs(path, classes.iter(), classes.len())
}

/// Load classes from an archive file, decompressing and decoding as a stream so
/// neither the compressed bytes nor the fully decompressed payload are held in memory.
pub fn load_archive_file<P: AsRef<Path>>(path: P) -> Result<Vec<ClassInfo>, String> {
    let p = path.as_ref();
    if !p.exists() {
        return Ok(Vec::new());
    }
    let mut reader = BufReader::new(File::open(p).map_err(|e| format!("Disk read error: {e}"))?);
    let is_zstd = reader
        .fill_buf()
        .map(|buf| buf.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]))
        .unwrap_or(false);
    if is_zstd {
        let mut decoder = zstd::Decoder::new(reader).map_err(|e| format!("Zstd decode error: {e}"))?;
        let archive: CompactClassArchive = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_limit(CACHE_DECODE_LIMIT)
            .deserialize_from(&mut decoder)
            .map_err(|e| format!("Bincode deserialize error: {e}"))?;
        return Ok(archive.into_classes());
    }
    // Legacy raw bincode / JSON fallback (small files only)
    let mut data = Vec::new();
    reader.read_to_end(&mut data).map_err(|e| format!("Disk read error: {e}"))?;
    deserialize_archive(&data)
}


/// Magic prefix identifying the pooled (P2) store cache layout.
const STORE_MAGIC: &[u8; 8] = b"JLSPCS2\0";

/// Save a pooled [`ArchivedStore`] snapshot, streaming straight to disk.
pub fn save_store_archive_file<P: AsRef<Path>>(path: P, archive: &ArchivedStore) -> Result<(), String> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = File::create(path).map_err(|e| format!("Disk write error: {e}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = zstd::Encoder::new(writer, 3).map_err(|e| format!("Zstd encode error: {e}"))?;
    encoder
        .write_all(STORE_MAGIC)
        .map_err(|e| format!("Disk write error: {e}"))?;
    bincode::serialize_into(&mut encoder, archive)
        .map_err(|e| format!("Bincode serialize error: {e}"))?;
    let mut writer = encoder.finish().map_err(|e| format!("Zstd encode error: {e}"))?;
    writer.flush().map_err(|e| format!("Disk write error: {e}"))?;
    Ok(())
}

/// Load a pooled [`ArchivedStore`] snapshot as a stream.
/// Returns `Ok(None)` when the file is not in the pooled format (e.g. an older cache).
pub fn load_store_archive_file<P: AsRef<Path>>(path: P) -> Result<Option<ArchivedStore>, String> {
    let p = path.as_ref();
    if !p.exists() {
        return Ok(None);
    }
    let mut reader = BufReader::new(File::open(p).map_err(|e| format!("Disk read error: {e}"))?);
    let is_zstd = reader
        .fill_buf()
        .map(|buf| buf.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]))
        .unwrap_or(false);
    if !is_zstd {
        return Ok(None);
    }
    let mut decoder = zstd::Decoder::new(reader).map_err(|e| format!("Zstd decode error: {e}"))?;
    let mut magic = [0u8; 8];
    if decoder.read_exact(&mut magic).is_err() || &magic != STORE_MAGIC {
        return Ok(None);
    }
    let archive = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(CACHE_DECODE_LIMIT)
        .deserialize_from(&mut decoder)
        .map_err(|e| format!("Bincode deserialize error: {e}"))?;
    Ok(Some(archive))
}

/// Magic prefix for the lazy names-index cache (`index.names.zst`).
const NAMES_MAGIC: &[u8; 8] = b"JLSPNM1\0";

/// Save `(fqn, jar_path)` pairs for lazy startup indexing.
pub fn save_names_file<P: AsRef<Path>>(path: P, entries: &[(String, String)]) -> Result<(), String> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = File::create(path).map_err(|e| format!("Disk write error: {e}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = zstd::Encoder::new(writer, 3).map_err(|e| format!("Zstd encode error: {e}"))?;
    encoder
        .write_all(NAMES_MAGIC)
        .map_err(|e| format!("Disk write error: {e}"))?;
    bincode::serialize_into(&mut encoder, entries)
        .map_err(|e| format!("Bincode serialize error: {e}"))?;
    let mut writer = encoder.finish().map_err(|e| format!("Zstd encode error: {e}"))?;
    writer.flush().map_err(|e| format!("Disk write error: {e}"))?;
    Ok(())
}

/// Load `(fqn, jar_path)` pairs saved by [`save_names_file`].
/// Returns `Ok(None)` when the file is missing or not in the names format.
pub fn load_names_file<P: AsRef<Path>>(path: P) -> Result<Option<Vec<(String, String)>>, String> {
    let p = path.as_ref();
    if !p.exists() {
        return Ok(None);
    }
    let mut reader = BufReader::new(File::open(p).map_err(|e| format!("Disk read error: {e}"))?);
    let is_zstd = reader
        .fill_buf()
        .map(|buf| buf.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]))
        .unwrap_or(false);
    if !is_zstd {
        return Ok(None);
    }
    let mut decoder = zstd::Decoder::new(reader).map_err(|e| format!("Zstd decode error: {e}"))?;
    let mut magic = [0u8; 8];
    if decoder.read_exact(&mut magic).is_err() || &magic != NAMES_MAGIC {
        return Ok(None);
    }
    let entries = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(CACHE_DECODE_LIMIT)
        .deserialize_from(&mut decoder)
        .map_err(|e| format!("Bincode deserialize error: {e}"))?;
    Ok(Some(entries))
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
