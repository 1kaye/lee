use crate::config::AppPaths;
use crate::digest::{recall_handle, run_command_digest};
use crate::docs::search_docs;
use crate::indexer::{
    build_outline, build_project_map, find_symbol_references, read_symbol_range,
    read_symbols_batch, smart_grep_search,
};
use rusqlite::Connection;
use serde_json::{json, Value};

enum ToolCallError {
    InvalidParams(String),
    Execution(anyhow::Error),
}

impl From<anyhow::Error> for ToolCallError {
    fn from(err: anyhow::Error) -> Self {
        Self::Execution(err)
    }
}

/// Dispatches an MCP `tools/call` invocation.
/// Returns `Err((-32602, msg))` for protocol-level parameter/unknown-tool errors,
/// and `Ok(CallToolResult)` (with `isError: true` on tool execution failures) otherwise.
pub fn dispatch_tool_call(
    conn: &mut Connection,
    paths: &AppPaths,
    tool_name: &str,
    args: &Value,
) -> std::result::Result<Value, (i64, String)> {
    match execute_tool(conn, paths, tool_name, args) {
        Ok(text) => Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ]
        })),
        Err(ToolCallError::InvalidParams(msg)) => Err((-32602, msg)),
        Err(ToolCallError::Execution(err)) => Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": format!("Error in `{tool_name}`: {err:#}")
                }
            ],
            "isError": true
        })),
    }
}

fn req_str<'a>(args: &'a Value, field: &str, allow_whitespace_only: bool) -> std::result::Result<&'a str, ToolCallError> {
    let Some(val) = args.get(field) else {
        return Err(ToolCallError::InvalidParams(format!(
            "Missing required parameter `{field}`"
        )));
    };
    if val.is_null() {
        return Err(ToolCallError::InvalidParams(format!(
            "Missing required parameter `{field}`"
        )));
    }
    let Some(s) = val.as_str() else {
        return Err(ToolCallError::InvalidParams(format!(
            "Parameter `{field}` must be a string"
        )));
    };
    let is_empty = if allow_whitespace_only {
        s.is_empty()
    } else {
        s.trim().is_empty()
    };
    if is_empty {
        return Err(ToolCallError::InvalidParams(format!(
            "Parameter `{field}` must be a non-empty string"
        )));
    }
    Ok(s)
}

fn opt_str<'a>(args: &'a Value, field: &str) -> std::result::Result<Option<&'a str>, ToolCallError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolCallError::InvalidParams(format!(
            "Parameter `{field}` must be a string"
        ))),
    }
}

fn opt_usize(args: &Value, field: &str) -> std::result::Result<Option<usize>, ToolCallError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(|n| Some(usize::try_from(n).unwrap_or(usize::MAX)))
            .ok_or_else(|| {
                ToolCallError::InvalidParams(format!(
                    "Parameter `{field}` must be a non-negative integer"
                ))
            }),
    }
}

fn opt_bool(args: &Value, field: &str, default: bool) -> std::result::Result<bool, ToolCallError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(ToolCallError::InvalidParams(format!(
            "Parameter `{field}` must be a boolean"
        ))),
    }
}

fn opt_str_array(args: &Value, field: &str) -> std::result::Result<Option<Vec<String>>, ToolCallError> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let Some(s) = item.as_str() else {
                    return Err(ToolCallError::InvalidParams(format!(
                        "Parameter `{field}` must be an array of strings"
                    )));
                };
                out.push(s.to_string());
            }
            Ok(Some(out))
        }
        Some(_) => Err(ToolCallError::InvalidParams(format!(
            "Parameter `{field}` must be an array of strings"
        ))),
    }
}

fn execute_tool(
    conn: &mut Connection,
    paths: &AppPaths,
    tool_name: &str,
    args: &Value,
) -> std::result::Result<String, ToolCallError> {
    match tool_name {
        "project_map" => {
            let subpath = opt_str(args, "subpath")?;
            let max_tokens = opt_usize(args, "max_tokens")?.unwrap_or(800);
            Ok(build_project_map(conn, paths, subpath, max_tokens)?)
        }
        "outline" => {
            let file = req_str(args, "file", false)?;
            let max_tokens = opt_usize(args, "max_tokens")?.unwrap_or(1000);
            let include_tests = opt_bool(args, "include_tests", false)?;
            Ok(build_outline(conn, paths, file, max_tokens, include_tests)?)
        }
        "read_symbol" => {
            let file = opt_str(args, "file")?;
            let symbol = opt_str(args, "symbol")?;
            let symbols = opt_str_array(args, "symbols")?;
            let start_line = opt_usize(args, "start_line")?;
            let end_line = opt_usize(args, "end_line")?;
            let context_lines = opt_usize(args, "context_lines")?.unwrap_or(2);
            let strip_comments = opt_bool(args, "strip_comments", false)?;
            let signatures_only = opt_bool(args, "signatures_only", false)?;
            let max_lines = opt_usize(args, "max_lines")?.unwrap_or(180);

            if let Some(ref sym_list) = symbols {
                if !sym_list.iter().any(|s| !s.trim().is_empty()) {
                    return Err(ToolCallError::InvalidParams(
                        "Parameter `symbols` must contain at least one non-empty string"
                            .to_string(),
                    ));
                }
                return Ok(read_symbols_batch(
                    conn,
                    paths,
                    file,
                    sym_list,
                    context_lines,
                    strip_comments,
                    signatures_only,
                    max_lines,
                )?);
            }

            let has_file = file.is_some_and(|s| !s.trim().is_empty());
            let has_symbol = symbol.is_some_and(|s| !s.trim().is_empty());
            if !has_file && !has_symbol {
                return Err(ToolCallError::InvalidParams(
                    "Provide `file`, `symbol`, or `symbols` to `read_symbol`".to_string(),
                ));
            }
            if symbol.is_some() && !has_symbol {
                return Err(ToolCallError::InvalidParams(
                    "Parameter `symbol` must be a non-empty string".to_string(),
                ));
            }

            Ok(read_symbol_range(
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
            )?)
        }
        "find_references" => {
            let symbol = req_str(args, "symbol", false)?;
            Ok(find_symbol_references(conn, paths, symbol)?)
        }
        "search_docs" => {
            let query = req_str(args, "query", false)?;
            let doc_name = opt_str(args, "doc")?;
            let scope = opt_str(args, "scope")?;
            if let Some(sc) = scope.map(str::trim).filter(|s| !s.is_empty())
                && !matches!(sc.to_ascii_lowercase().as_str(), "workspace" | "global" | "all")
            {
                return Err(ToolCallError::InvalidParams(format!(
                    "Invalid `scope` '{sc}'. Expected 'workspace', 'global', or 'all'."
                )));
            }
            let file_filter = opt_str(args, "file_filter")?;
            let budget_tokens = opt_usize(args, "budget_tokens")?.unwrap_or(800);
            Ok(search_docs(
                conn,
                paths,
                query,
                file_filter,
                doc_name,
                scope,
                budget_tokens,
            )?)
        }
        "smart_grep" => {
            let pattern = req_str(args, "pattern", true)?;
            let glob = opt_str(args, "glob")?;
            let path = opt_str(args, "path")?;
            let files_only = opt_bool(args, "files_only", false)?;
            let case_insensitive = opt_bool(args, "case_insensitive", false)?;
            let whole_word = opt_bool(args, "whole_word", false)?;
            let max_per_file = opt_usize(args, "max_per_file")?.unwrap_or(8);
            let max_results = opt_usize(args, "max_results")?.unwrap_or(30);
            Ok(smart_grep_search(
                conn,
                paths,
                pattern,
                glob,
                path,
                files_only,
                case_insensitive,
                whole_word,
                max_per_file,
                max_results,
            )?)
        }
        "run_digest" => {
            let command = req_str(args, "command", false)?;
            let cwd = opt_str(args, "cwd")?;
            Ok(run_command_digest(conn, paths, command, cwd)?)
        }
        "recall" => {
            let key = req_str(args, "key", false)?;
            let item = opt_str(args, "item")?;
            let failures_only = opt_bool(args, "failures_only", false)?;
            let filter = opt_str(args, "filter")?;
            let start_line = opt_usize(args, "start_line")?;
            let end_line = opt_usize(args, "end_line")?;
            Ok(recall_handle(
                conn,
                key,
                item,
                failures_only,
                filter,
                start_line,
                end_line,
            )?)
        }
        other => Err(ToolCallError::InvalidParams(format!(
            "Unknown MCP tool '{other}'"
        ))),
    }
}
