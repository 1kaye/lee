use crate::config::AppPaths;
use crate::digest::{recall_handle, run_command_digest};
use crate::indexer::{
    build_outline, build_project_map, find_symbol_references, read_symbol_range, smart_grep_search,
};
use crate::memory::{recall_memories, search_docs, store_memory};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn dispatch_tool_call(
    conn: &mut Connection,
    paths: &AppPaths,
    tool_name: &str,
    args: &Value,
) -> Value {
    match execute_tool(conn, paths, tool_name, args) {
        Ok((text, _raw_bytes)) => json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "isError": false
        }),
        Err(err) => json!({
            "content": [
                {
                    "type": "text",
                    "text": format!("Error in `{tool_name}`: {err:#}")
                }
            ],
            "isError": true
        }),
    }
}

fn execute_tool(
    conn: &mut Connection,
    paths: &AppPaths,
    tool_name: &str,
    args: &Value,
) -> Result<(String, usize)> {
    match tool_name {
        "project_map" => {
            let subpath = args.get("subpath").and_then(|v| v.as_str());
            let max_tokens = args
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(800) as usize;
            build_project_map(conn, paths, subpath, max_tokens)
        }
        "outline" => {
            let Some(file) = args.get("file").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `file`");
            };
            let max_tokens = args
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(1000) as usize;
            let include_tests = args
                .get("include_tests")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            build_outline(conn, paths, file, max_tokens, include_tests)
        }
        "read_symbol" => {
            let file = args.get("file").and_then(|v| v.as_str());
            let symbol = args.get("symbol").and_then(|v| v.as_str());
            if file.is_none() && symbol.is_none() {
                bail!("Provide `file` or `symbol` (e.g. function name) to `read_symbol`");
            }
            let start_line = args
                .get("start_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let end_line = args
                .get("end_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let context_lines = args
                .get("context_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(2) as usize;
            let strip_comments = args
                .get("strip_comments")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let signatures_only = args
                .get("signatures_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let max_lines = args
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(180) as usize;
            read_symbol_range(
                conn,
                paths,
                file,
                symbol,
                start_line,
                end_line,
                context_lines,
                strip_comments,
                signatures_only,
                max_lines,
            )
        }
        "find_references" => {
            let Some(symbol) = args.get("symbol").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `symbol`");
            };
            find_symbol_references(conn, paths, symbol)
        }
        "search_docs" => {
            let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `query`");
            };
            let doc_name = args.get("doc").and_then(|v| v.as_str());
            let scope = args.get("scope").and_then(|v| v.as_str());
            let file_filter = args.get("file_filter").and_then(|v| v.as_str());
            let budget_tokens = args
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(800) as usize;
            search_docs(conn, paths, query, file_filter, doc_name, scope, budget_tokens)
        }
        "smart_grep" => {
            let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `pattern`");
            };
            let glob = args.get("glob").and_then(|v| v.as_str());
            let path = args.get("path").and_then(|v| v.as_str());
            let files_only = args
                .get("files_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let case_insensitive = args
                .get("case_insensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let max_per_file = args
                .get("max_per_file")
                .and_then(|v| v.as_u64())
                .unwrap_or(8) as usize;
            let max_results = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(30) as usize;
            smart_grep_search(
                conn,
                paths,
                pattern,
                glob,
                path,
                files_only,
                case_insensitive,
                max_per_file,
                max_results,
            )
        }
        "run_digest" => {
            let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `command`");
            };
            let cwd = args.get("cwd").and_then(|v| v.as_str());
            run_command_digest(conn, paths, command, cwd)
        }
        "recall" => {
            let Some(key) = args.get("key").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `key`");
            };
            let item = args.get("item").and_then(|v| v.as_str());
            let failures_only = args
                .get("failures_only")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let filter = args.get("filter").and_then(|v| v.as_str());
            let start_line = args
                .get("start_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            let end_line = args
                .get("end_line")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize);
            recall_handle(conn, key, item, failures_only, filter, start_line, end_line)
        }
        "remember" => {
            let Some(topic) = args.get("topic").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `topic`");
            };
            let Some(content) = args.get("content").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `content`");
            };
            let tags = args.get("tags").and_then(|v| v.as_str());
            let scope = args.get("scope").and_then(|v| v.as_str());
            store_memory(conn, paths, topic, content, tags, scope)
        }
        "recall_memory" => {
            let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                bail!("Missing required parameter `query`");
            };
            let budget_tokens = args
                .get("budget_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(600) as usize;
            recall_memories(conn, paths, query, budget_tokens)
        }
        other => bail!("Unknown MCP tool '{other}'"),
    }
}
