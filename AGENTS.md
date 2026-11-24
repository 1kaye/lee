<!-- lee:start -->
# `lee` — Locate Exact Elements (Agent Rules)

Prefer `lee` MCP tools for large files, AST navigation, refactoring impact, docs search, and noisy build/test logs (for tiny files <100 lines, direct `read_file` is fine).

## MCP Tools (8)
- **`project_map(subpath?)`**: Cold-start PageRank-ranked file & symbol map with `[start..end]` ranges.
- **`outline(file)`**: AST symbol table, `#` headings, or `.toml`/`.yml`/`.json` keys (`mod tests` collapsed unless `include_tests=true`).
- **`read_symbol(file?, symbol?, symbols?, start_line?, end_line?)`**: Read exact function/struct/heading slices; omit `file` for workspace-wide fuzzy lookup, or pass `symbols=["fn_a", "fn_b"]` to batch-read in one call. Supports `strip_comments`, `signatures_only`, `max_lines`.
- **`find_references(symbol)`**: Call before refactoring to list AST `import`/`type_use`/`call` references, enclosing callers, and 2-hop caller impact.
- **`search_docs(query, doc?, scope?)`**: Column-weighted SQLite FTS5 BM25 search across `.md`/`.mdx` (fence-safe) and full multi-language code files in workspace/global docs.
- **`smart_grep(pattern, path?, glob?, files_only?, whole_word?)`**: Replaces `rg -l`/`rg -n`/`rg -w` with enclosing `[in fn ...]` / `# Heading` context.
- **`run_digest(command, cwd?)` + `recall(key, item?, failures_only?, filter?)`**: Compress noisy build/test/diff logs (>35 lines stored as `h:ID`; <=35 lines returned raw).

## CLI
- `lee init [--global]` | `lee index` | `lee status` | `lee undo [-y]`
- `lee add-docs <path> [--w|--g] [--name <name>]` | `lee rm-docs [target|--all] [--w|--g]`
- `lee docs [--json]` | `lee docs update` (incremental sync; skips unchanged docs)
- `lee exec <cmd...>` (digest wrapper used by PreToolUse hook)
<!-- lee:end -->
