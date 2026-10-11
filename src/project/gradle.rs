use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GradleDependency {
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
}

pub struct GradleProject {
    pub gradle_file: PathBuf,
    pub dependencies: Vec<GradleDependency>,
}

const GRADLE_CONFIG_PREFIXES: &[&str] = &[
    "implementation",
    "testImplementation",
    "api",
    "testApi",
    "compileOnly",
    "testCompileOnly",
    "runtimeOnly",
    "testRuntimeOnly",
    "annotationProcessor",
    "kapt",
    "compile",
    "testCompile",
];

impl GradleProject {
    pub fn parse<P: AsRef<Path>>(gradle_path: P) -> Result<Self, String> {
        let content = fs::read_to_string(&gradle_path)
            .map_err(|e| format!("Failed to read gradle file: {e}"))?;

        let mut dependencies = Vec::new();

        // 1. Check for Gradle Version Catalog (gradle/libs.versions.toml)
        let catalog_libs = if let Some(parent) = gradle_path.as_ref().parent() {
            let direct_catalog = parent.join("gradle").join("libs.versions.toml");
            let root_catalog = parent.parent().map(|p| p.join("gradle").join("libs.versions.toml"));
            let catalog_path = if direct_catalog.exists() {
                Some(direct_catalog)
            } else if let Some(rc) = root_catalog {
                if rc.exists() { Some(rc) } else { None }
            } else {
                None
            };

            if let Some(cp) = catalog_path {
                if let Ok(toml_str) = fs::read_to_string(&cp) {
                    Self::parse_version_catalog(&toml_str)
                } else {
                    HashMap::new()
                }
            } else {
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

        // 2. Parse dependencies in build.gradle / build.gradle.kts
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
                continue;
            }

            for prefix in GRADLE_CONFIG_PREFIXES {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let rest = rest.trim_start();
                    // Case A: Version catalog reference, e.g. implementation(libs.junit.jupiter) or testImplementation libs.junit_jupiter
                    if let Some(libs_idx) = rest.find("libs.") {
                        let after_libs = &rest[libs_idx + 5..];
                        let alias = after_libs
                            .trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-')
                            .replace(['-', '_'], ".");
                        if let Some(dep) = catalog_libs.get(&alias) {
                            if !dependencies.contains(dep) {
                                dependencies.push(dep.clone());
                            }
                            break;
                        }
                    }

                    // Case B: Groovy / Kotlin Map syntax:
                    // implementation group: 'org.slf4j', name: 'slf4j-api', version: '2.0.7'
                    // implementation(group = "org.slf4j", name = "slf4j-api", version = "2.0.7")
                    if (rest.contains("group") && rest.contains("name")) || (rest.contains("group:") && rest.contains("name:")) {
                        let mut group = String::new();
                        let mut name = String::new();
                        let mut version = String::new();
                        for part in rest.split(',') {
                            let part = part.trim();
                            for delim in &[':', '='] {
                                if let Some((k, v)) = part.split_once(*delim) {
                                    let k_clean = k.trim().trim_matches(|c: char| !c.is_alphabetic());
                                    let v_clean = v.trim().trim_matches(|c: char| c == '\'' || c == '"' || c == '(' || c == ')' || c == ' ');
                                    if k_clean == "group" {
                                        group = v_clean.to_string();
                                    } else if k_clean == "name" {
                                        name = v_clean.to_string();
                                    } else if k_clean == "version" {
                                        version = v_clean.to_string();
                                    }
                                }
                            }
                        }
                        if !group.is_empty() && !name.is_empty() {
                            let dep = GradleDependency {
                                group_id: group,
                                artifact_id: name,
                                version,
                            };
                            if !dependencies.contains(&dep) {
                                dependencies.push(dep);
                            }
                            break;
                        }
                    }

                    // Case C: Standard string notation & Kotlin DSL functions:
                    // implementation 'group:artifact:version'
                    // testImplementation("group:artifact:version")
                    // implementation(platform("group:artifact:version"))
                    // implementation(kotlin("stdlib", "1.9.0"))
                    let mut cleaned_rest = rest;
                    if let Some(p_idx) = cleaned_rest.find("platform(") {
                        cleaned_rest = &cleaned_rest[p_idx + 9..];
                    } else if let Some(ep_idx) = cleaned_rest.find("enforcedPlatform(") {
                        cleaned_rest = &cleaned_rest[ep_idx + 17..];
                    }
                    if let Some(k_idx) = cleaned_rest.find("kotlin(") {
                        let after_k = &cleaned_rest[k_idx + 7..];
                        let k_cleaned = after_k.trim_matches(|c| c == '(' || c == ')' || c == '\'' || c == '"' || c == ',' || c == ' ');
                        let k_parts: Vec<&str> = k_cleaned.split(|c| c == ',' || c == '"' || c == '\'').filter(|s| !s.trim().is_empty()).collect();
                        if let Some(module) = k_parts.first() {
                            let dep = GradleDependency {
                                group_id: "org.jetbrains.kotlin".to_string(),
                                artifact_id: format!("kotlin-{}", module.trim()),
                                version: k_parts.get(1).map(|v| v.trim().to_string()).unwrap_or_default(),
                            };
                            if !dependencies.contains(&dep) {
                                dependencies.push(dep);
                            }
                            break;
                        }
                    }

                    let cleaned = cleaned_rest
                        .trim_matches(|c| c == '(' || c == ')' || c == '\'' || c == '"' || c == ',' || c == ' ');
                    let parts: Vec<&str> = cleaned.split(':').collect();
                    if parts.len() >= 2 {
                        let ver = if parts.len() >= 3 { parts[2].trim().to_string() } else { String::new() };
                        let dep = GradleDependency {
                            group_id: parts[0].trim().to_string(),
                            artifact_id: parts[1].trim().to_string(),
                            version: ver,
                        };
                        if !dependencies.contains(&dep) {
                            dependencies.push(dep);
                        }
                        break;
                    }
                }
            }
        }

        // If version catalog was present, include all defined libraries so they are accessible
        for (_, dep) in catalog_libs {
            if !dependencies.contains(&dep) {
                dependencies.push(dep);
            }
        }

        Ok(Self {
            gradle_file: gradle_path.as_ref().to_path_buf(),
            dependencies,
        })
    }

    /// Parse Gradle Version Catalog TOML format (gradle/libs.versions.toml)
    pub fn parse_version_catalog(toml_content: &str) -> HashMap<String, GradleDependency> {
        let mut versions: HashMap<String, String> = HashMap::new();
        let mut libraries: HashMap<String, GradleDependency> = HashMap::new();
        let mut current_section = "";

        for line in toml_content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('[') && trimmed.ends_with(']') {
                current_section = trimmed[1..trimmed.len() - 1].trim();
                continue;
            }

            if current_section == "versions" {
                if let Some((k, v)) = trimmed.split_once('=') {
                    let key = k.trim().trim_matches('"').trim_matches('\'').replace('-', "_");
                    let val = v.trim().trim_matches('"').trim_matches('\'').trim();
                    versions.insert(key, val.to_string());
                }
            } else if current_section == "libraries" {
                if let Some((k, v)) = trimmed.split_once('=') {
                    let alias = k.trim().trim_matches('"').trim_matches('\'').trim().replace(['-', '_'], ".");
                    let val = v.trim();
                    // Pattern 1: alias = "group:artifact:version"
                    if val.starts_with('"') || val.starts_with('\'') {
                        let cleaned = val.trim_matches('"').trim_matches('\'');
                        let parts: Vec<&str> = cleaned.split(':').collect();
                        if parts.len() >= 2 {
                            let ver = if parts.len() >= 3 { parts[2] } else { "" };
                            libraries.insert(alias.clone(), GradleDependency {
                                group_id: parts[0].to_string(),
                                artifact_id: parts[1].to_string(),
                                version: ver.to_string(),
                            });
                        }
                    } else if val.starts_with('{') && val.ends_with('}') {
                        // Pattern 2: table inline: { module = "...", version.ref = "..." } or { group = "...", name = "...", version = "..." }
                        let inner = &val[1..val.len() - 1];
                        let mut group = String::new();
                        let mut artifact = String::new();
                        let mut version = String::new();

                        for pair in inner.split(',') {
                            if let Some((pk, pv)) = pair.split_once('=') {
                                let pkey = pk.trim().replace('-', ".");
                                let pval = pv.trim().trim_matches('"').trim_matches('\'').trim();
                                if pkey == "module" {
                                    if let Some((g, a)) = pval.split_once(':') {
                                        group = g.to_string();
                                        artifact = a.to_string();
                                    }
                                } else if pkey == "group" {
                                    group = pval.to_string();
                                } else if pkey == "name" {
                                    artifact = pval.to_string();
                                } else if pkey == "version" {
                                    version = pval.to_string();
                                } else if pkey == "version.ref" {
                                    let ref_key = pval.replace('-', "_");
                                    if let Some(v) = versions.get(&ref_key) {
                                        version = v.clone();
                                    } else {
                                        version = pval.to_string();
                                    }
                                }
                            }
                        }

                        if !group.is_empty() && !artifact.is_empty() {
                            libraries.insert(alias.clone(), GradleDependency {
                                group_id: group,
                                artifact_id: artifact,
                                version,
                            });
                        }
                    }
                }
            }
        }

        libraries
    }

    /// Resolve jar files from Gradle local cache (~/.gradle/caches/modules-2/files-2.1/)
    /// or fallback to ~/.m2/repository
    pub fn resolve_local_jars(&self) -> Vec<PathBuf> {
        let mut jars = Vec::new();
        let home = match std::env::var("HOME") {
            Ok(h) => PathBuf::from(h),
            Err(_) => return jars,
        };

        // 1. Check ~/.gradle/caches/modules-2/files-2.1/<group>/<artifact>/<version>/*/jar
        let gradle_cache = home.join(".gradle").join("caches").join("modules-2").join("files-2.1");
        // 2. Check ~/.m2/repository
        let m2_repo = home.join(".m2").join("repository");

        for dep in &self.dependencies {
            let mut resolved = false;

            if gradle_cache.exists() {
                let dep_dir = gradle_cache.join(&dep.group_id).join(&dep.artifact_id).join(&dep.version);
                if dep_dir.exists() {
                    for entry in walkdir::WalkDir::new(&dep_dir).into_iter().filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.extension().and_then(|ext| ext.to_str()) == Some("jar") {
                            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                            if !file_name.ends_with("-sources.jar") && !file_name.ends_with("-javadoc.jar") {
                                info!("Resolved Gradle dependency: {:?}", path);
                                jars.push(path.to_path_buf());
                                resolved = true;
                                break;
                            }
                        }
                    }
                }
            }

            if !resolved && m2_repo.exists() {
                let group_path = dep.group_id.replace('.', "/");
                let jar_file_name = format!("{}-{}.jar", dep.artifact_id, dep.version);
                let candidate = m2_repo
                    .join(&group_path)
                    .join(&dep.artifact_id)
                    .join(&dep.version)
                    .join(&jar_file_name);

                if candidate.exists() {
                    info!("Resolved Gradle dependency via local .m2 fallback: {:?}", candidate);
                    jars.push(candidate);
                }
            }
        }

        jars
    }

    /// Recursively resolve jars across all subprojects defined in settings.gradle / settings.gradle.kts
    pub fn resolve_all_modules_jars(&self) -> Vec<PathBuf> {
        let mut jars = self.resolve_local_jars();
        if let Some(parent) = self.gradle_file.parent() {
            let settings_files = [parent.join("settings.gradle"), parent.join("settings.gradle.kts")];
            for set_file in settings_files {
                if set_file.exists()
                    && let Ok(content) = fs::read_to_string(&set_file) {
                        for line in content.lines() {
                            let trimmed = line.trim();
                            if trimmed.starts_with("include") {
                                // Extract project names, e.g. include ':core', ':api' or include("core", "api")
                                for part in trimmed.split(|c| c == ',' || c == ' ' || c == '(' || c == ')') {
                                    let clean = part.trim_matches(|c| c == '\'' || c == '"' || c == ':');
                                    if !clean.is_empty() && clean != "include" {
                                        let sub_dir = parent.join(clean);
                                        let sub_gradle = sub_dir.join("build.gradle");
                                        let sub_gradle_kts = sub_dir.join("build.gradle.kts");
                                        let target_gradle = if sub_gradle.exists() {
                                            Some(sub_gradle)
                                        } else if sub_gradle_kts.exists() {
                                            Some(sub_gradle_kts)
                                        } else {
                                            None
                                        };

                                        if let Some(sg) = target_gradle
                                            && let Ok(sub_proj) = GradleProject::parse(&sg) {
                                                info!("Found Gradle sub-project at: {:?}", sg);
                                                jars.extend(sub_proj.resolve_local_jars());
                                            }
                                    }
                                }
                            }
                        }
                    }
            }
        }
        jars
    }

    /// Trigger background daemon compile via `./gradlew classes` if wrapper exists
    pub async fn execute_daemon_compile(&self) -> Result<(), String> {
        let parent = self.gradle_file.parent().ok_or("No parent dir")?;
        let gradlew = parent.join("gradlew");
        if gradlew.exists() {
            info!("Triggering Gradle daemon background compile: {:?}", gradlew);
            let res = tokio::process::Command::new(gradlew)
                .current_dir(parent)
                .arg("classes")
                .arg("--daemon")
                .status()
                .await;
            if let Ok(st) = res && st.success() {
                return Ok(());
            }
        }
        Err("Gradle daemon build failed or wrapper not found".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_gradle_dependencies() {
        let sample = r#"
            plugins {
                id 'java'
            }

            dependencies {
                implementation 'org.springframework.boot:spring-boot-starter:3.2.0'
                api("com.google.guava:guava:32.1.2-jre")
                // comment line
                compileOnly 'org.projectlombok:lombok:1.18.30'
            }
        "#;

        let tmp_file = std::env::temp_dir().join("test_build.gradle");
        fs::write(&tmp_file, sample).unwrap();
        let project = GradleProject::parse(&tmp_file).unwrap();
        assert_eq!(project.dependencies.len(), 3);
        assert_eq!(project.dependencies[0].group_id, "org.springframework.boot");
        assert_eq!(project.dependencies[0].artifact_id, "spring-boot-starter");
        assert_eq!(project.dependencies[0].version, "3.2.0");
        assert_eq!(project.dependencies[1].group_id, "com.google.guava");
        assert_eq!(project.dependencies[2].artifact_id, "lombok");
        let _ = fs::remove_file(tmp_file);
    }

    #[test]
    fn test_parse_gradle_version_catalog_and_map_syntax() {
        let toml_sample = r#"
            [versions]
            junit = "5.9.2"
            slf4j = "2.0.7"

            [libraries]
            junit-jupiter = { module = "org.junit.jupiter:junit-jupiter", version.ref = "junit" }
            slf4j-api = { group = "org.slf4j", name = "slf4j-api", version.ref = "slf4j" }
            guava = "com.google.guava:guava:32.1.2-jre"
        "#;

        let catalog_map = GradleProject::parse_version_catalog(toml_sample);
        assert_eq!(catalog_map.len(), 3);
        assert_eq!(catalog_map["junit.jupiter"].version, "5.9.2");
        assert_eq!(catalog_map["slf4j.api"].artifact_id, "slf4j-api");
        assert_eq!(catalog_map["slf4j.api"].version, "2.0.7");
        assert_eq!(catalog_map["guava"].artifact_id, "guava");

        let gradle_sample = r#"
            dependencies {
                testImplementation(libs.junit.jupiter)
                implementation group: 'org.slf4j', name: 'slf4j-api', version: '2.0.7'
                testImplementation 'org.assertj:assertj-core:3.24.2'
            }
        "#;

        let temp_dir = std::env::temp_dir().join("gradle_test_proj");
        let gradle_dir = temp_dir.join("gradle");
        let _ = fs::create_dir_all(&gradle_dir);
        let catalog_file = gradle_dir.join("libs.versions.toml");
        let build_file = temp_dir.join("build.gradle");

        fs::write(&catalog_file, toml_sample).unwrap();
        fs::write(&build_file, gradle_sample).unwrap();

        let project = GradleProject::parse(&build_file).unwrap();
        assert!(project.dependencies.iter().any(|d| d.artifact_id == "junit-jupiter" && d.version == "5.9.2"));
        assert!(project.dependencies.iter().any(|d| d.artifact_id == "slf4j-api" && d.version == "2.0.7"));
        assert!(project.dependencies.iter().any(|d| d.artifact_id == "assertj-core" && d.version == "3.24.2"));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_gradle_kotlin_dsl_and_platform_parsing() {
        let kts_sample = r#"
            dependencies {
                implementation("org.springframework.boot:spring-boot-starter-web:3.2.0")
                implementation(platform("org.springframework.boot:spring-boot-dependencies:3.2.0"))
                implementation(kotlin("stdlib", "1.9.22"))
                testImplementation("org.junit.jupiter:junit-jupiter:5.10.0")
            }
        "#;
        let tmp_file = std::env::temp_dir().join(format!("build.gradle_{}.kts", std::process::id()));
        fs::write(&tmp_file, kts_sample).unwrap();

        let project = GradleProject::parse(&tmp_file).unwrap();
        assert!(project.dependencies.iter().any(|d| d.artifact_id == "spring-boot-starter-web" && d.version == "3.2.0"));
        assert!(project.dependencies.iter().any(|d| d.artifact_id == "spring-boot-dependencies" && d.version == "3.2.0"));
        assert!(project.dependencies.iter().any(|d| d.group_id == "org.jetbrains.kotlin" && d.artifact_id == "kotlin-stdlib" && d.version == "1.9.22"));
        assert!(project.dependencies.iter().any(|d| d.artifact_id == "junit-jupiter" && d.version == "5.10.0"));

        let _ = fs::remove_file(tmp_file);
    }
}
