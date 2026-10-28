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
    pub dependency_management: Option<PomDependencyManagement>,
    pub dependencies: Option<PomDependencies>,
    pub modules: Option<PomModules>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PomDependencyManagement {
    #[serde(default)]
    pub dependencies: Option<PomDependencies>,
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
    pub properties: std::collections::HashMap<String, String>,
}

pub fn extract_pom_properties(content: &str) -> std::collections::HashMap<String, String> {
    let mut props = std::collections::HashMap::new();
    if let Some(start) = content.find("<properties>") {
        if let Some(end) = content[start..].find("</properties>") {
            let inner = &content[start + 12..start + end];
            for line in inner.lines() {
                let trimmed = line.trim();
                if let (Some(open_end), Some(close_start)) = (trimmed.find('>'), trimmed.rfind("</")) {
                    if open_end < close_start && trimmed.starts_with('<') {
                        let tag = &trimmed[1..open_end];
                        let val = &trimmed[open_end + 1..close_start];
                        if !tag.is_empty() && !tag.contains(' ') {
                            props.insert(tag.to_string(), val.to_string());
                        }
                    }
                }
            }
        }
    }
    props
}

fn substitute_property(val: &str, props: &std::collections::HashMap<String, String>) -> String {
    let mut result = val.to_string();
    for (k, v) in props {
        let pattern = format!("${{{}}}", k);
        if result.contains(&pattern) {
            result = result.replace(&pattern, v);
        }
    }
    result
}

impl MavenProject {
    pub fn parse<P: AsRef<Path>>(pom_path: P) -> Result<Self, String> {
        let content = fs::read_to_string(&pom_path)
            .map_err(|e| format!("Failed to read pom.xml: {e}"))?;
        
        let pom: PomProject = from_str(&content)
            .map_err(|e| format!("Failed to parse pom.xml: {e}"))?;

        let mut properties = extract_pom_properties(&content);
        if let Some(ver) = &pom.version {
            properties.insert("project.version".to_string(), ver.clone());
        }
        if let Some(gid) = &pom.group_id {
            properties.insert("project.groupId".to_string(), gid.clone());
        }

        // Check if parent pom.xml exists to inherit properties
        if let Some(parent_dir) = pom_path.as_ref().parent() {
            if let Some(grandparent) = parent_dir.parent() {
                let parent_pom = grandparent.join("pom.xml");
                if parent_pom.exists() && parent_pom != pom_path.as_ref() {
                    if let Ok(parent_content) = fs::read_to_string(&parent_pom) {
                        for (k, v) in extract_pom_properties(&parent_content) {
                            properties.entry(k).or_insert(v);
                        }
                    }
                }
            }
        }

        // Build BOM mapping from dependencyManagement
        let mut bom_map = std::collections::HashMap::new();
        if let Some(dm) = pom.dependency_management {
            if let Some(deps) = dm.dependencies {
                for d in deps.dependency {
                    if let Some(ver) = d.version {
                        let resolved_ver = substitute_property(&ver, &properties);
                        bom_map.insert((d.group_id, d.artifact_id), resolved_ver);
                    }
                }
            }
        }

        let mut dependencies = pom.dependencies.map(|d| d.dependency).unwrap_or_default();
        for dep in &mut dependencies {
            dep.group_id = substitute_property(&dep.group_id, &properties);
            dep.artifact_id = substitute_property(&dep.artifact_id, &properties);
            if dep.version.is_none() {
                if let Some(bom_ver) = bom_map.get(&(dep.group_id.clone(), dep.artifact_id.clone())) {
                    dep.version = Some(bom_ver.clone());
                }
            }
            if let Some(v) = &mut dep.version {
                *v = substitute_property(v, &properties);
            }
        }

        let modules = pom.modules.map(|m| m.module).unwrap_or_default();
        Ok(Self {
            pom_path: pom_path.as_ref().to_path_buf(),
            dependencies,
            modules,
            properties,
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

    /// Locate the local Maven repository path.
    ///
    /// Resolution order, mirroring Maven:
    /// 1. `<localRepository>` in the user-level `~/.m2/settings.xml`
    /// 2. `<localRepository>` in the global `$MAVEN_HOME/conf/settings.xml`
    ///    (or `$M2_HOME/conf/settings.xml`)
    /// 3. `~/.m2/repository` under the OS-level user home (e.g. macOS
    ///    `/Users/<name>`, Linux `/home/<name>`)
    ///
    /// Returns `None` when the OS user home directory cannot be determined. A
    /// relative fallback must never be used: it would resolve against the
    /// process working directory (typically the opened project root) and create
    /// a stray `.m2/repository` there.
    pub fn find_m2_repository() -> Option<PathBuf> {
        let home = std::env::home_dir()?;

        // 1. User-level settings: ~/.m2/settings.xml
        if let Ok(content) = fs::read_to_string(home.join(".m2").join("settings.xml")) {
            if let Some(p) = Self::parse_local_repository(&content, &home) {
                return Some(p);
            }
        }

        // 2. Global settings from the Maven installation directory.
        for var in ["MAVEN_HOME", "M2_HOME"] {
            if let Ok(dir) = std::env::var(var) {
                if dir.is_empty() {
                    continue;
                }
                let global = PathBuf::from(dir).join("conf").join("settings.xml");
                if let Ok(content) = fs::read_to_string(global) {
                    if let Some(p) = Self::parse_local_repository(&content, &home) {
                        return Some(p);
                    }
                }
            }
        }

        // 3. Default: <user home>/.m2/repository
        Some(home.join(".m2").join("repository"))
    }

    /// Extract `<localRepository>` from settings.xml content.
    ///
    /// XML comments are stripped first: the stock settings.xml ships a
    /// commented-out `<localRepository>` example that must not be treated as
    /// real configuration. `${user.home}` is expanded against `home`.
    fn parse_local_repository(content: &str, home: &std::path::Path) -> Option<PathBuf> {
        let mut cleaned = String::with_capacity(content.len());
        let mut rest = content;
        while let Some(start) = rest.find("<!--") {
            cleaned.push_str(&rest[..start]);
            match rest[start..].find("-->") {
                Some(end) => rest = &rest[start + end + 3..],
                None => return None,
            }
        }
        cleaned.push_str(rest);

        let start = cleaned.find("<localRepository>")?;
        let end = cleaned[start..].find("</localRepository>")?;
        let raw = cleaned[start + 17..start + end].trim();
        let expanded = raw.replace("${user.home}", &home.to_string_lossy().to_string());
        if expanded.is_empty() {
            return None;
        }
        Some(PathBuf::from(expanded))
    }

    /// Resolve scoped jars: (compile_and_runtime_jars, test_scoped_jars)
    pub fn resolve_scoped_jars(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let all_jars = self.resolve_local_jars();
        let mut compile_jars = Vec::new();
        let mut test_jars = Vec::new();

        for dep in &self.dependencies {
            let is_test = dep.scope.as_deref() == Some("test");
            let file_pattern = format!("{}-", dep.artifact_id);
            for jar in &all_jars {
                if let Some(file_name) = jar.file_name().and_then(|n| n.to_str()) {
                    if file_name.starts_with(&file_pattern) {
                        if is_test {
                            if !test_jars.contains(jar) {
                                test_jars.push(jar.clone());
                            }
                        } else {
                            if !compile_jars.contains(jar) {
                                compile_jars.push(jar.clone());
                            }
                        }
                    }
                }
            }
        }

        // Add any remaining jars to compile_jars
        for jar in all_jars {
            if !test_jars.contains(&jar) && !compile_jars.contains(&jar) {
                compile_jars.push(jar);
            }
        }

        (compile_jars, test_jars)
    }

    /// Locate jar files in local Maven repository (~/.m2/repository), including test scope
    pub fn resolve_local_jars(&self) -> Vec<PathBuf> {
        let mut jars = Vec::new();
        let m2_repo = match Self::find_m2_repository() {
            Some(p) => p,
            None => return jars,
        };
        if !m2_repo.exists() {
            return jars;
        }

        for dep in &self.dependencies {
            let group_path = dep.group_id.replace('.', std::path::MAIN_SEPARATOR_STR);
            if let Some(version) = &dep.version {
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
            } else {
                // If version is managed by parent POM or BOM, find latest version in ~/.m2
                let artifact_dir = m2_repo.join(&group_path).join(&dep.artifact_id);
                if artifact_dir.exists() {
                    if let Ok(entries) = std::fs::read_dir(&artifact_dir) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if p.is_dir() {
                                let ver = p.file_name().unwrap_or_default().to_string_lossy();
                                let jar = p.join(format!("{}-{}.jar", dep.artifact_id, ver));
                                if jar.exists() && !jars.contains(&jar) {
                                    info!("Resolved unversioned Maven dependency: {:?}", jar);
                                    jars.push(jar);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }

        // If Spring Boot starter is in dependencies, resolve core Spring transitive jars in ~/.m2
        let has_spring = self.dependencies.iter().any(|d| d.group_id.contains("springframework") || d.artifact_id.contains("spring-boot-starter"));
        if has_spring {
            let spring_repo = m2_repo.join("org").join("springframework");
            if spring_repo.exists() {
                for sub in &["spring-web", "spring-webmvc", "spring-context", "spring-beans", "spring-core"] {
                    let sub_dir = spring_repo.join(sub);
                    if sub_dir.exists() {
                        if let Ok(entries) = std::fs::read_dir(&sub_dir) {
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.is_dir() {
                                    let ver = p.file_name().unwrap_or_default().to_string_lossy();
                                    let jar = p.join(format!("{}-{}.jar", sub, ver));
                                    if jar.exists() && !jars.contains(&jar) {
                                        info!("Resolved transitive Spring dependency: {:?}", jar);
                                        jars.push(jar);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // If MyBatis or MyBatis-Plus is in dependencies, resolve mybatis core and mybatis-spring jars in ~/.m2
        let has_mybatis = self.dependencies.iter().any(|d| d.group_id.contains("mybatis") || d.artifact_id.contains("mybatis"));
        if has_mybatis {
            let mybatis_repo = m2_repo.join("org").join("mybatis");
            if mybatis_repo.exists() {
                for sub in &["mybatis", "mybatis-spring"] {
                    let sub_dir = mybatis_repo.join(sub);
                    if sub_dir.exists() {
                        if let Ok(entries) = std::fs::read_dir(&sub_dir) {
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.is_dir() {
                                    let ver = p.file_name().unwrap_or_default().to_string_lossy();
                                    let jar = p.join(format!("{}-{}.jar", sub, ver));
                                    if jar.exists() && !jars.contains(&jar) {
                                        info!("Resolved transitive MyBatis dependency: {:?}", jar);
                                        jars.push(jar);
                                        break;
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

    /// Resolve jars locally or download from Maven Central if missing (including test scope)
    pub async fn resolve_and_download_jars(&self) -> Vec<PathBuf> {
        let mut jars = self.resolve_local_jars();
        // Without a determinable local repository, downloading is skipped entirely
        // (it would otherwise write into a stray relative path).
        let m2_repo = match Self::find_m2_repository() {
            Some(p) => p,
            None => return jars,
        };

        for dep in &self.dependencies {
            if let Some(version) = &dep.version {
                // Filesystem path uses the OS separator; the download URL keeps `/`.
                let group_path = dep.group_id.replace('.', std::path::MAIN_SEPARATOR_STR);
                let jar_file_name = format!("{}-{}.jar", dep.artifact_id, version);
                let target_path = m2_repo
                    .join(&group_path)
                    .join(&dep.artifact_id)
                    .join(version)
                    .join(&jar_file_name);

                if !target_path.exists() {
                    let remote_url = format!(
                        "https://repo1.maven.org/maven2/{}/{}/{}/{}",
                        dep.group_id.replace('.', "/"), dep.artifact_id, version, jar_file_name
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
    fn test_parse_local_repository_custom_path() {
        let home = PathBuf::from("/home/tester");
        let xml = r#"
            <settings>
                <localRepository>/data/maven-repo</localRepository>
            </settings>
        "#;
        assert_eq!(
            MavenProject::parse_local_repository(xml, &home),
            Some(PathBuf::from("/data/maven-repo"))
        );
    }

    #[test]
    fn test_parse_local_repository_ignores_commented_example() {
        let home = PathBuf::from("/home/tester");
        // Stock settings.xml ships the tag inside a comment; it must be ignored.
        let xml = r#"
            <settings>
                <!--
                <localRepository>/path/to/local/repo</localRepository>
                -->
            </settings>
        "#;
        assert_eq!(MavenProject::parse_local_repository(xml, &home), None);
    }

    #[test]
    fn test_parse_local_repository_expands_user_home() {
        let home = PathBuf::from("/home/tester");
        let xml = r#"<settings><localRepository>${user.home}/custom-repo</localRepository></settings>"#;
        assert_eq!(
            MavenProject::parse_local_repository(xml, &home),
            Some(PathBuf::from("/home/tester/custom-repo"))
        );
    }

    #[test]
    fn test_parse_local_repository_absent() {
        let home = PathBuf::from("/home/tester");
        assert_eq!(MavenProject::parse_local_repository("<settings/>", &home), None);
    }

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

    #[test]
    fn test_maven_properties_substitution_and_bom() {
        let temp_dir = std::env::temp_dir().join(format!("jlsp_maven_test_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let pom_file = temp_dir.join("pom.xml");
        let xml = r#"
            <project>
                <groupId>com.example</groupId>
                <artifactId>app</artifactId>
                <version>2.0.0</version>
                <properties>
                    <spring.version>6.1.2</spring.version>
                    <junit.version>5.10.1</junit.version>
                </properties>
                <dependencyManagement>
                    <dependencies>
                        <dependency>
                            <groupId>org.springframework</groupId>
                            <artifactId>spring-web</artifactId>
                            <version>${spring.version}</version>
                        </dependency>
                    </dependencies>
                </dependencyManagement>
                <dependencies>
                    <dependency>
                        <groupId>org.springframework</groupId>
                        <artifactId>spring-web</artifactId>
                    </dependency>
                    <dependency>
                        <groupId>org.junit.jupiter</groupId>
                        <artifactId>junit-jupiter</artifactId>
                        <version>${junit.version}</version>
                        <scope>test</scope>
                    </dependency>
                </dependencies>
            </project>
        "#;
        std::fs::write(&pom_file, xml).unwrap();
        let proj = MavenProject::parse(&pom_file).expect("Parse should succeed");

        assert_eq!(proj.dependencies.len(), 2);
        // Spring web should get resolved version 6.1.2 from dependencyManagement + property
        let spring_dep = proj.dependencies.iter().find(|d| d.artifact_id == "spring-web").unwrap();
        assert_eq!(spring_dep.version, Some("6.1.2".to_string()));

        // JUnit should get substituted version 5.10.1 and retain test scope
        let junit_dep = proj.dependencies.iter().find(|d| d.artifact_id == "junit-jupiter").unwrap();
        assert_eq!(junit_dep.version, Some("5.10.1".to_string()));
        assert_eq!(junit_dep.scope.as_deref(), Some("test"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
