pub mod handlers;
pub mod protocol;

use crate::config::AppPaths;
use crate::db::init_connection;
use crate::mcp::handlers::dispatch_tool_call;
use crate::mcp::protocol::{mcp_tools_list, JsonRpcResponse};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramingMode {
    Ndjson,
    ContentLength,
}

fn parse_header_line(line: &str) -> Option<(&str, &str)> {
    let (name, val) = line.split_once(':')?;
    if name.trim().is_empty() || name.contains('{') || name.contains('"') {
        return None;
    }
    Some((name.trim(), val.trim()))
}

fn read_framed_message<R: BufRead>(reader: &mut R) -> Result<Option<(String, FramingMode)>> {
    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        let bytes_read = reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            return Ok(None);
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Check for LSP/MCP Content-Length header framing (headers may arrive in any order)
        if let Some((hdr_name, hdr_val)) = parse_header_line(trimmed) {
            let mut content_len: Option<usize> = None;
            if hdr_name.eq_ignore_ascii_case("content-length") {
                content_len = Some(
                    hdr_val
                        .parse::<usize>()
                        .map_err(|_| anyhow::anyhow!("Invalid Content-Length header: {hdr_val}"))?,
                );
            }
            loop {
                line_buf.clear();
                let n = reader.read_line(&mut line_buf)?;
                if n == 0 {
                    return Ok(None);
                }
                let h = line_buf.trim();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = parse_header_line(h)
                    && k.eq_ignore_ascii_case("content-length")
                {
                    content_len = Some(
                        v.parse::<usize>()
                            .map_err(|_| anyhow::anyhow!("Invalid Content-Length header: {v}"))?,
                    );
                }
            }
            let Some(len) = content_len else {
                bail!("Missing Content-Length header in framed MCP message");
            };
            if len > MAX_FRAME_BYTES {
                bail!("MCP message Content-Length ({len}) exceeds {MAX_FRAME_BYTES}-byte limit");
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body)?;
            return Ok(Some((
                String::from_utf8_lossy(&body).into_owned(),
                FramingMode::ContentLength,
            )));
        }

        return Ok(Some((trimmed.to_string(), FramingMode::Ndjson)));
    }
}

fn write_framed_response<W: Write>(
    writer: &mut W,
    resp: &JsonRpcResponse,
    mode: FramingMode,
) -> Result<()> {
    let out_json = serde_json::to_string(resp)?;
    match mode {
        FramingMode::Ndjson => {
            writeln!(writer, "{out_json}")?;
        }
        FramingMode::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n{}", out_json.len(), out_json)?;
        }
    }
    writer.flush()?;
    Ok(())
}

pub fn run_stdio_server(paths: &AppPaths) -> Result<()> {
    paths.ensure_local_dirs()?;
    let mut conn = init_connection(&paths.local_db)?;

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    while let Some((payload, mode)) = read_framed_message(&mut reader)? {
        if let Some(resp) = handle_jsonrpc_message(&mut conn, paths, &payload) {
            write_framed_response(&mut writer, &resp, mode)?;
        }
    }

    Ok(())
}

pub fn handle_jsonrpc_message(
    conn: &mut rusqlite::Connection,
    paths: &AppPaths,
    raw_json: &str,
) -> Option<JsonRpcResponse> {
    let val: Value = match serde_json::from_str(raw_json) {
        Ok(v) => v,
        Err(e) => {
            return Some(JsonRpcResponse::error(
                Value::Null,
                -32700,
                format!("Parse error: {e}"),
            ));
        }
    };

    let Some(obj) = val.as_object() else {
        return Some(JsonRpcResponse::error(
            Value::Null,
            -32600,
            "Invalid Request: expected JSON object",
        ));
    };

    // Validate `id`: absent means notification; present must be string or number (MCP forbids null)
    let id_opt: Option<Value> = match obj.get("id") {
        None => None,
        Some(v @ (Value::String(_) | Value::Number(_))) => Some(v.clone()),
        Some(_) => {
            return Some(JsonRpcResponse::error(
                Value::Null,
                -32600,
                "Invalid Request: `id` must be a string or number",
            ));
        }
    };

    let err_id = id_opt.clone().unwrap_or(Value::Null);

    if obj.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(JsonRpcResponse::error(
            err_id,
            -32600,
            "Invalid Request: `jsonrpc` must be \"2.0\"",
        ));
    }

    let Some(method) = obj.get("method").and_then(|v| v.as_str()).filter(|m| !m.is_empty()) else {
        return Some(JsonRpcResponse::error(
            err_id,
            -32600,
            "Invalid Request: missing or non-string `method`",
        ));
    };

    let params = match obj.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => {
            return Some(JsonRpcResponse::error(
                err_id,
                -32602,
                "Invalid params: `params` must be an object",
            ));
        }
    };

    // Valid notifications do not have an `id` and must not receive a response
    let id = id_opt?;

    match method {
        "initialize" => {
            let negotiated = params
                .get("protocolVersion")
                .and_then(|v| v.as_str())
                .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
            Some(JsonRpcResponse::success(
                id,
                json!({
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "lee",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": "Use lee MCP tools for AST navigation, docs search, and log digesting: project_map (PageRank overview), outline/read_symbol (supports batch `symbols` & fuzzy lookup) for large files, find_references (1-hop & 2-hop impact) before refactors, smart_grep (`whole_word`) over rg, search_docs for Markdown/registered docs, and run_digest/recall for noisy build logs."
                }),
            ))
        }
        "ping" => Some(JsonRpcResponse::success(id, json!({}))),
        "tools/list" => Some(JsonRpcResponse::success(id, mcp_tools_list())),
        "tools/call" => {
            let Some(tool_name) = params.get("name").and_then(|v| v.as_str()) else {
                return Some(JsonRpcResponse::error(
                    id,
                    -32602,
                    "Missing `name` in tools/call params",
                ));
            };
            let args = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(v @ Value::Object(_)) => v.clone(),
                Some(_) => {
                    return Some(JsonRpcResponse::error(
                        id,
                        -32602,
                        "Invalid `arguments` in tools/call params: expected object",
                    ));
                }
            };
            match dispatch_tool_call(conn, paths, tool_name, &args) {
                Ok(result) => Some(JsonRpcResponse::success(id, result)),
                Err((code, msg)) => Some(JsonRpcResponse::error(id, code, msg)),
            }
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
    use std::io::Cursor;

    #[test]
    fn test_mcp_all_8_tools_and_protocol() {
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
            "# Kaye Context Guide\n\nIntro paragraph.\n\n## Hybrid Search Architecture\n\nUses SQLite FTS5 BM25.\n",
        )
        .unwrap();

        let paths = AppPaths::from_root_with_global_dir(temp.clone(), temp.join("global_cfg"));
        paths.ensure_local_dirs().unwrap();
        paths.ensure_global_dirs().unwrap();

        let mut conn = init_connection(&paths.local_db).unwrap();

        // Protocol: initialize (negotiates supported vs unknown version), ping, notifications
        let init_supported = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
        )
        .unwrap();
        assert_eq!(init_supported.result.unwrap()["protocolVersion"], "2024-11-05");

        let init_unknown = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .unwrap();
        assert_eq!(init_unknown.result.unwrap()["protocolVersion"], "2025-03-26");

        let ping = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":"p1","method":"ping"}"#,
        )
        .unwrap();
        assert_eq!(ping.result.unwrap(), json!({}));

        assert!(handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .is_none());

        // Protocol error codes: -32700, -32600, -32601, -32602
        assert_eq!(
            handle_jsonrpc_message(&mut conn, &paths, "{bad json").unwrap().error.unwrap().code,
            -32700
        );
        assert_eq!(
            handle_jsonrpc_message(&mut conn, &paths, r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#)
                .unwrap()
                .error
                .unwrap()
                .code,
            -32600
        );
        assert_eq!(
            handle_jsonrpc_message(&mut conn, &paths, r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)
                .unwrap()
                .error
                .unwrap()
                .code,
            -32600
        );
        assert_eq!(
            handle_jsonrpc_message(&mut conn, &paths, r#"{"jsonrpc":"2.0","id":1,"method":"no_such"}"#)
                .unwrap()
                .error
                .unwrap()
                .code,
            -32601
        );
        assert_eq!(
            handle_jsonrpc_message(
                &mut conn,
                &paths,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#
            )
            .unwrap()
            .error
            .unwrap()
            .code,
            -32602
        );
        assert_eq!(
            handle_jsonrpc_message(
                &mut conn,
                &paths,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"outline","arguments":{"file":123}}}"#
            )
            .unwrap()
            .error
            .unwrap()
            .code,
            -32602
        );

        // 1. tools/list
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
        assert_eq!(tools_arr, 8);

        // 2. project_map (success omits redundant `isError`)
        let map_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"project_map","arguments":{}}}"#,
        )
        .unwrap();
        let map_res = map_resp.result.unwrap();
        assert!(map_res.get("isError").is_none());
        let map_txt = map_res["content"][0]["text"].as_str().unwrap();
        assert!(map_txt.contains("src/lib.rs"));
        assert!(map_txt.contains("README.md"));

        // 3. outline on .md (and execution error sets isError: true)
        let out_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"outline","arguments":{"file":"README.md"}}}"#,
        )
        .unwrap();
        assert!(out_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("## Hybrid Search Architecture"));

        let missing_out = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":30,"method":"tools/call","params":{"name":"outline","arguments":{"file":"missing.rs"}}}"#,
        )
        .unwrap();
        assert_eq!(missing_out.result.unwrap()["isError"], true);

        // 4. read_symbol on .rs (single & batch `symbols`)
        let read_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_symbol","arguments":{"file":"src/lib.rs","symbol":"start"}}}"#,
        )
        .unwrap();
        assert!(read_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pub fn start"));

        let batch_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"read_symbol","arguments":{"symbols":["Engine","start"]}}}"#,
        )
        .unwrap();
        let batch_txt = batch_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(batch_txt.contains("pub struct Engine"));
        assert!(batch_txt.contains("pub fn start"));

        // 5. search_docs
        let docs_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_docs","arguments":{"query":"SQLite FTS5 BM25"}}}"#,
        )
        .unwrap();
        assert!(docs_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Hybrid Search Architecture"));

        // 6. smart_grep (grouped + files_only)
        let grep_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"smart_grep","arguments":{"pattern":"self.active","path":"src/"}}}"#,
        )
        .unwrap();
        assert!(grep_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[in fn start"));

        let files_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"smart_grep","arguments":{"pattern":"self.active","files_only":true}}}"#,
        )
        .unwrap();
        assert!(files_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("src/lib.rs (1 matches)"));

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

        // 8. run_digest + recall
        let digest_resp = handle_jsonrpc_message(
            &mut conn,
            &paths,
            r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"run_digest","arguments":{"command":"i=1; while [ $i -le 50 ]; do echo \"Compiling noisy_test_crate_$i v0.1.0 (/workspace/crates/crate_$i)\"; i=$((i+1)); done"}}}"#,
        )
        .unwrap();
        let digest_txt = digest_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        let h_start = digest_txt.find("h:").unwrap();
        let handle_id = &digest_txt[h_start..h_start + 8];

        let recall_req = json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "recall",
                "arguments": { "key": handle_id, "start_line": 48, "end_line": 50 }
            }
        })
        .to_string();
        let recall_resp = handle_jsonrpc_message(&mut conn, &paths, &recall_req).unwrap();
        let recall_txt = recall_resp.result.unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(recall_txt.contains("noisy_test_crate_50"));

        // 9. Named workspace & global docs filtering via `search_docs(doc=..., scope=...)`
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

        // 10. Content-Length framing round-trip with mixed-case and multi-header input
        let body = r#"{"jsonrpc":"2.0","id":99,"method":"ping"}"#;
        let wire = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let mut cursor = Cursor::new(wire.as_bytes());
        let (parsed_body, mode) = read_framed_message(&mut cursor).unwrap().unwrap();
        assert_eq!(parsed_body, body);
        assert_eq!(mode, FramingMode::ContentLength);

        let mut out_buf = Vec::new();
        let resp = handle_jsonrpc_message(&mut conn, &paths, &parsed_body).unwrap();
        write_framed_response(&mut out_buf, &resp, mode).unwrap();
        let out_str = String::from_utf8(out_buf).unwrap();
        assert!(out_str.starts_with("Content-Length: "));
        assert!(out_str.contains("\r\n\r\n{"));

        drop(conn);
        let _ = fs::remove_dir_all(&temp);
    }
}
