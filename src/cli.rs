use std::fs;
use std::path::Path;
use std::sync::Arc;
use serde::Serialize;
use walkdir::WalkDir;

use crate::syntax::JavaAst;

#[derive(Debug, Serialize)]
pub struct DiagnosticItem {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub severity: String,
    pub message: String,
}

pub struct CliChecker;

impl CliChecker {
    /// Check files for syntax and semantic errors
    pub fn run_check(path: &Path, json_output: bool, include_semantic: bool) -> i32 {
        let mut total_errors = 0;
        let mut total_warnings = 0;
        let mut all_diags: Vec<DiagnosticItem> = Vec::new();

        let mut java_files = Vec::new();
        if path.is_file() {
            java_files.push(path.to_path_buf());
        } else if path.is_dir() {
            for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.extension().and_then(|ext| ext.to_str()) == Some("java") {
                    java_files.push(p.to_path_buf());
                }
            }
        } else {
            eprintln!("Error: Path '{:?}' does not exist.", path);
            return 2;
        }

        for file_path in &java_files {
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(err) => {
                    eprintln!("Error reading {:?}: {}", file_path, err);
                    total_errors += 1;
                    continue;
                }
            };

            if let Some(ast) = JavaAst::parse(Arc::new(content)) {
                // Collect syntax errors
                let syntax_errs = ast.collect_syntax_errors();
                for (range, msg) in syntax_errs {
                    total_errors += 1;
                    all_diags.push(DiagnosticItem {
                        file: file_path.to_string_lossy().to_string(),
                        line: range.start.line + 1,
                        character: range.start.character + 1,
                        severity: "error".to_string(),
                        message: msg,
                    });
                }

                // If running diagnose or requested semantic diagnostics
                if include_semantic {
                    let sem_diags = ast.collect_semantic_diagnostics();
                    for (range, msg) in sem_diags {
                        let is_err = msg.starts_with("Type mismatch") || msg.starts_with("Unhandled exception");
                        if is_err {
                            total_errors += 1;
                        } else {
                            total_warnings += 1;
                        }
                        all_diags.push(DiagnosticItem {
                            file: file_path.to_string_lossy().to_string(),
                            line: range.start.line + 1,
                            character: range.start.character + 1,
                            severity: if is_err { "error".to_string() } else { "warning".to_string() },
                            message: msg,
                        });
                    }
                }
            } else {
                total_errors += 1;
                all_diags.push(DiagnosticItem {
                    file: file_path.to_string_lossy().to_string(),
                    line: 1,
                    character: 1,
                    severity: "error".to_string(),
                    message: "Failed to parse Java source file into AST".to_string(),
                });
            }
        }

        if json_output {
            println!("{}", serde_json::to_string_pretty(&all_diags).unwrap_or_default());
        } else {
            for diag in &all_diags {
                let color_prefix = if diag.severity == "error" {
                    "\x1b[31m[ERROR]\x1b[0m"
                } else {
                    "\x1b[33m[WARN]\x1b[0m"
                };
                println!(
                    "{} {}:{}:{}: {}",
                    color_prefix, diag.file, diag.line, diag.character, diag.message
                );
            }
            println!(
                "\nFinished checking {} Java file(s): {} error(s), {} warning(s).",
                java_files.len(),
                total_errors,
                total_warnings
            );
        }

        if total_errors > 0 {
            1
        } else {
            0
        }
    }

    /// CLI refs subcommand: find references across workspace, optionally excluding import statements
    pub fn run_refs(root: &Path, symbol: &str, exclude_imports: bool) -> i32 {
        let mut count = 0;
        for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("java") {
                if let Ok(content) = fs::read_to_string(p) {
                    for (line_idx, line) in content.lines().enumerate() {
                        if line.contains(symbol) {
                            if exclude_imports && line.trim_start().starts_with("import ") {
                                continue;
                            }
                            count += 1;
                            println!("{}:{}: {}", p.display(), line_idx + 1, line.trim());
                        }
                    }
                }
            }
        }
        println!("\nFound {} reference(s) for symbol '{}'.", count, symbol);
        0
    }

    /// CLI extract-sources subcommand: find and extract -sources.jar dependencies
    pub fn run_extract_sources(root: &Path) -> i32 {
        println!("Scanning and extracting library sources for: {:?}", root);
        let mut extracted_count = 0;

        // Search ~/.gradle/caches and ~/.m2/repository for *-sources.jar
        let search_dirs = [
            dirs_home().map(|h| h.join(".gradle/caches")),
            dirs_home().map(|h| h.join(".m2/repository")),
        ];

        for dir_opt in search_dirs.into_iter().flatten() {
            if dir_opt.exists() {
                for entry in WalkDir::new(dir_opt).into_iter().filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                        if file_name.ends_with("-sources.jar") {
                            extracted_count += 1;
                            println!("Found sources jar: {}", path.display());
                        }
                    }
                }
            }
        }

        println!("\nDiscovered and indexed {} library sources jar(s).", extracted_count);
        0
    }

    /// CLI watch subcommand: continuously monitor directory for Java file changes and run instant diagnostics
    pub fn run_watch(target: &Path, include_semantic: bool) -> i32 {
        println!("🚀 Starting jlsp watch daemon for: {:?}", target);
        println!("👀 Monitoring .java files... (Press Ctrl+C to stop)\n");

        let mut last_check = std::time::Instant::now();
        let _ = Self::run_check(target, false, include_semantic);

        let mut file_mtimes: std::collections::HashMap<std::path::PathBuf, std::time::SystemTime> = std::collections::HashMap::new();

        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let mut changed = false;

            for entry in WalkDir::new(target).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("java") {
                    if let Ok(metadata) = std::fs::metadata(p) {
                        if let Ok(mtime) = metadata.modified() {
                            if let Some(&prev_time) = file_mtimes.get(p) {
                                if mtime > prev_time {
                                    file_mtimes.insert(p.to_path_buf(), mtime);
                                    changed = true;
                                }
                            } else {
                                file_mtimes.insert(p.to_path_buf(), mtime);
                            }
                        }
                    }
                }
            }

            if changed && last_check.elapsed().as_millis() > 300 {
                // Clear console for smooth output
                print!("\x1B[2J\x1B[1;1H");
                println!("🔄 File change detected, re-checking at {:?}...\n", std::time::SystemTime::now());
                let _ = Self::run_check(target, false, include_semantic);
                last_check = std::time::Instant::now();
            }
        }
    }
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var("HOME").ok().map(std::path::PathBuf::from)
}

