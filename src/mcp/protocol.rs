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

/// Returns the byte size of the serialized MCP tool list schema loaded at session start.
pub fn mcp_schema_overhead_bytes() -> usize {
    serde_json::to_string(&mcp_tools_list())
        .map(|s| s.len())
        .unwrap_or(0)
}

pub fn mcp_tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "project_map",
                "description": "Cold-start codebase & .md map: directory tree, AST symbols, # Headings, and [start..end] line ranges.",
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
                "description": "Symbol TOC for code, # headings for .md, or top-level keys for .yml/.toml/.json with [start..end] ranges.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Relative path from root or registered source." },
                        "max_tokens": { "type": "integer", "description": "Max token budget (default: 1000).", "default": 1000 },
                        "include_tests": { "type": "boolean", "description": "Expand `mod tests` symbols (default: false).", "default": false }
                    },
                    "required": ["file"]
                }
            },
            {
                "name": "read_symbol",
                "description": "Read exact lines for a code symbol/function (across workspace if `file` omitted), Markdown heading, config key, or line range.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file": { "type": "string", "description": "Optional relative file path; omit to search `symbol` across workspace & sources." },
                        "symbol": { "type": "string", "description": "Function/struct/class/symbol name, Markdown heading, or config key." },
                        "start_line": { "type": "integer", "description": "1-based start line." },
                        "end_line": { "type": "integer", "description": "1-based end line." },
                        "context_lines": { "type": "integer", "description": "Context lines around symbol (default: 2).", "default": 2 },
                        "strip_comments": { "type": "boolean", "description": "Strip non-doc comments to save tokens (default: false).", "default": false },
                        "signatures_only": { "type": "boolean", "description": "Aggressive filter: keep imports, types, and function signatures while eliding implementation bodies (default: false).", "default": false },
                        "max_lines": { "type": "integer", "description": "Max lines per slice before paginating (default: 180).", "default": 180 }
                    }
                }
            },
            {
                "name": "find_references",
                "description": "Find all AST symbol references (call, type_use, import) and enclosing caller functions across the workspace.",
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
                "description": "Hybrid FTS5 BM25 + BGE-Small (RRF) search over project & external `sources` .md files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Topic, API, or concept to search in docs." },
                        "file_filter": { "type": "string", "description": "Optional substring or glob filter." },
                        "budget_tokens": { "type": "integer", "description": "Max token budget (default: 800).", "default": 800 }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "smart_grep",
                "description": "Grouped regex/text search (replaces rg -l / rg -n) with enclosing AST symbol or # Heading [start..end].",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Keyword or regex pattern." },
                        "path": { "type": "string", "description": "Optional subdir or space/comma-separated file paths (e.g. 'src/')." },
                        "glob": { "type": "string", "description": "Optional glob (e.g. '*.rs', '*.md')." },
                        "files_only": { "type": "boolean", "description": "Return matching files + counts only, like rg -l (default: false).", "default": false },
                        "case_insensitive": { "type": "boolean", "description": "Case-insensitive regex match (default: false).", "default": false },
                        "max_per_file": { "type": "integer", "description": "Max matches shown per file (default: 8).", "default": 8 },
                        "max_results": { "type": "integer", "description": "Max total matches (default: 30).", "default": 30 }
                    },
                    "required": ["pattern"]
                }
            },
            {
                "name": "run_digest",
                "description": "Run shell/build/test/diff command; if >35 lines, stores full output as handle h:ID and returns extracted errors/summary.",
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
            },
            {
                "name": "remember",
                "description": "Append decision, rule, session handoff, or backlog item to Markdown (.lee/.notes/ or global vault) with [[wikilinks]] & BGE index.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic": { "type": "string", "description": "Short title/topic." },
                        "content": { "type": "string", "description": "Rule, decision, handoff state, or backlog task (supports [[wikilinks]])." },
                        "tags": { "type": "string", "description": "Comma-separated tags." },
                        "scope": { "type": "string", "description": "'project' (default), 'global', 'handoff', or 'backlog'.", "default": "project" }
                    },
                    "required": ["topic", "content"]
                }
            },
            {
                "name": "recall_memory",
                "description": "Hybrid FTS5 BM25 + BGE-Small (cos_floor=0.53, RRF) recall over project/global/handoff/backlog memories + 1-hop [[wikilinks]].",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Context keywords or topic to recall." },
                        "budget_tokens": { "type": "integer", "description": "Max token budget (default: 600).", "default": 600 }
                    },
                    "required": ["query"]
                }
            }
        ]
    })
}
