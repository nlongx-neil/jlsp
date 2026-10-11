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
    /// Mapping: Config property key (e.g. "spring.datasource.url") -> (PathBuf, line_number)
    config_properties: RwLock<HashMap<String, (PathBuf, u32)>>,
}

/// Check if directory should be skipped during workspace scanning (e.g., .git, node_modules, classes)
pub fn is_ignored_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.file_type().is_dir() {
        let name = entry.file_name().to_string_lossy();
        if matches!(
            name.as_ref(),
            ".git" | ".svn" | ".hg" | "node_modules" | ".gradle" | ".idea" | ".vscode" | ".settings" | ".mvn"
                | "classes" | "test-classes" | "intermediates"
        ) {
            return true;
        }
    }
    false
}

impl WorkspaceIndex {
    pub fn new() -> Self {
        Self {
            symbols_by_fqn: RwLock::new(HashMap::new()),
            symbols_by_simple_name: RwLock::new(HashMap::new()),
            mybatis_mappers: RwLock::new(HashMap::new()),
            config_properties: RwLock::new(HashMap::new()),
        }
    }

    /// Remove all symbols, mappings, and configuration properties associated with a deleted/modified file path
    pub async fn remove_file(&self, path: &Path) {
        let mut fqn_map = self.symbols_by_fqn.write().await;
        let mut name_map = self.symbols_by_simple_name.write().await;

        let mut removed_fqns = Vec::new();
        let mut removed_simple_names = Vec::new();

        for (fqn, sym) in fqn_map.iter() {
            if sym.path == path {
                removed_fqns.push(fqn.clone());
                removed_simple_names.push((sym.simple_name.clone(), fqn.clone()));
            }
        }

        for fqn in removed_fqns {
            fqn_map.remove(&fqn);
        }

        for (simple_name, fqn) in removed_simple_names {
            if let Some(list) = name_map.get_mut(&simple_name) {
                list.retain(|item| item != &fqn);
                if list.is_empty() {
                    name_map.remove(&simple_name);
                }
            }
        }

        // Clean up MyBatis XML mappers if xml_path == path
        let mut mybatis_map = self.mybatis_mappers.write().await;
        mybatis_map.retain(|_, mapping| mapping.xml_path != path);

        // Clean up config properties if source path == path
        let mut config_map = self.config_properties.write().await;
        config_map.retain(|_, (prop_path, _)| prop_path != path);
    }

    pub async fn index_file(&self, path: &Path, content: &str) {
        self.remove_file(path).await;

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "properties" || ext == "yml" || ext == "yaml" {
                self.index_config_properties(path, content).await;
                return;
            }
            if ext == "xml" {
                self.index_mybatis_xml(path, content).await;
                return;
            }
        }

        if let Some(ast) = JavaAst::parse(Arc::new(content.to_string())) {
            let classes = ast.extract_classes(Some(path.to_string_lossy().to_string()));
            let mut fqn_map = self.symbols_by_fqn.write().await;
            let mut name_map = self.symbols_by_simple_name.write().await;

            for mut cls in classes {
                let lombok_generated = crate::semantic::lombok::process_lombok_annotations(&mut cls, content);
                let apt_generated = crate::semantic::apt_jdwp::AnnotationProcessorRunner::process_apt_annotations(&mut cls, content);
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

                // Also index generated APT & Lombok classes (e.g. MapStruct XxxImpl, JPA Xxx_, Lombok XxxBuilder)
                for gen_cls in apt_generated.into_iter().chain(lombok_generated) {
                    let gen_sym = SourceSymbol {
                        fqn: gen_cls.fqn.clone(),
                        simple_name: gen_cls.simple_name.clone(),
                        path: path.to_path_buf(),
                        class_info: gen_cls,
                    };
                    name_map
                        .entry(gen_sym.simple_name.clone())
                        .or_default()
                        .push(gen_sym.fqn.clone());
                    fqn_map.insert(gen_sym.fqn.clone(), gen_sym);
                }
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

    /// Index Spring Boot / Java application properties or YAML configuration files
    pub async fn index_config_properties(&self, path: &Path, content: &str) {
        let mut map = self.config_properties.write().await;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "properties" {
            for (line_idx, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('!') {
                    continue;
                }
                if let Some(eq_idx) = trimmed.find('=') {
                    let key = trimmed[..eq_idx].trim().to_string();
                    if !key.is_empty() {
                        map.insert(key, (path.to_path_buf(), line_idx as u32));
                    }
                }
            }
        } else if ext == "yml" || ext == "yaml" {
            let mut stack: Vec<(usize, String)> = Vec::new();
            for (line_idx, raw_line) in content.lines().enumerate() {
                let trimmed = raw_line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let indent = raw_line.len() - raw_line.trim_start().len();
                while let Some((prev_indent, _)) = stack.last() {
                    if *prev_indent >= indent {
                        stack.pop();
                    } else {
                        break;
                    }
                }
                if let Some(colon_idx) = trimmed.find(':') {
                    let key = trimmed[..colon_idx].trim();
                    let val = trimmed[colon_idx + 1..].trim();
                    if !key.is_empty() {
                        stack.push((indent, key.to_string()));
                        let full_key = stack.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>().join(".");
                        if !val.is_empty() {
                            map.insert(full_key, (path.to_path_buf(), line_idx as u32));
                        }
                    }
                }
            }
        }
    }

    /// Lookup configuration property definition location
    pub async fn find_config_property(&self, key: &str) -> Option<(PathBuf, u32)> {
        let map = self.config_properties.read().await;
        map.get(key).cloned()
    }

    /// List all indexed configuration properties
    pub async fn all_config_properties(&self) -> Vec<(String, PathBuf)> {
        let map = self.config_properties.read().await;
        map.iter().map(|(k, (p, _))| (k.clone(), p.clone())).collect()
    }

    pub async fn scan_workspace(&self, root: &Path) {
        let cache_path = root.join(".jlsp").join("index.bin");
        if self.try_load_cache(&cache_path).await {
            info!("Successfully restored workspace index from disk cache: {:?}", cache_path);
            return;
        }

        info!("Indexing Java source and XML files in workspace: {:?}", root);
        let mut count = 0;
        let mut collected_symbols = Vec::new();
        let mut xml_files: Vec<(PathBuf, String)> = Vec::new();

        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_entry(|e| !is_ignored_entry(e))
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            let ext = p.extension().and_then(|ext| ext.to_str());
            if ext == Some("xml") {
                if let Ok(content) = fs::read_to_string(p) {
                    xml_files.push((p.to_path_buf(), content));
                }
            } else if ext == Some("properties") || ext == Some("yml") || ext == Some("yaml") {
                if let Ok(content) = fs::read_to_string(p) {
                    self.index_config_properties(p, &content).await;
                }
            } else if ext == Some("java") {
                if let Ok(content) = fs::read_to_string(p) {
                    if let Some(ast) = JavaAst::parse(Arc::new(content.clone())) {
                        let classes = ast.extract_classes(Some(p.to_string_lossy().to_string()));
                        for mut cls in classes {
                            let lombok_generated = crate::semantic::lombok::process_lombok_annotations(&mut cls, &content);
                            let apt_generated = crate::semantic::apt_jdwp::AnnotationProcessorRunner::process_apt_annotations(&mut cls, &content);
                            collected_symbols.push(SourceSymbol {
                                fqn: cls.fqn.clone(),
                                simple_name: cls.simple_name.clone(),
                                path: p.to_path_buf(),
                                class_info: cls,
                            });
                            for gen_cls in apt_generated.into_iter().chain(lombok_generated) {
                                collected_symbols.push(SourceSymbol {
                                    fqn: gen_cls.fqn.clone(),
                                    simple_name: gen_cls.simple_name.clone(),
                                    path: p.to_path_buf(),
                                    class_info: gen_cls,
                                });
                            }
                        }
                    }
                    count += 1;
                }
            }
        }

        // Batch insert symbols into index under single write locks
        if !collected_symbols.is_empty() {
            let mut fqn_map = self.symbols_by_fqn.write().await;
            let mut name_map = self.symbols_by_simple_name.write().await;
            for symbol in collected_symbols {
                name_map
                    .entry(symbol.simple_name.clone())
                    .or_default()
                    .push(symbol.fqn.clone());
                fqn_map.insert(symbol.fqn.clone(), symbol);
            }
        }

        // Index XML files
        for (xml_p, xml_content) in xml_files {
            self.index_mybatis_xml(&xml_p, &xml_content).await;
            count += 1;
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

    pub async fn find_by_fqn(&self, fqn: &str) -> Option<SourceSymbol> {
        let fqn_map = self.symbols_by_fqn.read().await;
        fqn_map.get(fqn).cloned()
    }

    /// Find best matching source symbol considering package name and imports
    pub async fn find_candidate(
        &self,
        name_or_fqn: &str,
        preferred_package: Option<&str>,
        imports: &[String],
    ) -> Option<SourceSymbol> {
        let fqn_map = self.symbols_by_fqn.read().await;

        // 1. Exact FQN match
        if let Some(sym) = fqn_map.get(name_or_fqn) {
            return Some(sym.clone());
        }

        // 2. Exact match via explicit imports
        for imp in imports {
            if imp.ends_with(&format!(".{}", name_or_fqn)) || imp == name_or_fqn {
                if let Some(sym) = fqn_map.get(imp) {
                    return Some(sym.clone());
                }
            }
        }

        // 3. Same package match
        if let Some(pkg) = preferred_package {
            let same_pkg_fqn = format!("{}.{}", pkg, name_or_fqn);
            if let Some(sym) = fqn_map.get(&same_pkg_fqn) {
                return Some(sym.clone());
            }
        }

        // 4. Fallback: match by simple name
        let name_map = self.symbols_by_simple_name.read().await;
        if let Some(fqns) = name_map.get(name_or_fqn) {
            if let Some(first_fqn) = fqns.first() {
                if let Some(sym) = fqn_map.get(first_fqn) {
                    return Some(sym.clone());
                }
            }
        }

        None
    }

    pub fn matches_query(simple_name: &str, fqn: &str, query: &str) -> bool {
        let q_lower = query.to_lowercase();
        if simple_name.to_lowercase().contains(&q_lower) || fqn.to_lowercase().contains(&q_lower) {
            return true;
        }

        // CamelHump matching on capital letters (e.g. "OS" matches "OrderService")
        let capitals: Vec<char> = simple_name.chars().filter(|c| c.is_uppercase()).collect();
        let q_upper: Vec<char> = query.to_uppercase().chars().collect();
        if !q_upper.is_empty() && q_upper.len() <= capitals.len() {
            let mut c_idx = 0;
            let mut match_count = 0;
            for qu in &q_upper {
                while c_idx < capitals.len() {
                    if capitals[c_idx] == *qu {
                        match_count += 1;
                        c_idx += 1;
                        break;
                    }
                    c_idx += 1;
                }
            }
            if match_count == q_upper.len() {
                return true;
            }
        }

        // Subsequence match on simple_name
        let q_chars: Vec<char> = query.chars().collect();
        let s_chars: Vec<char> = simple_name.chars().collect();
        let mut q_idx = 0;
        let mut s_idx = 0;
        while q_idx < q_chars.len() && s_idx < s_chars.len() {
            if q_chars[q_idx].to_ascii_lowercase() == s_chars[s_idx].to_ascii_lowercase() {
                q_idx += 1;
            }
            s_idx += 1;
        }
        q_idx == q_chars.len()
    }

    pub async fn search_symbols(&self, query: &str) -> Vec<SourceSymbol> {
        let fqn_map = self.symbols_by_fqn.read().await;
        if query.is_empty() {
            return fqn_map.values().cloned().collect();
        }
        fqn_map
            .values()
            .filter(|s| Self::matches_query(&s.simple_name, &s.fqn, query))
            .cloned()
            .collect()
    }

    pub async fn all_symbols(&self) -> Vec<SourceSymbol> {
        let fqn_map = self.symbols_by_fqn.read().await;
        fqn_map.values().cloned().collect()
    }

    /// Find Spring bean by its bean name or qualified class name
    pub async fn find_spring_bean(&self, bean_name: &str) -> Option<SourceSymbol> {
        let clean_name = bean_name.trim_matches('"');
        let fqn_map = self.symbols_by_fqn.read().await;

        // 1. Direct match on simple_name (case-insensitive or exact)
        let cap_name = {
            let mut c = clean_name.chars();
            match c.next() {
                Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
                None => clean_name.to_string(),
            }
        };

        for s in fqn_map.values() {
            if s.simple_name == clean_name || s.simple_name == cap_name || s.fqn == clean_name {
                return Some(s.clone());
            }
        }

        // 2. Check if any source file contains @Component("bean_name") or @Service("bean_name")
        for s in fqn_map.values() {
            if let Ok(content) = fs::read_to_string(&s.path) {
                if content.contains(&format!("\"{}\"", clean_name))
                    && (content.contains("@Component")
                        || content.contains("@Service")
                        || content.contains("@Repository")
                        || content.contains("@Bean"))
                {
                    return Some(s.clone());
                }
            }
        }

        None
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

    /// Lookup method location in a specific class or across workspace, matching class and package
    pub async fn find_method_location(&self, class_name: Option<&str>, method_name: &str) -> Option<(PathBuf, u32)> {
        self.find_member_location_with_context(class_name, method_name, true, None, None, &[]).await
    }

    /// Lookup method location in a specific class with package context and imports
    pub async fn find_method_location_with_context(
        &self,
        class_name: Option<&str>,
        method_name: &str,
        preferred_package: Option<&str>,
        imports: &[String],
    ) -> Option<(PathBuf, u32)> {
        self.find_member_location_with_context(class_name, method_name, true, None, preferred_package, imports).await
    }

    /// Lookup member (method or field) location in a specific class with package context, imports, and overload resolution
    pub async fn find_member_location_with_context(
        &self,
        class_name: Option<&str>,
        member_name: &str,
        is_method: bool,
        arg_count: Option<usize>,
        preferred_package: Option<&str>,
        imports: &[String],
    ) -> Option<(PathBuf, u32)> {
        let fqn_map = self.symbols_by_fqn.read().await;

        // Collect candidate symbols
        let mut candidates: Vec<&SourceSymbol> = Vec::new();
        for symbol in fqn_map.values() {
            if let Some(target_cls) = class_name {
                if target_cls.contains('.') {
                    // Rule 1: If class_name contains '.', it is an explicit FQN and MUST strictly match symbol.fqn
                    if symbol.fqn != target_cls {
                        continue;
                    }
                } else {
                    // Simple name match
                    if symbol.simple_name != target_cls {
                        continue;
                    }
                }
            }

            // Member match check
            let has_member = if is_method {
                symbol.class_info.methods.iter().any(|m| m.name == member_name)
            } else {
                symbol.class_info.fields.iter().any(|f| f.name == member_name)
            };

            if has_member {
                candidates.push(symbol);
            }
        }

        if candidates.is_empty() {
            return None;
        }

        // Sort candidates by package proximity:
        // Rule 1: Exact FQN match
        // Rule 2: Explicit single-class import (import a.b.Class;)
        // Rule 3: Wildcard import (import a.b.*;)
        // Rule 4: Same package
        // Rule 5: Fallback
        candidates.sort_by_key(|sym| {
            if let Some(target_cls) = class_name {
                if sym.fqn == target_cls {
                    return 0;
                } else if imports.iter().any(|imp| imp == &sym.fqn) {
                    return 1;
                } else if imports.iter().any(|imp| imp.ends_with(&format!(".{}", target_cls))) {
                    return 2;
                }
            } else {
                // If class_name is None (e.g. bare static field/method), check if class is explicitly imported
                if imports.iter().any(|imp| imp == &sym.fqn) {
                    return 1;
                } else if imports.iter().any(|imp| imp.ends_with(&format!(".{}", sym.simple_name))) {
                    return 2;
                }
            }

            if imports.iter().any(|imp| {
                if imp.ends_with(".*") {
                    let pkg = &imp[..imp.len() - 2];
                    pkg == sym.class_info.package_name
                } else {
                    false
                }
            }) {
                3
            } else if preferred_package.map(|pkg| pkg == sym.class_info.package_name).unwrap_or(false) {
                4
            } else {
                5
            }
        });

        fn contains_word(line: &str, word: &str) -> bool {
            let mut start = 0;
            while let Some(pos) = line[start..].find(word) {
                let abs_pos = start + pos;
                let before_ok = if abs_pos == 0 {
                    true
                } else {
                    let prev = line[..abs_pos].chars().last().unwrap();
                    !prev.is_alphanumeric() && prev != '_' && prev != '$'
                };
                let after_pos = abs_pos + word.len();
                let after_ok = if after_pos >= line.len() {
                    true
                } else {
                    let next = line[after_pos..].chars().next().unwrap();
                    !next.is_alphanumeric() && next != '_' && next != '$'
                };
                if before_ok && after_ok {
                    return true;
                }
                start = abs_pos + word.len();
            }
            false
        }

        let best = candidates[0];
        if let Ok(content) = fs::read_to_string(&best.path) {
            // Priority 1: Use AST for accurate line resolution & overload matching
            if let Some(ast) = JavaAst::parse(Arc::new(content.clone())) {
                if let Some(target_line) = ast.find_member_declaration_line(member_name, is_method, arg_count) {
                    return Some((best.path.clone(), target_line));
                }
                // If member_name is a synthetic getter/setter (e.g. Lombok @Data/@Getter), match backing field
                if is_method {
                    let field_cand = if (member_name.starts_with("get") || member_name.starts_with("set")) && member_name.len() > 3 {
                        Some(format!("{}{}", member_name[3..4].to_lowercase(), &member_name[4..]))
                    } else if member_name.starts_with("is") && member_name.len() > 2 {
                        Some(format!("{}{}", member_name[2..3].to_lowercase(), &member_name[3..]))
                    } else {
                        None
                    };
                    if let Some(fc) = field_cand {
                        if let Some(target_line) = ast.find_member_declaration_line(&fc, false, None) {
                            return Some((best.path.clone(), target_line));
                        }
                    }
                }
            }

            // Priority 2: Fallback text search line-by-line
            for (line_no, line_str) in content.lines().enumerate() {
                if contains_word(line_str, member_name) {
                    if is_method {
                        if line_str.contains('(') || line_str.contains("void") || line_str.contains("public") || line_str.contains("private") || line_str.contains("protected") {
                            return Some((best.path.clone(), line_no as u32));
                        }
                    } else {
                        // Field matching: contains member_name and assignment/semicolon/modifiers
                        if line_str.contains(';') || line_str.contains('=') || line_str.contains("public") || line_str.contains("private") || line_str.contains("protected") || line_str.contains("static") || line_str.contains("final") {
                            return Some((best.path.clone(), line_no as u32));
                        }
                    }
                }
            }
        }
        Some((best.path.clone(), 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_indexer_remove_file_and_incremental_cleanup() {
        let indexer = WorkspaceIndex::new();
        let path = PathBuf::from("/tmp/TestService.java");
        let content = r#"
            package com.example;
            public class TestService {
                public void hello() {}
            }
        "#;

        indexer.index_file(&path, content).await;
        assert!(indexer.find_by_fqn("com.example.TestService").await.is_some());
        assert!(!indexer.find_by_simple_name("TestService").await.is_empty());

        // Remove file
        indexer.remove_file(&path).await;
        assert!(indexer.find_by_fqn("com.example.TestService").await.is_none());
        assert!(indexer.find_by_simple_name("TestService").await.is_empty());
    }

    #[tokio::test]
    async fn test_indexer_camel_hump_and_subsequence_search() {
        let indexer = WorkspaceIndex::new();
        let path1 = PathBuf::from("/tmp/OrderService.java");
        let content1 = "package com.example; public class OrderService {}";
        let path2 = PathBuf::from("/tmp/MybatisPlusConfig.java");
        let content2 = "package com.example; public class MybatisPlusConfig {}";

        indexer.index_file(&path1, content1).await;
        indexer.index_file(&path2, content2).await;

        // 1. CamelHump match "OS" -> OrderService
        let res_os = indexer.search_symbols("OS").await;
        assert_eq!(res_os.len(), 1, "Should find OrderService with 'OS'");
        assert_eq!(res_os[0].simple_name, "OrderService");

        // 2. CamelHump match "MPC" -> MybatisPlusConfig
        let res_mpc = indexer.search_symbols("MPC").await;
        assert_eq!(res_mpc.len(), 1, "Should find MybatisPlusConfig with 'MPC'");
        assert_eq!(res_mpc[0].simple_name, "MybatisPlusConfig");

        // 3. Subsequence match "ordser" -> OrderService
        let res_sub = indexer.search_symbols("ordser").await;
        assert_eq!(res_sub.len(), 1, "Should find OrderService with 'ordser'");
        assert_eq!(res_sub[0].simple_name, "OrderService");
    }

    #[tokio::test]
    async fn test_indexer_prunes_ignored_directories() {
        let temp_dir = std::env::temp_dir().join("jlsp_test_prune");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let src_dir = temp_dir.join("src/main/java/com/demo");
        let git_dir = temp_dir.join(".git/objects");
        let node_dir = temp_dir.join("node_modules/pkg");
        let classes_dir = temp_dir.join("target/classes/com/demo");

        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&node_dir).unwrap();
        std::fs::create_dir_all(&classes_dir).unwrap();

        std::fs::write(src_dir.join("RealService.java"), "package com.demo; public class RealService {}").unwrap();
        std::fs::write(git_dir.join("GitDummy.java"), "package com.demo; public class GitDummy {}").unwrap();
        std::fs::write(node_dir.join("NodeDummy.java"), "package com.demo; public class NodeDummy {}").unwrap();
        std::fs::write(classes_dir.join("ClassesDummy.java"), "package com.demo; public class ClassesDummy {}").unwrap();

        let indexer = WorkspaceIndex::new();
        indexer.scan_workspace(&temp_dir).await;

        // RealService should be indexed
        assert!(indexer.find_by_simple_name("RealService").await.iter().any(|s| s.simple_name == "RealService"));
        // Ignored directories should be pruned completely
        assert!(indexer.find_by_simple_name("GitDummy").await.is_empty());
        assert!(indexer.find_by_simple_name("NodeDummy").await.is_empty());
        assert!(indexer.find_by_simple_name("ClassesDummy").await.is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}

