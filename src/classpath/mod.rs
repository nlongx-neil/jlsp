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
use crate::model::{ClassInfo, ClassKind, MethodInfo, TypeSignature};

#[derive(Default)]
pub struct ClasspathRegistry {
    /// Mapping from FQN (e.g. "java.lang.String") to ClassInfo
    classes: RwLock<HashMap<String, Arc<ClassInfo>>>,
    /// Mapping from Simple Name (e.g. "String") to list of FQNs
    simple_name_index: RwLock<HashMap<String, Vec<String>>>,
    /// Mapping from FQN to list of source paths/jars (for conflict detection)
    origins: RwLock<HashMap<String, Vec<String>>>,
    /// Saved JDK Home directory (for src.zip lookup)
    jdk_home: RwLock<Option<std::path::PathBuf>>,
}

impl ClasspathRegistry {
    pub fn new() -> Self {
        Self {
            classes: RwLock::new(HashMap::new()),
            simple_name_index: RwLock::new(HashMap::new()),
            origins: RwLock::new(HashMap::new()),
            jdk_home: RwLock::new(None),
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
        if let Some(cls) = classes.get(fqn) {
            return Some(cls.clone());
        }
        drop(classes);

        if let Some(builtin) = get_builtin_framework_class(fqn) {
            return Some(Arc::new(builtin));
        }

        if let Some(jdk_cls) = get_builtin_jdk_class(fqn) {
            return Some(Arc::new(jdk_cls));
        }

        None
    }

    pub async fn find_classes_by_simple_name(&self, simple_name: &str) -> Vec<Arc<ClassInfo>> {
        let s_idx = self.simple_name_index.read().await;
        let classes = self.classes.read().await;

        let mut res = Vec::new();
        if let Some(fqns) = s_idx.get(simple_name) {
            res.extend(fqns.iter().filter_map(|fqn| classes.get(fqn).cloned()));
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

    pub async fn all_classes(&self) -> Vec<Arc<ClassInfo>> {
        let classes = self.classes.read().await;
        classes.values().cloned().collect()
    }

    pub async fn has_classes(&self) -> bool {
        let classes = self.classes.read().await;
        !classes.is_empty()
    }

    pub async fn has_jdk_loaded(&self) -> bool {
        let classes = self.classes.read().await;
        classes.contains_key("java.lang.Object")
            || classes.contains_key("java.util.List")
            || classes.contains_key("java.io.File")
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
            let cache_key = format!("jdk_{:016x}.json", hasher.finish());
            let cache_file = cache_dir.join(cache_key);

            // Fast warm start from disk cache
            if cache_file.exists() {
                if let Ok(loaded) = self.load_cache(&cache_file).await {
                    if loaded > 0 {
                        info!("Fast warm start: loaded {} JDK classes from cache file: {:?}", loaded, cache_file);
                        return Ok(loaded);
                    }
                }
            }

            // Strategy A: jimage CLI tool (extract standard java.* modules)
            let jimage_bin = path.join("bin").join(if cfg!(windows) { "jimage.exe" } else { "jimage" });
            if jimage_bin.exists() {
                info!("Using JDK jimage tool at {:?}", jimage_bin);
                let temp_extract_dir = std::env::temp_dir().join(format!("jlsp_jimage_extract_{}_{:x}", std::process::id(), hasher.finish()));
                let _ = std::fs::create_dir_all(&temp_extract_dir);

                let extract_res = tokio::process::Command::new(&jimage_bin)
                    .arg("extract")
                    .arg("--include")
                    .arg("glob:/java.*/**")
                    .arg("--dir")
                    .arg(&temp_extract_dir)
                    .arg(&modules_path)
                    .output()
                    .await;

                if let Ok(output) = extract_res {
                    if output.status.success() {
                        if let Ok(parsed_classes) = crate::classpath::class_reader::parse_class_dir(&temp_extract_dir) {
                            let count = parsed_classes.len();
                            for cls in parsed_classes {
                                self.register_class(cls).await;
                            }
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
                    for cls in classes {
                        self.register_class(cls).await;
                    }
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
                for cls in classes {
                    self.register_class(cls).await;
                }
                return Ok(count);
            }
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

pub fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

/// Check if a directory is a valid JDK/JRE directory containing modules, rt.jar or src.zip
pub fn is_valid_jdk_dir<P: AsRef<std::path::Path>>(p: P) -> bool {
    let path = p.as_ref();
    path.join("lib/modules").exists()
        || path.join("jre/lib/rt.jar").exists()
        || path.join("lib/rt.jar").exists()
        || path.join("Classes/classes.jar").exists()
        || path.join("lib/src.zip").exists()
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
        candidates.push(home.join(".sdkman/candidates/java/current"));
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
    ];
    FRAMEWORK_PREFIXES.iter().any(|prefix| imp.starts_with(prefix))
}

/// Retrieve or synthesize a built-in ClassInfo for core framework annotations and classes
pub fn get_builtin_framework_class(fqn: &str) -> Option<ClassInfo> {
    if !is_known_framework_or_library_class(fqn) {
        return None;
    }

    let simple_name = fqn.rsplit('.').next().unwrap_or(fqn).to_string();
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
}

