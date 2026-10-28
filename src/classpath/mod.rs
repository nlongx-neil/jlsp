pub mod cache;
pub mod class_reader;
pub mod decompiler;
pub mod descriptor;
pub mod jimage;

pub mod store;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use tracing::info;

use crate::classpath::class_reader::parse_jar_file;
use crate::classpath::store::{simple_name_of, ArchivedStore, ClassStore};
use crate::model::{ClassInfo, ClassKind, MethodInfo, TypeSignature};

/// Upper bound on materialized `ClassInfo` views kept for hot lookups.
const VIEW_CACHE_CAPACITY: usize = 2048;
/// Upper bound on methods returned by a classpath-wide name search.
const METHOD_SEARCH_LIMIT: usize = 256;

/// Names-only index of classes whose jar has not been parsed yet (lazy loading).
/// Keeps one `Arc<str>` per fqn and one shared `Arc<str>` per jar path.
#[derive(Default)]
struct PendingIndex {
    /// fqn -> jar path
    by_fqn: HashMap<Arc<str>, Arc<str>>,
    /// simple name -> fqns
    by_simple: HashMap<Arc<str>, Vec<Arc<str>>>,
}

impl PendingIndex {
    fn insert(&mut self, fqn: Arc<str>, path: Arc<str>) {
        if self.by_fqn.contains_key(&fqn) {
            return;
        }
        self.by_simple
            .entry(Arc::from(simple_name_of(&fqn)))
            .or_default()
            .push(fqn.clone());
        self.by_fqn.insert(fqn, path);
    }

    fn remove_jar(&mut self, path: &str) {
        let fqns: Vec<Arc<str>> = self
            .by_fqn
            .iter()
            .filter(|(_, p)| &***p == path)
            .map(|(f, _)| f.clone())
            .collect();
        for fqn in fqns {
            self.by_fqn.remove(&fqn);
            let simple = simple_name_of(&fqn);
            if let Some(list) = self.by_simple.get_mut(simple) {
                list.retain(|f| f != &fqn);
                if list.is_empty() {
                    self.by_simple.remove(simple);
                }
            }
        }
    }

    fn contains(&self, fqn: &str) -> bool {
        self.by_fqn.contains_key(fqn)
    }

    fn is_empty(&self) -> bool {
        self.by_fqn.is_empty()
    }

    fn has_jar(&self, jar_path: &str) -> bool {
        self.by_fqn.values().any(|p| &**p == jar_path)
    }

    fn len(&self) -> usize {
        self.by_fqn.len()
    }
}

#[derive(Default)]
struct ViewCache {
    map: HashMap<u32, Arc<ClassInfo>>,
    order: std::collections::VecDeque<u32>,
}

impl ViewCache {
    fn get(&self, idx: u32) -> Option<Arc<ClassInfo>> {
        self.map.get(&idx).cloned()
    }

    fn insert(&mut self, idx: u32, view: Arc<ClassInfo>) {
        if self.map.insert(idx, view).is_none() {
            self.order.push_back(idx);
        }
        while self.order.len() > VIEW_CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

#[derive(Default)]
pub struct ClasspathRegistry {
    /// Compact, pooled storage for every classpath class.
    store: RwLock<ClassStore>,
    /// On-demand materialized views of recently used classes.
    views: Mutex<ViewCache>,
    /// Classes known by name whose jar has not been parsed yet (lazy loading).
    pending: RwLock<PendingIndex>,
    /// Serializes on-demand jar loads.
    lazy_lock: tokio::sync::Mutex<()>,
    /// Workspace archive used as a last-resort source for lazy loads.
    fallback_archive: RwLock<Option<std::path::PathBuf>>,
    /// Decoded fallback archive, memoized so repeated misses stay cheap.
    fallback_store: tokio::sync::OnceCell<ClassStore>,
    /// Mapping from FQN to list of source paths/jars (only populated if conflicts exist)
    origins: RwLock<HashMap<String, Vec<String>>>,
    /// Set of already loaded jar paths (avoid duplicate scanning & indexing)
    loaded_jars: RwLock<std::collections::HashSet<std::path::PathBuf>>,
    /// Saved JDK Home directory (for src.zip lookup)
    jdk_home: RwLock<Option<std::path::PathBuf>>,
}

impl ClasspathRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register_class(&self, class: ClassInfo) {
        self.register_classes(vec![class]).await;
    }

    /// Batch register multiple classes, interning them into the compact store.
    pub async fn register_classes(&self, new_classes: Vec<ClassInfo>) {
        if new_classes.is_empty() {
            return;
        }

        let archive = ArchivedStore::from_classes(&new_classes);
        self.append_archive(archive).await;

        // Only track origins when a REAL multi-jar conflict occurs.
        let mut conflicts: Vec<(String, String, String)> = Vec::new();
        {
            let store = self.store.read().await;
            for class in &new_classes {
                let Some(idx) = store.class_index(&class.fqn) else {
                    continue;
                };
                let Some(prev_path) = store.class_source_path(idx) else {
                    continue;
                };
                let Some(cur_path) = class.source_path.as_deref() else {
                    continue;
                };
                if prev_path != cur_path {
                    conflicts.push((class.fqn.clone(), prev_path.to_string(), cur_path.to_string()));
                }
            }
        }
        if !conflicts.is_empty() {
            let mut origins = self.origins.write().await;
            for (fqn, prev_path, cur_path) in conflicts {
                let list = origins.entry(fqn).or_default();
                if !list.contains(&prev_path) {
                    list.push(prev_path);
                }
                if !list.contains(&cur_path) {
                    list.push(cur_path);
                }
            }
        }
    }

    async fn append_archive(&self, archive: ArchivedStore) {
        self.store.write().await.append_archive(archive);
        self.views.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Materialize (and cache) a class view for a store index.
    async fn materialized(&self, idx: u32) -> Arc<ClassInfo> {
        if let Some(view) = self.views.lock().unwrap_or_else(|e| e.into_inner()).get(idx) {
            return view;
        }
        let view = Arc::new(self.store.read().await.materialize(idx));
        self.views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(idx, view.clone());
        view
    }

    /// Register class names without loading their metadata (lazy jar indexing).
    async fn register_pending(&self, jar_path: &str, fqns: Vec<String>) {
        if fqns.is_empty() {
            return;
        }
        let path: Arc<str> = Arc::from(jar_path);
        let mut pending = self.pending.write().await;
        for fqn in fqns {
            pending.insert(Arc::from(fqn.as_str()), path.clone());
        }
    }

    /// Load a pending jar's full metadata on demand. Idempotent; the jar's
    /// pending entries are dropped afterwards whether or not loading succeeded.
    async fn ensure_jar_loaded(&self, jar_path: &str) {
        if !self.pending.read().await.has_jar(jar_path) {
            return;
        }
        let _guard = self.lazy_lock.lock().await;
        // Re-check: another task may have loaded this jar while we waited.
        if !self.pending.read().await.has_jar(jar_path) {
            return;
        }

        let p = std::path::Path::new(jar_path);
        let mut appended = false;

        // 1. Global per-jar disk cache
        if let Some(key) = crate::classpath::cache::compute_jar_cache_key(p) {
            let cf = crate::classpath::cache::global_jar_cache_dir().join(key);
            if cf.exists() {
                if let Ok(Some(archive)) = crate::classpath::cache::load_store_archive_file(&cf) {
                    if archive.class_count() > 0 {
                        self.append_archive(archive).await;
                        appended = true;
                    }
                }
                if !appended {
                    // Legacy (unpooled) per-jar cache: decode, re-intern, rewrite once.
                    if let Ok(classes) = crate::classpath::cache::load_archive_file(&cf) {
                        if !classes.is_empty() {
                            let archive = ArchivedStore::from_classes(&classes);
                            self.register_classes(classes).await;
                            let _ = crate::classpath::cache::save_store_archive_file(&cf, &archive);
                            appended = true;
                        }
                    }
                }
            }
        }

        // 2. Parse the jar itself
        if !appended && p.exists() {
            if let Ok(parsed) = parse_jar_file(p) {
                let archive = ArchivedStore::from_classes(&parsed);
                drop(parsed);
                if let Some(key) = crate::classpath::cache::compute_jar_cache_key(p) {
                    let cf = crate::classpath::cache::global_jar_cache_dir().join(key);
                    if let Some(parent) = cf.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = crate::classpath::cache::save_store_archive_file(&cf, &archive);
                }
                self.append_archive(archive).await;
                appended = true;
            }
        }

        // 3. Last resort: the workspace archive recorded these classes.
        // The decoded archive is memoized so thousands of misses (e.g. stale
        // paths) never trigger a full decode per jar.
        if !appended {
            let fallback = self.fallback_archive.read().await.clone();
            if let Some(fb) = fallback {
                let tmp = self
                    .fallback_store
                    .get_or_try_init(|| async {
                        let archive = crate::classpath::cache::load_store_archive_file(&fb)?
                            .ok_or_else(|| "not a pooled archive".to_string())?;
                        let mut tmp = ClassStore::new();
                        tmp.append_archive(archive);
                        Ok::<ClassStore, String>(tmp)
                    })
                    .await;
                if let Ok(tmp) = tmp {
                    let filtered = tmp.export(|_, sp| sp == Some(jar_path));
                    if filtered.class_count() > 0 {
                        self.append_archive(filtered).await;
                        appended = true;
                    }
                }
            }
        }

        if appended {
            info!("Lazily loaded classes from {}", jar_path);
        }
        self.pending.write().await.remove_jar(jar_path);
    }

    /// Make the class with this fqn available in the store, lazily loading the
    /// jar that contains it when necessary.
    async fn ensure_class_loaded(&self, fqn: &str) {
        if self.store.read().await.contains(fqn) {
            return;
        }
        let jar = { self.pending.read().await.by_fqn.get(fqn).cloned() };
        if let Some(jar) = jar {
            self.ensure_jar_loaded(&jar).await;
        }
    }

    /// Eagerly load every pending jar. Used by whole-classpath fuzzy queries
    /// whose result set must match a fully indexed classpath.
    async fn ensure_all_pending_loaded(&self) {
        loop {
            let next = { self.pending.read().await.by_fqn.values().next().cloned() };
            match next {
                Some(jar) => self.ensure_jar_loaded(&jar).await,
                None => break,
            }
        }
    }

    pub async fn get_class_origins(&self, fqn: &str) -> Vec<String> {
        {
            let origins = self.origins.read().await;
            if let Some(list) = origins.get(fqn) {
                return list.clone();
            }
        }
        {
            let store = self.store.read().await;
            if let Some(idx) = store.class_index(fqn) {
                if let Some(path) = store.class_source_path(idx) {
                    return vec![path.to_string()];
                }
            }
        }
        let pending = self.pending.read().await;
        if let Some(path) = pending.by_fqn.get(fqn) {
            return vec![path.to_string()];
        }
        Vec::new()
    }

    pub async fn get_class(&self, fqn: &str) -> Option<Arc<ClassInfo>> {
        let idx = { self.store.read().await.class_index(fqn) };
        if let Some(idx) = idx {
            return Some(self.materialized(idx).await);
        }

        // Lazily load the jar that contains this class
        self.ensure_class_loaded(fqn).await;
        let idx = { self.store.read().await.class_index(fqn) };
        if let Some(idx) = idx {
            return Some(self.materialized(idx).await);
        }

        if let Some(builtin) = get_builtin_framework_class(fqn) {
            return Some(Arc::new(builtin));
        }

        if let Some(jdk_cls) = get_builtin_jdk_class(fqn) {
            return Some(Arc::new(jdk_cls));
        }

        None
    }

    /// Like [`get_class`], but only consults real indexed data (class store +
    /// lazily loaded jars) and never synthesizes builtin framework/JDK stubs.
    ///
    /// Use this where Java resolution semantics require a nonexistent class to
    /// stay unresolved — e.g. same-package or wildcard-import candidates. A
    /// synthesized stub would otherwise pretend that `org.apache.commons.lang.String`
    /// exists and shadow the real `java.lang.String`, and goto-definition would
    /// then decompile the empty stub into a bogus `.java` file.
    pub async fn get_class_strict(&self, fqn: &str) -> Option<Arc<ClassInfo>> {
        let idx = { self.store.read().await.class_index(fqn) };
        if let Some(idx) = idx {
            return Some(self.materialized(idx).await);
        }
        self.ensure_class_loaded(fqn).await;
        let idx = { self.store.read().await.class_index(fqn) };
        if let Some(idx) = idx {
            return Some(self.materialized(idx).await);
        }
        None
    }

    pub async fn find_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassInfo>> {
        // Load any pending jars containing classes with this simple name
        let jars: Vec<Arc<str>> = {
            let pending = self.pending.read().await;
            let mut set = std::collections::HashSet::new();
            let mut jars = Vec::new();
            if let Some(fqns) = pending.by_simple.get(simple_name) {
                for fqn in fqns {
                    if let Some(path) = pending.by_fqn.get(fqn) {
                        if set.insert(path.clone()) {
                            jars.push(path.clone());
                        }
                    }
                }
            }
            jars
        };
        for jar in jars {
            self.ensure_jar_loaded(&jar).await;
        }

        let idxs = { self.store.read().await.simple_class_indices(simple_name) };
        let mut res = Vec::with_capacity(idxs.len());
        for idx in idxs {
            res.push(self.materialized(idx).await);
        }

        if res.is_empty() {
            if let Some(builtin) = get_builtin_framework_class_by_simple_name(simple_name) {
                res.push(Arc::new(builtin));
            }
        }
        if res.is_empty() {
            if let Some(jdk_cls) = get_builtin_jdk_class_by_simple_name(simple_name) {
                res.push(Arc::new(jdk_cls));
            }
        }
        res
    }

    /// Methods with a matching name across the classpath (used by signature help).
    /// Loads every pending jar first so results match a fully indexed classpath.
    pub async fn find_methods_by_name(&self, name: &str) -> Vec<MethodInfo> {
        self.ensure_all_pending_loaded().await;
        self.store
            .read()
            .await
            .search_methods(name, METHOD_SEARCH_LIMIT)
    }

    /// Classes directly extending/implementing `target` (fqn or simple name).
    /// Loads every pending jar first so results match a fully indexed classpath.
    pub async fn find_subclasses(&self, target: &str) -> Vec<Arc<ClassInfo>> {
        self.ensure_all_pending_loaded().await;
        let idxs = { self.store.read().await.subclass_indices(target) };
        let mut res = Vec::with_capacity(idxs.len());
        for idx in idxs {
            res.push(self.materialized(idx).await);
        }
        res
    }

    /// `(simple_name, fqn)` pairs for class-name completion, without materializing
    /// classes. Includes pending (not yet parsed) classes so completion coverage
    /// matches a fully indexed classpath.
    pub async fn class_summaries(&self) -> Vec<(String, String)> {
        let mut out = { self.store.read().await.summaries() };
        let seen: std::collections::HashSet<String> =
            out.iter().map(|(_, f)| f.clone()).collect();
        let pending = self.pending.read().await;
        for fqn in pending.by_fqn.keys() {
            if !seen.contains(&**fqn) {
                out.push((simple_name_of(fqn).to_string(), fqn.to_string()));
            }
        }
        out
    }

    pub async fn has_classes(&self) -> bool {
        if !self.pending.read().await.is_empty() {
            return true;
        }
        !self.store.read().await.is_empty()
    }

    pub async fn has_jdk_loaded(&self) -> bool {
        for fqn in ["java.lang.Object", "java.util.List", "java.io.File"] {
            if self.store.read().await.contains(fqn) || self.pending.read().await.contains(fqn) {
                return true;
            }
        }
        false
    }

    /// Index a jar's class names (zip directory scan only, no class parsing).
    /// Full metadata is loaded on first actual use via [`Self::ensure_jar_loaded`].
    pub async fn add_jar<P: AsRef<Path>>(&self, jar_path: P) -> Result<usize, String> {
        let p = jar_path.as_ref();
        let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        {
            let mut loaded = self.loaded_jars.write().await;
            if !loaded.insert(canonical) {
                // Jar already indexed, skip redundant scanning
                return Ok(0);
            }
        }

        let path_str = p.to_string_lossy().to_string();
        let fqns = crate::classpath::class_reader::scan_jar_class_names(p)?;
        let count = fqns.len();
        self.register_pending(&path_str, fqns).await;
        Ok(count)
    }

    pub async fn get_jdk_home(&self) -> Option<std::path::PathBuf> {
        self.jdk_home.read().await.clone()
    }

    /// Load standard classes from JDK or rt.jar
    pub async fn add_jdk_home<P: AsRef<Path>>(&self, jdk_home: P) -> Result<usize, String> {
        let path = jdk_home.as_ref();
        info!("Indexing JDK at: {:?}", path);
        *self.jdk_home.write().await = Some(path.to_path_buf());

        // 1. JDK 6~8 layout: rt.jar, classes.jar (Apple JDK 6), and companion jars (jsse.jar, jce.jar, charsets.jar)
        let mut loaded_jdk_jars = 0;
        let mut candidate_jars = Vec::new();

        for lib_dir in [path.join("jre").join("lib"), path.join("lib"), path.join("Classes")] {
            if lib_dir.exists() {
                for jar_name in ["rt.jar", "classes.jar", "jsse.jar", "jce.jar", "charsets.jar", "tools.jar"] {
                    let jar_path = lib_dir.join(jar_name);
                    if jar_path.exists() && !candidate_jars.contains(&jar_path) {
                        candidate_jars.push(jar_path);
                    }
                }
                let ext_dir = lib_dir.join("ext");
                if ext_dir.exists() {
                    if let Ok(entries) = std::fs::read_dir(&ext_dir) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if p.extension().and_then(|e| e.to_str()) == Some("jar") && !candidate_jars.contains(&p) {
                                candidate_jars.push(p);
                            }
                        }
                    }
                }
            }
        }

        if !candidate_jars.is_empty() {
            for jar in candidate_jars {
                if let Ok(n) = self.add_jar(&jar).await {
                    loaded_jdk_jars += n;
                }
            }
            if loaded_jdk_jars > 0 {
                info!("Successfully loaded {} classes from JDK 6-8 JARs", loaded_jdk_jars);
                return Ok(loaded_jdk_jars);
            }
        }

        // 2. Java 9+ modules (lib/modules)
        let modules_path = path.join("lib").join("modules");
        if modules_path.exists() {
            info!("Found JDK 9+ modules file at {}", modules_path.display());

            // Build persistent cache path for this JDK
            let cache_dir = dirs_home().unwrap_or_else(std::env::temp_dir).join(".jlsp").join("cache");
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            use std::hash::{Hash, Hasher};
            path.to_string_lossy().hash(&mut hasher);
            if let Ok(metadata) = std::fs::metadata(&modules_path) {
                metadata.len().hash(&mut hasher);
                if let Ok(mtime) = metadata.modified() {
                    mtime.hash(&mut hasher);
                }
            }
            // Mix in the first 64 KiB of the modules image so an in-place JDK
            // update that preserves size/mtime still invalidates the cache.
            if let Ok(mut f) = std::fs::File::open(&modules_path) {
                use std::io::Read;
                let mut head = [0u8; 64 * 1024];
                if let Ok(n) = f.read(&mut head) {
                    head[..n].hash(&mut hasher);
                }
            }
            let cache_key = format!("jdk_v3_{:016x}.bin.zst", hasher.finish());
            let legacy_key = format!("jdk_{:016x}.json", hasher.finish());
            let cache_file = cache_dir.join(&cache_key);
            let legacy_file = cache_dir.join(&legacy_key);
            if legacy_file.exists() {
                // Remove obsolete uncompressed legacy JSON cache that consumed 100MB+ disk and 500MB+ heap
                let _ = std::fs::remove_file(&legacy_file);
            }

            // Fast warm start from binary compressed disk cache
            if cache_file.exists() {
                if let Ok(loaded) = self.load_cache(&cache_file).await {
                    if loaded > 0 {
                        info!("Fast warm start: loaded {} JDK classes from cache file: {:?}", loaded, cache_file);
                        return Ok(loaded);
                    }
                }
            }

            // Strategy A: jimage CLI tool (extract standard public java.* packages across all modules)
            let jimage_bin = path.join("bin").join(if cfg!(windows) { "jimage.exe" } else { "jimage" });
            if jimage_bin.exists() {
                info!("Using JDK jimage tool at {:?}", jimage_bin);
                let temp_extract_dir = std::env::temp_dir().join(format!("jlsp_jimage_extract_{}_{:x}", std::process::id(), hasher.finish()));
                let _ = std::fs::create_dir_all(&temp_extract_dir);

                let extract_res = tokio::process::Command::new(&jimage_bin)
                    .arg("extract")
                    .arg("--include")
                    .arg("glob:/java.*/java/**")
                    .arg("--dir")
                    .arg(&temp_extract_dir)
                    .arg(&modules_path)
                    .output()
                    .await;

                if let Ok(output) = extract_res {
                    if output.status.success() {
                        if let Ok(parsed_classes) = crate::classpath::class_reader::parse_class_dir(&temp_extract_dir) {
                            let count = parsed_classes.len();
                            self.register_classes(parsed_classes).await;
                            info!("Loaded {} JDK classes from extracted modules", count);
                            let _ = self.save_cache(&cache_file).await;
                            let _ = std::fs::remove_dir_all(&temp_extract_dir);
                            return Ok(count);
                        }
                    } else {
                        tracing::warn!("jimage extract failed: {}", String::from_utf8_lossy(&output.stderr));
                    }
                }
                let _ = std::fs::remove_dir_all(&temp_extract_dir);
            }

            // Strategy B: Fallback to lib/src.zip
            let src_zip = path.join("lib").join("src.zip");
            if src_zip.exists() {
                info!("Fallback: indexing JDK from src.zip at {:?}", src_zip);
                if let Ok(classes) = crate::classpath::class_reader::parse_src_zip(&src_zip) {
                    let count = classes.len();
                    self.register_classes(classes).await;
                    info!("Loaded {} JDK classes from src.zip", count);
                    let _ = self.save_cache(&cache_file).await;
                    return Ok(count);
                }
            }
        }

        // 3. Standalone src.zip in root or lib
        let src_zip_alt = path.join("src.zip");
        if src_zip_alt.exists() {
            if let Ok(classes) = crate::classpath::class_reader::parse_src_zip(&src_zip_alt) {
                let count = classes.len();
                self.register_classes(classes).await;
                return Ok(count);
            }
        }

        Ok(0)
    }

    /// Persist project dependency classes (excluding JDK classes) to workspace disk cache
    pub async fn save_workspace_cache<P: AsRef<Path>>(&self, cache_file: P) -> Result<usize, String> {
        let p = cache_file.as_ref();
        let archive = {
            let store = self.store.read().await;
            store.export(|_, path| match path {
                // Only save classes that came from project jars (avoid duplicating JDK in workspace cache)
                Some(path) => {
                    path.ends_with(".jar") && !path.contains("rt.jar") && !path.contains("classes.jar")
                }
                None => false,
            })
        };
        let count = archive.class_count();

        crate::classpath::cache::save_store_archive_file(p, &archive)?;

        // Name index for lazy startup: pending entries + everything already loaded.
        // Only jar paths qualify: non-jar source paths (e.g. JDK temp extraction
        // dirs) cannot be re-fetched lazily and would just pollute the index.
        let mut entries: Vec<(String, String)> = Vec::new();
        {
            let pending = self.pending.read().await;
            entries.reserve(pending.len());
            for (fqn, path) in &pending.by_fqn {
                if path.ends_with(".jar") {
                    entries.push((fqn.to_string(), path.to_string()));
                }
            }
        }
        {
            let store = self.store.read().await;
            for (fqn, path) in store.fqn_path_pairs() {
                if let Some(path) = path {
                    if path.ends_with(".jar") {
                        entries.push((fqn, path));
                    }
                }
            }
        }
        let names_file = p.with_file_name("index.names.zst");
        let _ = crate::classpath::cache::save_names_file(&names_file, &entries);
        *self.fallback_archive.write().await = Some(p.to_path_buf());

        info!("Saved {} project dependency classes to workspace index cache: {:?}", count, p);
        Ok(count)
    }

    /// Lazy workspace startup: restore only the `(fqn, jar)` name index from
    /// disk; class metadata is loaded on demand. Returns the number of indexed
    /// class names.
    pub async fn load_workspace_index<P: AsRef<Path>>(&self, cache_file: P) -> Result<usize, String> {
        let cache_file = cache_file.as_ref();
        let names_file = cache_file.with_file_name("index.names.zst");

        if let Some(entries) = crate::classpath::cache::load_names_file(&names_file)? {
            let count = entries.len();
            self.register_pending_entries(entries).await;
            *self.fallback_archive.write().await = Some(cache_file.to_path_buf());
            self.mark_pending_jars_loaded().await;
            info!("Fast cold-start: indexed {} class names from {:?}", count, names_file);
            return Ok(count);
        }

        if !cache_file.exists() {
            return Ok(0);
        }

        // One-time migration from the full workspace archive: keep only the name
        // index; classes without a jar path cannot be re-fetched lazily and stay eager.
        let store_archive = crate::classpath::cache::load_store_archive_file(cache_file).unwrap_or(None);
        match store_archive {
            Some(archive) => {
                let mut tmp = ClassStore::new();
                tmp.append_archive(archive);
                let pairs = tmp.fqn_path_pairs();
                let mut names_entries: Vec<(String, String)> = Vec::new();
                for (fqn, path) in pairs {
                    if let Some(path) = path {
                        if path.ends_with(".jar") {
                            names_entries.push((fqn, path));
                        }
                    }
                }
                let count = names_entries.len();
                self.register_pending_entries(names_entries.clone()).await;
                let eager = tmp.export(|_, p| p.is_none());
                if eager.class_count() > 0 {
                    self.append_archive(eager).await;
                }
                let _ = crate::classpath::cache::save_names_file(&names_file, &names_entries);
                *self.fallback_archive.write().await = Some(cache_file.to_path_buf());
                self.mark_pending_jars_loaded().await;
                info!("Converted workspace cache to lazy name index: {} class names", count);
                Ok(count)
            }
            None => {
                // Legacy unpooled / JSON cache: load eagerly (old behavior).
                self.load_cache(cache_file).await
            }
        }
    }

    async fn register_pending_entries(&self, entries: Vec<(String, String)>) {
        let mut pending = self.pending.write().await;
        for (fqn, path) in entries {
            // Only jars can be re-fetched lazily; skip stale non-jar paths
            // (e.g. JDK temp extraction dirs written by older caches).
            if !path.ends_with(".jar") {
                continue;
            }
            pending.insert(Arc::from(fqn.as_str()), Arc::from(path.as_str()));
        }
    }

    /// Jars named by the pending index count as indexed, so re-walking the
    /// workspace does not scan them again.
    async fn mark_pending_jars_loaded(&self) {
        let paths: Vec<String> = {
            let pending = self.pending.read().await;
            let mut set = std::collections::HashSet::new();
            for p in pending.by_fqn.values() {
                set.insert(p.to_string());
            }
            set.into_iter().collect()
        };
        let mut loaded = self.loaded_jars.write().await;
        for path in paths {
            let p = std::path::PathBuf::from(&path);
            let canonical = p.canonicalize().unwrap_or(p);
            loaded.insert(canonical);
        }
    }

    /// Persist the in-memory registry to a binary compressed disk cache file for instant startup
    pub async fn save_cache<P: AsRef<Path>>(&self, cache_file: P) -> Result<usize, String> {
        let p = cache_file.as_ref();
        let archive = {
            let store = self.store.read().await;
            store.export(|_, _| true)
        };
        let count = archive.class_count();

        crate::classpath::cache::save_store_archive_file(p, &archive)?;
        info!("Saved {} classes to persistent index cache: {:?}", count, p);
        Ok(count)
    }

    /// Load pre-indexed classes from disk cache file (supports compact .bin.zst, .bin, and legacy .json)
    pub async fn load_cache<P: AsRef<Path>>(&self, cache_file: P) -> Result<usize, String> {
        let p = cache_file.as_ref();

        // If file doesn't exist directly, check alternatives (.bin.zst vs .json)
        let resolved_path = if p.exists() {
            p.to_path_buf()
        } else {
            let str_path = p.to_string_lossy();
            if str_path.ends_with(".bin.zst") {
                let alt = std::path::PathBuf::from(str_path.trim_end_matches(".bin.zst").to_string() + ".json");
                if alt.exists() {
                    alt
                } else {
                    return Ok(0);
                }
            } else if str_path.ends_with(".json") {
                let alt = std::path::PathBuf::from(str_path.trim_end_matches(".json").to_string() + ".bin.zst");
                if alt.exists() {
                    alt
                } else {
                    return Ok(0);
                }
            } else {
                return Ok(0);
            }
        };

        // A corrupted or older cache format must never break startup: fall back to legacy decode.
        let store_archive = crate::classpath::cache::load_store_archive_file(&resolved_path).unwrap_or(None);
        let count = match store_archive {
            Some(archive) => {
                let count = archive.class_count();
                self.append_archive(archive).await;
                count
            }
            None => {
                // Legacy (unpooled) cache: decode, re-intern, then rewrite in the
                // pooled format so later startups skip the costly materialization.
                let list = crate::classpath::cache::load_archive_file(&resolved_path)?;
                let count = list.len();
                let archive = ArchivedStore::from_classes(&list);
                self.register_classes(list).await;
                let _ = crate::classpath::cache::save_store_archive_file(&resolved_path, &archive);
                count
            }
        };
        self.mark_cached_jars_loaded().await;
        info!("Loaded {} classes from persistent index cache: {:?}", count, resolved_path);
        Ok(count)
    }

    /// Cached classes know which jars they came from; treat those jars as loaded
    /// so re-walking the workspace does not parse them again.
    async fn mark_cached_jars_loaded(&self) {
        let paths = { self.store.read().await.source_paths() };
        if paths.is_empty() {
            return;
        }
        let mut loaded = self.loaded_jars.write().await;
        for path in paths {
            if !path.ends_with(".jar") {
                continue;
            }
            let p = std::path::PathBuf::from(&path);
            let canonical = p.canonicalize().unwrap_or(p);
            loaded.insert(canonical);
        }
    }
}

pub fn dirs_home() -> Option<std::path::PathBuf> {
    // OS-level user home (e.g. macOS `/Users/<name>`). Never falls back to a
    // relative path: returning None is always safer than writing into the
    // process CWD (which is typically the opened project root).
    std::env::home_dir()
}

/// Check if a directory is a valid JDK/JRE directory containing modules, rt.jar or src.zip
pub fn is_valid_jdk_dir<P: AsRef<std::path::Path>>(p: P) -> bool {
    let path = p.as_ref();
    path.join("lib").join("modules").exists()
        || path.join("jre").join("lib").join("rt.jar").exists()
        || path.join("lib").join("rt.jar").exists()
        || path.join("Classes").join("classes.jar").exists()
        || path.join("lib").join("src.zip").exists()
        || path.join("src.zip").exists()
}

/// Automatically locate the local JDK home path across environments
pub fn find_java_home() -> Option<std::path::PathBuf> {
    // 1. Env variable JAVA_HOME
    if let Ok(val) = std::env::var("JAVA_HOME") {
        let p = std::path::PathBuf::from(val);
        if is_valid_jdk_dir(&p) {
            return Some(p);
        }
    }

    // 2. macOS /usr/libexec/java_home
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("/usr/libexec/java_home").output() {
            if output.status.success() {
                let path_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                let p = std::path::PathBuf::from(path_str);
                if is_valid_jdk_dir(&p) {
                    return Some(p);
                }
            }
        }
    }

    // 3. Resolve `which java` from PATH (following symlinks to find parent of bin)
    if let Ok(output) = std::process::Command::new("which").arg("java").output() {
        if output.status.success() {
            let bin_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if let Ok(canonical) = std::fs::canonicalize(bin_path) {
                if let Some(bin_dir) = canonical.parent() {
                    if let Some(jdk_dir) = bin_dir.parent() {
                        if is_valid_jdk_dir(jdk_dir) {
                            return Some(jdk_dir.to_path_buf());
                        }
                    }
                }
            }
        }
    }

    // 4. Common candidate directories (SDKMAN, Homebrew, Linux standard, macOS standard)
    let mut candidates = Vec::new();

    // SDKMAN standard location
    if let Some(home) = dirs_home() {
        candidates.push(home.join(".sdkman").join("candidates").join("java").join("current"));
    }

    // macOS /Library/Java/JavaVirtualMachines/*/Contents/Home
    #[cfg(target_os = "macos")]
    {
        let jvm_dir = std::path::Path::new("/Library/Java/JavaVirtualMachines");
        if jvm_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(jvm_dir) {
                for entry in entries.flatten() {
                    let home_path = entry.path().join("Contents/Home");
                    if is_valid_jdk_dir(&home_path) {
                        candidates.push(home_path);
                    }
                }
            }
        }
        candidates.push(std::path::PathBuf::from("/opt/homebrew/opt/openjdk/libexec/openjdk.jdk/Contents/Home"));
        candidates.push(std::path::PathBuf::from("/usr/local/opt/openjdk/libexec/openjdk.jdk/Contents/Home"));
    }

    // Linux & Unix standard locations
    candidates.extend([
        std::path::PathBuf::from("/Library/Java/Home"),
        std::path::PathBuf::from("/usr/lib/jvm/default-java"),
        std::path::PathBuf::from("/usr/lib/jvm/java-21-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-17-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-11-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-8-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-7-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-6-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-1.8.0-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-1.7.0-openjdk"),
        std::path::PathBuf::from("/usr/lib/jvm/java-1.6.0-openjdk"),
    ]);

    for cand in candidates {
        if is_valid_jdk_dir(&cand) {
            return Some(cand);
        }
    }

    None
}

/// Returns true if the import belongs to standard JDK core packages
pub fn is_standard_jdk_package(imp: &str) -> bool {
    const STANDARD_PREFIXES: &[&str] = &[
        "java.",
        "javax.",
        "jdk.",
        "sun.",
        "com.sun.",
    ];
    STANDARD_PREFIXES.iter().any(|prefix| imp.starts_with(prefix))
}

/// Returns true if the type is a known core JDK class
pub fn is_known_core_jdk_class(imp: &str) -> bool {
    is_standard_jdk_package(imp)
}

pub fn is_standard_java_lang_class(name: &str) -> bool {
    matches!(
        name,
        "String"
            | "Object"
            | "Class"
            | "System"
            | "Math"
            | "Thread"
            | "Runnable"
            | "Throwable"
            | "Exception"
            | "RuntimeException"
            | "Error"
            | "Void"
            | "Boolean"
            | "Byte"
            | "Character"
            | "Short"
            | "Integer"
            | "Long"
            | "Float"
            | "Double"
            | "Number"
            | "CharSequence"
            | "StringBuilder"
            | "StringBuffer"
            | "Cloneable"
            | "Comparable"
            | "Iterable"
            | "AutoCloseable"
            | "Readable"
            | "Appendable"
            | "ThreadLocal"
            | "InheritableThreadLocal"
            | "ThreadGroup"
            | "Package"
            | "Module"
            | "ClassLoader"
            | "StackTraceElement"
            | "Process"
            | "ProcessBuilder"
            | "Enum"
            | "Record"
            | "Deprecated"
            | "Override"
            | "SuppressWarnings"
            | "SafeVarargs"
            | "FunctionalInterface"
            | "AssertionError"
            | "NullPointerException"
            | "IllegalArgumentException"
            | "IllegalStateException"
            | "IndexOutOfBoundsException"
            | "ArrayIndexOutOfBoundsException"
            | "StringIndexOutOfBoundsException"
            | "UnsupportedOperationException"
            | "NoSuchMethodException"
            | "NoSuchFieldException"
            | "ClassNotFoundException"
            | "InterruptedException"
    )
}

/// Return full qualified name(s) of standard JDK classes matching a simple name
pub fn find_common_jdk_classes_by_name(name: &str) -> Vec<String> {
    match name {
        "List" => vec!["java.util.List".to_string(), "java.awt.List".to_string()],
        "Map" => vec!["java.util.Map".to_string()],
        "Set" => vec!["java.util.Set".to_string()],
        "Queue" => vec!["java.util.Queue".to_string()],
        "Deque" => vec!["java.util.Deque".to_string()],
        "ArrayList" => vec!["java.util.ArrayList".to_string()],
        "LinkedList" => vec!["java.util.LinkedList".to_string()],
        "HashMap" => vec!["java.util.HashMap".to_string()],
        "HashSet" => vec!["java.util.HashSet".to_string()],
        "LinkedHashMap" => vec!["java.util.LinkedHashMap".to_string()],
        "LinkedHashSet" => vec!["java.util.LinkedHashSet".to_string()],
        "TreeMap" => vec!["java.util.TreeMap".to_string()],
        "TreeSet" => vec!["java.util.TreeSet".to_string()],
        "Arrays" => vec!["java.util.Arrays".to_string()],
        "Collections" => vec!["java.util.Collections".to_string()],
        "Optional" => vec!["java.util.Optional".to_string()],
        "Objects" => vec!["java.util.Objects".to_string()],
        "UUID" => vec!["java.util.UUID".to_string()],
        "Date" => vec!["java.util.Date".to_string(), "java.sql.Date".to_string()],
        "Pattern" => vec!["java.util.regex.Pattern".to_string()],
        "Matcher" => vec!["java.util.regex.Matcher".to_string()],
        "File" => vec!["java.io.File".to_string()],
        "Path" => vec!["java.nio.file.Path".to_string()],
        "Paths" => vec!["java.nio.file.Paths".to_string()],
        "Files" => vec!["java.nio.file.Files".to_string()],
        "InputStream" => vec!["java.io.InputStream".to_string()],
        "OutputStream" => vec!["java.io.OutputStream".to_string()],
        "FileInputStream" => vec!["java.io.FileInputStream".to_string()],
        "FileOutputStream" => vec!["java.io.FileOutputStream".to_string()],
        "BufferedReader" => vec!["java.io.BufferedReader".to_string()],
        "BufferedWriter" => vec!["java.io.BufferedWriter".to_string()],
        "InputStreamReader" => vec!["java.io.InputStreamReader".to_string()],
        "OutputStreamWriter" => vec!["java.io.OutputStreamWriter".to_string()],
        "IOException" => vec!["java.io.IOException".to_string()],
        "SQLException" => vec!["java.sql.SQLException".to_string()],
        "CompletableFuture" => vec!["java.util.concurrent.CompletableFuture".to_string()],
        "ConcurrentHashMap" => vec!["java.util.concurrent.ConcurrentHashMap".to_string()],
        "CountDownLatch" => vec!["java.util.concurrent.CountDownLatch".to_string()],
        "ExecutorService" => vec!["java.util.concurrent.ExecutorService".to_string()],
        "Executors" => vec!["java.util.concurrent.Executors".to_string()],
        "TimeUnit" => vec!["java.util.concurrent.TimeUnit".to_string()],
        "BigDecimal" => vec!["java.math.BigDecimal".to_string()],
        "BigInteger" => vec!["java.math.BigInteger".to_string()],
        "Duration" => vec!["java.time.Duration".to_string()],
        "Instant" => vec!["java.time.Instant".to_string()],
        "LocalDate" => vec!["java.time.LocalDate".to_string()],
        "LocalDateTime" => vec!["java.time.LocalDateTime".to_string()],
        "LocalTime" => vec!["java.time.LocalTime".to_string()],
        "ZonedDateTime" => vec!["java.time.ZonedDateTime".to_string()],
        "Stream" => vec!["java.util.stream.Stream".to_string()],
        "Collectors" => vec!["java.util.stream.Collectors".to_string()],
        "Consumer" => vec!["java.util.function.Consumer".to_string()],
        "Function" => vec!["java.util.function.Function".to_string()],
        "Predicate" => vec!["java.util.function.Predicate".to_string()],
        "Supplier" => vec!["java.util.function.Supplier".to_string()],
        _ => Vec::new(),
    }
}

/// Retrieve or synthesize a built-in ClassInfo for standard JDK classes
pub fn get_builtin_jdk_class(fqn: &str) -> Option<ClassInfo> {
    if !is_standard_jdk_package(fqn) {
        return None;
    }

    let simple_name = fqn.rsplit('.').next().unwrap_or(fqn).to_string();
    if !crate::syntax::is_valid_java_identifier(&simple_name) {
        return None;
    }
    if simple_name.is_empty() || !simple_name.chars().next().unwrap_or(' ').is_uppercase() || simple_name.contains('_') {
        return None;
    }

    let package_name = if let Some(idx) = fqn.rfind('.') {
        fqn[..idx].to_string()
    } else {
        String::new()
    };

    if package_name == "java.lang" && !is_standard_java_lang_class(&simple_name) {
        return None;
    }

    let is_interface = matches!(
        simple_name.as_str(),
        "List"
            | "Set"
            | "Map"
            | "Collection"
            | "Queue"
            | "Deque"
            | "Iterable"
            | "Iterator"
            | "Comparable"
            | "Cloneable"
            | "Serializable"
            | "Closeable"
            | "AutoCloseable"
            | "Runnable"
            | "Callable"
            | "Future"
            | "Lock"
            | "ReadWriteLock"
            | "Condition"
            | "BlockingQueue"
            | "Executor"
            | "ExecutorService"
            | "ScheduledExecutorService"
            | "Stream"
            | "Consumer"
            | "Supplier"
            | "Function"
            | "Predicate"
            | "BiConsumer"
            | "BiFunction"
            | "BiPredicate"
    );

    let kind = if is_interface {
        ClassKind::Interface
    } else {
        ClassKind::Class
    };

    Some(ClassInfo {
        fqn: fqn.to_string(),
        simple_name,
        package_name,
        kind,
        super_class: Some("java.lang.Object".to_string()),
        interfaces: vec![],
        methods: vec![],
        fields: vec![],
        source_path: None,
        is_public: true,
    })
}

/// Fallback lookup by simple name for popular JDK standard classes
pub fn get_builtin_jdk_class_by_simple_name(simple_name: &str) -> Option<ClassInfo> {
    let fqn = match simple_name {
        "String" => "java.lang.String",
        "Object" => "java.lang.Object",
        "Class" => "java.lang.Class",
        "Integer" => "java.lang.Integer",
        "Long" => "java.lang.Long",
        "Boolean" => "java.lang.Boolean",
        "Double" => "java.lang.Double",
        "Float" => "java.lang.Float",
        "Byte" => "java.lang.Byte",
        "Short" => "java.lang.Short",
        "Character" => "java.lang.Character",
        "CharSequence" => "java.lang.CharSequence",
        "StringBuilder" => "java.lang.StringBuilder",
        "StringBuffer" => "java.lang.StringBuffer",
        "Thread" => "java.lang.Thread",
        "Runnable" => "java.lang.Runnable",
        "Throwable" => "java.lang.Throwable",
        "Exception" => "java.lang.Exception",
        "RuntimeException" => "java.lang.RuntimeException",
        "System" => "java.lang.System",
        "Math" => "java.lang.Math",
        "CountDownLatch" => "java.util.concurrent.CountDownLatch",
        "AtomicBoolean" => "java.util.concurrent.atomic.AtomicBoolean",
        "AtomicInteger" => "java.util.concurrent.atomic.AtomicInteger",
        "AtomicLong" => "java.util.concurrent.atomic.AtomicLong",
        "AtomicReference" => "java.util.concurrent.atomic.AtomicReference",
        "ConcurrentHashMap" => "java.util.concurrent.ConcurrentHashMap",
        "CopyOnWriteArrayList" => "java.util.concurrent.CopyOnWriteArrayList",
        "CopyOnWriteArraySet" => "java.util.concurrent.CopyOnWriteArraySet",
        "BlockingQueue" => "java.util.concurrent.BlockingQueue",
        "ArrayBlockingQueue" => "java.util.concurrent.ArrayBlockingQueue",
        "LinkedBlockingQueue" => "java.util.concurrent.LinkedBlockingQueue",
        "ConcurrentLinkedQueue" => "java.util.concurrent.ConcurrentLinkedQueue",
        "ConcurrentLinkedDeque" => "java.util.concurrent.ConcurrentLinkedDeque",
        "ExecutorService" => "java.util.concurrent.ExecutorService",
        "ThreadPoolExecutor" => "java.util.concurrent.ThreadPoolExecutor",
        "ScheduledExecutorService" => "java.util.concurrent.ScheduledExecutorService",
        "Executors" => "java.util.concurrent.Executors",
        "Future" => "java.util.concurrent.Future",
        "CompletableFuture" => "java.util.concurrent.CompletableFuture",
        "Callable" => "java.util.concurrent.Callable",
        "Semaphore" => "java.util.concurrent.Semaphore",
        "CyclicBarrier" => "java.util.concurrent.CyclicBarrier",
        "ReentrantLock" => "java.util.concurrent.locks.ReentrantLock",
        "ReentrantReadWriteLock" => "java.util.concurrent.locks.ReentrantReadWriteLock",
        "Lock" => "java.util.concurrent.locks.Lock",
        "ReadWriteLock" => "java.util.concurrent.locks.ReadWriteLock",
        "Condition" => "java.util.concurrent.locks.Condition",
        "File" => "java.io.File",
        "InputStream" => "java.io.InputStream",
        "OutputStream" => "java.io.OutputStream",
        "FileInputStream" => "java.io.FileInputStream",
        "FileOutputStream" => "java.io.FileOutputStream",
        "BufferedReader" => "java.io.BufferedReader",
        "BufferedWriter" => "java.io.BufferedWriter",
        "List" => "java.util.List",
        "ArrayList" => "java.util.ArrayList",
        "LinkedList" => "java.util.LinkedList",
        "Map" => "java.util.Map",
        "HashMap" => "java.util.HashMap",
        "Set" => "java.util.Set",
        "HashSet" => "java.util.HashSet",
        "Collection" => "java.util.Collection",
        "Collections" => "java.util.Collections",
        "Arrays" => "java.util.Arrays",
        "Objects" => "java.util.Objects",
        "Optional" => "java.util.Optional",
        "Stream" => "java.util.stream.Stream",
        "Collectors" => "java.util.stream.Collectors",
        "Function" => "java.util.function.Function",
        "Consumer" => "java.util.function.Consumer",
        "Supplier" => "java.util.function.Supplier",
        "Predicate" => "java.util.function.Predicate",
        "Duration" => "java.time.Duration",
        "Instant" => "java.time.Instant",
        "LocalDate" => "java.time.LocalDate",
        "LocalTime" => "java.time.LocalTime",
        "LocalDateTime" => "java.time.LocalDateTime",
        "BigDecimal" => "java.math.BigDecimal",
        "BigInteger" => "java.math.BigInteger",
        _ => {
            if is_standard_java_lang_class(simple_name) {
                let cand = format!("java.lang.{simple_name}");
                return get_builtin_jdk_class(&cand);
            }
            let common = find_common_jdk_classes_by_name(simple_name);
            if let Some(first) = common.into_iter().next() {
                return get_builtin_jdk_class(&first);
            }
            return None;
        }
    };
    get_builtin_jdk_class(fqn)
}

/// Returns true if the import matches common Java ecosystem frameworks (Spring, Jakarta, Lombok, Slf4j, etc.)
pub fn is_known_framework_or_library_class(imp: &str) -> bool {
    const FRAMEWORK_PREFIXES: &[&str] = &[
        "org.springframework.",
        "jakarta.",
        "javax.",
        "lombok.",
        "org.apache.ibatis.",
        "org.mybatis.",
        "com.baomidou.mybatisplus.",
        "org.slf4j.",
        "org.apache.logging.log4j.",
        "org.junit.",
        "org.testng.",
        "org.mockito.",
        "com.google.common.",
        "com.fasterxml.jackson.",
        "com.alibaba.fastjson.",
        "com.alibaba.fastjson2.",
        "org.apache.commons.",
        "cn.hutool.",
        "com.alibaba.druid.",
        "com.alibaba.nacos.",
        "org.apache.dubbo.",
        "org.apache.kafka.",
        "org.apache.rocketmq.",
        "org.apache.poi.",
        "org.apache.http.",
        "org.apache.shiro.",
        "com.google.gson.",
        "io.swagger.",
        "feign.",
        "org.hibernate.",
        "io.micrometer.",
        "io.netty.",
        "io.reactivex.",
        "reactor.",
        "redis.clients.jedis.",
        "org.redisson.",
        "com.zaxxer.hikari.",
        "org.quartz.",
        "org.mapstruct.",
        "com.mongodb.",
        "org.elasticsearch.",
        "org.opensearch.",
        "io.grpc.",
        "com.squareup.okhttp3.",
        "okhttp3.",
        "io.undertow.",
        "org.eclipse.jetty.",
        "org.apache.catalina.",
        "org.apache.tomcat.",
        "org.apache.curator.",
        "org.apache.zookeeper.",
        "org.apache.shardingsphere.",
        "com.github.benmanes.caffeine.",
        "org.assertj.",
        "org.hamcrest.",
        "org.json.",
        "io.jsonwebtoken.",
        "com.auth0.jwt.",
        "com.squareup.retrofit2.",
        "retrofit2.",
        "com.google.protobuf.",
        "org.apache.thrift.",
        "org.apache.avro.",
        "io.minio.",
        "com.aliyun.oss.",
        "com.qcloud.cos.",
        "org.yaml.snakeyaml.",
        "com.netflix.",
        "org.aspectj.",
        "ch.qos.logback.",
        "org.apache.logging.",
        "io.github.resilience4j.",
        "com.alibaba.csp.sentinel.",
        "com.github.pagehelper.",
        "io.lettuce.core.",
        "com.rabbitmq.",
    ];
    FRAMEWORK_PREFIXES.iter().any(|prefix| imp.starts_with(prefix))
}

/// Retrieve or synthesize a built-in ClassInfo for core framework annotations and classes
pub fn get_builtin_framework_class(fqn: &str) -> Option<ClassInfo> {
    if !is_known_framework_or_library_class(fqn) {
        return None;
    }

    let simple_name = fqn.rsplit('.').next().unwrap_or(fqn).to_string();
    if !crate::syntax::is_valid_java_identifier(&simple_name) {
        return None;
    }
    let package_name = if let Some(idx) = fqn.rfind('.') {
        fqn[..idx].to_string()
    } else {
        String::new()
    };

    // 1. High-fidelity definitions for standard Spring MVC / REST annotations
    if fqn == "org.springframework.web.bind.annotation.RestController" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "RestController".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Class("java.lang.String".to_string()),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: Some("The value may indicate a suggestion for a logical component name".to_string()),
                body: Some("default \"\"".to_string()),
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.springframework.stereotype.Controller" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Controller".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Class("java.lang.String".to_string()),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: None,
                body: Some("default \"\"".to_string()),
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.springframework.web.bind.annotation.ResponseBody" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "ResponseBody".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    // MyBatis annotations
    if fqn == "org.apache.ibatis.annotations.Param" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Param".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Class("java.lang.String".to_string()),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: Some("The parameter name in SQL statement".to_string()),
                body: None,
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.apache.ibatis.annotations.Update" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Update".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Array(Box::new(TypeSignature::Class("java.lang.String".to_string()))),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: Some("SQL update statement".to_string()),
                body: None,
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.apache.ibatis.annotations.Select" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Select".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Array(Box::new(TypeSignature::Class("java.lang.String".to_string()))),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: Some("SQL select statement".to_string()),
                body: None,
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.apache.ibatis.annotations.Insert" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Insert".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Array(Box::new(TypeSignature::Class("java.lang.String".to_string()))),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: Some("SQL insert statement".to_string()),
                body: None,
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.apache.ibatis.annotations.Delete" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Delete".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![MethodInfo {
                name: "value".to_string(),
                return_type: TypeSignature::Array(Box::new(TypeSignature::Class("java.lang.String".to_string()))),
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: Some("SQL delete statement".to_string()),
                body: None,
            }],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    if fqn == "org.apache.ibatis.annotations.Mapper" {
        return Some(ClassInfo {
            fqn: fqn.to_string(),
            simple_name: "Mapper".to_string(),
            package_name: package_name.clone(),
            kind: ClassKind::Annotation,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: None,
            is_public: true,
        });
    }

    // 2. Synthesize ClassInfo for other recognized framework types
    let is_annotation = package_name.contains(".annotation")
        || package_name.contains(".annotations")
        || package_name.contains(".constraints")
        || package_name.contains(".ibatis.")
        || package_name.contains(".mybatis.")
        || simple_name.ends_with("Mapping")
        || matches!(
            simple_name.as_str(),
            "Data"
                | "Getter"
                | "Setter"
                | "Builder"
                | "NoArgsConstructor"
                | "AllArgsConstructor"
                | "RequiredArgsConstructor"
                | "ToString"
                | "EqualsAndHashCode"
                | "Value"
                | "Slf4j"
                | "Autowired"
                | "Qualifier"
                | "Override"
                | "Bean"
                | "Configuration"
                | "Service"
                | "Component"
                | "Repository"
                | "Controller"
                | "RestController"
                | "ResponseBody"
                | "RequestBody"
                | "PathVariable"
                | "RequestParam"
                | "RequestHeader"
                | "ResponseStatus"
                | "ExceptionHandler"
                | "Transactional"
                | "SpringBootApplication"
                | "Entity"
                | "Table"
                | "Id"
                | "GeneratedValue"
                | "Column"
                | "NotNull"
                | "NotBlank"
                | "NotEmpty"
                | "Size"
                | "Min"
                | "Max"
                | "Resource"
                | "PostConstruct"
                | "PreDestroy"
                | "Param"
                | "Update"
                | "Select"
                | "Insert"
                | "Delete"
                | "Mapper"
                | "Options"
                | "Results"
                | "Result"
                | "ResultMap"
                | "One"
                | "Many"
                | "SelectProvider"
                | "InsertProvider"
                | "UpdateProvider"
                | "DeleteProvider"
                | "TableName"
                | "TableId"
                | "TableField"
        );

    let kind = if is_annotation {
        ClassKind::Annotation
    } else if simple_name.ends_with("Repository")
        || (simple_name.ends_with("Service") && simple_name.starts_with('I'))
    {
        ClassKind::Interface
    } else {
        ClassKind::Class
    };

    Some(ClassInfo {
        fqn: fqn.to_string(),
        simple_name,
        package_name,
        kind,
        super_class: None,
        interfaces: vec![],
        methods: vec![],
        fields: vec![],
        source_path: None,
        is_public: true,
    })
}

/// Fallback lookup by simple name for popular framework annotations and classes
pub fn get_builtin_framework_class_by_simple_name(simple_name: &str) -> Option<ClassInfo> {
    match simple_name {
        "RestController" => get_builtin_framework_class("org.springframework.web.bind.annotation.RestController"),
        "Controller" => get_builtin_framework_class("org.springframework.stereotype.Controller"),
        "ResponseBody" => get_builtin_framework_class("org.springframework.web.bind.annotation.ResponseBody"),
        "RequestBody" => get_builtin_framework_class("org.springframework.web.bind.annotation.RequestBody"),
        "RequestMapping" => get_builtin_framework_class("org.springframework.web.bind.annotation.RequestMapping"),
        "GetMapping" => get_builtin_framework_class("org.springframework.web.bind.annotation.GetMapping"),
        "PostMapping" => get_builtin_framework_class("org.springframework.web.bind.annotation.PostMapping"),
        "PutMapping" => get_builtin_framework_class("org.springframework.web.bind.annotation.PutMapping"),
        "DeleteMapping" => get_builtin_framework_class("org.springframework.web.bind.annotation.DeleteMapping"),
        "PathVariable" => get_builtin_framework_class("org.springframework.web.bind.annotation.PathVariable"),
        "RequestParam" => get_builtin_framework_class("org.springframework.web.bind.annotation.RequestParam"),
        "RequestHeader" => get_builtin_framework_class("org.springframework.web.bind.annotation.RequestHeader"),
        "ResponseStatus" => get_builtin_framework_class("org.springframework.web.bind.annotation.ResponseStatus"),
        "ExceptionHandler" => get_builtin_framework_class("org.springframework.web.bind.annotation.ExceptionHandler"),
        "Service" => get_builtin_framework_class("org.springframework.stereotype.Service"),
        "Component" => get_builtin_framework_class("org.springframework.stereotype.Component"),
        "Repository" => get_builtin_framework_class("org.springframework.stereotype.Repository"),
        "Autowired" => get_builtin_framework_class("org.springframework.beans.factory.annotation.Autowired"),
        "Qualifier" => get_builtin_framework_class("org.springframework.beans.factory.annotation.Qualifier"),
        "Value" => get_builtin_framework_class("org.springframework.beans.factory.annotation.Value"),
        "SpringBootApplication" => get_builtin_framework_class("org.springframework.boot.autoconfigure.SpringBootApplication"),
        "Configuration" => get_builtin_framework_class("org.springframework.context.annotation.Configuration"),
        "Bean" => get_builtin_framework_class("org.springframework.context.annotation.Bean"),
        "Transactional" => get_builtin_framework_class("org.springframework.transaction.annotation.Transactional"),
        "Param" => get_builtin_framework_class("org.apache.ibatis.annotations.Param"),
        "Update" => get_builtin_framework_class("org.apache.ibatis.annotations.Update"),
        "Select" => get_builtin_framework_class("org.apache.ibatis.annotations.Select"),
        "Insert" => get_builtin_framework_class("org.apache.ibatis.annotations.Insert"),
        "Delete" => get_builtin_framework_class("org.apache.ibatis.annotations.Delete"),
        "Mapper" => get_builtin_framework_class("org.apache.ibatis.annotations.Mapper"),
        "Options" => get_builtin_framework_class("org.apache.ibatis.annotations.Options"),
        "Results" => get_builtin_framework_class("org.apache.ibatis.annotations.Results"),
        "Result" => get_builtin_framework_class("org.apache.ibatis.annotations.Result"),
        "TableName" => get_builtin_framework_class("com.baomidou.mybatisplus.annotation.TableName"),
        "TableId" => get_builtin_framework_class("com.baomidou.mybatisplus.annotation.TableId"),
        "TableField" => get_builtin_framework_class("com.baomidou.mybatisplus.annotation.TableField"),
        "Data" => get_builtin_framework_class("lombok.Data"),
        "Getter" => get_builtin_framework_class("lombok.Getter"),
        "Setter" => get_builtin_framework_class("lombok.Setter"),
        "Builder" => get_builtin_framework_class("lombok.Builder"),
        "Slf4j" => get_builtin_framework_class("lombok.extern.slf4j.Slf4j"),
        "Logger" => get_builtin_framework_class("org.slf4j.Logger"),
        "LoggerFactory" => get_builtin_framework_class("org.slf4j.LoggerFactory"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_live_jdk_home() {
        let jh = find_java_home();
        println!("find_java_home() = {:?}", jh);
        assert!(jh.is_some());
        let reg = ClasspathRegistry::new();
        let res = reg.add_jdk_home(jh.unwrap()).await;
        println!("add_jdk_home result = {:?}", res);
        println!("get_class(File) = {:?}", reg.get_class("java.io.File").await.is_some());
        println!("get_class(ArrayList) = {:?}", reg.get_class("java.util.ArrayList").await.is_some());
        println!("get_class(Collection) = {:?}", reg.get_class("java.util.Collection").await.is_some());
    }

    #[tokio::test]
    async fn test_measure_actual_rss_footprint() {
        fn get_rss_kb() -> usize {
            let pid = std::process::id();
            let out = std::process::Command::new("ps")
                .arg("-o")
                .arg("rss=")
                .arg("-p")
                .arg(pid.to_string())
                .output()
                .expect("ps command");
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            s.parse::<usize>().unwrap_or(0)
        }

        let rss_start = get_rss_kb();
        let registry = ClasspathRegistry::new();
        if let Some(jh) = find_java_home() {
            let _ = registry.add_jdk_home(jh).await;
        }
        let rss_after_jdk = get_rss_kb();

        // Also load all jars in ~/.jlsp/cache/jars
        let jar_dir = crate::classpath::cache::global_jar_cache_dir();
        if let Ok(entries) = std::fs::read_dir(&jar_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.to_string_lossy().ends_with(".bin.zst") {
                    let _ = registry.load_cache(&p).await;
                }
            }
        }
        let rss_after_jars = get_rss_kb();
        let class_count = registry.store.read().await.class_count();

        println!("=== RSS MEMORY MEASUREMENT ===");
        println!("Start RSS: {:.2} MB", rss_start as f64 / 1024.0);
        println!("After JDK RSS: {:.2} MB", rss_after_jdk as f64 / 1024.0);
        println!("After Jars RSS: {:.2} MB (Total classes: {})", rss_after_jars as f64 / 1024.0, class_count);
        println!("Net Memory Used: {:.2} MB", (rss_after_jars.saturating_sub(rss_start)) as f64 / 1024.0);
    }

    #[tokio::test]
    async fn test_cache_save_and_load_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("jlsp_test_cache_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let cache_file = temp_dir.join("index.bin.zst");

        let registry = ClasspathRegistry::new();
        registry.register_class(ClassInfo {
            fqn: "com.example.Service".to_string(),
            simple_name: "Service".to_string(),
            package_name: "com.example".to_string(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: Some("/path/to/lib.jar".to_string()),
            is_public: true,
        }).await;

        let saved = registry.save_cache(&cache_file).await.expect("save should succeed");
        assert_eq!(saved, 1);
        assert!(cache_file.exists());

        // Create new registry and load from disk
        let new_registry = ClasspathRegistry::new();
        let loaded = new_registry.load_cache(&cache_file).await.expect("load should succeed");
        assert_eq!(loaded, 1);

        let cls = new_registry.get_class("com.example.Service").await;
        assert!(cls.is_some());
        let cls = cls.unwrap();
        assert_eq!(cls.simple_name, "Service");
        assert_eq!(cls.package_name, "com.example");
        assert_eq!(cls.source_path, Some("/path/to/lib.jar".to_string()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_cache_legacy_json_fallback() {
        let temp_dir = std::env::temp_dir().join(format!("jlsp_test_legacy_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let json_file = temp_dir.join("index.json");

        let classes = vec![ClassInfo {
            fqn: "com.example.OldClass".to_string(),
            simple_name: "OldClass".to_string(),
            package_name: "com.example".to_string(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: None,
            is_public: true,
        }];
        std::fs::write(&json_file, serde_json::to_vec(&classes).unwrap()).unwrap();

        let registry = ClasspathRegistry::new();
        // Request index.bin.zst, should fallback to index.json transparently
        let requested_bin = temp_dir.join("index.bin.zst");
        let loaded = registry.load_cache(&requested_bin).await.expect("load should succeed");
        assert_eq!(loaded, 1);
        assert!(registry.get_class("com.example.OldClass").await.is_some());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_memory_optimizations_and_deduplication() {
        let registry = ClasspathRegistry::new();

        // 1. Register a class from jar A
        registry.register_class(ClassInfo {
            fqn: "com.demo.Foo".to_string(),
            simple_name: "Foo".to_string(),
            package_name: "com.demo".to_string(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: Some("/libs/foo-1.0.jar".to_string()),
            is_public: true,
        }).await;

        // Since it's unique, origins map should NOT allocate an entry
        assert_eq!(registry.origins.read().await.len(), 0);
        // But get_class_origins should still return its source path
        assert_eq!(registry.get_class_origins("com.demo.Foo").await, vec!["/libs/foo-1.0.jar"]);

        // 2. Register the same class again from the same jar (should NOT duplicate simple_name_index or origins)
        registry.register_class(ClassInfo {
            fqn: "com.demo.Foo".to_string(),
            simple_name: "Foo".to_string(),
            package_name: "com.demo".to_string(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: Some("/libs/foo-1.0.jar".to_string()),
            is_public: true,
        }).await;

        let duplicate_count = registry.store.read().await.simple_class_indices("Foo").len();
        assert_eq!(duplicate_count, 1, "Simple name index must NOT contain duplicate FQNs!");
        assert_eq!(registry.origins.read().await.len(), 0, "No duplicate origin entry should exist!");

        // 3. Register conflicting class from jar B -> Now origins should record the conflict
        registry.register_class(ClassInfo {
            fqn: "com.demo.Foo".to_string(),
            simple_name: "Foo".to_string(),
            package_name: "com.demo".to_string(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: Some("/libs/foo-2.0.jar".to_string()),
            is_public: true,
        }).await;

        assert_eq!(registry.origins.read().await.len(), 1, "Conflict should now be recorded!");
        let origins = registry.get_class_origins("com.demo.Foo").await;
        assert_eq!(origins.len(), 2);
        assert!(origins.contains(&"/libs/foo-1.0.jar".to_string()));
        assert!(origins.contains(&"/libs/foo-2.0.jar".to_string()));
    }
}

