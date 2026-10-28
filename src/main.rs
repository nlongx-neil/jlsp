use std::env;
use std::path::{Path, PathBuf};
use tower_lsp::{LspService, Server};
use tracing_subscriber::EnvFilter;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub mod cli;
pub mod classpath;
pub mod indexer;
pub mod mcp;
pub mod model;
pub mod project;
pub mod semantic;
pub mod server;
pub mod syntax;

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    // Check if running as a CLI subcommand or MCP server
    if args.len() > 1 {
        match args[1].as_str() {
            "check" => {
                let target = args.get(2).map(|s| s.as_str()).unwrap_or(".");
                let json_mode = args.iter().any(|a| a == "--json");
                let exit_code = cli::CliChecker::run_check(Path::new(target), json_mode, false);
                std::process::exit(exit_code);
            }
            "diagnose" => {
                let target = args.get(2).map(|s| s.as_str()).unwrap_or(".");
                let json_mode = args.iter().any(|a| a == "--json");
                let exit_code = cli::CliChecker::run_check(Path::new(target), json_mode, true);
                std::process::exit(exit_code);
            }
            "mcp" => {
                let root = args.get(2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
                let mcp = mcp::McpServer::new(root);
                mcp.run_stdio().await;
                return;
            }
            "refs" => {
                let symbol = args.get(2).map(|s| s.as_str()).unwrap_or("");
                let exclude_imports = args.iter().any(|a| a == "--exclude-imports");
                let exit_code = cli::CliChecker::run_refs(Path::new("."), symbol, exclude_imports);
                std::process::exit(exit_code);
            }
            "extract-sources" => {
                let exit_code = cli::CliChecker::run_extract_sources(Path::new("."));
                std::process::exit(exit_code);
            }
            "watch" => {
                let target = args.get(2).map(|s| s.as_str()).unwrap_or(".");
                let include_semantic = args.iter().any(|a| a == "--diagnose" || a == "-d");
                let exit_code = cli::CliChecker::run_watch(Path::new(target), include_semantic);
                std::process::exit(exit_code);
            }
            "--version" | "-v" => {
                println!("jlsp {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            _ => {}
        }
    }

    // Default mode: LSP server over stdio
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(server::JlspServer::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn print_help() {
    println!(
        r#"jlsp - Ultra-Fast, Zero-JVM Java Language Server in Pure Rust

USAGE:
    jlsp [SUBCOMMAND | OPTIONS]

SUBCOMMANDS:
    check <path> [--json]       Quickly check Java files or directories for syntax errors
    diagnose <path> [--json]    Run in-depth semantic diagnostics (type mismatches, unhandled throws, unused vars)
    watch <path> [--diagnose]   Continuously watch files and run instant re-checking on changes
    refs <symbol> [--exclude-imports] Find usages of a symbol across workspace files
    extract-sources             Scan and index dependency sources jars from Gradle/Maven cache
    mcp [workspace_root]        Start Model Context Protocol (MCP) server for AI coding agents
    (no subcommand)             Run standard Language Server Protocol (LSP) over stdio

OPTIONS:
    -h, --help                  Print this help message
    -v, --version               Print version information
"#
    );
}
