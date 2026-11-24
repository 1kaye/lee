pub mod config;
pub mod db;
pub mod digest;
pub mod hooks;
pub mod indexer;
pub mod mcp;
pub mod memory;
pub mod security;

use crate::config::{default_doc_name, AppPaths, DocEntry, DocScope};
use crate::db::init_connection;
use crate::digest::run_command_digest_with_exit;
use crate::hooks::{
    auto_sync_agents_md, install_antigravity_hook_plugin, process_antigravity_hook,
    process_claude_hook,
};
use crate::indexer::{
    index_global_docs, index_workspace, is_unsafe_workspace_root, purge_source_from_db,
    update_registered_docs,
};
use crate::mcp::run_stdio_server;
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
    /// Initialize .lee/, PreToolUse hook, and SQLite FTS5 schemas
    Init {
        /// Also initialize global database and hook in ~/.config/lee & ~/.gemini/config/plugins/lee
        #[arg(long)]
        global: bool,
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
    /// List registered workspace & global docs (`--json`) or run `lee docs update`
    Docs {
        #[command(subcommand)]
        action: Option<DocsAction>,
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
    /// Show current workspace index status and registered docs
    Status,
    /// Revert configuration & index files created by `init` without deleting .lee/.notes/
    Undo {
        /// Skip `[y/N]` confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },
    /// PreToolUse hook interceptor (`antigravity` or `claude`)
    Hook {
        /// Hook event mode: `antigravity` or `claude`
        #[arg(value_enum)]
        event: HookEvent,
    },
}

#[derive(Subcommand)]
enum DocsAction {
    /// Incrementally update changed, new, or deleted files in workspace & global docs (skips unchanged docs)
    Update,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HookEvent {
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
        Commands::Init { global } => cmd_init(&paths, global),
        Commands::Exec { command } => cmd_exec(&paths, &command),
        Commands::AddDocs {
            path,
            name,
            workspace,
            global,
        } => {
            let _ = workspace;
            let scope = if global {
                DocScope::Global
            } else {
                DocScope::Workspace
            };
            cmd_add_docs(&paths, &path, name.as_deref(), scope)
        }
        Commands::Docs { action, json } => match action {
            Some(DocsAction::Update) => cmd_docs_update(&paths),
            None => cmd_docs(&paths, json),
        },
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
        Commands::Undo { yes } => cmd_undo(&paths, yes),
        Commands::Hook { event } => cmd_hook(event),
    }
}

fn cmd_init(paths: &AppPaths, global: bool) -> Result<()> {
    if is_unsafe_workspace_root(&paths.project_root) {
        anyhow::bail!(
            "Refusing to initialize lee workspace in '{}' (HOME or filesystem root). Run `lee init` inside a project directory.",
            paths.project_root.display()
        );
    }
    paths.ensure_local_dirs()?;
    paths.ensure_global_dirs()?;
    paths.ensure_gitignore(&[".lee/", ".agents/plugins/lee/"])?;

    let cfg = paths.load_config().unwrap_or_default();

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
    if global || !paths.global_db.exists() {
        let _ = init_connection(&paths.global_db)?;
    }

    println!(
        "[lee] Initialized local workspace at {}",
        paths.local_dir.display()
    );
    println!(
        "[lee] Global directory ready at {}",
        paths.global_dir.display()
    );

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
    let (out, _raw_bytes, exit_code) =
        run_command_digest_with_exit(&conn, paths, &full_cmd, None)?;
    print!("{out}");
    if !out.ends_with('\n') {
        println!();
    }
    if exit_code > 0 {
        std::process::exit(exit_code);
    }
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
    if is_unsafe_workspace_root(&canonical) {
        anyhow::bail!(
            "Refusing to register '{}' (HOME or filesystem root) as a docs directory.",
            canonical.display()
        );
    }

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

    match scope {
        DocScope::Workspace => {
            let mut conn = init_connection(&paths.local_db)?;
            let stats = index_workspace(&mut conn, paths, None)?;
            println!(
                "[lee] Re-indexed workspace + workspace docs: {} files, {} code symbols, {} symbol refs, {} md headings, {} doc sections ({} ms).",
                stats.files_scanned,
                stats.code_symbols,
                stats.symbol_refs,
                stats.md_headings,
                stats.doc_sections,
                stats.elapsed_ms
            );
        }
        DocScope::Global => {
            let stats = index_global_docs(paths)?;
            println!(
                "[lee] Indexed global docs in {}: {} files, {} code symbols, {} md headings, {} doc sections ({} ms).",
                paths.global_db.display(),
                stats.files_scanned,
                stats.code_symbols,
                stats.md_headings,
                stats.doc_sections,
                stats.elapsed_ms
            );
        }
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

fn cmd_docs_update(paths: &AppPaths) -> Result<()> {
    let start = std::time::Instant::now();
    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;
    let has_ws_docs = !ws_docs.is_empty();

    let mut all_docs: Vec<(DocScope, DocEntry)> = Vec::with_capacity(ws_docs.len() + global_docs.len());
    for doc in ws_docs {
        all_docs.push((DocScope::Workspace, doc));
    }
    for doc in global_docs {
        all_docs.push((DocScope::Global, doc));
    }

    if all_docs.is_empty() {
        println!("[lee] No workspace or global docs registered. Use `lee add-docs <path>` first.");
        return Ok(());
    }

    let mut local_conn = if has_ws_docs {
        paths.ensure_local_dirs()?;
        Some(init_connection(&paths.local_db)?)
    } else {
        None
    };
    let reports = update_registered_docs(local_conn.as_mut(), paths, &all_docs)?;

    let mut updated_docs = 0usize;
    let mut skipped_docs = 0usize;
    for r in &reports {
        if r.missing {
            println!(
                "  - [{}] {}: missing path ({})",
                r.scope.as_str(),
                r.name,
                r.path.display()
            );
        } else if r.has_changes() {
            updated_docs += 1;
            println!(
                "  - [{}] {}: updated (+{} new, ~{} modified, -{} removed | {} files)",
                r.scope.as_str(),
                r.name,
                r.added,
                r.updated,
                r.removed,
                r.files_scanned
            );
        } else {
            skipped_docs += 1;
            println!(
                "  - [{}] {}: unchanged ({} files, skipped)",
                r.scope.as_str(),
                r.name,
                r.files_scanned
            );
        }
    }

    println!(
        "[lee] Docs update finished in {} ms ({updated_docs} updated, {skipped_docs} unchanged).",
        start.elapsed().as_millis()
    );
    Ok(())
}

fn purge_removed_docs_if_unused(
    paths: &AppPaths,
    scope: DocScope,
    removed: &[DocEntry],
) -> Result<usize> {
    if removed.is_empty() {
        return Ok(0);
    }
    let (db_path, remaining) = match scope {
        DocScope::Workspace => (&paths.local_db, paths.workspace_doc_paths()),
        DocScope::Global => (&paths.global_db, paths.global_doc_paths()),
    };
    if !db_path.exists() {
        return Ok(0);
    }
    let conn = init_connection(db_path)?;
    let mut purged_files = 0usize;
    for entry in removed {
        if !remaining.contains(&entry.path) {
            purged_files += purge_source_from_db(&conn, &entry.path)?;
        }
    }
    Ok(purged_files)
}

fn cmd_remove_docs(
    paths: &AppPaths,
    target: Option<&str>,
    scope_filter: Option<DocScope>,
    all: bool,
) -> Result<()> {
    if all {
        let target_scope = scope_filter.unwrap_or(DocScope::Workspace);
        let removed = paths.clear_docs(target_scope)?;
        if removed.is_empty() {
            println!("[lee] No {} docs registered.", target_scope.as_str());
            return Ok(());
        }
        let purged_files = purge_removed_docs_if_unused(paths, target_scope, &removed)?;
        println!(
            "[lee] Removed all {} {} doc(s) and purged {} indexed file(s) from SQLite.",
            removed.len(),
            target_scope.as_str(),
            purged_files
        );
        return Ok(());
    }

    if let Some(raw_target) = target.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(scope) = scope_filter {
            let Some(removed) = paths.remove_doc(scope, raw_target)? else {
                anyhow::bail!(
                    "No {} doc matched '{}'. Run `lee docs` to see registered docs.",
                    scope.as_str(),
                    raw_target
                );
            };
            let purged = purge_removed_docs_if_unused(paths, scope, std::slice::from_ref(&removed))?;
            println!(
                "[lee] Removed {} doc '{}' ({}) and purged {} indexed file(s).",
                scope.as_str(),
                removed.name,
                removed.path.display(),
                purged
            );
            return Ok(());
        }

        if let Some(removed) = paths.remove_doc(DocScope::Workspace, raw_target)? {
            let purged = purge_removed_docs_if_unused(
                paths,
                DocScope::Workspace,
                std::slice::from_ref(&removed),
            )?;
            println!(
                "[lee] Removed workspace doc '{}' ({}) and purged {} indexed file(s).",
                removed.name,
                removed.path.display(),
                purged
            );
            return Ok(());
        }
        if let Some(removed) = paths.remove_doc(DocScope::Global, raw_target)? {
            let purged = purge_removed_docs_if_unused(
                paths,
                DocScope::Global,
                std::slice::from_ref(&removed),
            )?;
            println!(
                "[lee] Removed global doc '{}' ({}) and purged {} indexed file(s).",
                removed.name,
                removed.path.display(),
                purged
            );
            return Ok(());
        }
        anyhow::bail!("No workspace or global doc matched '{raw_target}'. Run `lee docs` to see registered docs.");
    }

    let ws_docs = if scope_filter == Some(DocScope::Global) {
        Vec::new()
    } else {
        paths.load_config().unwrap_or_default().docs
    };
    let global_docs = if scope_filter == Some(DocScope::Workspace) {
        Vec::new()
    } else {
        paths.load_global_config().unwrap_or_default().docs
    };

    let mut combined: Vec<(DocScope, DocEntry)> = Vec::new();
    for d in ws_docs {
        combined.push((DocScope::Workspace, d));
    }
    for d in global_docs {
        combined.push((DocScope::Global, d));
    }

    if combined.is_empty() {
        println!("[lee] No registered docs to remove.");
        return Ok(());
    }

    println!("Registered Docs:");
    for (i, (sc, doc)) in combined.iter().enumerate() {
        println!(
            "  [{}] ({}) {} -> {}",
            i + 1,
            sc.as_str(),
            doc.name,
            doc.path.display()
        );
    }
    print!("Enter number or name to remove (or 'all', empty to cancel): ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice = input.trim();
    if choice.is_empty() {
        println!("[lee] Cancelled.");
        return Ok(());
    }
    if choice.eq_ignore_ascii_case("all") {
        let mut total_removed = 0usize;
        let mut purged = 0usize;
        if scope_filter != Some(DocScope::Global) {
            let rem = paths.clear_docs(DocScope::Workspace)?;
            total_removed += rem.len();
            purged += purge_removed_docs_if_unused(paths, DocScope::Workspace, &rem)?;
        }
        if scope_filter != Some(DocScope::Workspace) {
            let rem = paths.clear_docs(DocScope::Global)?;
            total_removed += rem.len();
            purged += purge_removed_docs_if_unused(paths, DocScope::Global, &rem)?;
        }
        println!("[lee] Removed {total_removed} doc(s) and purged {purged} indexed file(s).");
        return Ok(());
    }

    if let Ok(idx) = choice.parse::<usize>()
        && idx >= 1
        && idx <= combined.len()
    {
        let (sc, ref entry) = combined[idx - 1];
        if let Some(removed) = paths.remove_doc(sc, &entry.name)? {
            let purged = purge_removed_docs_if_unused(paths, sc, std::slice::from_ref(&removed))?;
            println!(
                "[lee] Removed {} doc '{}' ({}) and purged {} indexed file(s).",
                sc.as_str(),
                removed.name,
                removed.path.display(),
                purged
            );
            return Ok(());
        }
    }

    cmd_remove_docs(paths, Some(choice), scope_filter, false)
}

fn cmd_index(paths: &AppPaths) -> Result<()> {
    paths.ensure_local_dirs()?;
    let mut conn = init_connection(&paths.local_db)?;
    let stats = index_workspace(&mut conn, paths, None)?;
    println!(
        "[lee] Indexed workspace: {} files ({} re-indexed, {} cached) | {} code symbols | {} symbol refs | {} md headings | {} doc sections in {} ms.",
        stats.files_scanned,
        stats.files_reindexed,
        stats.files_cached,
        stats.code_symbols,
        stats.symbol_refs,
        stats.md_headings,
        stats.doc_sections,
        stats.elapsed_ms
    );
    if !paths.global_doc_paths().is_empty() {
        let gstats = index_global_docs(paths)?;
        println!(
            "[lee] Indexed global docs ({}): {} files ({} re-indexed, {} cached) | {} md headings | {} doc sections in {} ms.",
            paths.global_db.display(),
            gstats.files_scanned,
            gstats.files_reindexed,
            gstats.files_cached,
            gstats.md_headings,
            gstats.doc_sections,
            gstats.elapsed_ms
        );
    }
    Ok(())
}

fn cmd_status(paths: &AppPaths) -> Result<()> {
    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;

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
        println!(
            "Index Counts : {files} files | {syms} code symbols | {sym_refs} symbol refs | {headings} md headings | {sections} doc windows | {handles} handles"
        );
    }

    if paths.global_db.exists() {
        if let Ok(gconn) = init_connection(&paths.global_db) {
            let gfiles: i64 = gconn
                .query_row("SELECT COUNT(*) FROM file_hashes", [], |r| r.get(0))
                .unwrap_or(0);
            let gheadings: i64 = gconn
                .query_row(
                    "SELECT COUNT(*) FROM symbols WHERE kind = 'md_heading'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let gsections: i64 = gconn
                .query_row("SELECT COUNT(*) FROM doc_sections_meta", [], |r| r.get(0))
                .unwrap_or(0);
            if gfiles > 0 {
                println!(
                    "Global Index : {gfiles} files | {gheadings} md headings | {gsections} doc windows ({})",
                    paths.global_db.display()
                );
            }
        }
    }

    Ok(())
}

fn cmd_undo(paths: &AppPaths, yes: bool) -> Result<()> {
    let local_hook_plugin = paths.project_root.join(".agents/plugins/lee");
    if !paths.local_dir.exists() && !local_hook_plugin.exists() {
        println!("[lee] Nothing to undo in {}.", paths.project_root.display());
        return Ok(());
    }

    let legacy_notes_dir = paths.local_dir.join(".notes");
    let has_notes = legacy_notes_dir.is_dir()
        && fs::read_dir(&legacy_notes_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);

    if !yes {
        println!(
            "[lee] This will remove lee index/config/hook files in {}:",
            paths.project_root.display()
        );
        if has_notes {
            println!(
                "  - Existing notes in {} WILL BE PRESERVED.",
                legacy_notes_dir.display()
            );
        } else {
            println!("  - .lee/ directory will be removed.");
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

    if local_hook_plugin.exists() {
        let _ = fs::remove_dir_all(&local_hook_plugin);
    }

    if !has_notes && paths.local_dir.exists() {
        fs::remove_dir_all(&paths.local_dir)?;
        println!(
            "[lee] Undo complete. Removed {} directory (no Markdown notes to preserve).",
            paths.local_dir.display()
        );
    } else if paths.local_dir.exists() {
        println!(
            "[lee] Undo complete. Removed index/config files in {} (preserved existing notes in {}).",
            paths.local_dir.display(),
            legacy_notes_dir.display()
        );
    } else {
        println!("[lee] Undo complete.");
    }
    Ok(())
}

fn cmd_hook(event: HookEvent) -> Result<()> {
    let mut raw = String::new();
    let _ = io::stdin().read_to_string(&mut raw);
    match event {
        HookEvent::Antigravity => {
            let out = process_antigravity_hook(&raw);
            let _ = writeln!(io::stdout(), "{out}");
        }
        HookEvent::Claude => {
            if let Some(out) = process_claude_hook(&raw) {
                let _ = writeln!(io::stdout(), "{out}");
            }
        }
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
        assert!(matches!(cli.command, Commands::Docs { action: None, json: true }));

        let cli = Cli::try_parse_from(["lee", "docs", "update"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Docs { action: Some(DocsAction::Update), .. }
        ));

        let cli = Cli::try_parse_from(["lee", "rm-docs", "--g", "gpui kit"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::RemoveDocs { global: true, workspace: false, target: Some(ref t), .. } if t == "gpui kit"
        ));

        let cli = Cli::try_parse_from(["lee", "add-source", "/tmp/gpui"]).unwrap();
        assert!(matches!(cli.command, Commands::AddDocs { .. }));
    }
}
