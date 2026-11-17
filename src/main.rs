pub mod config;
pub mod db;
pub mod digest;
pub mod hooks;
pub mod indexer;
pub mod mcp;
pub mod memory;
pub mod security;

use crate::config::AppPaths;
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
    /// Register an external code/documentation directory or file for read-only indexing (search_docs, smart_grep, outline, read_symbol)
    AddSource {
        /// Path to the external directory or file
        path: PathBuf,
    },
    /// Remove a registered external source (or `--all`) from config and SQLite index
    #[command(visible_alias = "rm-source")]
    RemoveSource {
        /// Path or 1-based number of the external source to remove
        path: Option<PathBuf>,
        /// Remove all registered external sources
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
        Commands::AddSource { path } => cmd_add_source(&paths, &path),
        Commands::RemoveSource { path, all } => cmd_remove_source(&paths, path.as_deref(), all),
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
        "[lee] Indexed {} files ({} code symbols, {} md headings, {} doc sections) in {} ms.",
        stats.files_scanned,
        stats.code_symbols,
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

fn cmd_add_source(paths: &AppPaths, source_path: &std::path::Path) -> Result<()> {
    paths.ensure_local_dirs()?;
    let canonical = paths.add_source(source_path)?;
    println!(
        "[lee] Registered external documentation source: {}",
        canonical.display()
    );

    let mut conn = init_connection(&paths.local_db)?;
    let stats = index_workspace(&mut conn, paths, None)?;
    println!(
        "[lee] Re-indexed workspace + sources: {} files, {} code symbols, {} md headings, {} doc sections ({} ms).",
        stats.files_scanned,
        stats.code_symbols,
        stats.md_headings,
        stats.doc_sections,
        stats.elapsed_ms
    );
    Ok(())
}

fn cmd_remove_source(
    paths: &AppPaths,
    source_path: Option<&std::path::Path>,
    all: bool,
) -> Result<()> {
    use std::io::Write;
    paths.ensure_local_dirs()?;
    let conn = init_connection(&paths.local_db)?;

    if all {
        let removed_list = paths.clear_sources()?;
        let mut total_purged = 0usize;
        for src in &removed_list {
            total_purged += purge_source_from_db(&conn, src)?;
        }
        println!(
            "[lee] Removed all {} external source(s) and purged {} indexed file(s) from SQLite.",
            removed_list.len(),
            total_purged
        );
        return Ok(());
    }

    let cfg = paths.load_config().unwrap_or_default();

    // If no argument provided, show numbered list and interactive selection prompt
    let target_buf: PathBuf = match source_path {
        Some(arg) => {
            // Also allow passing 1-based number directly: `lee rm-source 1`
            if let Some(num_str) = arg.to_str() {
                if let Ok(idx) = num_str.parse::<usize>() {
                    if idx >= 1 && idx <= cfg.sources.len() {
                        cfg.sources[idx - 1].clone()
                    } else {
                        anyhow::bail!(
                            "Invalid source number {idx} (registered sources: 1..={}).",
                            cfg.sources.len()
                        );
                    }
                } else {
                    arg.to_path_buf()
                }
            } else {
                arg.to_path_buf()
            }
        }
        None => {
            if cfg.sources.is_empty() {
                println!("[lee] No external documentation sources registered.");
                return Ok(());
            }
            println!("Registered External Documentation Sources:");
            for (i, src) in cfg.sources.iter().enumerate() {
                println!("  [{}] {}", i + 1, src.display());
            }
            println!("  [a] Remove all sources");
            println!("  [q] Cancel");
            print!("\nSelect source to remove [1-{}/a/q]: ", cfg.sources.len());
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let choice = input.trim();

            if choice.is_empty() || choice.eq_ignore_ascii_case("q") {
                println!("[lee] Cancelled.");
                return Ok(());
            }
            if choice.eq_ignore_ascii_case("a") || choice.eq_ignore_ascii_case("all") {
                let removed_list = paths.clear_sources()?;
                let mut total_purged = 0usize;
                for src in &removed_list {
                    total_purged += purge_source_from_db(&conn, src)?;
                }
                println!(
                    "[lee] Removed all {} external source(s) and purged {} indexed file(s) from SQLite.",
                    removed_list.len(),
                    total_purged
                );
                return Ok(());
            }
            if let Ok(idx) = choice.parse::<usize>() {
                if idx >= 1 && idx <= cfg.sources.len() {
                    cfg.sources[idx - 1].clone()
                } else {
                    anyhow::bail!(
                        "Invalid selection '{choice}'. Expected 1..={}.",
                        cfg.sources.len()
                    );
                }
            } else {
                PathBuf::from(choice)
            }
        }
    };

    if let Some(removed) = paths.remove_source(&target_buf)? {
        let purged = purge_source_from_db(&conn, &removed)?;
        println!(
            "[lee] Removed external source '{}' and purged {} indexed file(s) from SQLite.",
            removed.display(),
            purged
        );
    } else {
        let purged = purge_source_from_db(&conn, &target_buf)?;
        println!(
            "[lee] Source '{}' was not in config.json (purged {} matching DB entries).",
            target_buf.display(),
            purged
        );
    }
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
        "[lee] Indexed {} files ({} re-indexed, {} cached) | {} code symbols | {} md headings | {} doc sections in {} ms.",
        stats.files_scanned,
        stats.files_reindexed,
        stats.files_cached,
        stats.code_symbols,
        stats.md_headings,
        stats.doc_sections,
        stats.elapsed_ms
    );
    Ok(())
}

fn cmd_status(paths: &AppPaths) -> Result<()> {
    let cfg = paths.load_config().unwrap_or_default();
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

    if cfg.sources.is_empty() {
        println!("Ext Sources  : (none)");
    } else {
        println!("Ext Sources  : {} registered", cfg.sources.len());
        for src in &cfg.sources {
            println!("  - {}", src.display());
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
            "Index Counts : {files} files | {syms} code symbols | {headings} md headings | {sections} doc windows | {memories} project memories | {handles} handles"
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
         MCP Tool Schema Overhead : {schema_bytes} bytes (~{schema_tokens} tokens for 9 tools)\n\
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
