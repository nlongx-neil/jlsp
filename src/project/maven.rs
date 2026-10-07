use std::fs;
use std::path::{Path, PathBuf};
use quick_xml::de::from_str;
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PomProject {
    pub group_id: Option<String>,
    pub artifact_id: Option<String>,
    pub version: Option<String>,
    pub dependencies: Option<PomDependencies>,
    pub modules: Option<PomModules>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PomModules {
    #[serde(default)]
    pub module: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PomDependencies {
    #[serde(default)]
    pub dependency: Vec<PomDependency>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PomDependency {
    pub group_id: String,
    pub artifact_id: String,
    pub version: Option<String>,
    pub scope: Option<String>,
}

pub struct MavenProject {
    pub pom_path: PathBuf,
    pub dependencies: Vec<PomDependency>,
    pub modules: Vec<String>,
}

impl MavenProject {
    pub fn parse<P: AsRef<Path>>(pom_path: P) -> Result<Self, String> {
        let content = fs::read_to_string(&pom_path)
            .map_err(|e| format!("Failed to read pom.xml: {e}"))?;
        
        let pom: PomProject = from_str(&content)
            .map_err(|e| format!("Failed to parse pom.xml: {e}"))?;

        let dependencies = pom.dependencies.map(|d| d.dependency).unwrap_or_default();
        let modules = pom.modules.map(|m| m.module).unwrap_or_default();
        Ok(Self {
            pom_path: pom_path.as_ref().to_path_buf(),
            dependencies,
            modules,
        })
    }

    /// Recursively resolve jars and sub-modules
    pub async fn resolve_all_modules_jars(&self) -> Vec<PathBuf> {
        let mut jars = self.resolve_and_download_jars().await;
        if let Some(parent) = self.pom_path.parent() {
            for sub in &self.modules {
                let sub_pom = parent.join(sub).join("pom.xml");
                if sub_pom.exists() {
                    info!("Found Maven sub-module at: {:?}", sub_pom);
                    if let Ok(sub_proj) = MavenProject::parse(&sub_pom) {
                        let sub_jars = Box::pin(sub_proj.resolve_all_modules_jars()).await;
                        jars.extend(sub_jars);
                    }
                }
            }
        }
        jars
    }

    /// Locate jar files in local Maven repository (~/.m2/repository)
    pub fn resolve_local_jars(&self) -> Vec<PathBuf> {
        let mut jars = Vec::new();
        let home = match std::env::var("HOME") {
            Ok(h) => PathBuf::from(h),
            Err(_) => return jars,
        };
        let m2_repo = home.join(".m2").join("repository");
        if !m2_repo.exists() {
            return jars;
        }

        for dep in &self.dependencies {
            if let Some(version) = &dep.version {
                // Ignore test scope or system scope for now
                if dep.scope.as_deref() == Some("test") {
                    continue;
                }
                let group_path = dep.group_id.replace('.', "/");
                let jar_file_name = format!("{}-{}.jar", dep.artifact_id, version);
                let candidate = m2_repo
                    .join(&group_path)
                    .join(&dep.artifact_id)
                    .join(version)
                    .join(&jar_file_name);

                if candidate.exists() {
                    info!("Resolved Maven dependency: {:?}", candidate);
                    jars.push(candidate);
                }
            }
        }

        jars
    }

    /// Resolve jars locally or download from Maven Central if missing
    pub async fn resolve_and_download_jars(&self) -> Vec<PathBuf> {
        let mut jars = self.resolve_local_jars();
        let home = match std::env::var("HOME") {
            Ok(h) => PathBuf::from(h),
            Err(_) => return jars,
        };
        let m2_repo = home.join(".m2").join("repository");

        for dep in &self.dependencies {
            if let Some(version) = &dep.version {
                if dep.scope.as_deref() == Some("test") {
                    continue;
                }
                let group_path = dep.group_id.replace('.', "/");
                let jar_file_name = format!("{}-{}.jar", dep.artifact_id, version);
                let target_path = m2_repo
                    .join(&group_path)
                    .join(&dep.artifact_id)
                    .join(version)
                    .join(&jar_file_name);

                if !target_path.exists() {
                    let remote_url = format!(
                        "https://repo1.maven.org/maven2/{}/{}/{}/{}",
                        group_path, dep.artifact_id, version, jar_file_name
                    );
                    info!("Missing dependency in ~/.m2, downloading from Maven Central: {}", remote_url);

                    // Use curl or system download tool if present
                    if let Some(parent) = target_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let res = tokio::process::Command::new("curl")
                        .arg("-sSL")
                        .arg("-o")
                        .arg(&target_path)
                        .arg(&remote_url)
                        .status()
                        .await;

                    if let Ok(st) = res
                        && st.success()
                        && target_path.exists()
                    {
                        info!("Successfully downloaded dependency: {:?}", target_path);
                        jars.push(target_path);
                    }
                }
            }
        }

        jars
    }

    /// Trigger background daemon compile via `./mvnw compile-sync` or `mvn compile`
    pub async fn execute_daemon_compile(&self) -> Result<(), String> {
        let parent = self.pom_path.parent().ok_or("No parent dir")?;
        let mvnw = parent.join("mvnw");
        let bin = if mvnw.exists() { mvnw.to_string_lossy().to_string() } else { "mvn".to_string() };
        info!("Triggering Maven background compile: {}", bin);
        let res = tokio::process::Command::new(bin)
            .current_dir(parent)
            .arg("compile")
            .arg("-q")
            .status()
            .await;
        if let Ok(st) = res && st.success() {
            return Ok(());
        }
        Err("Maven compile failed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_pom_xml() {
        let xml = r#"
            <project>
                <groupId>com.example</groupId>
                <artifactId>demo</artifactId>
                <version>1.0.0</version>
                <dependencies>
                    <dependency>
                        <groupId>org.slf4j</groupId>
                        <artifactId>slf4j-api</artifactId>
                        <version>2.0.7</version>
                    </dependency>
                </dependencies>
            </project>
        "#;
        let pom: PomProject = from_str(xml).expect("XML parse should succeed");
        assert_eq!(pom.artifact_id, Some("demo".to_string()));
        let deps = pom.dependencies.unwrap().dependency;
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].group_id, "org.slf4j");
        assert_eq!(deps[0].artifact_id, "slf4j-api");
        assert_eq!(deps[0].version, Some("2.0.7".to_string()));
    }
}
