use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

pub fn mcp_tools_list() -> Value {
    let read_only = json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": false
    });
    let exec_tool = json!({
        "readOnlyHint": false,
        "destructiveHint": true,
        "idempotentHint": false,
        "openWorldHint": true
    });

    json!({
        "tools": [
            {
                "name": "project_map",
                "description": "PageRank-prioritized codebase & .md map: directory tree, AST symbols, # Headings, and [start..end] line ranges.",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subpath": { "type": "string", "description": "Relative directory subpath.", "default": "." },
                        "max_tokens": { "type": "integer", "minimum": 1, "description": "Max token budget.", "default": 800 }
                    }
                }
            },
            {
                "name": "outline",
                "description": "Symbol TOC for code, # headings for .md/.mdx, or top-level keys for .yml/.toml/.json with [start..end] ranges.",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "minLength": 1, "description": "Relative path from project root or registered doc." },
                        "max_tokens": { "type": "integer", "minimum": 1, "description": "Max token budget.", "default": 1000 },
                        "include_tests": { "type": "boolean", "description": "Expand `mod tests` symbols.", "default": false }
                    },
                    "required": ["file"]
                }
            },
            {
                "name": "read_symbol",
                "description": "Read exact lines for one or more symbols (`symbol` or batch `symbols`; fuzzy-matched across workspace if `file` omitted), heading, config key, or line range. Provide `file`, `symbol`, or `symbols`.",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Relative file path; omit to search across workspace." },
                        "symbol": { "type": "string", "description": "Single function/struct/class/heading/config key name." },
                        "symbols": { "type": "array", "minItems": 1, "items": { "type": "string", "minLength": 1 }, "description": "Batch list of symbol names to read in one call (takes precedence over `symbol`/line range)." },
                        "start_line": { "type": "integer", "minimum": 1, "description": "1-based start line." },
                        "end_line": { "type": "integer", "minimum": 1, "description": "1-based end line." },
                        "context_lines": { "type": "integer", "minimum": 0, "description": "Context lines around symbol (0 for explicit line ranges).", "default": 2 },
                        "strip_comments": { "type": "boolean", "description": "Strip non-doc comments.", "default": false },
                        "signatures_only": { "type": "boolean", "description": "Keep imports, types, and signatures while eliding function bodies.", "default": false },
                        "max_lines": { "type": "integer", "minimum": 1, "description": "Max lines per slice before paginating.", "default": 180 }
                    }
                }
            },
            {
                "name": "find_references",
                "description": "Find AST symbol references (call, type_use, import), enclosing caller functions, and 2-hop caller impact across the workspace.",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "minLength": 1, "description": "Function, struct, enum, trait, or type name to trace." }
                    },
                    "required": ["symbol"]
                }
            },
            {
                "name": "search_docs",
                "description": "Column-weighted SQLite FTS5 BM25 search over workspace & global registered docs (.md/.mdx fence-safe sections and full multi-language code files).",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "minLength": 1, "description": "Topic, API, or identifier to search in docs." },
                        "doc": { "type": "string", "description": "Registered doc name from `lee docs --json`." },
                        "scope": { "type": "string", "enum": ["workspace", "global", "all"], "description": "Doc scope to search.", "default": "all" },
                        "file_filter": { "type": "string", "description": "Substring or glob file path filter." },
                        "budget_tokens": { "type": "integer", "minimum": 1, "description": "Max token budget (top match always intact).", "default": 800 }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "smart_grep",
                "description": "Grouped regex/text search (replaces rg -l / rg -n / rg -w) with enclosing AST symbol or # Heading [start..end].",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "minLength": 1, "description": "Keyword or regex pattern." },
                        "path": { "type": "string", "description": "Subdir or space/comma-separated file paths (e.g. 'src/')." },
                        "glob": { "type": "string", "description": "Glob filter (e.g. '*.rs', '*.md')." },
                        "files_only": { "type": "boolean", "description": "Return matching files and counts only (like rg -l).", "default": false },
                        "case_insensitive": { "type": "boolean", "description": "Case-insensitive regex match.", "default": false },
                        "whole_word": { "type": "boolean", "description": "Match whole words only using \\b boundaries (like rg -w).", "default": false },
                        "max_per_file": { "type": "integer", "minimum": 1, "description": "Max matches shown per file.", "default": 8 },
                        "max_results": { "type": "integer", "minimum": 1, "description": "Max total matches.", "default": 30 }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "run_digest",
                "description": "Run noisy build/test/diff command; if >35 lines, stores full output as handle h:ID and returns extracted errors/summary.",
                "annotations": exec_tool,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "minLength": 1, "description": "Shell command to run." },
                        "cwd": { "type": "string", "description": "Relative working directory." }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "recall",
                "description": "Pull a specific item block (test/fn/diff file), failures, regex filter (+3 context lines), or line range (default: lines 1..80) from handle h:ID. Precedence: item > failures_only > filter > line range.",
                "annotations": read_only,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "minLength": 1, "description": "Handle ID (e.g. 'h:8f2a9c')." },
                        "item": { "type": "string", "description": "Test name, symbol name, or diff file path to extract." },
                        "failures_only": { "type": "boolean", "description": "Return only extracted error/failure blocks.", "default": false },
                        "filter": { "type": "string", "description": "Substring or regex filter (+3 context lines)." },
                        "start_line": { "type": "integer", "minimum": 1, "description": "1-based start line." },
                        "end_line": { "type": "integer", "minimum": 1, "description": "1-based end line." }
                    },
                    "required": ["key"]
                }
            }
        ]
    })
}
