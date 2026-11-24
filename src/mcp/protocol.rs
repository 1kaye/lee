use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
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
                data: None,
            }),
        }
    }
}

pub fn mcp_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "project_map",
                "description": "PageRank-prioritized codebase & .md map: directory tree, AST symbols, # Headings, and [start..end] line ranges.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subpath": { "type": "string", "description": "Relative directory subpath (default: '.').", "default": "." },
                        "max_tokens": { "type": "integer", "description": "Max token budget (default: 800).", "default": 800 }
                    }
                }
            },
            {
                "name": "outline",
                "description": "Symbol TOC for code, # headings for .md/.mdx, or top-level keys for .yml/.toml/.json with [start..end] ranges.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Relative path from root or registered doc." },
                        "max_tokens": { "type": "integer", "description": "Max token budget (default: 1000).", "default": 1000 },
                        "include_tests": { "type": "boolean", "description": "Expand `mod tests` symbols (default: false).", "default": false }
                    },
                    "required": ["file"]
                }
            },
            {
                "name": "read_symbol",
                "description": "Read exact lines for one or more symbols (batch via `symbols`, fuzzy-matched across workspace if `file` omitted), Markdown heading, config key, or line range.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Optional relative file path; omit to search across workspace." },
                        "symbol": { "type": "string", "description": "Single function/struct/class/heading/config key name." },
                        "symbols": { "type": "array", "items": { "type": "string" }, "description": "Batch list of symbol names to read in one call." },
                        "start_line": { "type": "integer", "description": "1-based start line." },
                        "end_line": { "type": "integer", "description": "1-based end line." },
                        "context_lines": { "type": "integer", "description": "Context lines around symbol (default: 2; 0 for explicit line ranges).", "default": 2 },
                        "strip_comments": { "type": "boolean", "description": "Strip non-doc comments (default: false).", "default": false },
                        "signatures_only": { "type": "boolean", "description": "Keep imports, types, and signatures while eliding function bodies (default: false).", "default": false },
                        "max_lines": { "type": "integer", "description": "Max lines per slice before paginating (default: 180).", "default": 180 }
                    }
                }
            },
            {
                "name": "find_references",
                "description": "Find all AST symbol references (call, type_use, import), enclosing caller functions, and 2-hop caller impact across the workspace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Function, struct, enum, trait, or type name to trace." }
                    },
                    "required": ["symbol"]
                }
            },
            {
                "name": "search_docs",
                "description": "Column-weighted SQLite FTS5 BM25 search over workspace & global registered docs (.md/.mdx fence-safe sections and full multi-language code files).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Topic, API, or identifier to search in docs." },
                        "doc": { "type": "string", "description": "Optional registered doc name from `lee docs --json`." },
                        "scope": { "type": "string", "description": "Optional scope: 'workspace', 'global', or 'all' (default: 'all').", "default": "all" },
                        "file_filter": { "type": "string", "description": "Optional substring or glob filter." },
                        "budget_tokens": { "type": "integer", "description": "Max token budget (default: 800; top match always intact).", "default": 800 }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "smart_grep",
                "description": "Grouped regex/text search (replaces rg -l / rg -n / rg -w) with enclosing AST symbol or # Heading [start..end].",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Keyword or regex pattern." },
                        "path": { "type": "string", "description": "Optional subdir or space/comma-separated file paths (e.g. 'src/')." },
                        "glob": { "type": "string", "description": "Optional glob (e.g. '*.rs', '*.md')." },
                        "files_only": { "type": "boolean", "description": "Return matching files + counts only, like rg -l (default: false).", "default": false },
                        "case_insensitive": { "type": "boolean", "description": "Case-insensitive regex match (default: false).", "default": false },
                        "whole_word": { "type": "boolean", "description": "Match whole words only using \\b boundaries, like rg -w (default: false).", "default": false },
                        "max_per_file": { "type": "integer", "description": "Max matches shown per file (default: 8).", "default": 8 },
                        "max_results": { "type": "integer", "description": "Max total matches (default: 30).", "default": 30 }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "run_digest",
                "description": "Run noisy build/test/diff command; if >35 lines, stores full output as handle h:ID and returns extracted errors/summary.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to run." },
                        "cwd": { "type": "string", "description": "Optional relative working directory." }
                    },
                    "required": ["command"]
                }
            },
            {
                "name": "recall",
                "description": "Pull specific item block (test/fn/diff file), failures, regex filter (+3 context lines), or line range from handle h:ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": { "type": "string", "description": "Handle ID (e.g. 'h:8f2a9c')." },
                        "item": { "type": "string", "description": "Test name, symbol name, or diff file path to extract." },
                        "failures_only": { "type": "boolean", "description": "Return only extracted error/failure blocks." },
                        "filter": { "type": "string", "description": "Substring or regex filter (+3 context lines)." },
                        "start_line": { "type": "integer", "description": "1-based start line." },
                        "end_line": { "type": "integer", "description": "1-based end line." }
                    },
                    "required": ["key"]
                }
            }
        ]
    })
}
