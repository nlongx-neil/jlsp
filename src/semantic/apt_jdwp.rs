use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use tracing::info;

use crate::model::{ClassInfo, ClassKind, FieldInfo, MethodInfo, TypeSignature};

/// Pure-Rust lightweight JDWP TCP connector
pub struct JdwpClient {
    pub host: String,
    pub port: u16,
}

impl JdwpClient {
    pub fn new(host: String, port: u16) -> Self {
        Self { host, port }
    }

    /// Establish handshake with target JVM JDWP port
    pub async fn handshake(&self) -> Result<tokio::net::TcpStream, String> {
        let addr = format!("{}:{}", self.host, self.port);
        info!("Connecting to JVM JDWP at: {}", addr);
        let mut stream = tokio::net::TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("JDWP connect failed: {e}"))?;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let handshake_bytes = b"JDWP-Handshake";
        stream
            .write_all(handshake_bytes)
            .await
            .map_err(|e| format!("Handshake write error: {e}"))?;

        let mut reply = [0u8; 14];
        stream
            .read_exact(&mut reply)
            .await
            .map_err(|e| format!("Handshake read error: {e}"))?;

        if &reply == handshake_bytes {
            info!("JDWP handshake successful with JVM at {}", addr);
            Ok(stream)
        } else {
            Err("Invalid JDWP handshake response from JVM".into())
        }
    }
}

/// Discovered Annotation Processor information from JAR files or SPI metadata
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessorMetadata {
    pub class_name: String,
    pub jar_path: PathBuf,
}

/// Discovered and generated APT source code file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSourceFile {
    pub fqn: String,
    pub path: PathBuf,
    pub content: String,
}

/// Pure-Rust Annotation Processing Tool (APT) Engine & SPI Discovery
pub struct AnnotationProcessorRunner;

impl AnnotationProcessorRunner {
    /// Discover annotation processors from jar files via SPI:
    /// `META-INF/services/javax.annotation.processing.Processor`
    pub fn discover_processors<P: AsRef<Path>>(jars: &[P]) -> Vec<ProcessorMetadata> {
        let mut processors = Vec::new();

        for jar in jars {
            let jar_path = jar.as_ref();
            if !jar_path.exists() {
                continue;
            }

            if let Ok(file) = fs::File::open(jar_path) {
                if let Ok(mut archive) = zip::ZipArchive::new(file) {
                    if let Ok(mut svc_file) = archive.by_name("META-INF/services/javax.annotation.processing.Processor") {
                        let mut content = String::new();
                        if svc_file.read_to_string(&mut content).is_ok() {
                            for line in content.lines() {
                                let trimmed = line.trim();
                                if !trimmed.is_empty() && !trimmed.starts_with('#') {
                                    processors.push(ProcessorMetadata {
                                        class_name: trimmed.to_string(),
                                        jar_path: jar_path.to_path_buf(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        processors
    }

    /// Discover standard generated source directories in workspace
    /// (e.g. `target/generated-sources/annotations`, `build/generated/sources/annotationProcessor/java/main`)
    pub fn discover_generated_source_dirs<P: AsRef<Path>>(root: P) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let root = root.as_ref();

        let candidates = [
            // Maven generated sources
            root.join("target").join("generated-sources").join("annotations"),
            root.join("target").join("generated-sources").join("apt"),
            // Gradle generated sources
            root.join("build").join("generated").join("sources").join("annotationProcessor").join("java").join("main"),
            root.join("build").join("generated").join("sources").join("annotationProcessor").join("java").join("test"),
            // IDE generated sources (.apt_generated, etc.)
            root.join(".apt_generated"),
        ];

        for c in candidates {
            if c.exists() && c.is_dir() {
                dirs.push(c);
            }
        }

        // Check submodules in multi-module projects
        if let Ok(entries) = fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let sub_maven = path.join("target").join("generated-sources").join("annotations");
                    if sub_maven.exists() && sub_maven.is_dir() {
                        dirs.push(sub_maven);
                    }
                    let sub_gradle = path.join("build").join("generated").join("sources").join("annotationProcessor").join("java").join("main");
                    if sub_gradle.exists() && sub_gradle.is_dir() {
                        dirs.push(sub_gradle);
                    }
                }
            }
        }

        dirs
    }

    /// Scan disk for pre-generated APT source files in known generated directories
    pub fn scan_generated_sources<P: AsRef<Path>>(root: P) -> Vec<GeneratedSourceFile> {
        let mut files = Vec::new();
        let gen_dirs = Self::discover_generated_source_dirs(root);

        for dir in gen_dirs {
            for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("java") {
                    if let Ok(content) = fs::read_to_string(p) {
                        // Extract package and class name from content
                        let pkg = content.lines().find(|l| l.trim().starts_with("package "))
                            .map(|l| l.trim().trim_start_matches("package ").trim_end_matches(';').trim().to_string())
                            .unwrap_or_default();
                        let simple_name = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
                        let fqn = if pkg.is_empty() {
                            simple_name.clone()
                        } else {
                            format!("{}.{}", pkg, simple_name)
                        };

                        files.push(GeneratedSourceFile {
                            fqn,
                            path: p.to_path_buf(),
                            content,
                        });
                    }
                }
            }
        }

        files
    }

    /// Trigger build tool daemon compile to generate APT classes, then load them
    pub async fn trigger_and_load_apt<P: AsRef<Path>>(root: P) -> Vec<GeneratedSourceFile> {
        let root = root.as_ref();
        
        // 1. Try Maven background compile
        let pom = root.join("pom.xml");
        if pom.exists() {
            if let Ok(project) = crate::project::MavenProject::parse(&pom) {
                let _ = project.execute_daemon_compile().await;
            }
        }

        // 2. Try Gradle background classes compile
        let gradle = root.join("build.gradle");
        let gradle_kts = root.join("build.gradle.kts");
        if gradle.exists() {
            if let Ok(project) = crate::project::GradleProject::parse(&gradle) {
                let _ = project.execute_daemon_compile().await;
            }
        } else if gradle_kts.exists() {
            if let Ok(project) = crate::project::GradleProject::parse(&gradle_kts) {
                let _ = project.execute_daemon_compile().await;
            }
        }

        // 3. Scan generated sources
        Self::scan_generated_sources(root)
    }

    /// Comprehensive pure-Rust synthetic AST processor for popular annotation processors:
    /// MapStruct, JPA Metamodel, AutoValue, Dagger/Hilt, Micronaut/Spring
    pub fn process_apt_annotations(cls: &mut ClassInfo, source_text: &str) -> Vec<ClassInfo> {
        let mut generated_classes = Vec::new();

        // 1. MapStruct: `@Mapper` on interface/abstract class generates `XxxImpl`
        if source_text.contains("@Mapper") || source_text.contains("@org.mapstruct.Mapper") {
            let impl_name = format!("{}Impl", cls.simple_name);
            let impl_fqn = if cls.package_name.is_empty() {
                impl_name.clone()
            } else {
                format!("{}.{}", cls.package_name, impl_name)
            };

            let impl_cls = ClassInfo {
                fqn: impl_fqn,
                simple_name: impl_name,
                package_name: cls.package_name.clone(),
                kind: ClassKind::Class,
                super_class: if cls.kind == ClassKind::Class { Some(cls.fqn.clone()) } else { None },
                interfaces: vec![cls.fqn.clone()],
                methods: cls.methods.clone(),
                fields: vec![],
                source_path: cls.source_path.clone(),
                is_public: true,
            };

            // MapStruct singleton accessor: `Mappers.getMapper(Xxx.class)` or `INSTANCE`
            if !cls.fields.iter().any(|f| f.name == "INSTANCE") {
                cls.fields.push(FieldInfo {
                    name: "INSTANCE".to_string(),
                    field_type: TypeSignature::Class(cls.fqn.clone()),
                    is_static: true,
                    is_public: true,
                    is_final: true,
                });
            }

            generated_classes.push(impl_cls);
        }

        // 2. JPA Static Metamodel: `@Entity` on class generates `Xxx_`
        if source_text.contains("@Entity") || source_text.contains("@jakarta.persistence.Entity") || source_text.contains("@javax.persistence.Entity") {
            let meta_name = format!("{}_", cls.simple_name);
            let meta_fqn = if cls.package_name.is_empty() {
                meta_name.clone()
            } else {
                format!("{}.{}", cls.package_name, meta_name)
            };

            let mut meta_fields = Vec::new();
            for f in &cls.fields {
                let singular_type = format!(
                    "jakarta.persistence.metamodel.SingularAttribute<{}, {}>",
                    cls.simple_name,
                    f.field_type.simple_name()
                );
                meta_fields.push(FieldInfo {
                    name: f.name.clone(),
                    field_type: TypeSignature::Class(singular_type),
                    is_static: true,
                    is_public: true,
                    is_final: true,
                });
            }

            let meta_cls = ClassInfo {
                fqn: meta_fqn,
                simple_name: meta_name,
                package_name: cls.package_name.clone(),
                kind: ClassKind::Class,
                super_class: None,
                interfaces: vec![],
                methods: vec![],
                fields: meta_fields,
                source_path: cls.source_path.clone(),
                is_public: true,
            };

            generated_classes.push(meta_cls);
        }

        // 3. AutoValue: `@AutoValue` generates `AutoValue_Xxx` with constructor and getters
        if source_text.contains("@AutoValue") {
            let auto_name = format!("AutoValue_{}", cls.simple_name);
            let auto_fqn = if cls.package_name.is_empty() {
                auto_name.clone()
            } else {
                format!("{}.{}", cls.package_name, auto_name)
            };

            let auto_cls = ClassInfo {
                fqn: auto_fqn,
                simple_name: auto_name,
                package_name: cls.package_name.clone(),
                kind: ClassKind::Class,
                super_class: Some(cls.fqn.clone()),
                interfaces: vec![],
                methods: cls.methods.clone(),
                fields: cls.fields.clone(),
                source_path: cls.source_path.clone(),
                is_public: false,
            };

            generated_classes.push(auto_cls);
        }

        // 4. Dagger 2 / Hilt: `@Component` generates `DaggerXxx`
        if source_text.contains("@Component") && (cls.kind == ClassKind::Interface || cls.kind == ClassKind::Class) {
            let dagger_name = format!("Dagger{}", cls.simple_name);
            let dagger_fqn = if cls.package_name.is_empty() {
                dagger_name.clone()
            } else {
                format!("{}.{}", cls.package_name, dagger_name)
            };

            let dagger_cls = ClassInfo {
                fqn: dagger_fqn.clone(),
                simple_name: dagger_name,
                package_name: cls.package_name.clone(),
                kind: ClassKind::Class,
                super_class: None,
                interfaces: vec![cls.fqn.clone()],
                methods: vec![
                    MethodInfo {
                        name: "create".to_string(),
                        return_type: TypeSignature::Class(cls.fqn.clone()),
                        parameters: vec![],
                        is_static: true,
                        is_public: true,
                        is_constructor: false,
                        javadoc: Some("Generated by Dagger component processor".into()),
                        body: None,
                    },
                    MethodInfo {
                        name: "builder".to_string(),
                        return_type: TypeSignature::Class(format!("{}.Builder", dagger_fqn)),
                        parameters: vec![],
                        is_static: true,
                        is_public: true,
                        is_constructor: false,
                        javadoc: Some("Generated by Dagger component builder".into()),
                        body: None,
                    },
                ],
                fields: vec![],
                source_path: cls.source_path.clone(),
                is_public: true,
            };

            generated_classes.push(dagger_cls);
        }

        generated_classes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mapstruct_apt_generation() {
        let mut mapper_cls = ClassInfo {
            fqn: "com.demo.UserMapper".into(),
            simple_name: "UserMapper".into(),
            package_name: "com.demo".into(),
            kind: ClassKind::Interface,
            super_class: None,
            interfaces: vec![],
            methods: vec![
                MethodInfo {
                    name: "toDto".into(),
                    return_type: TypeSignature::Class("UserDto".into()),
                    parameters: vec![("user".into(), TypeSignature::Class("User".into()))],
                    is_static: false,
                    is_public: true,
                    is_constructor: false,
                    javadoc: None,
                    body: None,
                }
            ],
            fields: vec![],
            source_path: None,
            is_public: true,
        };

        let src = "@Mapper\npublic interface UserMapper { UserDto toDto(User user); }";
        let generated = AnnotationProcessorRunner::process_apt_annotations(&mut mapper_cls, src);

        assert_eq!(generated.len(), 1);
        let impl_cls = &generated[0];
        assert_eq!(impl_cls.simple_name, "UserMapperImpl");
        assert_eq!(impl_cls.fqn, "com.demo.UserMapperImpl");
        assert_eq!(impl_cls.interfaces, vec!["com.demo.UserMapper"]);
        assert!(impl_cls.methods.iter().any(|m| m.name == "toDto"));
        assert!(mapper_cls.fields.iter().any(|f| f.name == "INSTANCE"));
    }

    #[test]
    fn test_jpa_metamodel_apt_generation() {
        let mut entity_cls = ClassInfo {
            fqn: "com.demo.User".into(),
            simple_name: "User".into(),
            package_name: "com.demo".into(),
            kind: ClassKind::Class,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![
                FieldInfo {
                    name: "id".into(),
                    field_type: TypeSignature::Long,
                    is_static: false,
                    is_public: false,
                    is_final: false,
                },
                FieldInfo {
                    name: "username".into(),
                    field_type: TypeSignature::Class("String".into()),
                    is_static: false,
                    is_public: false,
                    is_final: false,
                },
            ],
            source_path: None,
            is_public: true,
        };

        let src = "@Entity\npublic class User { private Long id; private String username; }";
        let generated = AnnotationProcessorRunner::process_apt_annotations(&mut entity_cls, src);

        assert_eq!(generated.len(), 1);
        let meta_cls = &generated[0];
        assert_eq!(meta_cls.simple_name, "User_");
        assert_eq!(meta_cls.fqn, "com.demo.User_");
        assert!(meta_cls.fields.iter().any(|f| f.name == "id" && f.is_static));
        assert!(meta_cls.fields.iter().any(|f| f.name == "username" && f.is_static));
    }

    #[test]
    fn test_dagger_component_apt_generation() {
        let mut comp_cls = ClassInfo {
            fqn: "com.demo.AppComponent".into(),
            simple_name: "AppComponent".into(),
            package_name: "com.demo".into(),
            kind: ClassKind::Interface,
            super_class: None,
            interfaces: vec![],
            methods: vec![],
            fields: vec![],
            source_path: None,
            is_public: true,
        };

        let src = "@Component\npublic interface AppComponent {}";
        let generated = AnnotationProcessorRunner::process_apt_annotations(&mut comp_cls, src);

        assert_eq!(generated.len(), 1);
        let dagger_cls = &generated[0];
        assert_eq!(dagger_cls.simple_name, "DaggerAppComponent");
        assert!(dagger_cls.methods.iter().any(|m| m.name == "create" && m.is_static));
        assert!(dagger_cls.methods.iter().any(|m| m.name == "builder" && m.is_static));
    }
}
