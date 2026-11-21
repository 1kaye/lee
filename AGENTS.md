<!-- lee:start -->
# AI Agent Operational Rules (`lee` — Locate Exact Elements)

This workspace uses **`lee`**, a local token-saving MCP Server & CLI Toolkit (60%–85% token reduction). All AI agents (**Antigravity CLI `agy`**, **Claude Code**, and subagents) **must** prefer `lee` MCP tools and `lee` CLI commands over raw file reads, `rg`/`grep`, or unfiltered build logs.

---

## 1. `lee` MCP Tools (Primary Agent Interface)

1. **Cold Start & Onboarding (`project_map` + `recall_memory`):**
   - Call **`project_map()`** first to get PageRank-prioritized files, AST symbols, `#` headings, and `[start..end]` ranges (pass `subpath="src/"` to expand overflow directories).
   - Call **`recall_memory(query="...")`** to load architectural decisions, UI/code conventions, session `handoff`, and `backlog`. Never read dozens of raw files blindly.
2. **Precision Code & Config Reading (`outline` + `read_symbol` + `find_references`):**
   - Call **`outline(file="...")`** to inspect AST symbols, `#` headings, or `.yml`/`.toml`/`.json` keys with `[start_line..end_line]` (`mod tests` collapsed unless `include_tests=true`).
   - Call **`read_symbol(file="...", symbol="...")`** (or `start_line`..`end_line`) to read only the exact slice needed (supports fuzzy subsequence/bigram symbol matching).
   - To **find and read a function/struct without knowing its file**, omit `file`: **`read_symbol(symbol="fn_name")`**.
   - Use **`strip_comments=true`** to remove non-doc comments, **`signatures_only=true`** to keep imports/types/signatures while eliding function bodies via AST spans, and **`max_lines`** for pagination.
   - Before refactoring a function signature, struct, or enum, call **`find_references(symbol="...")`** to list every AST reference (`call`, `type_use`, `import`), enclosing caller function, and 2-hop caller impact across the workspace.
3. **Reading & Searching `.md` / `.mdx` & Code Documentation (`search_docs`):**
   - Never read long `.md`/`.mdx` or reference docs files in full.
   - Call **`search_docs(query="...", doc="...", scope="...")`** for column-weighted SQLite FTS5 BM25 search (with CamelCase/snake_case subword & prefix matching) across workspace & global docs, or use `outline(file="doc.md")` + `read_symbol(file="doc.md", symbol="Section Heading")`.
4. **Context-Aware Search Replacing `rg` / `grep` (`smart_grep`):**
   - List matching files (with automatic directory prefix folding): **`smart_grep(pattern="...", path="src/", files_only=true)`**.
   - Search lines grouped by file with enclosing AST symbol or Markdown heading: **`smart_grep(pattern="...", path="src/")`**.
5. **Token-Saving Build, Test & Diff Execution (`run_digest` + `recall`):**
   - Run noisy commands (`cargo check`, `cargo test`, `cargo clippy`, `vitest`, `jest`, `tsc`, `pytest`, `go test`, `git status`, `git log`, `git diff`, `kaye diff`) through **`run_digest(command="...")`** (ANSI stripped, `cargo clippy` grouped by lint rule, `--nocapture` preserved, `git status`/`log` compacted, logs > 35 lines stored as `h:ID`).
   - Pull details only when needed via **`recall(key="h:ID", item="TEST_OR_FILE")`** or **`recall(key="h:ID", failures_only=true)`**.
6. **Persistent Memory & `[[wikilinks]]` (`remember`):**
   - Save decisions immediately via **`remember(topic="...", content="...", tags="...", scope="...")`** (`[[topic-slug]]` links related notes):
     - `scope="project"` (default) -> `.lee/.notes/`
     - `scope="global"` -> cross-project vault `~/.config/lee/vault/`
     - `scope="handoff"` -> session progress & next steps (`.lee/.notes/handoff.md`)
     - `scope="backlog"` -> deferred work items (`.lee/.notes/backlog.md`)

---

## 2. `lee` CLI Commands (Shell Usage)

When operating from the terminal or managing docs/hooks:
- **`lee exec <cmd...>`** — Run any shell command through the `run_digest` compressor (also used automatically by the `PreToolUse` hook).
- **`lee init [--global]`** — Initialize `.lee/`, `.lee/.notes/`, `PreToolUse` hook plugin (`.agents/plugins/lee/`), and SQLite FTS5 DB.
- **`lee add-docs <path> [--w|--g] [--name <name>]`** / **`lee rm-docs [name|path|number|--all] [--w|--g]`** — Register or remove named workspace (`--w`) or global (`--g`) docs directories.
- **`lee docs [--json]`** / **`lee docs update`** — List registered workspace & global docs, or incrementally update only docs whose files changed/added/removed (unchanged docs are skipped).
- **`lee index`** / **`lee status`** — Re-index workspace + registered docs in parallel or inspect index status.
- **`lee undo [-y]`** — Remove generated config/DB/hook files while preserving `.lee/.notes/` if non-empty.
<!-- lee:end -->
