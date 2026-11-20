pub mod config;
pub mod db;
pub mod digest;
pub mod hooks;
pub mod indexer;
pub mod mcp;
pub mod memory;
pub mod security;

use crate::config::{default_doc_name, AppPaths, DocEntry, DocScope};
use crate::db::{
    init_connection, query_daily_token_metrics, query_recent_token_history, query_token_metrics,
    record_token_metric,
};
use crate::digest::{digest_raw_output, run_command_digest_with_exit};
use crate::hooks::{
    auto_sync_agents_md, discover_missed_savings, install_antigravity_hook_plugin,
    process_antigravity_hook, process_claude_hook,
};
use crate::indexer::{index_workspace, purge_source_from_db};
use crate::mcp::protocol::mcp_schema_overhead_bytes;
use crate::mcp::run_stdio_server;
use crate::memory::embedder::{download_and_verify_model, is_model_cached};
use crate::memory::{
    backfill_missing_memory_embeddings, list_backlog_and_handoff, recall_memories_hybrid,
};
use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub const AGENTS_MD_RULES: &str = include_str!("../AGENTS.md");

#[derive(Parser)]
#[command(
    name = "lee",
    version,
    about = "Locate Exact Elements — Local Token-Saving MCP Server & CLI Toolkit"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the JSON-RPC 2.0 MCP server over stdin/stdout
    Serve,
    /// Initialize .lee/, .lee/.notes/, PreToolUse hook, SQLite schemas, and BAAI/bge-small-en-v1.5 model cache
    Init {
        /// Also initialize global vault, database, and hook in ~/.config/lee & ~/.gemini/config/plugins/lee
        #[arg(long)]
        global: bool,
        /// Skip downloading the BGE-Small ONNX model (uses FTS5 BM25 lexical fallback until downloaded)
        #[arg(long)]
        skip_model: bool,
    },
    /// Execute a shell command and compress its output via `run_digest` (used by PreToolUse hooks or CLI)
    Exec {
        /// Command string or arguments to execute and digest
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// Register a named external code/documentation directory or file (`--w` workspace, `--g` global)
    #[command(alias = "add-source")]
    AddDocs {
        /// Path to the documentation or code directory/file
        path: PathBuf,
        /// Custom name for this docs entry (prompts interactively if omitted)
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// Register in current workspace (.lee/config.json, default)
        #[arg(short = 'w', long = "workspace", visible_alias = "w", conflicts_with = "global")]
        workspace: bool,
        /// Register globally (~/.config/lee/config.json, available in all workspaces)
        #[arg(short = 'g', long = "global", visible_alias = "g", conflicts_with = "workspace")]
        global: bool,
    },
    /// List registered workspace and global docs (use `--json` for LLM/machine output)
    Docs {
        /// Output machine-readable JSON (for LLMs and scripts)
        #[arg(long)]
        json: bool,
    },
    /// Remove a registered workspace or global doc (or `--all`) from config and SQLite index
    #[command(visible_alias = "rm-docs", alias = "rm-source", alias = "remove-source")]
    RemoveDocs {
        /// Name, path, or 1-based number of the doc to remove
        target: Option<String>,
        /// Target workspace docs only
        #[arg(short = 'w', long = "workspace", visible_alias = "w", conflicts_with = "global")]
        workspace: bool,
        /// Target global docs only
        #[arg(short = 'g', long = "global", visible_alias = "g", conflicts_with = "workspace")]
        global: bool,
        /// Remove all registered docs in the target scope (workspace by default, or global with `--g`)
        #[arg(long)]
        all: bool,
    },
    /// Scan and index all project source code and Markdown documentation files in parallel
    Index,
    /// Show current workspace index status, model cache status, and registered external sources
    Status,
    /// Display token-saving audit table and net token report (supports `--daily` and `--history`)
    Stats {
        /// Show daily token savings breakdown with ASCII bar chart
        #[arg(long)]
        daily: bool,
        /// Show recent individual tool calls and saved tokens
        #[arg(long)]
        history: bool,
    },
    /// Scan AI session transcripts (.jsonl) to find bypassed raw commands/file reads and missed token savings
    Discover {
        /// Optional path to a transcript .jsonl file or directory (defaults to Antigravity & Claude Code logs)
        path: Option<PathBuf>,
    },
    /// List latest Session Handoffs and aging Backlog work items from .lee/.notes/
    Backlog,
    /// Revert configuration & index files created by `init` without deleting .lee/.notes/
    Undo {
        /// Skip `[y/N]` confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// Lifecycle hook interceptor (`antigravity`, `claude`, `prompt`, or `digest`)
    Hook {
        /// Hook event mode: `antigravity`, `claude`, `prompt`, or `digest`
        #[arg(value_enum)]
        event: HookEvent,
        /// Optional query string for `hook prompt`
        query: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HookEvent {
    Prompt,
    Digest,
    Antigravity,
    Claude,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir()?;
    let paths = AppPaths::discover(&cwd)?;

    if !matches!(cli.command, Commands::Undo { .. }) {
        let _ = auto_sync_agents_md(&paths, AGENTS_MD_RULES);
    }

    match cli.command {
        Commands::Serve => run_stdio_server(&paths),
        Commands::Init { global, skip_model } => cmd_init(&paths, global, skip_model),
        Commands::Exec { command } => cmd_exec(&paths, &command),
        Commands::AddDocs {
            path,
            name,
            workspace,
            global,
        } => {
            let _ = workspace; // default scope when neither --w nor --g is set
            let scope = if global {
                DocScope::Global
            } else {
                DocScope::Workspace
            };
            cmd_add_docs(&paths, &path, name.as_deref(), scope)
        }
        Commands::Docs { json } => cmd_docs(&paths, json),
        Commands::RemoveDocs {
            target,
            workspace,
            global,
            all,
        } => {
            let scope_filter = if global {
                Some(DocScope::Global)
            } else if workspace {
                Some(DocScope::Workspace)
            } else {
                None
            };
            cmd_remove_docs(&paths, target.as_deref(), scope_filter, all)
        }
        Commands::Index => cmd_index(&paths),
        Commands::Status => cmd_status(&paths),
        Commands::Stats { daily, history } => cmd_stats(&paths, daily, history),
        Commands::Discover { path } => cmd_discover(path.as_deref()),
        Commands::Backlog => cmd_backlog(&paths),
        Commands::Undo { yes } => cmd_undo(&paths, yes),
        Commands::Hook { event, query } => cmd_hook(&paths, event, query.as_deref()),
    }
}

fn cmd_init(paths: &AppPaths, global: bool, skip_model: bool) -> Result<()> {
    paths.ensure_local_dirs()?;
    paths.ensure_global_dirs()?;
    paths.ensure_gitignore(&[".lee/", ".agents/plugins/lee/"])?;

    let cfg = paths.load_config().unwrap_or_default();

    // Remove redundant local `AGENTS.md`; active agent rules are installed in
    // `.agents/plugins/lee/rules/AGENTS.md` (and `~/.gemini/config/plugins/lee/rules/AGENTS.md` for `--global`)
    // so we never duplicate rules or pollute a project's root `./AGENTS.md`.
    let legacy_local_agents = paths.local_dir.join("AGENTS.md");
    if legacy_local_agents.exists() {
        let _ = fs::remove_file(&legacy_local_agents);
    }

    if global {
        let global_agents_md = paths.global_dir.join("AGENTS.md");
        fs::write(&global_agents_md, AGENTS_MD_RULES.trim_start())?;
    }
    let _ = auto_sync_agents_md(paths, AGENTS_MD_RULES);
    paths.save_config(&cfg)?;

    let mut local_conn = init_connection(&paths.local_db)?;
    let global_conn = if global || !paths.global_db.exists() {
        Some(init_connection(&paths.global_db)?)
    } else {
        None
    };

    println!(
        "[lee] Initialized local workspace at {}",
        paths.local_dir.display()
    );
    println!(
        "[lee] Notes directory ready at {}",
        paths.notes_dir.display()
    );
    println!(
        "[lee] Global directory ready at {}",
        paths.global_dir.display()
    );

    if !skip_model {
        println!(
            "[lee] Verifying / downloading BAAI/bge-small-en-v1.5 (BGESmallENV15Q) in {}...",
            paths.models_dir.display()
        );
        match download_and_verify_model(&paths.models_dir, true) {
            Ok(()) => {
                println!(
                    "[lee] Model BGESmallENV15Q (384-dim) ready for offline inference."
                );
                let _ = backfill_missing_memory_embeddings(&local_conn, &paths.models_dir);
                if let Some(ref gconn) = global_conn {
                    let _ = backfill_missing_memory_embeddings(gconn, &paths.models_dir);
                }
            }
            Err(e) => eprintln!(
                "[lee] Warning: could not download BGE model now ({e}). Hybrid search will gracefully fallback to SQLite FTS5 BM25 until online."
            ),
        }
    }

    let stats = index_workspace(&mut local_conn, paths, None)?;
    println!(
        "[lee] Indexed {} files ({} code symbols, {} symbol refs, {} md headings, {} doc sections) in {} ms.",
        stats.files_scanned,
        stats.code_symbols,
        stats.symbol_refs,
        stats.md_headings,
        stats.doc_sections,
        stats.elapsed_ms
    );

    let plugin_dir =
        install_antigravity_hook_plugin(&paths.project_root, global, AGENTS_MD_RULES)?;
    println!(
        "[lee] Installed Antigravity PreToolUse hook plugin at {}",
        plugin_dir.display()
    );

    Ok(())
}

fn cmd_exec(paths: &AppPaths, command_parts: &[String]) -> Result<()> {
    paths.ensure_local_dirs()?;
    let conn = init_connection(&paths.local_db)?;
    let full_cmd = command_parts.join(" ");
    let (out, raw_bytes, exit_code) =
        run_command_digest_with_exit(&conn, paths, &full_cmd, None)?;
    let _ = record_token_metric(&conn, "exec", raw_bytes, out.len());
    print!("{out}");
    if !out.ends_with('\n') {
        println!();
    }
    if exit_code != 0 && exit_code > 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn cmd_discover(custom_path: Option<&std::path::Path>) -> Result<()> {
    let report = discover_missed_savings(custom_path)?;
    print!("{report}");
    Ok(())
}

fn cmd_add_docs(
    paths: &AppPaths,
    source_path: &std::path::Path,
    name: Option<&str>,
    scope: DocScope,
) -> Result<()> {
    let canonical = fs::canonicalize(source_path)
        .map_err(|e| anyhow::anyhow!("Docs path does not exist: {} ({e})", source_path.display()))?;

    let final_name = match name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(n) => n.to_string(),
        None => {
            let default_name = default_doc_name(&canonical);
            print!("Enter doc name [default: {default_name}]: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let chosen = input.trim();
            if chosen.is_empty() {
                default_name
            } else {
                chosen.to_string()
            }
        }
    };

    match scope {
        DocScope::Workspace => paths.ensure_local_dirs()?,
        DocScope::Global => paths.ensure_global_dirs()?,
    }

    let entry = paths.add_doc(scope, &canonical, &final_name)?;
    println!(
        "[lee] Registered {} doc '{}' -> {}",
        scope.as_str(),
        entry.name,
        entry.path.display()
    );

    if scope == DocScope::Workspace || paths.local_db.exists() {
        let mut conn = init_connection(&paths.local_db)?;
        let stats = index_workspace(&mut conn, paths, None)?;
        println!(
            "[lee] Re-indexed workspace + docs: {} files, {} code symbols, {} symbol refs, {} md headings, {} doc sections ({} ms).",
            stats.files_scanned,
            stats.code_symbols,
            stats.symbol_refs,
            stats.md_headings,
            stats.doc_sections,
            stats.elapsed_ms
        );
    } else {
        println!(
            "[lee] Saved globally in {}. It will be indexed automatically in any workspace on next query or `lee index`.",
            paths.global_config_file.display()
        );
    }
    Ok(())
}

fn cmd_docs(paths: &AppPaths, as_json: bool) -> Result<()> {
    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;

    if as_json {
        let payload = serde_json::json!({
            "workspace": ws_docs,
            "global": global_docs,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    println!("Workspace Docs ({}):", ws_docs.len());
    if ws_docs.is_empty() {
        println!("  (none)");
    } else {
        for (i, doc) in ws_docs.iter().enumerate() {
            println!("  [{}] {} -> {}", i + 1, doc.name, doc.path.display());
        }
    }

    println!("\nGlobal Docs ({}):", global_docs.len());
    if global_docs.is_empty() {
        println!("  (none)");
    } else {
        for (i, doc) in global_docs.iter().enumerate() {
            println!("  [{}] {} -> {}", i + 1, doc.name, doc.path.display());
        }
    }
    Ok(())
}

fn purge_removed_docs_if_unused(paths: &AppPaths, removed: &[DocEntry]) -> Result<usize> {
    if !paths.local_db.exists() {
        return Ok(0);
    }
    let remaining = paths.all_doc_paths();
    let conn = init_connection(&paths.local_db)?;
    let mut total = 0usize;
    for entry in removed {
        if !remaining.contains(&entry.path) {
            total += purge_source_from_db(&conn, &entry.path)?;
        }
    }
    Ok(total)
}

fn cmd_remove_docs(
    paths: &AppPaths,
    target: Option<&str>,
    scope_filter: Option<DocScope>,
    all: bool,
) -> Result<()> {
    if all {
        let scope = scope_filter.unwrap_or(DocScope::Workspace);
        let removed_list = paths.clear_docs(scope)?;
        let total_purged = purge_removed_docs_if_unused(paths, &removed_list)?;
        println!(
            "[lee] Removed all {} {} doc(s) and purged {} indexed file(s) from SQLite.",
            removed_list.len(),
            scope.as_str(),
            total_purged
        );
        return Ok(());
    }

    let mut entries: Vec<(DocScope, DocEntry)> = Vec::new();
    if scope_filter.is_none() || scope_filter == Some(DocScope::Workspace) {
        for d in paths.load_config().unwrap_or_default().docs {
            entries.push((DocScope::Workspace, d));
        }
    }
    if scope_filter.is_none() || scope_filter == Some(DocScope::Global) {
        for d in paths.load_global_config().unwrap_or_default().docs {
            entries.push((DocScope::Global, d));
        }
    }

    let chosen: (Option<DocScope>, String) = match target {
        Some(arg) => {
            if let Ok(idx) = arg.trim().parse::<usize>() {
                if idx >= 1 && idx <= entries.len() {
                    let (sc, ref entry) = entries[idx - 1];
                    (Some(sc), entry.path.to_string_lossy().to_string())
                } else {
                    anyhow::bail!(
                        "Invalid doc number {idx} (registered docs: 1..={}).",
                        entries.len()
                    );
                }
            } else {
                (scope_filter, arg.to_string())
            }
        }
        None => {
            if entries.is_empty() {
                println!("[lee] No docs registered.");
                return Ok(());
            }
            println!("Registered Docs:");
            for (i, (sc, doc)) in entries.iter().enumerate() {
                println!(
                    "  [{}] [{}] {} -> {}",
                    i + 1,
                    sc.as_str(),
                    doc.name,
                    doc.path.display()
                );
            }
            let clear_scope = scope_filter.unwrap_or(DocScope::Workspace);
            println!("  [a] Remove all {} docs", clear_scope.as_str());
            println!("  [q] Cancel");
            print!("\nSelect doc to remove [1-{}/a/q]: ", entries.len());
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let choice = input.trim();

            if choice.is_empty() || choice.eq_ignore_ascii_case("q") {
                println!("[lee] Cancelled.");
                return Ok(());
            }
            if choice.eq_ignore_ascii_case("a") || choice.eq_ignore_ascii_case("all") {
                let removed_list = paths.clear_docs(clear_scope)?;
                let total_purged = purge_removed_docs_if_unused(paths, &removed_list)?;
                println!(
                    "[lee] Removed all {} {} doc(s) and purged {} indexed file(s) from SQLite.",
                    removed_list.len(),
                    clear_scope.as_str(),
                    total_purged
                );
                return Ok(());
            }
            if let Ok(idx) = choice.parse::<usize>() {
                if idx >= 1 && idx <= entries.len() {
                    let (sc, ref entry) = entries[idx - 1];
                    (Some(sc), entry.path.to_string_lossy().to_string())
                } else {
                    anyhow::bail!(
                        "Invalid selection '{choice}'. Expected 1..={}.",
                        entries.len()
                    );
                }
            } else {
                (scope_filter, choice.to_string())
            }
        }
    };

    let scopes_to_try: Vec<DocScope> = match chosen.0 {
        Some(sc) => vec![sc],
        None => vec![DocScope::Workspace, DocScope::Global],
    };

    for sc in scopes_to_try {
        if let Some(removed) = paths.remove_doc(sc, &chosen.1)? {
            let purged = purge_removed_docs_if_unused(paths, std::slice::from_ref(&removed))?;
            println!(
                "[lee] Removed {} doc '{}' ({}) and purged {} indexed file(s) from SQLite.",
                sc.as_str(),
                removed.name,
                removed.path.display(),
                purged
            );
            return Ok(());
        }
    }

    let purged = if paths.local_db.exists() {
        let conn = init_connection(&paths.local_db)?;
        purge_source_from_db(&conn, std::path::Path::new(&chosen.1))?
    } else {
        0
    };
    println!(
        "[lee] Doc '{}' was not found in config (purged {} matching DB entries).",
        chosen.1, purged
    );
    Ok(())
}

fn cmd_index(paths: &AppPaths) -> Result<()> {
    paths.ensure_local_dirs()?;
    let mut conn = init_connection(&paths.local_db)?;
    let _ = backfill_missing_memory_embeddings(&conn, &paths.models_dir);
    if paths.global_db.exists() {
        if let Ok(gconn) = init_connection(&paths.global_db) {
            let _ = backfill_missing_memory_embeddings(&gconn, &paths.models_dir);
        }
    }
    let stats = index_workspace(&mut conn, paths, None)?;
    println!(
        "[lee] Indexed {} files ({} re-indexed, {} cached) | {} code symbols | {} symbol refs | {} md headings | {} doc sections in {} ms.",
        stats.files_scanned,
        stats.files_reindexed,
        stats.files_cached,
        stats.code_symbols,
        stats.symbol_refs,
        stats.md_headings,
        stats.doc_sections,
        stats.elapsed_ms
    );
    Ok(())
}

fn cmd_status(paths: &AppPaths) -> Result<()> {
    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;
    let model_ready = is_model_cached(&paths.models_dir);

    println!("=== lee Status ===");
    println!("Project Root : {}", paths.project_root.display());
    println!(
        "Local DB     : {} ({})",
        paths.local_db.display(),
        if paths.local_db.exists() {
            "initialized"
        } else {
            "not initialized"
        }
    );
    println!("Global Vault : {}", paths.vault_dir.display());
    println!(
        "BGE Model    : {} ({})",
        paths.models_dir.display(),
        if model_ready {
            "BGESmallENV15Q cached & offline-ready"
        } else {
            "not cached (using FTS5 BM25 fallback)"
        }
    );

    if ws_docs.is_empty() {
        println!("Workspace Docs: (none)");
    } else {
        println!("Workspace Docs: {} registered", ws_docs.len());
        for doc in &ws_docs {
            println!("  - {} -> {}", doc.name, doc.path.display());
        }
    }

    if global_docs.is_empty() {
        println!("Global Docs  : (none)");
    } else {
        println!("Global Docs  : {} registered", global_docs.len());
        for doc in &global_docs {
            println!("  - {} -> {}", doc.name, doc.path.display());
        }
    }

    if paths.local_db.exists() {
        let conn = init_connection(&paths.local_db)?;
        let files: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_hashes", [], |r| r.get(0))
            .unwrap_or(0);
        let syms: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE kind != 'md_heading'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let sym_refs: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbol_refs", [], |r| r.get(0))
            .unwrap_or(0);
        let headings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE kind = 'md_heading'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let sections: i64 = conn
            .query_row("SELECT COUNT(*) FROM doc_sections_meta", [], |r| r.get(0))
            .unwrap_or(0);
        let handles: i64 = conn
            .query_row("SELECT COUNT(*) FROM handles", [], |r| r.get(0))
            .unwrap_or(0);
        let memories: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories_meta", [], |r| r.get(0))
            .unwrap_or(0);
        println!(
            "Index Counts : {files} files | {syms} code symbols | {sym_refs} symbol refs | {headings} md headings | {sections} doc windows | {memories} project memories | {handles} handles"
        );
    }

    Ok(())
}

fn cmd_stats(paths: &AppPaths, daily: bool, history: bool) -> Result<()> {
    if !paths.local_db.exists() {
        println!(
            "[lee] No database found at {}. Run `lee init` first.",
            paths.local_db.display()
        );
        return Ok(());
    }
    let conn = init_connection(&paths.local_db)?;
    let rows = query_token_metrics(&conn)?;

    println!(
        "{:<16} | {:>8} | {:>12} | {:>12} | {:>10} | {:>14}",
        "Tool Name", "Calls", "Raw Bytes", "Sent Bytes", "Saved %", "Saved Tokens"
    );
    println!("{}", "-".repeat(86));

    let mut grand_calls = 0i64;
    let mut grand_raw = 0i64;
    let mut grand_sent = 0i64;
    let mut grand_saved = 0i64;

    for r in &rows {
        grand_calls += r.calls;
        grand_raw += r.raw_bytes;
        grand_sent += r.sent_bytes;
        grand_saved += r.saved_bytes;

        let pct = if r.raw_bytes > 0 {
            (r.saved_bytes as f64 / r.raw_bytes as f64) * 100.0
        } else {
            0.0
        };
        let saved_tokens = r.saved_bytes / 4;
        println!(
            "{:<16} | {:>8} | {:>12} | {:>12} | {:>9.1}% | {:>14}",
            r.tool_name, r.calls, r.raw_bytes, r.sent_bytes, pct, saved_tokens
        );
    }

    println!("{}", "-".repeat(86));
    let total_pct = if grand_raw > 0 {
        (grand_saved as f64 / grand_raw as f64) * 100.0
    } else {
        0.0
    };
    let gross_saved_tokens = grand_saved / 4;
    let schema_bytes = mcp_schema_overhead_bytes() as i64;
    let schema_tokens = schema_bytes / 4;
    let net_saved_tokens = gross_saved_tokens - schema_tokens;
    let ratio = if grand_sent + schema_bytes > 0 {
        grand_raw as f64 / (grand_sent + schema_bytes) as f64
    } else {
        1.0
    };

    println!(
        "{:<16} | {:>8} | {:>12} | {:>12} | {:>9.1}% | {:>14}",
        "GROSS TOTAL", grand_calls, grand_raw, grand_sent, total_pct, gross_saved_tokens
    );
    println!(
        "\n--- Net Token Attribution ---\n\
         MCP Tool Schema Overhead : {schema_bytes} bytes (~{schema_tokens} tokens for 10 tools)\n\
         Gross Tokens Saved       : {gross_saved_tokens} tokens\n\
         Net Tokens Saved         : {net_saved_tokens} tokens ({ratio:.1}x net compression ratio)"
    );

    if daily {
        let daily_rows = query_daily_token_metrics(&conn)?;
        let max_saved = daily_rows
            .iter()
            .map(|d| d.saved_bytes / 4)
            .max()
            .unwrap_or(1)
            .max(1);
        println!(
            "\n--- Daily Savings Breakdown (Last {} Day(s)) ---",
            daily_rows.len()
        );
        println!(
            "{:<12} | {:>6} | {:>12} | {:>9} | Graph",
            "Date", "Calls", "Saved Tokens", "Saved %"
        );
        println!("{}", "-".repeat(78));
        for d in &daily_rows {
            let saved_tok = d.saved_bytes / 4;
            let pct = if d.raw_bytes > 0 {
                (d.saved_bytes as f64 / d.raw_bytes as f64) * 100.0
            } else {
                0.0
            };
            let bar_len = ((saved_tok as f64 / max_saved as f64) * 24.0).round() as usize;
            let bar = "█".repeat(bar_len.max(usize::from(saved_tok > 0)));
            println!(
                "{:<12} | {:>6} | {:>12} | {:>8.1}% | {bar}",
                d.day, d.calls, saved_tok, pct
            );
        }
    }

    if history {
        let hist = query_recent_token_history(&conn, 25)?;
        println!(
            "\n--- Recent Tool Call History (Last {} Call(s)) ---",
            hist.len()
        );
        println!(
            "{:<19} | {:<15} | {:>10} | {:>10} | {:>12}",
            "Timestamp", "Tool", "Raw Bytes", "Sent Bytes", "Saved Tokens"
        );
        println!("{}", "-".repeat(78));
        for h in &hist {
            println!(
                "{:<19} | {:<15} | {:>10} | {:>10} | {:>12}",
                h.timestamp,
                h.tool_name,
                h.raw_bytes,
                h.sent_bytes,
                h.saved_bytes / 4
            );
        }
    }

    Ok(())
}

fn cmd_backlog(paths: &AppPaths) -> Result<()> {
    paths.ensure_local_dirs()?;
    let conn = init_connection(&paths.local_db)?;
    let report = list_backlog_and_handoff(&conn)?;
    print!("{report}");
    Ok(())
}

fn cmd_undo(paths: &AppPaths, yes: bool) -> Result<()> {
    let local_hook_plugin = paths.project_root.join(".agents/plugins/lee");
    if !paths.local_dir.exists() && !local_hook_plugin.exists() {
        println!("[lee] Nothing to undo in {}.", paths.project_root.display());
        return Ok(());
    }

    let has_notes = paths.notes_dir.is_dir()
        && fs::read_dir(&paths.notes_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);

    if !yes {
        println!(
            "[lee] This will remove lee index/config/hook files in {}:",
            paths.project_root.display()
        );
        if has_notes {
            println!(
                "  - Markdown notes in {} WILL BE PRESERVED.",
                paths.notes_dir.display()
            );
        } else {
            println!("  - .lee/ directory will be removed (no notes found).");
        }
        print!("Proceed with undo? [y/N]: ");
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("[lee] Undo cancelled.");
            return Ok(());
        }
    }

    // Remove generated config/DB/rule files inside .lee/ while preserving .lee/.notes/
    for file in [
        paths.local_db.clone(),
        paths.local_dir.join("lee.db-wal"),
        paths.local_dir.join("lee.db-shm"),
        paths.config_file.clone(),
        paths.local_dir.join("AGENTS.md"),
    ] {
        if file.exists() {
            let _ = fs::remove_file(&file);
        }
    }

    // Also remove local .agents/plugins/lee/ hook plugin installed by `lee init`
    if local_hook_plugin.exists() {
        let _ = fs::remove_dir_all(&local_hook_plugin);
    }

    // Only remove .lee/ itself if .notes/ has no user Markdown notes
    if !has_notes && paths.local_dir.exists() {
        fs::remove_dir_all(&paths.local_dir)?;
        println!(
            "[lee] Undo complete. Removed {} directory (no Markdown notes to preserve).",
            paths.local_dir.display()
        );
    } else if paths.local_dir.exists() {
        println!(
            "[lee] Undo complete. Removed index/config files in {} (preserved Markdown notes in {}).",
            paths.local_dir.display(),
            paths.notes_dir.display()
        );
    } else {
        println!("[lee] Undo complete.");
    }
    Ok(())
}

fn cmd_hook(paths: &AppPaths, event: HookEvent, query: Option<&str>) -> Result<()> {
    match event {
        HookEvent::Antigravity => {
            let mut raw = String::new();
            let _ = io::stdin().read_to_string(&mut raw);
            let out = process_antigravity_hook(&raw);
            let _ = writeln!(io::stdout(), "{out}");
            return Ok(());
        }
        HookEvent::Claude => {
            let mut raw = String::new();
            let _ = io::stdin().read_to_string(&mut raw);
            if let Some(out) = process_claude_hook(&raw) {
                let _ = writeln!(io::stdout(), "{out}");
            }
            return Ok(());
        }
        HookEvent::Prompt | HookEvent::Digest => {}
    }

    paths.ensure_local_dirs()?;
    let conn = init_connection(&paths.local_db)?;

    match event {
        HookEvent::Prompt => {
            let q = match query {
                Some(s) if !s.trim().is_empty() => s.to_string(),
                _ => {
                    let mut buf = String::new();
                    let _ = io::stdin().read_to_string(&mut buf);
                    buf
                }
            };
            let q_trimmed = q.trim();
            if q_trimmed.is_empty() {
                return Ok(());
            }
            let (out, raw_bytes) = recall_memories_hybrid(&conn, paths, q_trimmed, 600)?;
            let _ = record_token_metric(&conn, "hook_prompt", raw_bytes, out.len());
            print!("{out}");
        }
        HookEvent::Digest => {
            let mut raw = String::new();
            io::stdin().read_to_string(&mut raw)?;
            if raw.trim().is_empty() {
                return Ok(());
            }
            let digest = digest_raw_output(&conn, "stdin_pipe", &raw, Some(0))?;
            let _ = record_token_metric(&conn, "hook_digest", raw.len(), digest.len());
            print!("{digest}");
        }
        HookEvent::Antigravity | HookEvent::Claude => unreachable!(),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_docs_flags_parsing() {
        let cli = Cli::try_parse_from(["lee", "add-docs", "--w", "/tmp/gpui"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::AddDocs { workspace: true, global: false, name: None, .. }
        ));

        let cli = Cli::try_parse_from([
            "lee", "add-docs", "--g", "/tmp/gpui", "--name", "gpui kit",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::AddDocs { workspace: false, global: true, name: Some(ref n), .. } if n == "gpui kit"
        ));

        let cli = Cli::try_parse_from(["lee", "docs", "--json"]).unwrap();
        assert!(matches!(cli.command, Commands::Docs { json: true }));

        let cli = Cli::try_parse_from(["lee", "rm-docs", "--g", "gpui kit"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::RemoveDocs { global: true, workspace: false, target: Some(ref t), .. } if t == "gpui kit"
        ));

        // Legacy alias still parses
        let cli = Cli::try_parse_from(["lee", "add-source", "/tmp/gpui"]).unwrap();
        assert!(matches!(cli.command, Commands::AddDocs { .. }));
    }
}
