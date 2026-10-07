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

impl GradleProject {
    pub fn parse<P: AsRef<Path>>(gradle_path: P) -> Result<Self, String> {
        let content = fs::read_to_string(&gradle_path)
            .map_err(|e| format!("Failed to read gradle file: {e}"))?;

        let mut dependencies = Vec::new();

        // Regex-free robust parser for:
        // implementation 'group:artifact:version'
        // implementation("group:artifact:version")
        // api 'group:artifact:version'
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("/*") {
                continue;
            }

            for prefix in &["implementation", "api", "compileOnly", "runtimeOnly"] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    let rest = rest.trim_start();
                    let cleaned = rest
                        .trim_matches(|c| c == '(' || c == ')' || c == '\'' || c == '"' || c == ',' || c == ' ');
                    let parts: Vec<&str> = cleaned.split(':').collect();
                    if parts.len() == 3 {
                        dependencies.push(GradleDependency {
                            group_id: parts[0].trim().to_string(),
                            artifact_id: parts[1].trim().to_string(),
                            version: parts[2].trim().to_string(),
                        });
                    }
                }
            }
        }

        Ok(Self {
            gradle_file: gradle_path.as_ref().to_path_buf(),
            dependencies,
        })
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
}
