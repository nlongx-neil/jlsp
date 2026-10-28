//! Compact, pooled in-memory storage for classpath classes.
//!
//! Instead of keeping one [`ClassInfo`] (with owned `String`s, boxed type
//! signatures and per-method `Vec`s) per class, the whole classpath is stored
//! as flat record tables whose strings and types are interned into shared
//! pools. A class is materialized back into [`ClassInfo`] on demand only.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo, TypeSignature};

/// Sentinel used in place of an optional pooled id.
pub const NO_ID: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompactType {
    Void,
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    /// Class type; payload is a name id.
    Class(u32),
    /// Array type; payload is the element type id.
    Array(u32),
    /// Type variable; payload is a name id.
    TypeVariable(u32),
    Wildcard,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ParamRecord {
    pub name: u32,
    pub typ: u32,
}

const METHOD_STATIC: u8 = 1 << 0;
const METHOD_PUBLIC: u8 = 1 << 1;
const METHOD_CONSTRUCTOR: u8 = 1 << 2;
const METHOD_HAS_BODY: u8 = 1 << 3;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MethodRecord {
    pub name: u32,
    pub return_type: u32,
    pub params_start: u32,
    pub params_len: u32,
    /// Text id for the javadoc, or [`NO_ID`].
    pub javadoc: u32,
    pub flags: u8,
}

const FIELD_STATIC: u8 = 1 << 0;
const FIELD_PUBLIC: u8 = 1 << 1;
const FIELD_FINAL: u8 = 1 << 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FieldRecord {
    pub name: u32,
    pub typ: u32,
    pub flags: u8,
}

const CLASS_PUBLIC: u8 = 1 << 0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ClassRecord {
    pub fqn: u32,
    pub kind: u8,
    pub super_class: u32,
    pub interfaces_start: u32,
    pub interfaces_len: u32,
    pub methods_start: u32,
    pub methods_len: u32,
    pub fields_start: u32,
    pub fields_len: u32,
    pub source_path: u32,
    pub flags: u8,
}

fn encode_kind(kind: &ClassKind) -> u8 {
    match kind {
        ClassKind::Class => 0,
        ClassKind::Interface => 1,
        ClassKind::Enum => 2,
        ClassKind::Record => 3,
        ClassKind::Annotation => 4,
    }
}

fn decode_kind(kind: u8) -> ClassKind {
    match kind {
        1 => ClassKind::Interface,
        2 => ClassKind::Enum,
        3 => ClassKind::Record,
        4 => ClassKind::Annotation,
        _ => ClassKind::Class,
    }
}

/// Plain-data snapshot of a [`ClassStore`], used as the on-disk cache layout.
/// All ids are relative to the pools held in this same struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchivedStore {
    pub names: Vec<String>,
    pub texts: Vec<String>,
    pub types: Vec<CompactType>,
    pub interfaces: Vec<u32>,
    pub classes: Vec<ClassRecord>,
    pub methods: Vec<MethodRecord>,
    pub params: Vec<ParamRecord>,
    pub fields: Vec<FieldRecord>,
}

#[derive(Default)]
struct ArchiveBuilder {
    data: ArchivedStore,
    name_ids: HashMap<String, u32>,
    text_ids: HashMap<String, u32>,
    type_ids: HashMap<CompactType, u32>,
}

impl ArchiveBuilder {
    fn intern_name(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let id = self.data.names.len() as u32;
        self.data.names.push(name.to_string());
        self.name_ids.insert(name.to_string(), id);
        id
    }

    fn intern_text(&mut self, text: &str) -> u32 {
        if let Some(&id) = self.text_ids.get(text) {
            return id;
        }
        let id = self.data.texts.len() as u32;
        self.data.texts.push(text.to_string());
        self.text_ids.insert(text.to_string(), id);
        id
    }

    fn intern_type_node(&mut self, node: CompactType) -> u32 {
        if let Some(&id) = self.type_ids.get(&node) {
            return id;
        }
        let id = self.data.types.len() as u32;
        self.data.types.push(node);
        self.type_ids.insert(node, id);
        id
    }

    fn intern_fqn_type(&mut self, fqn: &str) -> u32 {
        let name = self.intern_name(fqn);
        self.intern_type_node(CompactType::Class(name))
    }

    /// Copy a type id from a live store into this archive, interning its pools.
    fn copy_type(&mut self, store: &ClassStore, id: u32) -> u32 {
        match store.types[id as usize] {
            CompactType::Class(name) => {
                let name = self.intern_name(store.name_str(name));
                self.intern_type_node(CompactType::Class(name))
            }
            CompactType::TypeVariable(name) => {
                let name = self.intern_name(store.name_str(name));
                self.intern_type_node(CompactType::TypeVariable(name))
            }
            CompactType::Array(elem) => {
                let elem = self.copy_type(store, elem);
                self.intern_type_node(CompactType::Array(elem))
            }
            other => self.intern_type_node(other),
        }
    }

    fn intern_type(&mut self, ty: &TypeSignature) -> u32 {
        match ty {
            TypeSignature::Void => self.intern_type_node(CompactType::Void),
            TypeSignature::Boolean => self.intern_type_node(CompactType::Boolean),
            TypeSignature::Byte => self.intern_type_node(CompactType::Byte),
            TypeSignature::Char => self.intern_type_node(CompactType::Char),
            TypeSignature::Short => self.intern_type_node(CompactType::Short),
            TypeSignature::Int => self.intern_type_node(CompactType::Int),
            TypeSignature::Long => self.intern_type_node(CompactType::Long),
            TypeSignature::Float => self.intern_type_node(CompactType::Float),
            TypeSignature::Double => self.intern_type_node(CompactType::Double),
            TypeSignature::Class(fqn) => self.intern_fqn_type(fqn),
            TypeSignature::Array(inner) => {
                let elem = self.intern_type(inner);
                self.intern_type_node(CompactType::Array(elem))
            }
            TypeSignature::TypeVariable(name) => {
                let name = self.intern_name(name);
                self.intern_type_node(CompactType::TypeVariable(name))
            }
            TypeSignature::Wildcard => self.intern_type_node(CompactType::Wildcard),
            TypeSignature::Unknown => self.intern_type_node(CompactType::Unknown),
        }
    }

    fn push_class(&mut self, class: &ClassInfo) {
        let fqn = self.intern_name(&class.fqn);
        let super_class = match &class.super_class {
            Some(fqn) => self.intern_fqn_type(fqn),
            None => NO_ID,
        };

        let interfaces_start = self.data.interfaces.len() as u32;
        let iface_ids: Vec<u32> = class
            .interfaces
            .iter()
            .map(|iface| self.intern_fqn_type(iface))
            .collect();
        self.data.interfaces.extend(iface_ids);
        let interfaces_len = class.interfaces.len() as u32;

        let methods_start = self.data.methods.len() as u32;
        for method in &class.methods {
            let name = self.intern_name(&method.name);
            let return_type = self.intern_type(&method.return_type);
            let params_start = self.data.params.len() as u32;
            for (pname, ptype) in &method.parameters {
                let pname_id = if pname.is_empty() {
                    NO_ID
                } else {
                    self.intern_name(pname)
                };
                let ptyp = self.intern_type(ptype);
                self.data.params.push(ParamRecord { name: pname_id, typ: ptyp });
            }
            let params_len = self.data.params.len() as u32 - params_start;
            let mut flags = 0u8;
            if method.is_static {
                flags |= METHOD_STATIC;
            }
            if method.is_public {
                flags |= METHOD_PUBLIC;
            }
            if method.is_constructor {
                flags |= METHOD_CONSTRUCTOR;
            }
            if method.body.is_some() {
                flags |= METHOD_HAS_BODY;
            }
            let javadoc = match &method.javadoc {
                Some(text) => self.intern_text(text),
                None => NO_ID,
            };
            self.data.methods.push(MethodRecord {
                name,
                return_type,
                params_start,
                params_len,
                javadoc,
                flags,
            });
        }
        let methods_len = class.methods.len() as u32;

        let fields_start = self.data.fields.len() as u32;
        for field in &class.fields {
            let name = self.intern_name(&field.name);
            let typ = self.intern_type(&field.field_type);
            let mut flags = 0u8;
            if field.is_static {
                flags |= FIELD_STATIC;
            }
            if field.is_public {
                flags |= FIELD_PUBLIC;
            }
            if field.is_final {
                flags |= FIELD_FINAL;
            }
            self.data.fields.push(FieldRecord { name, typ, flags });
        }
        let fields_len = class.fields.len() as u32;

        let source_path = match &class.source_path {
            Some(path) => self.intern_name(path),
            None => NO_ID,
        };
        let flags = if class.is_public { CLASS_PUBLIC } else { 0 };

        self.data.classes.push(ClassRecord {
            fqn,
            kind: encode_kind(&class.kind),
            super_class,
            interfaces_start,
            interfaces_len,
            methods_start,
            methods_len,
            fields_start,
            fields_len,
            source_path,
            flags,
        });
    }
}

impl ArchivedStore {
    /// Intern a batch of materialized classes into a flat snapshot.
    pub fn from_classes(classes: &[ClassInfo]) -> Self {
        let mut builder = ArchiveBuilder::default();
        for class in classes {
            builder.push_class(class);
        }
        builder.data
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }
}

/// Flat, pooled classpath storage with lookup indexes.
#[derive(Default)]
pub struct ClassStore {
    names: Vec<Arc<str>>,
    name_ids: HashMap<Arc<str>, u32>,
    texts: Vec<Arc<str>>,
    types: Vec<CompactType>,
    type_ids: HashMap<CompactType, u32>,
    interfaces: Vec<u32>,
    classes: Vec<ClassRecord>,
    methods: Vec<MethodRecord>,
    params: Vec<ParamRecord>,
    fields: Vec<FieldRecord>,
    by_fqn: HashMap<Arc<str>, u32>,
    by_simple: HashMap<Arc<str>, Vec<u32>>,
    methods_by_name: HashMap<Arc<str>, Vec<u32>>,
    /// Parent fqn (super class or interface) -> direct subtype class indexes.
    subclasses: HashMap<Arc<str>, Vec<u32>>,
}

impl ClassStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    fn intern_name(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let arc: Arc<str> = Arc::from(name);
        let id = self.names.len() as u32;
        self.names.push(arc.clone());
        self.name_ids.insert(arc, id);
        id
    }

    fn intern_text(&mut self, text: &str) -> u32 {
        let arc: Arc<str> = Arc::from(text);
        let id = self.texts.len() as u32;
        self.texts.push(arc);
        id
    }

    fn intern_type_node(&mut self, node: CompactType) -> u32 {
        if let Some(&id) = self.type_ids.get(&node) {
            return id;
        }
        let id = self.types.len() as u32;
        self.types.push(node);
        self.type_ids.insert(node, id);
        id
    }

    fn name_str(&self, id: u32) -> &str {
        if id == NO_ID {
            ""
        } else {
            self.names.get(id as usize).map(|s| &**s).unwrap_or("")
        }
    }

    pub fn class_index(&self, fqn: &str) -> Option<u32> {
        self.by_fqn.get(fqn).copied()
    }

    pub fn contains(&self, fqn: &str) -> bool {
        self.by_fqn.contains_key(fqn)
    }

    pub fn fqn_of(&self, idx: u32) -> &str {
        &self.names[self.classes[idx as usize].fqn as usize]
    }

    pub fn class_source_path(&self, idx: u32) -> Option<&str> {
        let rec = &self.classes[idx as usize];
        if rec.source_path == NO_ID {
            None
        } else {
            Some(self.name_str(rec.source_path))
        }
    }

    pub fn simple_class_indices(&self, simple_name: &str) -> Vec<u32> {
        self.by_simple
            .get(simple_name)
            .cloned()
            .unwrap_or_default()
    }

    /// `(fqn, source_path)` for every stored class (cheap; no materialization).
    pub fn fqn_path_pairs(&self) -> Vec<(String, Option<String>)> {
        self.classes
            .iter()
            .map(|rec| {
                let fqn = self.name_str(rec.fqn).to_string();
                let path = if rec.source_path == NO_ID {
                    None
                } else {
                    Some(self.name_str(rec.source_path).to_string())
                };
                (fqn, path)
            })
            .collect()
    }

    /// Unique source paths (jar paths) referenced by stored classes.
    pub fn source_paths(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for rec in &self.classes {
            if rec.source_path == NO_ID {
                continue;
            }
            let path = self.name_str(rec.source_path);
            if seen.insert(path.to_string()) {
                out.push(path.to_string());
            }
        }
        out
    }

    /// Lightweight `(simple_name, fqn)` list for completion without materializing classes.
    pub fn summaries(&self) -> Vec<(String, String)> {
        self.classes
            .iter()
            .map(|rec| {
                let fqn = self.name_str(rec.fqn);
                (simple_name_of(fqn).to_string(), fqn.to_string())
            })
            .collect()
    }

    /// Append a snapshot, remapping its pool ids into this store.
    /// Classes whose fqn already exists are skipped (first definition wins).
    pub fn append_archive(&mut self, archive: ArchivedStore) {
        if archive.classes.is_empty() {
            return;
        }
        // Reserve up front so growing these tables never has to copy the whole
        // (multi-megabyte) table mid-append.
        self.names.reserve(archive.names.len());
        self.texts.reserve(archive.texts.len());
        self.types.reserve(archive.types.len());
        self.interfaces.reserve(archive.interfaces.len());
        self.classes.reserve(archive.classes.len());
        self.methods.reserve(archive.methods.len());
        self.params.reserve(archive.params.len());
        self.fields.reserve(archive.fields.len());
        self.by_fqn.reserve(archive.classes.len());

        let ArchivedStore {
            names,
            texts,
            types,
            interfaces,
            classes,
            methods,
            params,
            fields,
        } = archive;

        let mut name_map = Vec::with_capacity(names.len());
        for name in &names {
            name_map.push(self.intern_name(name));
        }
        let mut text_map = Vec::with_capacity(texts.len());
        for text in &texts {
            text_map.push(self.intern_text(text));
        }
        // Types are interned in dependency order (array element before array),
        // so the element type id is always already remapped.
        let mut type_map = Vec::with_capacity(types.len());
        for node in &types {
            let mapped = match *node {
                CompactType::Class(id) => CompactType::Class(name_map[id as usize]),
                CompactType::TypeVariable(id) => CompactType::TypeVariable(name_map[id as usize]),
                CompactType::Array(id) => CompactType::Array(type_map[id as usize]),
                other => other,
            };
            type_map.push(self.intern_type_node(mapped));
        }
        let map_name = |id: u32, map: &Vec<u32>| -> u32 {
            if id == NO_ID {
                NO_ID
            } else {
                map[id as usize]
            }
        };

        for rec in &classes {
            let fqn_id = name_map[rec.fqn as usize];
            let fqn = self.names[fqn_id as usize].clone();
            if self.by_fqn.contains_key(&*fqn) {
                continue;
            }

            let interfaces_start = self.interfaces.len() as u32;
            for i in rec.interfaces_start..rec.interfaces_start + rec.interfaces_len {
                let tid = interfaces[i as usize];
                self.interfaces.push(type_map[tid as usize]);
            }

            let methods_start = self.methods.len() as u32;
            for mi in rec.methods_start..rec.methods_start + rec.methods_len {
                let m = methods[mi as usize];
                let params_start = self.params.len() as u32;
                for pi in m.params_start..m.params_start + m.params_len {
                    let p = params[pi as usize];
                    let typ = type_map[p.typ as usize];
                    self.params.push(ParamRecord {
                        name: map_name(p.name, &name_map),
                        typ,
                    });
                }
                let params_len = self.params.len() as u32 - params_start;
                let name_id = name_map[m.name as usize];
                let method_idx = self.methods.len() as u32;
                self.methods.push(MethodRecord {
                    name: name_id,
                    return_type: type_map[m.return_type as usize],
                    params_start,
                    params_len,
                    javadoc: map_name(m.javadoc, &text_map),
                    flags: m.flags,
                });
                let name_arc: Arc<str> = self.names[name_id as usize].clone();
                self.methods_by_name
                    .entry(name_arc)
                    .or_default()
                    .push(method_idx);
            }
            let methods_len = self.methods.len() as u32 - methods_start;

            let fields_start = self.fields.len() as u32;
            for fi in rec.fields_start..rec.fields_start + rec.fields_len {
                let f = fields[fi as usize];
                self.fields.push(FieldRecord {
                    name: name_map[f.name as usize],
                    typ: type_map[f.typ as usize],
                    flags: f.flags,
                });
            }
            let fields_len = self.fields.len() as u32 - fields_start;

            let idx = self.classes.len() as u32;
            self.classes.push(ClassRecord {
                fqn: fqn_id,
                kind: rec.kind,
                super_class: if rec.super_class == NO_ID {
                    NO_ID
                } else {
                    type_map[rec.super_class as usize]
                },
                interfaces_start,
                interfaces_len: self.interfaces.len() as u32 - interfaces_start,
                methods_start,
                methods_len,
                fields_start,
                fields_len,
                source_path: map_name(rec.source_path, &name_map),
                flags: rec.flags,
            });

            self.by_fqn.insert(fqn.clone(), idx);
            self.by_simple
                .entry(Arc::from(simple_name_of(&fqn)))
                .or_default()
                .push(idx);

            // Reverse inheritance index
            if rec.super_class != NO_ID {
                let node = self.types[type_map[rec.super_class as usize] as usize];
                self.index_subclass(node, idx);
            }
            let iface_len = self.classes[idx as usize].interfaces_len;
            for i in interfaces_start..interfaces_start + iface_len {
                let node = self.types[self.interfaces[i as usize] as usize];
                self.index_subclass(node, idx);
            }
        }
    }

    fn index_subclass(&mut self, node: CompactType, idx: u32) {
        let name_id = match node {
            CompactType::Class(id) => id,
            _ => return,
        };
        let arc: Arc<str> = self.names[name_id as usize].clone();
        self.subclasses.entry(arc).or_default().push(idx);
    }

    fn materialize_type(&self, id: u32) -> TypeSignature {
        match self.types[id as usize] {
            CompactType::Void => TypeSignature::Void,
            CompactType::Boolean => TypeSignature::Boolean,
            CompactType::Byte => TypeSignature::Byte,
            CompactType::Char => TypeSignature::Char,
            CompactType::Short => TypeSignature::Short,
            CompactType::Int => TypeSignature::Int,
            CompactType::Long => TypeSignature::Long,
            CompactType::Float => TypeSignature::Float,
            CompactType::Double => TypeSignature::Double,
            CompactType::Class(name) => TypeSignature::Class(self.name_str(name).to_string()),
            CompactType::Array(elem) => TypeSignature::Array(Box::new(self.materialize_type(elem))),
            CompactType::TypeVariable(name) => {
                TypeSignature::TypeVariable(self.name_str(name).to_string())
            }
            CompactType::Wildcard => TypeSignature::Wildcard,
            CompactType::Unknown => TypeSignature::Unknown,
        }
    }

    fn materialize_method(&self, idx: u32) -> MethodInfo {
        let rec = &self.methods[idx as usize];
        let parameters = (rec.params_start..rec.params_start + rec.params_len)
            .map(|pi| {
                let p = &self.params[pi as usize];
                (
                    self.name_str(p.name).to_string(),
                    self.materialize_type(p.typ),
                )
            })
            .collect();
        MethodInfo {
            name: self.name_str(rec.name).to_string(),
            return_type: self.materialize_type(rec.return_type),
            parameters,
            is_static: rec.flags & METHOD_STATIC != 0,
            is_public: rec.flags & METHOD_PUBLIC != 0,
            is_constructor: rec.flags & METHOD_CONSTRUCTOR != 0,
            javadoc: if rec.javadoc == NO_ID {
                None
            } else {
                Some(self.texts[rec.javadoc as usize].to_string())
            },
            body: if rec.flags & METHOD_HAS_BODY != 0 {
                Some(String::new())
            } else {
                None
            },
        }
    }

    /// Materialize a class into the public [`ClassInfo`] view.
    pub fn materialize(&self, idx: u32) -> ClassInfo {
        let rec = &self.classes[idx as usize];
        let fqn = self.name_str(rec.fqn).to_string();
        let package_name = match fqn.rfind('.') {
            Some(pos) => fqn[..pos].to_string(),
            None => String::new(),
        };
        let simple_name = simple_name_of(&fqn).to_string();

        let super_class = if rec.super_class == NO_ID {
            None
        } else {
            match self.materialize_type(rec.super_class) {
                TypeSignature::Class(fqn) => Some(fqn),
                _ => None,
            }
        };

        let interfaces = (rec.interfaces_start..rec.interfaces_start + rec.interfaces_len)
            .filter_map(|i| match self.materialize_type(self.interfaces[i as usize]) {
                TypeSignature::Class(fqn) => Some(fqn),
                _ => None,
            })
            .collect();

        let methods = (rec.methods_start..rec.methods_start + rec.methods_len)
            .map(|i| self.materialize_method(i))
            .collect();

        let fields = (rec.fields_start..rec.fields_start + rec.fields_len)
            .map(|i| {
                let f = &self.fields[i as usize];
                FieldInfo {
                    name: self.name_str(f.name).to_string(),
                    field_type: self.materialize_type(f.typ),
                    is_static: f.flags & FIELD_STATIC != 0,
                    is_public: f.flags & FIELD_PUBLIC != 0,
                    is_final: f.flags & FIELD_FINAL != 0,
                }
            })
            .collect();

        ClassInfo {
            fqn,
            simple_name,
            package_name,
            kind: decode_kind(rec.kind),
            super_class,
            interfaces,
            methods,
            fields,
            source_path: if rec.source_path == NO_ID {
                None
            } else {
                Some(self.name_str(rec.source_path).to_string())
            },
            is_public: rec.flags & CLASS_PUBLIC != 0,
        }
    }

    /// Methods whose name matches, across the whole classpath (capped).
    pub fn search_methods(&self, name: &str, limit: usize) -> Vec<MethodInfo> {
        let Some(ids) = self.methods_by_name.get(name) else {
            return Vec::new();
        };
        ids.iter()
            .take(limit)
            .map(|&idx| self.materialize_method(idx))
            .collect()
    }

    /// Classes directly extending/implementing `target` (fqn or simple name).
    pub fn subclass_indices(&self, target: &str) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if let Some(list) = self.subclasses.get(target) {
            out.extend_from_slice(list);
        }
        let mut candidate_parents: Vec<Arc<str>> = Vec::new();
        if let Some(candidates) = self.by_simple.get(target) {
            for &idx in candidates {
                let fqn = self.names[self.classes[idx as usize].fqn as usize].clone();
                if &*fqn != target {
                    candidate_parents.push(fqn);
                }
            }
        }
        for parent in candidate_parents {
            if let Some(list) = self.subclasses.get(&*parent) {
                out.extend_from_slice(list);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Export classes into a snapshot. `filter` receives `(fqn, source_path)`.
    ///
    /// Records are copied straight across the flat tables; no [`ClassInfo`] is
    /// materialized, so exporting the whole classpath stays cheap.
    pub fn export<F>(&self, filter: F) -> ArchivedStore
    where
        F: Fn(&str, Option<&str>) -> bool,
    {
        let mut builder = ArchiveBuilder::default();
        for rec in &self.classes {
            let fqn_id = rec.fqn;
            let fqn = self.name_str(fqn_id);
            let path = if rec.source_path == NO_ID {
                None
            } else {
                Some(self.name_str(rec.source_path))
            };
            if !filter(fqn, path) {
                continue;
            }
            let fqn = builder.intern_name(fqn);
            let super_class = if rec.super_class == NO_ID {
                NO_ID
            } else {
                builder.copy_type(self, rec.super_class)
            };

            let interfaces_start = builder.data.interfaces.len() as u32;
            for i in rec.interfaces_start..rec.interfaces_start + rec.interfaces_len {
                let id = builder.copy_type(self, self.interfaces[i as usize]);
                builder.data.interfaces.push(id);
            }
            let interfaces_len = builder.data.interfaces.len() as u32 - interfaces_start;

            let methods_start = builder.data.methods.len() as u32;
            for mi in rec.methods_start..rec.methods_start + rec.methods_len {
                let m = &self.methods[mi as usize];
                let params_start = builder.data.params.len() as u32;
                for pi in m.params_start..m.params_start + m.params_len {
                    let p = &self.params[pi as usize];
                    let name = if p.name == NO_ID {
                        NO_ID
                    } else {
                        builder.intern_name(self.name_str(p.name))
                    };
                    let typ = builder.copy_type(self, p.typ);
                    builder.data.params.push(ParamRecord { name, typ });
                }
                let params_len = builder.data.params.len() as u32 - params_start;
                let name = builder.intern_name(self.name_str(m.name));
                let return_type = builder.copy_type(self, m.return_type);
                let javadoc = if m.javadoc == NO_ID {
                    NO_ID
                } else {
                    builder.intern_text(&self.texts[m.javadoc as usize])
                };
                builder.data.methods.push(MethodRecord {
                    name,
                    return_type,
                    params_start,
                    params_len,
                    javadoc,
                    flags: m.flags,
                });
            }
            let methods_len = builder.data.methods.len() as u32 - methods_start;

            let fields_start = builder.data.fields.len() as u32;
            for fi in rec.fields_start..rec.fields_start + rec.fields_len {
                let f = &self.fields[fi as usize];
                let name = builder.intern_name(self.name_str(f.name));
                let typ = builder.copy_type(self, f.typ);
                builder.data.fields.push(FieldRecord { name, typ, flags: f.flags });
            }
            let fields_len = builder.data.fields.len() as u32 - fields_start;

            let source_path = if rec.source_path == NO_ID {
                NO_ID
            } else {
                builder.intern_name(self.name_str(rec.source_path))
            };

            builder.data.classes.push(ClassRecord {
                fqn,
                kind: rec.kind,
                super_class,
                interfaces_start,
                interfaces_len,
                methods_start,
                methods_len,
                fields_start,
                fields_len,
                source_path,
                flags: rec.flags,
            });
        }
        builder.data
    }
}

pub(crate) fn simple_name_of(fqn: &str) -> &str {
    match fqn.rfind('.') {
        Some(pos) => &fqn[pos + 1..],
        None => fqn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_classes() -> Vec<ClassInfo> {
        vec![
            ClassInfo {
                fqn: "com.example.Base".to_string(),
                simple_name: "Base".to_string(),
                package_name: "com.example".to_string(),
                kind: ClassKind::Class,
                super_class: Some("java.lang.Object".to_string()),
                interfaces: vec!["java.io.Serializable".to_string()],
                methods: vec![MethodInfo {
                    name: "run".to_string(),
                    return_type: TypeSignature::Array(Box::new(TypeSignature::Class(
                        "java.lang.String".to_string(),
                    ))),
                    parameters: vec![
                        ("count".to_string(), TypeSignature::Int),
                        (String::new(), TypeSignature::TypeVariable("T".to_string())),
                    ],
                    is_static: true,
                    is_public: false,
                    is_constructor: false,
                    javadoc: Some("Runs.".to_string()),
                    body: Some(String::new()),
                }],
                fields: vec![FieldInfo {
                    name: "instances".to_string(),
                    field_type: TypeSignature::Long,
                    is_static: true,
                    is_public: true,
                    is_final: true,
                }],
                source_path: Some("/lib/base.jar".to_string()),
                is_public: true,
            },
            ClassInfo {
                fqn: "com.example.Derived".to_string(),
                simple_name: "Derived".to_string(),
                package_name: "com.example".to_string(),
                kind: ClassKind::Annotation,
                super_class: Some("com.example.Base".to_string()),
                interfaces: vec![],
                methods: vec![],
                fields: vec![],
                source_path: Some("/lib/base.jar".to_string()),
                is_public: false,
            },
        ]
    }

    #[test]
    fn pooled_store_roundtrip_preserves_classes() {
        let originals = sample_classes();
        let mut store = ClassStore::new();
        store.append_archive(ArchivedStore::from_classes(&originals));

        assert_eq!(store.class_count(), 2);
        for original in &originals {
            let idx = store.class_index(&original.fqn).expect("class must be indexed");
            let view = store.materialize(idx);
            assert_eq!(view.fqn, original.fqn);
            assert_eq!(view.simple_name, original.simple_name);
            assert_eq!(view.package_name, original.package_name);
            assert_eq!(view.kind, original.kind);
            assert_eq!(view.super_class, original.super_class);
            assert_eq!(view.interfaces, original.interfaces);
            assert_eq!(view.source_path, original.source_path);
            assert_eq!(view.is_public, original.is_public);
            assert_eq!(view.methods.len(), original.methods.len());
            assert_eq!(view.fields.len(), original.fields.len());
            for (got, want) in view.methods.iter().zip(&original.methods) {
                assert_eq!(got.name, want.name);
                assert_eq!(got.return_type, want.return_type);
                assert_eq!(got.parameters, want.parameters);
                assert_eq!(got.is_static, want.is_static);
                assert_eq!(got.is_public, want.is_public);
                assert_eq!(got.javadoc, want.javadoc);
                assert_eq!(got.body.is_some(), want.body.is_some());
            }
            for (got, want) in view.fields.iter().zip(&original.fields) {
                assert_eq!(got.name, want.name);
                assert_eq!(got.field_type, want.field_type);
                assert_eq!(got.is_static, want.is_static);
                assert_eq!(got.is_public, want.is_public);
                assert_eq!(got.is_final, want.is_final);
            }
        }

        assert_eq!(store.simple_class_indices("Base"), vec![0]);
        let derived = store.subclass_indices("com.example.Base");
        assert_eq!(derived, vec![1]);
        assert_eq!(store.search_methods("run", 8).len(), 1);
        assert_eq!(store.summaries()[0], ("Base".to_string(), "com.example.Base".to_string()));
    }

    #[test]
    fn duplicate_fqn_is_skipped() {
        let mut store = ClassStore::new();
        store.append_archive(ArchivedStore::from_classes(&sample_classes()));
        store.append_archive(ArchivedStore::from_classes(&sample_classes()));
        assert_eq!(store.class_count(), 2);
        assert_eq!(store.simple_class_indices("Base").len(), 1);
    }

    #[test]
    fn export_roundtrip_reimports_identically() {
        let originals = sample_classes();
        let mut store = ClassStore::new();
        store.append_archive(ArchivedStore::from_classes(&originals));

        let exported = store.export(|_, _| true);
        assert_eq!(exported.class_count(), 2);

        let mut reimported = ClassStore::new();
        reimported.append_archive(exported);
        let idx = reimported.class_index("com.example.Base").expect("must survive export");
        let view = reimported.materialize(idx);
        assert_eq!(view.methods[0].return_type, originals[0].methods[0].return_type);
        assert_eq!(view.methods[0].parameters, originals[0].methods[0].parameters);
        assert_eq!(view.methods[0].javadoc, originals[0].methods[0].javadoc);
        assert_eq!(view.fields[0].field_type, originals[0].fields[0].field_type);
    }
}
