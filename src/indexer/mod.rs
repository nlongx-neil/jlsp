use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::model::ClassInfo;
use crate::syntax::JavaAst;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSymbol {
    pub fqn: String,
    pub simple_name: String,
    pub path: PathBuf,
    pub class_info: ClassInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyBatisXmlStatement {
    pub id: String,
    pub tag: String, // select, insert, update, delete
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyBatisXmlMapping {
    pub namespace: String,
    pub xml_path: PathBuf,
    pub statements: Vec<MyBatisXmlStatement>,
}

#[derive(Default)]
pub struct WorkspaceIndex {
    /// Mapping: FQN -> SourceSymbol
    symbols_by_fqn: RwLock<HashMap<String, SourceSymbol>>,
    /// Mapping: SimpleName -> List of FQNs
    symbols_by_simple_name: RwLock<HashMap<String, Vec<String>>>,
    /// Mapping: Mapper FQN / SimpleName -> MyBatisXmlMapping
    mybatis_mappers: RwLock<HashMap<String, MyBatisXmlMapping>>,
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self {
            symbols_by_fqn: RwLock::new(HashMap::new()),
            symbols_by_simple_name: RwLock::new(HashMap::new()),
            mybatis_mappers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn index_file(&self, path: &Path, content: &str) {
        if path.extension().and_then(|e| e.to_str()) == Some("xml") {
            self.index_mybatis_xml(path, content).await;
            return;
        }

        if let Some(ast) = JavaAst::parse(Arc::new(content.to_string())) {
            let classes = ast.extract_classes(Some(path.to_string_lossy().to_string()));
            let mut fqn_map = self.symbols_by_fqn.write().await;
            let mut name_map = self.symbols_by_simple_name.write().await;

            for cls in classes {
                let symbol = SourceSymbol {
                    fqn: cls.fqn.clone(),
                    simple_name: cls.simple_name.clone(),
                    path: path.to_path_buf(),
                    class_info: cls,
                };
                name_map
                    .entry(symbol.simple_name.clone())
                    .or_default()
                    .push(symbol.fqn.clone());
                fqn_map.insert(symbol.fqn.clone(), symbol);
            }
        }
    }

    pub async fn index_mybatis_xml(&self, path: &Path, content: &str) {
        // Fast robust XML scanning without heavy dependencies
        if !content.contains("<mapper") {
            return;
        }

        let mut namespace = String::new();
        let mut statements = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.contains("<mapper") && trimmed.contains("namespace=") {
                if let Some(ns_part) = trimmed.split("namespace=").nth(1) {
                    let ns = ns_part
                        .trim_start_matches(|c| c == '"' || c == '\'')
                        .split(|c| c == '"' || c == '\'')
                        .next()
                        .unwrap_or_default();
                    namespace = ns.to_string();
                }
            }

            for tag in &["select", "insert", "update", "delete"] {
                let tag_prefix = format!("<{}", tag);
                if trimmed.starts_with(&tag_prefix) && trimmed.contains("id=") {
                    if let Some(id_part) = trimmed.split("id=").nth(1) {
                        let id = id_part
                            .trim_start_matches(|c| c == '"' || c == '\'')
                            .split(|c| c == '"' || c == '\'')
                            .next()
                            .unwrap_or_default();
                        if !id.is_empty() {
                            statements.push(MyBatisXmlStatement {
                                id: id.to_string(),
                                tag: tag.to_string(),
                                line: line_idx as u32,
                            });
                        }
                    }
                }
            }
        }

        if !namespace.is_empty() {
            let simple_ns = namespace.split('.').last().unwrap_or(&namespace).to_string();
            let mapping = MyBatisXmlMapping {
                namespace: namespace.clone(),
                xml_path: path.to_path_buf(),
                statements,
            };

            let mut map = self.mybatis_mappers.write().await;
            map.insert(namespace, mapping.clone());
            map.insert(simple_ns, mapping);
        }
    }

    pub async fn scan_workspace(&self, root: &Path) {
        let cache_path = root.join(".jlsp").join("index.bin");
        if self.try_load_cache(&cache_path).await {
            info!("Successfully restored workspace index from disk cache: {:?}", cache_path);
            return;
        }

        info!("Indexing Java source and XML files in workspace: {:?}", root);
        let mut count = 0;
        for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            let ext = p.extension().and_then(|ext| ext.to_str());
            if (ext == Some("java") || ext == Some("xml"))
                && let Ok(content) = fs::read_to_string(p) {
                    self.index_file(p, &content).await;
                    count += 1;
                }
        }
        info!("Indexed {} source/mapping files in workspace.", count);

        // Save index to disk cache for fast warm-starts
        self.save_cache(&cache_path).await;
    }

    pub async fn try_load_cache(&self, cache_file: &Path) -> bool {
        if !cache_file.exists() {
            return false;
        }
        if let Ok(bytes) = fs::read(cache_file) {
            if let Ok(symbols) = bincode::deserialize::<Vec<SourceSymbol>>(&bytes) {
                let mut fqn_map = self.symbols_by_fqn.write().await;
                let mut name_map = self.symbols_by_simple_name.write().await;
                for sym in symbols {
                    name_map
                        .entry(sym.simple_name.clone())
                        .or_default()
                        .push(sym.fqn.clone());
                    fqn_map.insert(sym.fqn.clone(), sym);
                }
                return true;
            }
        }
        false
    }

    pub async fn save_cache(&self, cache_file: &Path) {
        if let Some(parent) = cache_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let fqn_map = self.symbols_by_fqn.read().await;
        let symbols: Vec<SourceSymbol> = fqn_map.values().cloned().collect();
        if let Ok(encoded) = bincode::serialize(&symbols) {
            let _ = fs::write(cache_file, encoded);
            info!("Persisted {} symbols to disk cache: {:?}", symbols.len(), cache_file);
        }
    }

    pub async fn find_by_simple_name(&self, simple_name: &str) -> Vec<SourceSymbol> {
        let name_map = self.symbols_by_simple_name.read().await;
        let fqn_map = self.symbols_by_fqn.read().await;

        if let Some(fqns) = name_map.get(simple_name) {
            fqns.iter().filter_map(|fqn| fqn_map.get(fqn).cloned()).collect()
        } else {
            Vec::new()
        }
    }

    pub async fn search_symbols(&self, query: &str) -> Vec<SourceSymbol> {
        let fqn_map = self.symbols_by_fqn.read().await;
        let query_lower = query.to_lowercase();
        fqn_map
            .values()
            .filter(|s| s.simple_name.to_lowercase().contains(&query_lower) || s.fqn.to_lowercase().contains(&query_lower))
            .cloned()
            .collect()
    }

    pub async fn all_source_files(&self) -> Vec<PathBuf> {
        let fqn_map = self.symbols_by_fqn.read().await;
        let mut paths: Vec<PathBuf> = fqn_map.values().map(|s| s.path.clone()).collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Search for classes implementing or extending target_name across the whole workspace
    pub async fn find_implementations(&self, target_name: &str) -> Vec<SourceSymbol> {
        let fqn_map = self.symbols_by_fqn.read().await;
        fqn_map
            .values()
            .filter(|s| {
                s.class_info.super_class.as_deref() == Some(target_name)
                    || s.class_info.interfaces.iter().any(|i| i == target_name || i.ends_with(&format!(".{target_name}")))
            })
            .cloned()
            .collect()
    }

    /// Lookup MyBatis XML mapping by mapper FQN or interface simple name
    pub async fn find_mybatis_mapping(&self, mapper_name: &str) -> Option<MyBatisXmlMapping> {
        let map = self.mybatis_mappers.read().await;
        map.get(mapper_name).cloned()
    }

    /// Lookup specific XML statement location: returns (xml_path, line_number)
    pub async fn find_mybatis_statement(&self, mapper_name: &str, method_name: &str) -> Option<(PathBuf, u32)> {
        let map = self.mybatis_mappers.read().await;
        let mapping = map.get(mapper_name)?;
        for stmt in &mapping.statements {
            if stmt.id == method_name {
                return Some((mapping.xml_path.clone(), stmt.line));
            }
        }
        // Fallback to the XML root if method not found
        Some((mapping.xml_path.clone(), 0))
    }

    /// Lookup method location in a specific class or across workspace
    pub async fn find_method_location(&self, class_name: Option<&str>, method_name: &str) -> Option<(PathBuf, u32)> {
        let fqn_map = self.symbols_by_fqn.read().await;
        for symbol in fqn_map.values() {
            if let Some(target_cls) = class_name {
                if symbol.simple_name != target_cls && symbol.fqn != target_cls {
                    continue;
                }
            }
            if symbol.class_info.methods.iter().any(|m| m.name == method_name) {
                // If the class contains this method, find the line from source if available
                if let Ok(content) = fs::read_to_string(&symbol.path) {
                    for (line_no, line_str) in content.lines().enumerate() {
                        if line_str.contains(method_name) && (line_str.contains("(") || line_str.contains("void") || line_str.contains("public") || line_str.contains("private") || line_str.contains("protected")) {
                            return Some((symbol.path.clone(), line_no as u32));
                        }
                    }
                }
                return Some((symbol.path.clone(), 0));
            }
        }
        None
    }
}

