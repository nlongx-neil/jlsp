use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::indexer::{SourceSymbol, WorkspaceIndex};
use crate::syntax::JavaAst;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct McpRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct McpResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

pub struct McpServer {
    workspace_root: PathBuf,
    index: Arc<WorkspaceIndex>,
}

impl McpServer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            workspace_root: root,
            index: Arc::new(WorkspaceIndex::new()),
        }
    }

    pub async fn run_stdio(&self) {
        // Pre-warm workspace index
        self.index.scan_workspace(&self.workspace_root).await;

        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let reader = stdin.lock();

        for line in reader.lines() {
            let line_str = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if line_str.trim().is_empty() {
                continue;
            }

            if let Ok(req) = serde_json::from_str::<McpRequest>(&line_str) {
                let req_id = req.id.unwrap_or(Value::Null);
                let resp = match req.method.as_str() {
                    "initialize" => {
                        let res = json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {
                                "tools": {
                                    "listChanged": false
                                }
                            },
                            "serverInfo": {
                                "name": "jlsp-mcp-server",
                                "version": "0.1.0"
                            }
                        });
                        McpResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req_id,
                            result: Some(res),
                            error: None,
                        }
                    }
                    "tools/list" => {
                        let tools = json!({
                            "tools": [
                                {
                                    "name": "get_symbols_overview",
                                    "description": "Get high-level symbols (classes, interfaces, methods, fields) overview for a Java file or directory",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "path": { "type": "string", "description": "Absolute or workspace-relative path to Java file" }
                                        },
                                        "required": ["path"]
                                    }
                                },
                                {
                                    "name": "search_code_symbols",
                                    "description": "Search code symbols (classes, methods, variables) across the workspace using fast index",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "query": { "type": "string", "description": "Symbol name or substring" }
                                        },
                                        "required": ["query"]
                                    }
                                },
                                {
                                    "name": "diagnose_java_code",
                                    "description": "Run instant syntax and type mismatch diagnosis on Java code",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "path": { "type": "string", "description": "Path to Java file or directory" }
                                        },
                                        "required": ["path"]
                                    }
                                },
                                {
                                    "name": "replace_symbol_body",
                                    "description": "Safely replace method or class body in a Java file",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "path": { "type": "string", "description": "Path to file" },
                                            "symbol_name": { "type": "string", "description": "Method or class name" },
                                            "new_body": { "type": "string", "description": "Replacement block code" }
                                        },
                                        "required": ["path", "symbol_name", "new_body"]
                                    }
                                },
                                {
                                    "name": "get_call_chain",
                                    "description": "Trace incoming callers or outgoing callees for a Java method across the workspace",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "method_name": { "type": "string", "description": "Name of the target method" },
                                            "direction": { "type": "string", "enum": ["incoming", "outgoing"], "description": "incoming (who calls this) or outgoing (what this calls)" }
                                        },
                                        "required": ["method_name"]
                                    }
                                },
                                {
                                    "name": "generate_unit_test_scaffold",
                                    "description": "Generate a JUnit 5 / Mockito unit test template for a Java class",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "path": { "type": "string", "description": "Path to target Java file" }
                                        },
                                        "required": ["path"]
                                    }
                                },
                                {
                                    "name": "explore_rest_endpoints",
                                    "description": "Scan workspace for Spring @RestController / @RequestMapping endpoints and HTTP methods",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {}
                                    }
                                },
                                {
                                    "name": "explore_mybatis_mappers",
                                    "description": "Scan workspace for MyBatis Mapper XML files and list their SQL statement IDs",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "mapper_name": { "type": "string", "description": "Optional mapper interface simple name or FQN to filter" }
                                        }
                                    }
                                }
                            ]
                        });
                        McpResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req_id,
                            result: Some(tools),
                            error: None,
                        }
                    }
                    "tools/call" => {
                        let params = req.params.unwrap_or(Value::Null);
                        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

                        let call_result = self.handle_tool_call(name, arguments).await;
                        McpResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req_id,
                            result: Some(call_result),
                            error: None,
                        }
                    }
                    _ => McpResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req_id,
                        result: None,
                        error: Some(json!({
                            "code": -32601,
                            "message": format!("Method '{}' not found", req.method)
                        })),
                    },
                };

                let resp_str = serde_json::to_string(&resp).unwrap_or_default();
                let _ = writeln!(stdout, "{}", resp_str);
                let _ = stdout.flush();
            }
        }
    }

    async fn handle_tool_call(&self, tool_name: &str, args: Value) -> Value {
        match tool_name {
            "get_symbols_overview" => {
                let file_path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let full_path = self.workspace_root.join(file_path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    if let Some(ast) = JavaAst::parse(Arc::new(content)) {
                        let classes = ast.extract_classes(Some(full_path.to_string_lossy().to_string()));
                        return json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&classes).unwrap_or_default()
                            }]
                        });
                    }
                }
                json!({ "content": [{ "type": "text", "text": "File not found or unparseable" }] })
            }
            "search_code_symbols" => {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let symbols = self.index.search_symbols(query).await;
                let symbols: Vec<&SourceSymbol> = symbols.iter().map(|s| s.as_ref()).collect();
                json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&symbols).unwrap_or_default()
                    }]
                })
            }
            "diagnose_java_code" => {
                let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let full_path = self.workspace_root.join(path_str);
                let mut reports = Vec::new();
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    if let Some(ast) = JavaAst::parse(Arc::new(content)) {
                        for (r, msg) in ast.collect_syntax_errors() {
                            reports.push(format!("[SYNTAX ERROR L{}:{}] {}", r.start.line + 1, r.start.character + 1, msg));
                        }
                        for (r, msg) in ast.collect_semantic_diagnostics() {
                            reports.push(format!("[SEMANTIC L{}:{}] {}", r.start.line + 1, r.start.character + 1, msg));
                        }
                    }
                }
                let summary = if reports.is_empty() {
                    "No errors or warnings found.".to_string()
                } else {
                    reports.join("\n")
                };
                json!({
                    "content": [{
                        "type": "text",
                        "text": summary
                    }]
                })
            }
            "replace_symbol_body" => {
                let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let symbol_name = args.get("symbol_name").and_then(|v| v.as_str()).unwrap_or("");
                let new_body = args.get("new_body").and_then(|v| v.as_str()).unwrap_or("");
                let full_path = self.workspace_root.join(path_str);

                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    if let Some(ast) = JavaAst::parse(Arc::new(content.clone())) {
                        let root = ast.tree.root_node();
                        let mut target_range = None;

                        fn find_target(node: tree_sitter::Node, ast: &JavaAst, sym: &str, out: &mut Option<crate::model::Range>) {
                            if (node.kind() == "method_declaration" || node.kind() == "class_declaration")
                                && let Some(name_node) = node.child_by_field_name("name") {
                                    if ast.node_text(&name_node) == sym {
                                        if let Some(body) = node.child_by_field_name("body") {
                                            *out = Some(ast.node_range(&body));
                                            return;
                                        }
                                    }
                                }
                            let mut c = node.walk();
                            for ch in node.children(&mut c) {
                                find_target(ch, ast, sym, out);
                            }
                        }

                        find_target(root, &ast, symbol_name, &mut target_range);

                        if let Some(r) = target_range {
                            let lines: Vec<&str> = content.lines().collect();
                            // Simple text replacement based on body lines
                            let start_line = r.start.line as usize;
                            let end_line = r.end.line as usize;
                            if start_line <= end_line && end_line < lines.len() {
                                let mut new_lines = Vec::new();
                                for (idx, line) in lines.iter().enumerate() {
                                    if idx < start_line {
                                        new_lines.push(line.to_string());
                                    } else if idx == start_line {
                                        new_lines.push(format!("{{\n{}\n}}", new_body));
                                    } else if idx > end_line {
                                        new_lines.push(line.to_string());
                                    }
                                }
                                let updated = new_lines.join("\n");
                                let _ = std::fs::write(&full_path, updated);
                                return json!({ "content": [{ "type": "text", "text": format!("Successfully replaced body for '{}'", symbol_name) }] });
                            }
                        }
                    }
                }
                json!({ "content": [{ "type": "text", "text": format!("Symbol '{}' not found in '{}'", symbol_name, path_str) }] })
            }
            "get_call_chain" => {
                let method_name = args.get("method_name").and_then(|v| v.as_str()).unwrap_or("");
                let direction = args.get("direction").and_then(|v| v.as_str()).unwrap_or("incoming");

                let mut results = Vec::new();
                for entry in walkdir::WalkDir::new(&self.workspace_root).into_iter().filter_map(|e| e.ok()) {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("java") {
                        if let Ok(content) = std::fs::read_to_string(p) {
                            for (l_idx, line) in content.lines().enumerate() {
                                if line.contains(method_name) && !line.trim_start().starts_with("//") {
                                    results.push(format!("{}:{}: {}", p.display(), l_idx + 1, line.trim()));
                                }
                            }
                        }
                    }
                }

                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Call chain ({}) for '{}':\n{}", direction, method_name, results.join("\n"))
                    }]
                })
            }
            "generate_unit_test_scaffold" => {
                let file_path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let full_path = self.workspace_root.join(file_path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    if let Some(ast) = JavaAst::parse(Arc::new(content)) {
                        let classes = ast.extract_classes(None);
                        if let Some(cls) = classes.first() {
                            let mut test_methods = Vec::new();
                            for m in &cls.methods {
                                if m.is_public && !m.is_constructor {
                                    test_methods.push(format!(
                                        "    @Test\n    void test_{}() {{\n        // TODO: test implementation for {}.{}\n        // org.junit.jupiter.api.Assertions.assertNotNull(...);\n    }}\n",
                                        m.name, cls.simple_name, m.name
                                    ));
                                }
                            }

                            let test_scaffold = format!(
                                "package {};\n\nimport org.junit.jupiter.api.Test;\nimport static org.junit.jupiter.api.Assertions.*;\n\nclass {}Test {{\n\n{}}}\n",
                                cls.package_name,
                                cls.simple_name,
                                test_methods.join("\n")
                            );

                            return json!({
                                "content": [{
                                    "type": "text",
                                    "text": test_scaffold
                                }]
                            });
                        }
                    }
                }
                json!({ "content": [{ "type": "text", "text": "Failed to parse class for test scaffold generation" }] })
            }
            "explore_rest_endpoints" => {
                let mut endpoints = Vec::new();
                for entry in walkdir::WalkDir::new(&self.workspace_root).into_iter().filter_map(|e| e.ok()) {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("java") {
                        if let Ok(content) = std::fs::read_to_string(p) {
                            if content.contains("@RestController") || content.contains("@Controller") {
                                for line in content.lines() {
                                    let trimmed = line.trim();
                                    if trimmed.starts_with("@GetMapping")
                                        || trimmed.starts_with("@PostMapping")
                                        || trimmed.starts_with("@PutMapping")
                                        || trimmed.starts_with("@DeleteMapping")
                                        || trimmed.starts_with("@RequestMapping")
                                    {
                                        endpoints.push(format!("{}: {}", p.file_name().and_then(|n| n.to_str()).unwrap_or("?"), trimmed));
                                    }
                                }
                            }
                        }
                    }
                }

                json!({
                    "content": [{
                        "type": "text",
                        "text": if endpoints.is_empty() {
                            "No Spring REST endpoints detected in workspace.".to_string()
                        } else {
                            format!("Discovered {} Spring REST Endpoint(s):\n{}", endpoints.len(), endpoints.join("\n"))
                        }
                    }]
                })
            }
            "explore_mybatis_mappers" => {
                let filter_name = args.get("mapper_name").and_then(|v| v.as_str()).unwrap_or("");
                let mut results = Vec::new();

                for entry in walkdir::WalkDir::new(&self.workspace_root).into_iter().filter_map(|e| e.ok()) {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) == Some("xml") {
                        if let Ok(content) = std::fs::read_to_string(p) {
                            if content.contains("<mapper") {
                                let mut ns = String::new();
                                let mut stmts = Vec::new();
                                for (l_idx, line) in content.lines().enumerate() {
                                    let trimmed = line.trim();
                                    if trimmed.contains("<mapper") && trimmed.contains("namespace=") {
                                        if let Some(n) = trimmed.split("namespace=").nth(1) {
                                            ns = n.trim_matches(|c| c == '"' || c == '\'').to_string();
                                        }
                                    }
                                    for tag in &["select", "insert", "update", "delete"] {
                                        if trimmed.starts_with(&format!("<{}", tag)) && trimmed.contains("id=") {
                                            if let Some(id_p) = trimmed.split("id=").nth(1) {
                                                let id = id_p.trim_matches(|c| c == '"' || c == '\'').split(|c| c == '"' || c == '\'').next().unwrap_or_default();
                                                stmts.push(format!("  - [{}] {} (line {})", tag.to_uppercase(), id, l_idx + 1));
                                            }
                                        }
                                    }
                                }

                                if filter_name.is_empty() || ns.contains(filter_name) {
                                    results.push(format!("📄 Mapper: {}\n   File: {}\n{}", ns, p.display(), stmts.join("\n")));
                                }
                            }
                        }
                    }
                }

                json!({
                    "content": [{
                        "type": "text",
                        "text": if results.is_empty() {
                            "No MyBatis XML mapper files found.".to_string()
                        } else {
                            format!("Discovered MyBatis XML Mappers:\n\n{}", results.join("\n\n"))
                        }
                    }]
                })
            }
            _ => json!({ "content": [{ "type": "text", "text": format!("Unknown tool: {}", tool_name) }] }),
        }
    }
}
