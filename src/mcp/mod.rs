pub mod handlers;
pub mod protocol;

use crate::config::AppPaths;
use crate::db::init_connection;
use crate::mcp::handlers::dispatch_tool_call;
use crate::mcp::protocol::{mcp_tools_list, JsonRpcRequest, JsonRpcResponse};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub fn run_stdio_server(paths: &AppPaths) -> Result<()> {
    paths.ensure_local_dirs()?;
    let mut conn = init_connection(&paths.local_db)?;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let bytes_read = reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            break; // EOF
        }

        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Support optional Content-Length framing if a client uses LSP-style headers
        let payload = if let Some(len_str) = trimmed.strip_prefix("Content-Length:") {
            let content_len: usize = len_str.trim().parse().unwrap_or(0);
            // Consume empty header separator line
            let mut sep = String::new();
            let _ = reader.read_line(&mut sep);
            let mut body = vec![0u8; content_len];
            use std::io::Read;
            reader.read_exact(&mut body)?;
            String::from_utf8_lossy(&body).to_string()
        } else {
            trimmed.to_string()
        };

        if let Some(resp) = handle_jsonrpc_message(&mut conn, paths, &payload) {
            let out_json = serde_json::to_string(&resp)?;
            writeln!(writer, "{out_json}")?;
            writer.flush()?;
        }
    }

    Ok(())
}

pub fn handle_jsonrpc_message(
    conn: &mut rusqlite::Connection,
    paths: &AppPaths,
    raw_json: &str,
) -> Option<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(raw_json) {
        Ok(r) => r,
        Err(e) => {
            return Some(JsonRpcResponse::error(
                Value::Null,
                -32700,
                format!("Parse error: {e}"),
            ));
        }
    };

    // Notifications do not have an `id` and must not receive a response
    let id = req.id.clone()?;

    match req.method.as_str() {
        "initialize" => {
            let client_version = req
                .params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .unwrap_or("2024-11-05");
            Some(JsonRpcResponse::success(
                id,
                json!({
                    "protocolVersion": client_version,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "lee",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": crate::AGENTS_MD_RULES
                }),
            ))
        }
        "ping" => Some(JsonRpcResponse::success(id, json!({}))),
        "tools/list" => Some(JsonRpcResponse::success(id, mcp_tools_list())),
        "tools/call" => {
            let Some(tool_name) = req.params.get("name").and_then(|v| v.as_str()) else {
                return Some(JsonRpcResponse::error(
                    id,
                    -32602,
                    "Missing `name` in tools/call params",
                ));
            };
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = dispatch_tool_call(conn, paths, tool_name, &args);
            Some(JsonRpcResponse::success(id, result))
        }
        other => Some(JsonRpcResponse::error(
            id,
            -32601,
            format!("Method not found: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_mcp_all_9_tools_and_token_metrics() {
        let temp = std::env::temp_dir().join(format!("lee_mcp_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(temp.join("src")).unwrap();
        fs::write(
            temp.join("src/lib.rs"),
            "pub struct Engine {\n    pub active: bool,\n}\nimpl Engine {\n    pub fn start(&self) -> bool {\n        self.active\n    }\n}\n",
        )
        .unwrap();
        fs::write(
            temp.join("README.md"),
            "# Kaye Context Guide\n\nIntro paragraph.\n\n## Hybrid Search Architecture\n\nUses SQLite FTS5 BM25 and BGE-Small-EN-v1.5.\n",
        )
        .unwrap();

        let mut paths = AppPaths::from_root(temp.clone()).unwrap();
        paths.global_dir = temp.join("global_cfg");
        paths.global_db = paths.global_dir.join("global.db");
        paths.global_config_file = paths.global_dir.join("config.json");
        paths.vault_dir = paths.global_dir.join("vault");
        paths.ensure_local_dirs().unwrap();
        paths.ensure_global_dirs().unwrap();

        let mut conn = init_connection(&paths.local_db).unwrap();

        // 1. initialize & tools/list
        let list_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        )
        .unwrap();
        let tools_arr = list_resp
            .result
            .unwrap()
            .get("tools")
            .unwrap()
            .as_array()
            .unwrap()
            .len();
        assert_eq!(tools_arr, 10);

        // 2. project_map
        let map_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"project_map","arguments":{}}}"#,
        )
        .unwrap();
        let map_txt = map_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(map_txt.contains("src/lib.rs"));
        assert!(map_txt.contains("README.md"));

        // 3. outline on .md
        let out_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"outline","arguments":{"file":"README.md"}}}"#,
        )
        .unwrap();
        let out_txt = out_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(out_txt.contains("## Hybrid Search Architecture"));

        // 4. read_symbol on .rs
        let read_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_symbol","arguments":{"file":"src/lib.rs","symbol":"start"}}}"#,
        )
        .unwrap();
        let read_txt = read_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(read_txt.contains("pub fn start"));

        // 5. search_docs
        let docs_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_docs","arguments":{"query":"SQLite FTS5 BM25"}}}"#,
        )
        .unwrap();
        let docs_txt = docs_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(docs_txt.contains("Hybrid Search Architecture"));

        // 6. smart_grep (grouped + files_only)
        let grep_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"smart_grep","arguments":{"pattern":"self.active","path":"src/"}}}"#,
        )
        .unwrap();
        let grep_txt = grep_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(grep_txt.contains("[in fn start"));

        let files_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"smart_grep","arguments":{"pattern":"self.active","files_only":true}}}"#,
        )
        .unwrap();
        let files_txt = files_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(files_txt.contains("src/lib.rs (1 matches)"));

        // 7. find_references (import, type_use, call)
        fs::write(
            temp.join("src/caller.rs"),
            "use crate::Engine;\npub fn boot(eng: &Engine) -> bool {\n    let _ = Engine::start(eng);\n    eng.start()\n}\n",
        )
        .unwrap();
        let refs_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"find_references","arguments":{"symbol":"Engine"}}}"#,
        )
        .unwrap();
        let refs_txt = refs_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(refs_txt.contains("Engine referenced in"));
        assert!(refs_txt.contains("src/caller.rs:1 [import]"));
        assert!(refs_txt.contains("src/caller.rs:2 [in fn boot] (type_use)"));
        assert!(refs_txt.contains("src/caller.rs:3 [in fn boot] (call)"));

        let fn_refs_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"find_references","arguments":{"symbol":"start"}}}"#,
        )
        .unwrap();
        let fn_refs_txt = fn_refs_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(fn_refs_txt.contains("src/caller.rs:3 [in fn boot] (call)"));
        assert!(fn_refs_txt.contains("src/caller.rs:4 [in fn boot] (call)"));

        // 8. Named workspace & global docs filtering via `search_docs(doc=..., scope=...)`
        let ws_doc_dir = temp.join("ext_ws_gpui");
        let gl_doc_dir = temp.join("ext_global_gpui");
        fs::create_dir_all(&ws_doc_dir).unwrap();
        fs::create_dir_all(&gl_doc_dir).unwrap();
        fs::write(
            ws_doc_dir.join("button.md"),
            "# Workspace Button\n\nWorkspace-specific button token alpha.\n",
        )
        .unwrap();
        fs::write(
            gl_doc_dir.join("button.md"),
            "# Global Button\n\nGlobal-specific button token omega.\n",
        )
        .unwrap();
        paths
            .add_doc(crate::config::DocScope::Workspace, &ws_doc_dir, "gpui kit")
            .unwrap();
        paths
            .add_doc(crate::config::DocScope::Global, &gl_doc_dir, "gpui kit")
            .unwrap();

        let gl_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"search_docs","arguments":{"query":"button","doc":"gpui kit","scope":"global"}}}"#,
        )
        .unwrap();
        let gl_txt = gl_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(gl_txt.contains("Global Button"));
        assert!(!gl_txt.contains("Workspace Button"));

        let ws_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"search_docs","arguments":{"query":"button","doc":"gpui kit","scope":"workspace"}}}"#,
        )
        .unwrap();
        let ws_txt = ws_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(ws_txt.contains("Workspace Button"));
        assert!(!ws_txt.contains("Global Button"));

        let _ = fs::remove_dir_all(&temp);
    }
}
