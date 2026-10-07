pub mod class_reader;
pub mod decompiler;
pub mod descriptor;
pub mod jimage;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::classpath::class_reader::parse_jar_file;
use crate::model::ClassInfo;

#[derive(Default)]
pub struct ClasspathRegistry {
    /// Mapping from FQN (e.g. "java.lang.String") to ClassInfo
    classes: RwLock<HashMap<String, Arc<ClassInfo>>>,
    /// Mapping from Simple Name (e.g. "String") to list of FQNs
    simple_name_index: RwLock<HashMap<String, Vec<String>>>,
    /// Mapping from FQN to list of source paths/jars (for conflict detection)
    origins: RwLock<HashMap<String, Vec<String>>>,
}

impl ClasspathRegistry {
    pub fn new() -> Self {
        Self {
            classes: RwLock::new(HashMap::new()),
            simple_name_index: RwLock::new(HashMap::new()),
            origins: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register_class(&self, class: ClassInfo) {
        let fqn = class.fqn.clone();
        let simple_name = class.simple_name.clone();
        let source_path = class.source_path.clone().unwrap_or_default();
        let arc_class = Arc::new(class);

        let mut classes = self.classes.write().await;
        classes.insert(fqn.clone(), arc_class);

        let mut s_idx = self.simple_name_index.write().await;
        s_idx.entry(simple_name).or_default().push(fqn.clone());

        if !source_path.is_empty() {
            let mut origins = self.origins.write().await;
            let list = origins.entry(fqn).or_default();
            if !list.contains(&source_path) {
                list.push(source_path);
            }
        }
    }

    pub async fn get_class_origins(&self, fqn: &str) -> Vec<String> {
        let origins = self.origins.read().await;
        origins.get(fqn).cloned().unwrap_or_default()
    }

    pub async fn get_class(&self, fqn: &str) -> Option<Arc<ClassInfo>> {
        let classes = self.classes.read().await;
        classes.get(fqn).cloned()
    }

    pub async fn find_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassInfo>> {
        let s_idx = self.simple_name_index.read().await;
        let classes = self.classes.read().await;

        if let Some(fqns) = s_idx.get(simple_name) {
            fqns.iter().filter_map(|fqn| classes.get(fqn).cloned()).collect()
        } else {
            Vec::new()
        }
    }

    pub async fn all_classes(&self) -> Vec<Arc<ClassInfo>> {
        let classes = self.classes.read().await;
        classes.values().cloned().collect()
    }

    /// Load all classes from a jar file
    pub async fn add_jar<P: AsRef<Path>>(&self, jar_path: P) -> Result<usize, String> {
        let path_str = jar_path.as_ref().display().to_string();
        info!("Loading jar: {}", path_str);
        let parsed = parse_jar_file(&jar_path)?;
        let count = parsed.len();
        for cls in parsed {
            self.register_class(cls).await;
        }
        info!("Loaded {} classes from {}", count, path_str);
        Ok(count)
    }

    /// Load standard classes from JDK or rt.jar
    pub async fn add_jdk_home<P: AsRef<Path>>(&self, jdk_home: P) -> Result<usize, String> {
        let path = jdk_home.as_ref();
        let rt_jar = path.join("jre").join("lib").join("rt.jar");
        if rt_jar.exists() {
            return self.add_jar(&rt_jar).await;
        }
        let rt_jar_alt = path.join("lib").join("rt.jar");
        if rt_jar_alt.exists() {
            return self.add_jar(&rt_jar_alt).await;
        }

        // Java 9+ modules
        let modules_path = path.join("lib").join("modules");
        if modules_path.exists() {
            info!("Found JDK 9+ modules file at {}", modules_path.display());
            // JImage reader can be hooked here.
        }

        Ok(0)
    }

    /// Persist the in-memory registry to a binary disk cache file for instant startup
    pub async fn save_cache<P: AsRef<Path>>(&self, cache_file: P) -> Result<usize, String> {
        let classes = self.classes.read().await;
        let list: Vec<ClassInfo> = classes.values().map(|c| (**c).clone()).collect();
        let count = list.len();

        let json_bytes = serde_json::to_vec(&list).map_err(|e| format!("Serialization error: {e}"))?;
        if let Some(parent) = cache_file.as_ref().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&cache_file, json_bytes).map_err(|e| format!("Disk write error: {e}"))?;
        info!("Saved {} classes to persistent index cache: {:?}", count, cache_file.as_ref());
        Ok(count)
    }

    /// Load pre-indexed classes from disk cache file
    pub async fn load_cache<P: AsRef<Path>>(&self, cache_file: P) -> Result<usize, String> {
        let p = cache_file.as_ref();
        if !p.exists() {
            return Ok(0);
        }
        let data = std::fs::read(p).map_err(|e| format!("Disk read error: {e}"))?;
        let list: Vec<ClassInfo> = serde_json::from_slice(&data).map_err(|e| format!("Deserialization error: {e}"))?;
        let count = list.len();
        for cls in list {
            self.register_class(cls).await;
        }
        info!("Loaded {} classes from persistent index cache: {:?}", count, p);
        Ok(count)
    }
}
