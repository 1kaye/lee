pub mod languages;
pub mod markdown;
pub mod parser;

use crate::config::{AppPaths, DocEntry, DocScope};
use crate::db::{init_connection, now_unix};
use crate::indexer::languages::CodeLanguage;
use crate::indexer::markdown::{DocSectionChunk, ExtractedSymbol};
use crate::indexer::parser::{is_non_doc_comment_line, parse_file_content, ConfigFormat, IndexedFileType};
use crate::security::{
    escape_sql_like, is_denied_relative_to, normalize_abs_key, to_index_path, validate_path,
};
use anyhow::{bail, Context, Result};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub files_scanned: usize,
    pub files_reindexed: usize,
    pub files_cached: usize,
    pub code_symbols: usize,
    pub symbol_refs: usize,
    pub md_headings: usize,
    pub doc_sections: usize,
    pub elapsed_ms: u128,
}

/// Skip files larger than 2 MB to avoid bloating SQLite with massive generated/data dumps.
pub const MAX_INDEX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Cap total indexed files per scan so sprawling non-project directories can never exhaust disk/RAM.
pub const MAX_INDEX_FILES: usize = 15_000;

/// Returns `true` if `path` is `$HOME`, `$HOME/.config`, or a filesystem root (`/`)
/// so `lee` never scans an entire home/config/system tree (specific subdirs like `~/.config/omarchy` are allowed).
pub fn is_unsafe_workspace_root(path: &Path) -> bool {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if canonical.parent().is_none() {
        return true;
    }
    if let Some(home) = dirs::home_dir() {
        let canon_home = fs::canonicalize(&home).unwrap_or(home);
        if canonical == canon_home {
            return true;
        }
    }
    if let Some(cfg) = dirs::config_dir() {
        let canon_cfg = fs::canonicalize(&cfg).unwrap_or(cfg);
        if canonical == canon_cfg {
            return true;
        }
    }
    false
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".kaye"
            | ".lee"
            | ".notes"
            | "target"
            | "node_modules"
            | "dist"
            | "build"
            | ".next"
            | ".astro"
            | ".docusaurus"
            | ".svelte-kit"
            | ".cache"
            | ".output"
            | ".cargo"
            | ".rustup"
            | ".local"
            | ".npm"
            | ".nvm"
            | ".pnpm-store"
            | ".bun"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".tox"
            | ".gradle"
            | ".m2"
            | ".gemini"
            | ".claude"
            | ".idea"
            | ".vscode"
            | "vendor"
            | "Cache"
            | "Code Cache"
            | "GPUCache"
            | "CachedData"
            | "IndexedDB"
            | "Local Storage"
            | "chromium"
            | "google-chrome"
            | "BraveSoftware"
            | "discord"
            | "Slack"
    )
}

fn is_within_index_size_limit(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.len() <= MAX_INDEX_FILE_BYTES)
        .unwrap_or(false)
}

/// Discovers indexable files (code, Markdown, and config) across `scan_root` and registered external `sources` in parallel.
pub fn collect_workspace_files(scan_root: &Path, extra_sources: &[PathBuf]) -> Vec<PathBuf> {
    let found: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut roots: Vec<PathBuf> = vec![scan_root.to_path_buf()];
    for src in extra_sources {
        if src.exists() && !roots.contains(src) {
            roots.push(src.clone());
        }
    }

    for root in roots {
        if root.is_file() {
            let base = root.parent().unwrap_or(Path::new("/"));
            if !is_denied_relative_to(base, &root)
                && IndexedFileType::classify(&root).is_some()
                && is_within_index_size_limit(&root)
            {
                found.lock().unwrap().push(root);
            }
            continue;
        }
        let filter_root = root.clone();
        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .filter_entry(move |entry| {
                if let Some(name) = entry.file_name().to_str()
                    && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                    && should_skip_dir(name)
                {
                    return false;
                }
                !is_denied_relative_to(&filter_root, entry.path())
            })
            .build_parallel();

        walker.run(|| {
            let found_ref = &found;
            let root_ref = &root;
            Box::new(move |res| {
                if let Ok(entry) = res
                    && entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                {
                    let p = entry.into_path();
                    if !is_denied_relative_to(root_ref, &p)
                        && IndexedFileType::classify(&p).is_some()
                        && is_within_index_size_limit(&p)
                        && let Ok(mut guard) = found_ref.lock()
                    {
                        if guard.len() >= MAX_INDEX_FILES {
                            return ignore::WalkState::Quit;
                        }
                        guard.push(p);
                    }
                }
                ignore::WalkState::Continue
            })
        });
    }

    let mut list = found.into_inner().unwrap_or_default();
    list.sort();
    list.dedup();
    list
}

fn collect_grep_files_from_root(root: &Path, out: &mut Vec<PathBuf>) {
    if root.is_file() {
        let base = root.parent().unwrap_or(Path::new("/"));
        if !is_denied_relative_to(base, root) && is_within_index_size_limit(root) {
            out.push(root.to_path_buf());
        }
        return;
    }
    let filter_root = root.to_path_buf();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .filter_entry(move |entry| {
            if let Some(name) = entry.file_name().to_str()
                && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && should_skip_dir(name)
            {
                return false;
            }
            !is_denied_relative_to(&filter_root, entry.path())
        })
        .build();

    for entry in walker.flatten() {
        if out.len() >= MAX_INDEX_FILES {
            break;
        }
        if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            let p = entry.into_path();
            if !is_denied_relative_to(root, &p) && is_within_index_size_limit(&p) {
                out.push(p);
            }
        }
    }
}

/// Removes all indexed files, symbols, and doc sections belonging to a removed doc source path.
pub fn purge_source_from_db(
    conn: &Connection,
    project_root: Option<&Path>,
    source_path: &Path,
) -> Result<usize> {
    let prefix = match project_root {
        Some(root) => to_index_path(root, source_path),
        None => normalize_abs_key(source_path),
    };
    let escaped = escape_sql_like(prefix.trim_end_matches('/'));
    let like_pat = format!("{escaped}/%");
    let deleted = conn.execute(
        "DELETE FROM file_hashes WHERE file_path = ?1 OR file_path LIKE ?2 ESCAPE '\\'",
        params![prefix, like_pat],
    )?;
    Ok(deleted)
}

fn expected_hash_suffix(ft: Option<IndexedFileType>, is_doc_source: bool) -> &'static str {
    match ft {
        Some(IndexedFileType::Markdown) => ":md3",
        Some(f) if f.is_code() => {
            if is_doc_source {
                ":d1"
            } else {
                ":r1"
            }
        }
        _ => "",
    }
}

fn is_under_doc_paths(file_path: &Path, doc_paths: &[PathBuf]) -> bool {
    doc_paths
        .iter()
        .any(|root| file_path == root || file_path.starts_with(root))
}

fn attach_full_code_doc_section(
    file_path: &Path,
    raw_content: &str,
    parsed: &mut parser::ParsedFileResult,
) {
    if parsed.file_type.is_code() && !raw_content.trim().is_empty() {
        let heading = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("code")
            .to_string();
        parsed.doc_sections.push(DocSectionChunk {
            heading,
            start_line: 1,
            end_line: parsed.line_count,
            content: raw_content.to_string(),
        });
    }
}

type ParallelParseRow = (String, bool, Option<(String, parser::ParsedFileResult)>);

/// Reads, hashes (`blake3`), and parses uncached files across CPU cores using `std::thread::scope`.
fn parse_files_parallel(
    to_check: &[(PathBuf, String, bool, &'static str)],
    existing_hashes: &HashMap<String, String>,
) -> Vec<ParallelParseRow> {
    if to_check.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(to_check.len())
        .max(1);
    let chunk_size = to_check.len().div_ceil(workers);

    let batches: Vec<Vec<ParallelParseRow>> = std::thread::scope(|s| {
        let handles: Vec<_> = to_check
            .chunks(chunk_size)
            .map(|chunk| {
                s.spawn(move || {
                    let mut out = Vec::with_capacity(chunk.len());
                    for (file_path, index_key, is_doc_source, expected_suffix) in chunk {
                        let Ok(raw_content) = fs::read_to_string(file_path) else {
                            continue;
                        };
                        let raw_hash =
                            blake3::hash(raw_content.as_bytes()).to_hex().to_string();
                        let new_hash = format!("{raw_hash}{expected_suffix}");

                        if existing_hashes.get(index_key).map(|h| h.as_str())
                            == Some(new_hash.as_str())
                        {
                            out.push((index_key.clone(), true, None));
                            continue;
                        }

                        if let Ok(Some(mut parsed)) = parse_file_content(file_path, &raw_content) {
                            if *is_doc_source {
                                attach_full_code_doc_section(file_path, &raw_content, &mut parsed);
                            }
                            out.push((index_key.clone(), false, Some((new_hash, parsed))));
                        } else {
                            out.push((index_key.clone(), false, None));
                        }
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .collect()
    });

    batches.into_iter().flatten().collect()
}

fn normalize_subpath(subpath: Option<&str>) -> Option<&str> {
    let sp = subpath?.trim();
    if sp.is_empty() || sp == "." || sp == "./" {
        None
    } else {
        Some(sp)
    }
}

fn load_existing_db_map(conn: &Connection) -> Result<HashMap<String, (String, i64)>> {
    let mut stmt =
        conn.prepare_cached("SELECT file_path, blake3_hash, last_indexed FROM file_hashes")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, i64>(2)?),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn sync_files_into_db(
    conn: &mut Connection,
    items: Vec<(PathBuf, String, bool)>,
    prune_missing: bool,
) -> Result<IndexStats> {
    let start_instant = Instant::now();
    let existing_db_map = load_existing_db_map(conn)?;

    let mut stats = IndexStats::default();
    let mut seen_keys: HashSet<String> = HashSet::with_capacity(items.len());
    let mut to_check: Vec<(PathBuf, String, bool, &'static str)> = Vec::new();

    for (file_path, index_key, is_doc_source) in items {
        seen_keys.insert(index_key.clone());
        let expected_suffix =
            expected_hash_suffix(IndexedFileType::classify(&file_path), is_doc_source);

        if let Some((old_hash, last_idx)) = existing_db_map.get(&index_key)
            && (expected_suffix.is_empty() || old_hash.ends_with(expected_suffix))
            && let Ok(meta) = fs::metadata(&file_path)
            && let Ok(mtime) = meta.modified()
            && let Ok(dur) = mtime.duration_since(UNIX_EPOCH)
            && (dur.as_secs() as i64) < *last_idx
        {
            stats.files_scanned += 1;
            stats.files_cached += 1;
            continue;
        }
        to_check.push((file_path, index_key, is_doc_source, expected_suffix));
    }

    let mut to_upsert: Vec<(String, String, parser::ParsedFileResult)> = Vec::new();
    if !to_check.is_empty() {
        let existing_hashes: HashMap<String, String> = existing_db_map
            .iter()
            .map(|(k, (h, _))| (k.clone(), h.clone()))
            .collect();
        for (index_key, is_cached, parsed_opt) in parse_files_parallel(&to_check, &existing_hashes) {
            stats.files_scanned += 1;
            if is_cached {
                stats.files_cached += 1;
            } else if let Some((new_hash, parsed)) = parsed_opt {
                stats.files_reindexed += 1;
                to_upsert.push((index_key, new_hash, parsed));
            }
        }
    }

    let to_delete: Vec<&String> = if prune_missing {
        existing_db_map
            .keys()
            .filter(|db_path| !seen_keys.contains(*db_path))
            .collect()
    } else {
        Vec::new()
    };

    if !to_upsert.is_empty() || !to_delete.is_empty() {
        let tx = conn.transaction()?;
        for (index_key, new_hash, parsed) in &to_upsert {
            upsert_parsed_file(&tx, index_key, new_hash, parsed)?;
        }
        if !to_delete.is_empty() {
            let mut del_stmt = tx.prepare_cached("DELETE FROM file_hashes WHERE file_path = ?1")?;
            for db_path in &to_delete {
                del_stmt.execute(params![db_path])?;
            }
        }
        tx.commit()?;
    }

    stats.code_symbols = conn
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE kind != 'md_heading'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize;
    stats.symbol_refs = conn
        .query_row("SELECT COUNT(*) FROM symbol_refs", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;
    stats.md_headings = conn
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE kind = 'md_heading'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0) as usize;
    stats.doc_sections = conn
        .query_row("SELECT COUNT(*) FROM doc_sections_meta", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;
    stats.elapsed_ms = start_instant.elapsed().as_millis();

    Ok(stats)
}

/// Scans and lazily indexes all supported code and Markdown files in `subpath` + workspace docs into `local_db`.
/// Global docs are indexed once in `~/.config/lee/global.db` via `index_global_docs` to avoid per-repo duplication.
pub fn index_workspace(
    conn: &mut Connection,
    paths: &AppPaths,
    subpath: Option<&str>,
) -> Result<IndexStats> {
    let ws_docs = paths.workspace_doc_paths();
    let all_docs = paths.all_doc_paths();
    let norm_subpath = normalize_subpath(subpath);

    let scan_root = match norm_subpath {
        Some(sp) => validate_path(&paths.project_root, &all_docs, Path::new(sp))?,
        None => {
            if is_unsafe_workspace_root(&paths.project_root) {
                bail!(
                    "Refusing to index unsafe workspace root '{}' (HOME or filesystem root). Run `lee` inside a project directory.",
                    paths.project_root.display()
                );
            }
            paths.project_root.clone()
        }
    };

    let include_sources = norm_subpath.is_none();
    let extra_sources = if include_sources {
        ws_docs.as_slice()
    } else {
        &[]
    };

    let files = collect_workspace_files(&scan_root, extra_sources);
    let items: Vec<(PathBuf, String, bool)> = files
        .into_iter()
        .map(|fp| {
            let index_key = to_index_path(&paths.project_root, &fp);
            let is_doc_source = is_under_doc_paths(&fp, &ws_docs);
            (fp, index_key, is_doc_source)
        })
        .collect();

    sync_files_into_db(conn, items, include_sources)
}

/// Indexes all registered global docs once into `~/.config/lee/global.db` (shared across all workspaces).
pub fn index_global_docs(paths: &AppPaths) -> Result<IndexStats> {
    let global_docs = paths.global_doc_paths();
    if global_docs.is_empty() && !paths.global_db.exists() {
        return Ok(IndexStats::default());
    }

    paths.ensure_global_dirs()?;
    let mut conn = init_connection(&paths.global_db)?;
    let files = if let Some((first, rest)) = global_docs.split_first() {
        collect_workspace_files(first, rest)
    } else {
        Vec::new()
    };

    let items: Vec<(PathBuf, String, bool)> = files
        .into_iter()
        .map(|fp| {
            let index_key = normalize_abs_key(&fp);
            (fp, index_key, true)
        })
        .collect();

    sync_files_into_db(&mut conn, items, true)
}

#[derive(Debug, Clone)]
pub struct DocUpdateReport {
    pub scope: DocScope,
    pub name: String,
    pub path: PathBuf,
    pub missing: bool,
    pub files_scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

impl DocUpdateReport {
    pub fn has_changes(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }
}

fn path_belongs_to_doc(file_path: &str, doc_key: &str) -> bool {
    let key = doc_key.trim_end_matches('/');
    file_path == key || file_path.starts_with(&format!("{key}/"))
}

fn update_docs_in_conn(
    conn: &mut Connection,
    project_root: Option<&Path>,
    docs: &[(DocScope, DocEntry)],
    reports: &mut Vec<DocUpdateReport>,
) -> Result<()> {
    let mut existing_db_map = load_existing_db_map(conn)?;
    let mut existing_hashes: HashMap<String, String> = existing_db_map
        .iter()
        .map(|(k, (h, _))| (k.clone(), h.clone()))
        .collect();

    for (scope, entry) in docs {
        if !entry.path.exists() {
            reports.push(DocUpdateReport {
                scope: *scope,
                name: entry.name.clone(),
                path: entry.path.clone(),
                missing: true,
                files_scanned: 0,
                added: 0,
                updated: 0,
                removed: 0,
            });
            continue;
        }

        let doc_key = match project_root {
            Some(root) => to_index_path(root, &entry.path),
            None => normalize_abs_key(&entry.path),
        };
        let files = collect_workspace_files(&entry.path, &[]);
        let mut seen_keys: HashSet<String> = HashSet::with_capacity(files.len());
        let mut to_check: Vec<(PathBuf, String, bool, &'static str)> = Vec::new();

        for file_path in &files {
            let index_key = match project_root {
                Some(root) => to_index_path(root, file_path),
                None => normalize_abs_key(file_path),
            };
            seen_keys.insert(index_key.clone());

            let expected_suffix = expected_hash_suffix(IndexedFileType::classify(file_path), true);
            if let Some((old_hash, last_idx)) = existing_db_map.get(&index_key)
                && (expected_suffix.is_empty() || old_hash.ends_with(expected_suffix))
                && let Ok(meta) = fs::metadata(file_path)
                && let Ok(mtime) = meta.modified()
                && let Ok(dur) = mtime.duration_since(UNIX_EPOCH)
                && (dur.as_secs() as i64) < *last_idx
            {
                continue;
            }
            to_check.push((file_path.clone(), index_key, true, expected_suffix));
        }

        let mut to_upsert: Vec<(String, String, parser::ParsedFileResult)> = Vec::new();
        let mut added = 0usize;
        let mut updated = 0usize;

        for (index_key, is_cached, parsed_opt) in parse_files_parallel(&to_check, &existing_hashes) {
            if !is_cached
                && let Some((new_hash, parsed)) = parsed_opt
            {
                if existing_db_map.contains_key(&index_key) {
                    updated += 1;
                } else {
                    added += 1;
                }
                to_upsert.push((index_key, new_hash, parsed));
            }
        }

        let to_remove: Vec<String> = existing_db_map
            .keys()
            .filter(|db_path| path_belongs_to_doc(db_path, &doc_key) && !seen_keys.contains(*db_path))
            .cloned()
            .collect();
        let removed = to_remove.len();

        if !to_upsert.is_empty() || !to_remove.is_empty() {
            let tx = conn.transaction()?;
            let ts = now_unix();
            for (index_key, new_hash, parsed) in &to_upsert {
                upsert_parsed_file(&tx, index_key, new_hash, parsed)?;
            }
            if !to_remove.is_empty() {
                let mut del_stmt = tx.prepare_cached("DELETE FROM file_hashes WHERE file_path = ?1")?;
                for db_path in &to_remove {
                    del_stmt.execute(params![db_path])?;
                }
            }
            tx.commit()?;

            for (index_key, new_hash, _) in &to_upsert {
                existing_db_map.insert(index_key.clone(), (new_hash.clone(), ts));
                existing_hashes.insert(index_key.clone(), new_hash.clone());
            }
            for db_path in &to_remove {
                existing_db_map.remove(db_path);
                existing_hashes.remove(db_path);
            }
        }

        reports.push(DocUpdateReport {
            scope: *scope,
            name: entry.name.clone(),
            path: entry.path.clone(),
            missing: false,
            files_scanned: files.len(),
            added,
            updated,
            removed,
        });
    }
    Ok(())
}

/// Incrementally updates registered workspace docs (in `local_db`) and global docs (in `global_db`).
/// Docs with no added, modified, or removed files are skipped without writing to SQLite.
pub fn update_registered_docs(
    local_conn: Option<&mut Connection>,
    paths: &AppPaths,
    docs: &[(DocScope, DocEntry)],
) -> Result<Vec<DocUpdateReport>> {
    let ws_docs: Vec<(DocScope, DocEntry)> = docs
        .iter()
        .filter(|(s, _)| *s == DocScope::Workspace)
        .cloned()
        .collect();
    let gl_docs: Vec<(DocScope, DocEntry)> = docs
        .iter()
        .filter(|(s, _)| *s == DocScope::Global)
        .cloned()
        .collect();

    let mut reports = Vec::with_capacity(docs.len());
    if !ws_docs.is_empty()
        && let Some(lconn) = local_conn
    {
        update_docs_in_conn(lconn, Some(&paths.project_root), &ws_docs, &mut reports)?;
    }
    if !gl_docs.is_empty() {
        paths.ensure_global_dirs()?;
        let mut gconn = init_connection(&paths.global_db)?;
        update_docs_in_conn(&mut gconn, None, &gl_docs, &mut reports)?;
    }
    Ok(reports)
}

fn upsert_parsed_file(
    conn: &Connection,
    index_key: &str,
    blake3_hash: &str,
    parsed: &parser::ParsedFileResult,
) -> Result<()> {
    conn.prepare_cached("DELETE FROM file_hashes WHERE file_path = ?1")?
        .execute(params![index_key])?;

    conn.prepare_cached(
        "INSERT INTO file_hashes (file_path, blake3_hash, line_count, file_type, last_indexed)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        index_key,
        blake3_hash,
        parsed.line_count as i64,
        parsed.file_type.as_db_str(),
        now_unix()
    ])?;

    {
        let mut sym_stmt = conn.prepare_cached(
            "INSERT INTO symbols (file_path, name, kind, signature, start_line, end_line, parent_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for sym in &parsed.symbols {
            sym_stmt.execute(params![
                index_key,
                sym.name,
                sym.kind,
                sym.signature,
                sym.start_line as i64,
                sym.end_line as i64,
                sym.parent_name,
            ])?;
        }
    }

    {
        let mut ref_stmt = conn.prepare_cached(
            "INSERT INTO symbol_refs (symbol_name, file_path, caller_symbol, line_number, ref_kind)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for r in &parsed.symbol_refs {
            ref_stmt.execute(params![
                r.symbol_name,
                index_key,
                r.caller_symbol,
                r.line_number as i64,
                r.ref_kind,
            ])?;
        }
    }

    {
        let mut sec_stmt = conn.prepare_cached(
            "INSERT INTO doc_sections_meta (file_path, heading, start_line, end_line, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for sec in &parsed.doc_sections {
            sec_stmt.execute(params![
                index_key,
                sec.heading,
                sec.start_line as i64,
                sec.end_line as i64,
                sec.content,
            ])?;
        }
    }

    Ok(())
}

fn is_under_root(root: &Path, candidate: &Path) -> bool {
    let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon_cand = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    canon_cand == canon_root || canon_cand.starts_with(&canon_root)
}

/// Lazily indexes a single file if its `blake3` hash is missing or changed.
/// Routes global-only docs to `global_db` so `local_db` stays clean.
/// Returns `(index_key, raw_content, in_global_db)`.
pub fn ensure_single_file_indexed(
    conn: &Connection,
    paths: &AppPaths,
    file_arg: &str,
) -> Result<(String, String, bool)> {
    let ws_docs = paths.workspace_doc_paths();
    let global_docs = paths.global_doc_paths();
    let allowed = paths.all_doc_paths();

    let resolved = resolve_file_with_sources(conn, paths, &allowed, file_arg)?;
    let raw_content = fs::read_to_string(&resolved)
        .with_context(|| format!("Failed to read file '{}'", resolved.display()))?;

    let is_global_only = !is_under_root(&paths.project_root, &resolved)
        && !is_under_doc_paths(&resolved, &ws_docs)
        && is_under_doc_paths(&resolved, &global_docs);
    let is_doc_source = is_global_only || is_under_doc_paths(&resolved, &ws_docs);
    let index_key = if is_global_only {
        normalize_abs_key(&resolved)
    } else {
        to_index_path(&paths.project_root, &resolved)
    };

    let raw_hash = blake3::hash(raw_content.as_bytes()).to_hex().to_string();
    let expected_suffix = expected_hash_suffix(IndexedFileType::classify(&resolved), is_doc_source);
    let new_hash = format!("{raw_hash}{expected_suffix}");

    let gconn_holder = if is_global_only {
        paths.ensure_global_dirs()?;
        Some(init_connection(&paths.global_db)?)
    } else {
        None
    };
    let target_conn = gconn_holder.as_ref().unwrap_or(conn);

    let existing_hash: Option<String> = target_conn
        .query_row(
            "SELECT blake3_hash FROM file_hashes WHERE file_path = ?1",
            params![&index_key],
            |row| row.get(0),
        )
        .optional()?;

    if existing_hash.as_deref() != Some(new_hash.as_str())
        && let Some(mut parsed) = parse_file_content(&resolved, &raw_content)?
    {
        if is_doc_source {
            attach_full_code_doc_section(&resolved, &raw_content, &mut parsed);
        }
        upsert_parsed_file(target_conn, &index_key, &new_hash, &parsed)?;
    }

    Ok((index_key, raw_content, is_global_only))
}

fn lookup_file_in_db(
    conn: &Connection,
    project_root: &Path,
    sources: &[PathBuf],
    clean_arg: &str,
) -> Option<PathBuf> {
    let like_pat = format!("%/{}", escape_sql_like(clean_arg));
    let matched_fp = conn
        .query_row(
            "SELECT file_path FROM file_hashes
             WHERE file_path = ?1 OR file_path LIKE ?2 ESCAPE '\\'
             ORDER BY LENGTH(file_path) ASC LIMIT 1",
            params![clean_arg, like_pat],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()??;
    let valid = validate_path(project_root, sources, Path::new(&matched_fp)).ok()?;
    valid.exists().then_some(valid)
}

fn resolve_file_with_sources(
    conn: &Connection,
    paths: &AppPaths,
    sources: &[PathBuf],
    file_arg: &str,
) -> Result<PathBuf> {
    let candidate = Path::new(file_arg);
    if let Ok(valid) = validate_path(&paths.project_root, sources, candidate)
        && valid.exists()
    {
        return Ok(valid);
    }
    for src in sources {
        let joined = src.join(candidate);
        if joined.exists() {
            return validate_path(&paths.project_root, sources, &joined);
        }
    }
    let clean_arg = file_arg.trim_start_matches("./").replace('\\', "/");
    if let Some(valid) = lookup_file_in_db(conn, &paths.project_root, sources, &clean_arg) {
        return Ok(valid);
    }
    if paths.global_db.exists()
        && let Ok(gconn) = init_connection(&paths.global_db)
        && let Some(valid) = lookup_file_in_db(&gconn, &paths.project_root, sources, &clean_arg)
    {
        return Ok(valid);
    }
    validate_path(&paths.project_root, sources, candidate)
}

fn map_file_priority(path: &str) -> u8 {
    if !path.contains('/') {
        return 0; // Root files (Cargo.toml, README.md, AGENTS.md, etc.)
    }
    let top = path.split('/').next().unwrap_or("");
    if matches!(top, "src" | "crates" | "app" | "lib" | "packages" | "locales" | "docs") {
        0
    } else {
        1
    }
}

/// Computes PageRank centrality scores over workspace files from the AST `symbol_refs` graph
/// (`import` = 3.0, `type_use` = 2.0, `call` = 1.0) so `project_map` surfaces core files first.
fn compute_file_pagerank(conn: &Connection) -> HashMap<String, f64> {
    let mut sym_to_files: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_files: HashSet<String> = HashSet::new();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT name, file_path FROM symbols
         WHERE kind != 'md_heading' AND file_path NOT LIKE '/%'",
    ) && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
    {
        for (name, fp) in rows.flatten() {
            all_files.insert(fp.clone());
            let list = sym_to_files.entry(name).or_default();
            if !list.contains(&fp) {
                list.push(fp);
            }
        }
    }

    if all_files.len() <= 1 {
        return HashMap::new();
    }

    let mut edges: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut out_weight: HashMap<String, f64> = HashMap::new();

    if let Ok(mut stmt) = conn.prepare(
        "SELECT file_path, symbol_name, ref_kind, COUNT(*)
         FROM symbol_refs
         WHERE file_path NOT LIKE '/%'
         GROUP BY file_path, symbol_name, ref_kind",
    ) && let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    }) {
        for (src_fp, sym, kind, cnt) in rows.flatten() {
            let Some(targets) = sym_to_files.get(&sym) else {
                continue;
            };
            let w_kind = match kind.as_str() {
                "import" => 3.0,
                "type_use" => 2.0,
                _ => 1.0,
            };
            let w = (cnt as f64) * w_kind / (targets.len() as f64);
            for dst_fp in targets {
                if dst_fp != &src_fp {
                    all_files.insert(src_fp.clone());
                    *edges
                        .entry(src_fp.clone())
                        .or_default()
                        .entry(dst_fp.clone())
                        .or_insert(0.0) += w;
                    *out_weight.entry(src_fp.clone()).or_insert(0.0) += w;
                }
            }
        }
    }

    let n = all_files.len() as f64;
    let init_score = 1.0 / n;
    let damping = 0.85;
    let base = (1.0 - damping) / n;
    let mut ranks: HashMap<String, f64> = all_files.iter().map(|f| (f.clone(), init_score)).collect();

    for _ in 0..10 {
        let mut next: HashMap<String, f64> = all_files.iter().map(|f| (f.clone(), base)).collect();
        let mut dangling_sum = 0.0;

        for f in &all_files {
            let r = ranks.get(f).copied().unwrap_or(0.0);
            if let Some(targets) = edges.get(f)
                && let Some(&total_w) = out_weight.get(f)
                && total_w > 0.0
            {
                for (dst, w) in targets {
                    *next.entry(dst.clone()).or_insert(base) += damping * r * (w / total_w);
                }
            } else {
                dangling_sum += r;
            }
        }
        if dangling_sum > 0.0 {
            let add = damping * dangling_sum / n;
            for val in next.values_mut() {
                *val += add;
            }
        }
        ranks = next;
    }

    ranks
}

/// Computes fuzzy similarity between `query` and `candidate` symbol names in `[0.0, 1.0]`,
/// combining case-insensitive prefix/substring, ordered subsequence, and character bigram Dice score.
fn fuzzy_symbol_score(query: &str, candidate: &str) -> f64 {
    let q = query.trim().to_lowercase();
    let c = candidate.trim().to_lowercase();
    if q.is_empty() || c.is_empty() {
        return 0.0;
    }
    if q == c {
        return 1.0;
    }
    if c.starts_with(&q) {
        return 0.92 - ((c.len() - q.len()) as f64 * 0.005).min(0.15);
    }
    if c.contains(&q) {
        return 0.82 - ((c.len() - q.len()) as f64 * 0.005).min(0.15);
    }

    // Ordered subsequence check (e.g. "parse_md" in "parse_markdown")
    let mut q_chars = q.chars().peekable();
    for ch in c.chars() {
        if q_chars.peek() == Some(&ch) {
            q_chars.next();
        }
    }
    let is_subseq = q_chars.peek().is_none();

    // Character bigram Dice similarity
    let q_bytes = q.as_bytes();
    let c_bytes = c.as_bytes();
    let dice = if q_bytes.len() >= 2 && c_bytes.len() >= 2 {
        let mut q_bigrams: Vec<[u8; 2]> = q_bytes.windows(2).map(|w| [w[0], w[1]]).collect();
        let mut matches = 0usize;
        for w in c_bytes.windows(2) {
            let bg = [w[0], w[1]];
            if let Some(pos) = q_bigrams.iter().position(|x| *x == bg) {
                matches += 1;
                q_bigrams.swap_remove(pos);
            }
        }
        (2.0 * matches as f64) / ((q_bytes.len() - 1 + c_bytes.len() - 1) as f64)
    } else {
        0.0
    };

    if is_subseq {
        let ratio = (q.len() as f64) / (c.len() as f64);
        (0.65 + 0.25 * ratio).max(dice)
    } else {
        dice
    }
}

type SymbolMatch = (String, String, String, i64, i64);
type FuzzySymbolMatch = (String, String, String, String, i64, i64, f64);

/// Finds top fuzzy-matched symbols from `symbols` when exact / SQL `LIKE` lookup returns no match.
fn fuzzy_find_symbols(
    conn: &Connection,
    file_filter: Option<&str>,
    clean_query: &str,
    limit: usize,
) -> Vec<FuzzySymbolMatch> {
    let mut candidates: Vec<FuzzySymbolMatch> = Vec::new();
    let sql = if file_filter.is_some() {
        "SELECT file_path, kind, name, signature, start_line, end_line FROM symbols WHERE file_path = ?1"
    } else {
        "SELECT file_path, kind, name, signature, start_line, end_line FROM symbols WHERE (?1 IS NULL OR file_path = ?1)"
    };
    let Ok(mut stmt) = conn.prepare(sql) else {
        return candidates;
    };
    let Ok(rows) = stmt.query_map(params![file_filter], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
        ))
    }) else {
        return candidates;
    };

    for (fp, kind, name, sig, st, en) in rows.flatten() {
        let sc = fuzzy_symbol_score(clean_query, &name);
        if sc >= 0.52 {
            candidates.push((fp, kind, name, sig, st, en, sc));
        }
    }

    candidates.sort_by(|a, b| {
        b.6.partial_cmp(&a.6)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.starts_with('/').cmp(&b.0.starts_with('/')))
            .then_with(|| a.2.len().cmp(&b.2.len()))
    });
    candidates.truncate(limit);
    candidates
}

/// Tool 1: `project_map` — builds a compact directory & symbol/heading map within `max_tokens`.
/// Prioritizes root + core code folders (`src/`, `crates/`, `lib/`, `app/`), collapses overflow folders
/// into 1-line directory summaries, and summarizes external `sources`.
pub fn build_project_map(
    conn: &mut Connection,
    paths: &AppPaths,
    subpath: Option<&str>,
    max_tokens: usize,
) -> Result<String> {
    let norm_subpath = normalize_subpath(subpath);
    index_workspace(conn, paths, norm_subpath)?;
    let max_bytes = max_tokens.saturating_mul(4).max(400);

    let prefix_filter = match norm_subpath {
        Some(sp) => {
            let allowed = paths.all_doc_paths();
            let resolved = validate_path(&paths.project_root, &allowed, Path::new(sp))?;
            Some(to_index_path(&paths.project_root, &resolved))
        }
        None => None,
    };

    let mut stmt = conn.prepare(
        "SELECT f.file_path, f.line_count, f.file_type, s.kind, s.name, s.signature, s.start_line, s.end_line, s.parent_name
         FROM file_hashes f
         LEFT JOIN symbols s ON f.file_path = s.file_path
         ORDER BY CASE WHEN f.file_path LIKE '/%' THEN 1 ELSE 0 END ASC, f.file_path ASC, s.start_line ASC",
    )?;

    let mut local_map: BTreeMap<String, (i64, String, Vec<String>)> = BTreeMap::new();
    let mut local_code_symbols = 0usize;
    let mut local_md_headings = 0usize;
    let mut ext_file_count = 0usize;
    let mut ext_symbol_count = 0usize;

    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let file_path: String = row.get(0)?;
        if let Some(ref pref) = prefix_filter {
            let p = pref.trim_end_matches('/');
            if file_path != p && !file_path.starts_with(&format!("{p}/")) {
                continue;
            }
        }
        let is_external = file_path.starts_with('/') && prefix_filter.is_none();
        let line_count: i64 = row.get(1)?;
        let file_type: String = row.get(2)?;
        let kind: Option<String> = row.get(3)?;
        let name: Option<String> = row.get(4)?;
        let signature: Option<String> = row.get(5)?;
        let start_line: Option<i64> = row.get(6)?;
        let end_line: Option<i64> = row.get(7)?;
        let parent_name: Option<String> = row.get(8)?;

        if is_external {
            if kind.is_some() {
                ext_symbol_count += 1;
            }
            continue;
        }

        if let Some(ref k) = kind {
            if k == "md_heading" {
                local_md_headings += 1;
            } else {
                local_code_symbols += 1;
            }
        }

        let entry = local_map
            .entry(file_path)
            .or_insert_with(|| (line_count, file_type, Vec::new()));

        if let (Some(k), Some(n), Some(sig), Some(st), Some(en)) =
            (kind, name, signature, start_line, end_line)
        {
            if parent_name.as_deref() == Some("tests") {
                continue;
            }
            let label = if k == "md_heading" {
                format!("{sig} [{st}..{en}]")
            } else {
                format!("{k} {n} [{st}..{en}]")
            };
            entry.2.push(label);
        }
    }

    if prefix_filter.is_none() {
        ext_file_count = conn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE file_path LIKE '/%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize;
        if paths.global_db.exists()
            && let Ok(gconn) = init_connection(&paths.global_db)
        {
            ext_file_count += gconn
                .query_row("SELECT COUNT(*) FROM file_hashes", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as usize;
            ext_symbol_count += gconn
                .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get::<_, i64>(0))
                .unwrap_or(0) as usize;
        }
    }

    let file_ranks = compute_file_pagerank(conn);
    let mut ordered: Vec<(String, i64, String, Vec<String>)> = local_map
        .into_iter()
        .map(|(f, (l, ft, s))| (f, l, ft, s))
        .collect();
    ordered.sort_by(|a, b| {
        map_file_priority(&a.0)
            .cmp(&map_file_priority(&b.0))
            .then_with(|| {
                let ra = file_ranks.get(&a.0).copied().unwrap_or(0.0);
                let rb = file_ranks.get(&b.0).copied().unwrap_or(0.0);
                rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut out = String::new();
    out.push_str(&format!(
        "# Project Map ({} files, {} code symbols, {} md headings)\n",
        ordered.len(),
        local_code_symbols,
        local_md_headings
    ));

    let reserve_ext = if ext_file_count > 0 { 220 } else { 0 };
    let budget_limit = max_bytes.saturating_sub(reserve_ext).max(300);

    let mut overflow_dirs: BTreeMap<String, (usize, i64, usize)> = BTreeMap::new();
    let mut overflowed = false;

    let rel_for_group = |f: &str| -> String {
        if let Some(ref pref) = prefix_filter {
            let trimmed = f
                .strip_prefix(pref.as_str())
                .unwrap_or(f)
                .trim_start_matches('/');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        f.trim_start_matches('/').to_string()
    };

    for (file, lines, ftype, syms) in &ordered {
        let rel = rel_for_group(file);
        if overflowed {
            let dir = rel.split('/').next().unwrap_or("root").to_string();
            let ent = overflow_dirs.entry(dir).or_insert((0, 0, 0));
            ent.0 += 1;
            ent.1 += *lines;
            ent.2 += syms.len();
            continue;
        }

        let line = if syms.is_empty() {
            format!("- {file} ({lines}L, {ftype})\n")
        } else {
            let preview: Vec<&str> = syms.iter().take(10).map(|s| s.as_str()).collect();
            let suffix = if syms.len() > 10 {
                format!(", +{} more", syms.len() - 10)
            } else {
                String::new()
            };
            format!("- {file} ({lines}L): {}{suffix}\n", preview.join(", "))
        };

        if out.len() + line.len() > budget_limit {
            overflowed = true;
            let dir = rel.split('/').next().unwrap_or("root").to_string();
            let ent = overflow_dirs.entry(dir).or_insert((0, 0, 0));
            ent.0 += 1;
            ent.1 += *lines;
            ent.2 += syms.len();
        } else {
            out.push_str(&line);
        }
    }

    if !overflow_dirs.is_empty() {
        out.push_str("\n# Additional Directories (pass `subpath` to expand)\n");
        for (dir, (fcount, lcount, scount)) in overflow_dirs {
            out.push_str(&format!(
                "- {dir}/ ({fcount} files, {lcount}L, {scount} symbols)\n"
            ));
        }
    }

    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;
    if ext_file_count > 0 || !ws_docs.is_empty() || !global_docs.is_empty() {
        out.push_str(&format!(
            "\n# Registered Docs ({} external files, {} symbols/headings — query via `search_docs(doc=\"...\", scope=\"...\")`, `smart_grep`, `outline`, or `lee docs --json`)\n",
            ext_file_count, ext_symbol_count
        ));
        for doc in &ws_docs {
            out.push_str(&format!("- [workspace] {}: {}\n", doc.name, doc.path.display()));
        }
        for doc in &global_docs {
            out.push_str(&format!("- [global] {}: {}\n", doc.name, doc.path.display()));
        }
    }

    Ok(out)
}

/// Tool 2: `outline` — returns AST symbols (code), Heading TOC (Markdown), or key/section TOC (YAML/TOML/JSON).
/// Collapses `mod tests` children by default and enforces `max_tokens` so outlines stay compact.
pub fn build_outline(
    conn: &Connection,
    paths: &AppPaths,
    file_arg: &str,
    max_tokens: usize,
    include_tests: bool,
) -> Result<String> {
    let (index_key, _content, is_global) = ensure_single_file_indexed(conn, paths, file_arg)?;
    let gconn_holder = if is_global {
        Some(init_connection(&paths.global_db)?)
    } else {
        None
    };
    let sym_conn = gconn_holder.as_ref().unwrap_or(conn);

    let mut stmt = sym_conn.prepare(
        "SELECT name, kind, signature, start_line, end_line, parent_name
         FROM symbols
         WHERE file_path = ?1
         ORDER BY start_line ASC",
    )?;

    let rows: Vec<ExtractedSymbol> = stmt
        .query_map(params![&index_key], |r| {
            Ok(ExtractedSymbol {
                name: r.get(0)?,
                kind: r.get(1)?,
                signature: r.get(2)?,
                start_line: r.get::<_, i64>(3)? as usize,
                end_line: r.get::<_, i64>(4)? as usize,
                parent_name: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let max_bytes = max_tokens.saturating_mul(4).max(400);
    let collapsed_test_count = rows
        .iter()
        .filter(|s| s.parent_name.as_deref() == Some("tests"))
        .count();

    let mut out = format!("# Outline: {index_key}\n");
    if rows.is_empty() {
        out.push_str("(No symbols or keys extracted; use `read_symbol` with `start_line` & `end_line`)\n");
    } else {
        let mut shown = 0usize;
        for (idx, s) in rows.iter().enumerate() {
            if !include_tests && s.parent_name.as_deref() == Some("tests") {
                continue;
            }
            let parent_str = s
                .parent_name
                .as_deref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default();
            let line = if s.kind == "md_heading" {
                format!(
                    "- [{start}..{end}] {sig}{parent_str}\n",
                    start = s.start_line,
                    end = s.end_line,
                    sig = s.signature
                )
            } else if !include_tests && s.kind == "mod" && s.name == "tests" && collapsed_test_count > 0
            {
                format!(
                    "- [{start}..{end}] (mod) tests ({collapsed_test_count} test items collapsed; pass `include_tests=true`)\n",
                    start = s.start_line,
                    end = s.end_line
                )
            } else {
                format!(
                    "- [{start}..{end}] ({kind}) {name}{parent_str}: `{sig}`\n",
                    start = s.start_line,
                    end = s.end_line,
                    kind = s.kind,
                    name = s.name,
                    sig = s.signature
                )
            };

            if out.len() + line.len() > max_bytes {
                let remaining = rows.len().saturating_sub(idx);
                out.push_str(&format!(
                    "... [+{remaining} more symbols truncated to max_tokens={max_tokens}; pass `max_tokens` or `read_symbol`]\n"
                ));
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
        if shown == 0 && collapsed_test_count > 0 {
            out.push_str(&format!(
                "({collapsed_test_count} test items collapsed; pass `include_tests=true` to expand)\n"
            ));
        }
    }

    Ok(out)
}

fn parse_symbol_query(sym_query: &str) -> (Option<&str>, &str) {
    let raw_clean = sym_query.trim().trim_start_matches('#').trim();
    if let Some((p, s)) = raw_clean.rsplit_once("::") {
        (Some(p.rsplit("::").next().unwrap_or(p).trim()), s.trim())
    } else if !raw_clean.contains(' ')
        && let Some((p, s)) = raw_clean.rsplit_once('.')
    {
        (Some(p.trim()), s.trim())
    } else {
        (None, raw_clean)
    }
}

fn find_workspace_symbol_matches(
    conn: &Connection,
    sym_query: &str,
    clean_query: &str,
    parent_hint: Option<&str>,
) -> Result<Vec<SymbolMatch>> {
    let like_pat = format!("%{}%", escape_sql_like(clean_query));
    let mut stmt = conn.prepare(
        "SELECT file_path, kind, name, start_line, end_line
         FROM symbols
         WHERE name = ?1 OR signature = ?2 OR LOWER(name) LIKE LOWER(?3) ESCAPE '\\'
         ORDER BY
           CASE WHEN ?4 IS NOT NULL AND parent_name = ?4 THEN 0 ELSE 1 END,
           CASE WHEN name = ?1 THEN 0 WHEN LOWER(name) = LOWER(?1) THEN 1 ELSE 2 END,
           CASE WHEN file_path LIKE '/%' THEN 1 ELSE 0 END ASC,
           CASE WHEN parent_name = 'tests' THEN 1 ELSE 0 END ASC,
           LENGTH(name) ASC,
           file_path ASC,
           start_line ASC
         LIMIT 6",
    )?;
    let mut matches: Vec<SymbolMatch> = stmt
        .query_map(
            params![clean_query, sym_query.trim(), like_pat, parent_hint],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?
        .filter_map(|r| r.ok())
        .collect();
    if matches.is_empty() {
        for (fp, kind, name, _sig, st, en, _sc) in fuzzy_find_symbols(conn, None, clean_query, 4) {
            matches.push((fp, kind, name, st, en));
        }
    }
    Ok(matches)
}

/// Tool 3: `read_symbol` — reads exact line range for a code symbol/function (across workspace if `file` is omitted),
/// Markdown heading, config key, or manual range.
#[allow(clippy::too_many_arguments)]
pub fn read_symbol_range(
    conn: &mut Connection,
    paths: &AppPaths,
    file_arg: Option<&str>,
    symbol: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    context_lines: usize,
    strip_comments: bool,
    signatures_only: bool,
    max_lines: usize,
) -> Result<String> {
    read_symbol_range_inner(
        conn,
        paths,
        file_arg,
        symbol,
        start_line,
        end_line,
        context_lines,
        strip_comments,
        signatures_only,
        max_lines,
        true,
    )
}

/// Batch variant of `read_symbol_range` that syncs the workspace index at most once across all requested symbols.
#[allow(clippy::too_many_arguments)]
pub fn read_symbols_batch(
    conn: &mut Connection,
    paths: &AppPaths,
    file_arg: Option<&str>,
    symbols: &[String],
    context_lines: usize,
    strip_comments: bool,
    signatures_only: bool,
    max_lines: usize,
) -> Result<String> {
    let cleaned: Vec<&str> = symbols
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        bail!("Provide at least one non-empty symbol name in `symbols`.");
    }
    if file_arg.map(|s| s.trim().is_empty()).unwrap_or(true) {
        let _ = index_workspace(conn, paths, None);
    }

    let mut sections = Vec::with_capacity(cleaned.len());
    let mut succeeded = 0usize;
    let mut last_err: Option<anyhow::Error> = None;

    for sym in cleaned {
        match read_symbol_range_inner(
            conn,
            paths,
            file_arg,
            Some(sym),
            None,
            None,
            context_lines,
            strip_comments,
            signatures_only,
            max_lines,
            false,
        ) {
            Ok(out) => {
                succeeded += 1;
                sections.push(out);
            }
            Err(e) => {
                sections.push(format!("# Error reading `{sym}`: {e}\n"));
                last_err = Some(e);
            }
        }
    }

    if succeeded == 0 {
        if let Some(err) = last_err {
            return Err(err);
        }
        bail!("None of the requested symbols could be found.");
    }

    Ok(sections.join("\n"))
}

#[allow(clippy::too_many_arguments)]
fn read_symbol_range_inner(
    conn: &mut Connection,
    paths: &AppPaths,
    file_arg: Option<&str>,
    symbol: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    context_lines: usize,
    strip_comments: bool,
    signatures_only: bool,
    max_lines: usize,
    sync_workspace: bool,
) -> Result<String> {
    if let Some(sym_raw) = symbol {
        let (_, clean) = parse_symbol_query(sym_raw);
        if clean.is_empty() {
            bail!("Provide a non-empty `symbol` to `read_symbol`");
        }
    }

    let mut other_matches_note = String::new();
    let resolved_file_owned: String;
    let effective_file_arg = if let Some(fa) = file_arg.filter(|s| !s.trim().is_empty()) {
        fa
    } else if let Some(sym_query) = symbol {
        let (parent_hint, clean_query) = parse_symbol_query(sym_query);
        if sync_workspace {
            let _ = index_workspace(conn, paths, None);
        }
        let mut matches = find_workspace_symbol_matches(conn, sym_query, clean_query, parent_hint)?;
        if matches.is_empty()
            && paths.global_db.exists()
            && let Ok(gconn) = init_connection(&paths.global_db)
        {
            matches = find_workspace_symbol_matches(&gconn, sym_query, clean_query, parent_hint)?;
        }

        let Some(first) = matches.first() else {
            bail!(
                "Symbol or function '{}' not found across indexed workspace or external sources.",
                sym_query
            );
        };
        resolved_file_owned = first.0.clone();
        if matches.len() > 1 {
            let alts: Vec<String> = matches[1..]
                .iter()
                .map(|(fp, k, nm, st, en)| format!("{fp} ({k} {nm} [{st}..{en}])"))
                .collect();
            other_matches_note = format!("\n# Other matching symbols:\n- {}\n", alts.join("\n- "));
        }
        resolved_file_owned.as_str()
    } else {
        bail!("Provide `file` or `symbol` (e.g. function name) to `read_symbol`");
    };

    let (index_key, raw_content, is_global) =
        ensure_single_file_indexed(conn, paths, effective_file_arg)?;
    let gconn_holder = if is_global {
        Some(init_connection(&paths.global_db)?)
    } else {
        None
    };
    let sym_conn = gconn_holder.as_ref().unwrap_or(conn);

    let lines: Vec<&str> = raw_content.lines().collect();
    let total_lines = lines.len().max(1);

    let (target_start, target_end, header_label) = if let Some(sym_query) = symbol {
        let (parent_hint, clean_query) = parse_symbol_query(sym_query);
        let like_pat = format!("%{}%", escape_sql_like(clean_query));
        let row: Option<(String, String, i64, i64)> = sym_conn
            .query_row(
                "SELECT name, signature, start_line, end_line
                 FROM symbols
                 WHERE file_path = ?1
                   AND (name = ?2 OR signature = ?3 OR LOWER(name) LIKE LOWER(?4) ESCAPE '\\' OR LOWER(signature) LIKE LOWER(?4) ESCAPE '\\')
                 ORDER BY
                   CASE WHEN ?5 IS NOT NULL AND parent_name = ?5 THEN 0 ELSE 1 END,
                   CASE WHEN name = ?2 THEN 0 WHEN LOWER(name) = LOWER(?2) THEN 1 ELSE 2 END,
                   start_line ASC
                 LIMIT 1",
                params![&index_key, clean_query, sym_query.trim(), like_pat, parent_hint],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        let row = row.or_else(|| {
            fuzzy_find_symbols(sym_conn, Some(&index_key), clean_query, 1)
                .into_iter()
                .next()
                .map(|(_fp, _k, name, sig, st, en, _sc)| (name, sig, st, en))
        });

        let Some((name, sig, st, en)) = row else {
            bail!(
                "Symbol or heading '{}' not found in '{}'. Run `outline` first to see available symbols.",
                sym_query,
                index_key
            );
        };
        (st as usize, en as usize, format!("{name} ({sig})"))
    } else if let (Some(st), Some(en)) = (start_line, end_line) {
        let st_clamped = st.max(1).min(total_lines);
        let en_clamped = en.max(st_clamped).min(total_lines);
        (st_clamped, en_clamped, format!("lines {st_clamped}..{en_clamped}"))
    } else if let Some(st) = start_line {
        let st_clamped = st.max(1).min(total_lines);
        (st_clamped, total_lines, format!("lines {st_clamped}..{total_lines}"))
    } else if let Some(en) = end_line {
        let en_clamped = en.max(1).min(total_lines);
        (1, en_clamped, format!("lines 1..{en_clamped}"))
    } else {
        (1, total_lines, format!("lines 1..{total_lines}"))
    };

    // Only pad with context_lines for symbol lookups; explicit line-range reads stay exact
    let effective_context = if symbol.is_some() { context_lines } else { 0 };
    let actual_start = target_start.saturating_sub(effective_context).max(1);
    let requested_end = target_end.saturating_add(effective_context).min(total_lines);
    let max_cap = max_lines.max(20);
    let cap_span = if signatures_only {
        max_cap.saturating_mul(4)
    } else {
        max_cap
    };
    let actual_end = requested_end.min(actual_start.saturating_add(cap_span).saturating_sub(1));

    let file_type = IndexedFileType::classify(Path::new(&index_key));
    let is_code_file = matches!(file_type, Some(IndexedFileType::Code(_)));
    let hash_comments = matches!(
        file_type,
        Some(IndexedFileType::Config(ConfigFormat::Toml | ConfigFormat::Yaml))
            | Some(IndexedFileType::Code(CodeLanguage::Python))
    );

    // Collect AST function spans if `signatures_only` is active on a code file
    let fn_spans: Vec<(usize, usize)> = if signatures_only && is_code_file {
        let mut stmt = sym_conn.prepare(
            "SELECT start_line, end_line FROM symbols
             WHERE file_path = ?1 AND kind = 'fn' AND end_line > start_line + 2
             ORDER BY start_line ASC",
        )?;
        stmt.query_map(params![&index_key], |r| {
            Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
        })?
        .filter_map(|r| r.ok())
        .collect()
    } else {
        Vec::new()
    };

    let strip_all_comments = strip_comments || signatures_only;
    let mut out = format!(
        "# {index_key} [{actual_start}..{actual_end} of {total_lines}] — {header_label}{}\n",
        if signatures_only { " (signatures_only)" } else { "" }
    );
    let mut prev_blank = false;
    let mut line_no = actual_start;

    while line_no <= actual_end {
        if let Some(&(fn_start, fn_end)) = fn_spans.iter().find(|&&(s, e)| line_no >= s && line_no <= e) {
            if line_no == fn_start {
                // Output signature header lines until `{` (or `:` in Python), up to 6 lines
                let sig_limit = fn_start.saturating_add(5).min(fn_end).min(actual_end);
                let mut header_end = fn_start;
                for lno in fn_start..=sig_limit {
                    if let Some(lstr) = lines.get(lno - 1) {
                        out.push_str(&format!("{lno:>4} | {lstr}\n"));
                        header_end = lno;
                        let t = lstr.trim();
                        if t.ends_with('{') || t.ends_with(':') || t.ends_with(';') {
                            break;
                        }
                    }
                }
                if header_end < fn_end {
                    out.push_str(&format!(
                        "     |     // ... body omitted [L{}..L{}]\n",
                        header_end + 1,
                        fn_end
                    ));
                    if !hash_comments
                        && let Some(close_line) = lines.get(fn_end - 1)
                        && close_line.trim().starts_with('}')
                    {
                        out.push_str(&format!("{fn_end:>4} | {close_line}\n"));
                    }
                }
            } else {
                out.push_str(&format!(
                    "     |     // ... inside fn body [L{fn_start}..L{fn_end}] omitted\n"
                ));
            }
            prev_blank = false;
            line_no = fn_end + 1;
            continue;
        }

        if let Some(line_str) = lines.get(line_no - 1) {
            if strip_all_comments && is_non_doc_comment_line(line_str, hash_comments) {
                line_no += 1;
                continue;
            }
            if strip_all_comments && line_str.trim().is_empty() {
                if prev_blank {
                    line_no += 1;
                    continue;
                }
                prev_blank = true;
            } else {
                prev_blank = false;
            }
            out.push_str(&format!("{line_no:>4} | {line_str}\n"));
        }
        line_no += 1;
    }

    if actual_end < requested_end {
        let next_start = actual_end.saturating_add(1);
        let next_end = actual_end.saturating_add(max_cap).min(requested_end);
        out.push_str(&format!(
            "... [paginated at {max_cap} lines; call read_symbol(file=\"{index_key}\", start_line={next_start}, end_line={next_end}) for next slice]\n"
        ));
    }
    if !other_matches_note.is_empty() {
        out.push_str(&other_matches_note);
    }

    Ok(out)
}

fn clean_match_line(line: &str, max_len: usize, re: &regex::Regex) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= max_len {
        return trimmed.to_string();
    }
    if let Some(m) = re.find(trimmed) {
        let char_pos = trimmed[..m.start()].chars().count();
        let chars: Vec<char> = trimmed.chars().collect();
        let char_len = chars.len();
        let start = char_pos.saturating_sub(max_len / 3);
        let end = (start + max_len).min(char_len);
        let start = if end == char_len {
            end.saturating_sub(max_len)
        } else {
            start
        };
        let slice: String = chars[start..end].iter().collect();
        if start > 0 && end < char_len {
            format!("...{slice}...")
        } else if start > 0 {
            format!("...{slice}")
        } else {
            format!("{slice}...")
        }
    } else {
        let short: String = trimmed.chars().take(max_len.saturating_sub(3)).collect();
        format!("{short}...")
    }
}

/// Tool 6: `smart_grep` — file-grouped regex search with enclosing AST/Heading context,
/// `path` directory/file filter, `files_only` (`rg -l`), `case_insensitive`, `whole_word` (`rg -w`), and `max_per_file` cap.
#[allow(clippy::too_many_arguments)]
pub fn smart_grep_search(
    conn: &Connection,
    paths: &AppPaths,
    pattern: &str,
    glob_filter: Option<&str>,
    path_filter: Option<&str>,
    files_only: bool,
    case_insensitive: bool,
    whole_word: bool,
    max_per_file: usize,
    max_results: usize,
) -> Result<String> {
    if pattern.is_empty() {
        bail!("Provide a non-empty search `pattern` to `smart_grep`");
    }

    let raw_pat = if whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern.to_string()
    };
    let escaped_pat = if whole_word {
        format!(r"\b{}\b", regex::escape(pattern))
    } else {
        regex::escape(pattern)
    };
    let re = RegexBuilder::new(&raw_pat)
        .case_insensitive(case_insensitive)
        .build()
        .or_else(|_| {
            RegexBuilder::new(&escaped_pat)
                .case_insensitive(case_insensitive)
                .build()
        })
        .context("Invalid search pattern")?;

    let glob_matcher = match glob_filter {
        Some(g) if !g.trim().is_empty() => Some(
            GlobBuilder::new(g.trim())
                .literal_separator(false)
                .build()
                .context("Invalid glob filter")?
                .compile_matcher(),
        ),
        _ => None,
    };

    let allowed_docs = paths.all_doc_paths();
    let trimmed_pf = path_filter.map(|s| s.trim()).filter(|s| !s.is_empty());
    if trimmed_pf.is_none() && is_unsafe_workspace_root(&paths.project_root) {
        bail!(
            "Refusing to run `smart_grep` across unsafe root '{}' (HOME or filesystem root). Pass an explicit `path` or run inside a project directory.",
            paths.project_root.display()
        );
    }

    let mut local_files = Vec::new();
    let mut ext_files = Vec::new();

    if let Some(pf) = trimmed_pf {
        let mut explicit_roots = Vec::new();
        let mut last_err: Option<anyhow::Error> = None;
        for token in pf.split(|c: char| c == ',' || c.is_whitespace()) {
            let t = token.trim();
            if t.is_empty() {
                continue;
            }
            match validate_path(&paths.project_root, &allowed_docs, Path::new(t)) {
                Ok(valid) if valid.exists() => explicit_roots.push(valid),
                Ok(valid) => {
                    last_err = Some(anyhow::anyhow!("Path '{}' does not exist", valid.display()));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        if explicit_roots.is_empty() {
            if let Some(err) = last_err {
                return Err(err);
            }
            bail!("No valid existing path found for `path` filter '{pf}'.");
        }
        for root in explicit_roots {
            collect_grep_files_from_root(&root, &mut local_files);
        }
        local_files.sort();
        local_files.dedup();
    } else {
        collect_grep_files_from_root(&paths.project_root, &mut local_files);
        local_files.sort();
        local_files.dedup();

        for src in &allowed_docs {
            if src.exists() {
                collect_grep_files_from_root(src, &mut ext_files);
            }
        }
        ext_files.sort();
        ext_files.dedup();
    }

    let mut seen_paths = HashSet::with_capacity(local_files.len() + ext_files.len());
    let all_files: Vec<PathBuf> = local_files
        .into_iter()
        .chain(ext_files)
        .filter(|p| seen_paths.insert(p.clone()))
        .collect();

    // Group matches by file (RTK-style) with per-file cap
    let mut grouped: Vec<(String, usize, Vec<String>)> = Vec::new();
    let mut total_matches = 0usize;
    let mut total_shown = 0usize;
    let per_file_cap = max_per_file.max(1);
    let max_results = max_results.max(1);
    let mut gconn_cache: Option<Connection> = None;

    for file_path in all_files {
        if !files_only && total_shown >= max_results {
            break;
        }
        let index_key = to_index_path(&paths.project_root, &file_path);
        if let Some(ref matcher) = glob_matcher {
            let fname = file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if !matcher.is_match(&index_key) && !matcher.is_match(fname) {
                continue;
            }
        }

        let Ok(raw_content) = fs::read_to_string(&file_path) else {
            continue;
        };

        let mut file_match_count = 0usize;
        let mut file_lines = Vec::new();
        let mut file_sym_key = index_key.clone();
        let mut file_in_global = false;

        for (idx, line) in raw_content.lines().enumerate() {
            if re.is_match(line) {
                if file_match_count == 0
                    && !files_only
                    && IndexedFileType::classify(&file_path).is_some()
                    && let Ok((sym_key, _, is_global)) =
                        ensure_single_file_indexed(conn, paths, &index_key)
                {
                    file_sym_key = sym_key;
                    file_in_global = is_global;
                }
                file_match_count += 1;
                total_matches += 1;

                if files_only || file_lines.len() >= per_file_cap || total_shown >= max_results {
                    continue;
                }

                let sym_conn = if file_in_global {
                    if gconn_cache.is_none() && paths.global_db.exists() {
                        gconn_cache = init_connection(&paths.global_db).ok();
                    }
                    gconn_cache.as_ref().unwrap_or(conn)
                } else {
                    conn
                };

                let line_no = (idx + 1) as i64;
                let enclosing: Option<(String, String, String, i64, i64)> = sym_conn
                    .query_row(
                        "SELECT kind, name, signature, start_line, end_line
                         FROM symbols
                         WHERE file_path = ?1 AND start_line <= ?2 AND end_line >= ?2
                         ORDER BY (end_line - start_line) ASC
                         LIMIT 1",
                        params![&file_sym_key, line_no],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()?;

                let ctx_tag = if let Some((kind, name, sig, st, en)) = enclosing {
                    if kind == "md_heading" {
                        format!(" [in {sig} ({st}..{en})]")
                    } else {
                        format!(" [in {kind} {name} ({st}..{en})]")
                    }
                } else {
                    String::new()
                };

                let compact_line = clean_match_line(line, 150, &re);
                file_lines.push(format!("  {line_no}{ctx_tag} -> {compact_line}"));
                total_shown += 1;
            }
        }

        if file_match_count > 0 {
            grouped.push((index_key, file_match_count, file_lines));
            if files_only && grouped.len() >= max_results {
                break;
            }
        }
    }

    let out = if grouped.is_empty() {
        format!("No matches found for pattern '{pattern}'.\n")
    } else if files_only {
        let header = format!(
            "# smart_grep ({total_matches} matches in {} files for `{pattern}`)\n",
            grouped.len()
        );
        let mut flat = header.clone();
        for (fp, cnt, _) in &grouped {
            flat.push_str(&format!("- {fp} ({cnt} matches)\n"));
        }

        // RTK-style directory prefix folding under never_worse guard
        let mut dir_groups: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
        for (fp, cnt, _) in &grouped {
            if let Some(slash_idx) = fp.rfind('/') {
                let dir = &fp[..slash_idx];
                let fname = &fp[slash_idx + 1..];
                dir_groups
                    .entry(dir.to_string())
                    .or_default()
                    .push((fname.to_string(), *cnt));
            } else {
                dir_groups
                    .entry(".".to_string())
                    .or_default()
                    .push((fp.clone(), *cnt));
            }
        }

        let mut folded = header;
        for (dir, items) in &dir_groups {
            if items.len() == 1 && dir != "." {
                let (fname, cnt) = &items[0];
                folded.push_str(&format!("- {dir}/{fname} ({cnt} matches)\n"));
            } else if dir == "." {
                for (fname, cnt) in items {
                    folded.push_str(&format!("- {fname} ({cnt} matches)\n"));
                }
            } else {
                let compact_items: Vec<String> = items
                    .iter()
                    .map(|(f, c)| format!("{f} ({c})"))
                    .collect();
                folded.push_str(&format!(
                    "- {dir}/ ({} files): {}\n",
                    items.len(),
                    compact_items.join(", ")
                ));
            }
        }

        if folded.len() <= flat.len() {
            folded
        } else {
            flat
        }
    } else {
        let mut s = format!(
            "# smart_grep ({total_matches} matches across {} files, showing {total_shown} for `{pattern}`)\n",
            grouped.len()
        );
        for (fp, cnt, lines) in grouped {
            s.push_str(&format!("{fp}:\n"));
            for l in &lines {
                s.push_str(l);
                s.push('\n');
            }
            if cnt > lines.len() {
                let already_scoped = trimmed_pf
                    .is_some_and(|pf| pf == fp || pf.trim_start_matches("./") == fp);
                if already_scoped {
                    s.push_str(&format!(
                        "  +{} more in {fp} (increase `max_per_file`)\n",
                        cnt - lines.len()
                    ));
                } else {
                    s.push_str(&format!(
                        "  +{} more in {fp} (use `path=\"{fp}\"`)\n",
                        cnt - lines.len()
                    ));
                }
            }
        }
        s
    };

    Ok(out)
}

const MAX_REFERENCES_SHOWN: usize = 80;

/// Finds all AST references (`call`, `type_use`, `import`) to `symbol` across the workspace.
pub fn find_symbol_references(
    conn: &mut Connection,
    paths: &AppPaths,
    symbol: &str,
) -> Result<String> {
    let raw_trimmed = symbol.trim();
    let clean = raw_trimmed
        .rsplit("::")
        .next()
        .unwrap_or(raw_trimmed)
        .rsplit('.')
        .next()
        .unwrap_or(raw_trimmed)
        .trim();
    if clean.is_empty() {
        bail!("Provide a non-empty `symbol` to `find_references`");
    }

    index_workspace(conn, paths, None)?;

    let mut stmt = conn.prepare(
        "SELECT file_path, line_number, caller_symbol, ref_kind
         FROM symbol_refs
         WHERE symbol_name = ?1
         ORDER BY
           CASE WHEN file_path LIKE '/%' THEN 1 ELSE 0 END ASC,
           CASE WHEN ref_kind = 'import' THEN 0 ELSE 1 END ASC,
           file_path ASC,
           line_number ASC",
    )?;

    let rows: Vec<(String, i64, Option<String>, String)> = stmt
        .query_map(params![clean], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        let mut suggestions: Vec<String> = fuzzy_find_symbols(conn, None, clean, 5)
            .into_iter()
            .filter(|(_, k, _, _, _, _, _)| k != "md_heading")
            .map(|(_, _, name, _, _, _, _)| name)
            .collect();
        suggestions.dedup();
        let hint = if suggestions.is_empty() {
            String::new()
        } else {
            format!(" Did you mean: `{}`?", suggestions.join("`, `"))
        };
        return Ok(format!("No references found for symbol '{clean}'.{hint}"));
    }

    let defs: Vec<String> = {
        let mut def_stmt = conn.prepare(
            "SELECT file_path, kind, start_line, end_line
             FROM symbols
             WHERE name = ?1 AND kind != 'md_heading'
             ORDER BY CASE WHEN file_path LIKE '/%' THEN 1 ELSE 0 END ASC, file_path ASC, start_line ASC
             LIMIT 3",
        )?;
        def_stmt
            .query_map(params![clean], |r| {
                Ok(format!(
                    "{}:{}..{} ({})",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(1)?
                ))
            })?
            .filter_map(|r| r.ok())
            .collect()
    };

    let mut out = if defs.is_empty() {
        format!("{clean} referenced in {} location(s):\n", rows.len())
    } else {
        format!(
            "{clean} referenced in {} location(s) (defined at: {}):\n",
            rows.len(),
            defs.join(", ")
        )
    };

    let mut hop1_callers: Vec<String> = Vec::new();
    for (idx, (fp, line, caller, kind)) in rows.iter().enumerate() {
        if let Some(c) = caller
            && c != clean
            && !hop1_callers.contains(c)
            && hop1_callers.len() < 12
        {
            hop1_callers.push(c.clone());
        }
        if idx >= MAX_REFERENCES_SHOWN {
            continue;
        }
        match (kind.as_str(), caller.as_deref()) {
            ("import", None) => {
                out.push_str(&format!("- {fp}:{line} [import]\n"));
            }
            ("import", Some(c)) => {
                out.push_str(&format!("- {fp}:{line} [in fn {c}] [import]\n"));
            }
            (_, Some(c)) => {
                out.push_str(&format!("- {fp}:{line} [in fn {c}] ({kind})\n"));
            }
            (_, None) => {
                out.push_str(&format!("- {fp}:{line} ({kind})\n"));
            }
        }
    }
    if rows.len() > MAX_REFERENCES_SHOWN {
        out.push_str(&format!(
            "... (+{} more references truncated)\n",
            rows.len() - MAX_REFERENCES_SHOWN
        ));
    }

    // 2-hop call graph impact: find callers of the 1-hop enclosing caller functions
    if !hop1_callers.is_empty()
        && let Ok(mut hop2_stmt) = conn.prepare(
            "SELECT file_path, line_number, caller_symbol
             FROM symbol_refs
             WHERE symbol_name = ?1 AND ref_kind = 'call'
             ORDER BY CASE WHEN file_path LIKE '/%' THEN 1 ELSE 0 END ASC, file_path ASC, line_number ASC
             LIMIT 5",
        )
    {
        let mut hop2_lines = Vec::new();
        for c1 in &hop1_callers {
            if let Ok(h2_rows) = hop2_stmt.query_map(params![c1], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            }) {
                for (fp2, ln2, caller2) in h2_rows.flatten() {
                    let via = match caller2.as_deref() {
                        Some(c2) => format!("- {fp2}:{ln2} [in fn {c2}] -> calls {c1}\n"),
                        None => format!("- {fp2}:{ln2} -> calls {c1}\n"),
                    };
                    if !hop2_lines.contains(&via) && hop2_lines.len() < 15 {
                        hop2_lines.push(via);
                    }
                }
            }
        }
        if !hop2_lines.is_empty() {
            out.push_str("2-hop caller impact:\n");
            for l in hop2_lines {
                out.push_str(&l);
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_connection;

    #[test]
    fn test_update_registered_docs_incremental() {
        let temp = std::env::temp_dir().join(format!("lee_docs_upd_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let ws_doc = temp.join("ws_doc");
        let gl_doc = temp.join("gl_doc");
        fs::create_dir_all(&ws_doc).unwrap();
        fs::create_dir_all(&gl_doc).unwrap();

        fs::write(ws_doc.join("a.mdx"), "# Alpha\nInitial alpha content.\n").unwrap();
        fs::write(ws_doc.join("old.mdx"), "# Old\nTo be deleted.\n").unwrap();
        fs::write(gl_doc.join("g.mdx"), "# Global\nStable global doc.\n").unwrap();

        let paths = AppPaths::from_root_with_global_dir(temp.clone(), temp.join("global_cfg"));
        paths.ensure_local_dirs().unwrap();
        paths.ensure_global_dirs().unwrap();

        let ws_entry = paths.add_doc(DocScope::Workspace, &ws_doc, "ws docs").unwrap();
        let gl_entry = paths.add_doc(DocScope::Global, &gl_doc, "gl docs").unwrap();
        let docs = vec![
            (DocScope::Workspace, ws_entry),
            (DocScope::Global, gl_entry),
        ];

        let mut conn = init_connection(&paths.local_db).unwrap();

        // First sync: workspace doc goes to local_db, global doc goes to global_db (zero duplication!)
        let first = update_registered_docs(Some(&mut conn), &paths, &docs).unwrap();
        assert_eq!(first[0].added, 2);
        assert_eq!(first[1].added, 1);

        let local_gl_cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE file_path LIKE '%g.mdx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(local_gl_cnt, 0, "global doc must not be duplicated in local_db");

        let gconn = init_connection(&paths.global_db).unwrap();
        let global_gl_cnt: i64 = gconn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE file_path LIKE '%g.mdx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(global_gl_cnt, 1, "global doc must live in global_db");

        // Second sync with no changes: both docs skipped
        let second = update_registered_docs(Some(&mut conn), &paths, &docs).unwrap();
        assert!(!second[0].has_changes());
        assert!(!second[1].has_changes());

        // Modify a.mdx, add b.mdx, remove old.mdx in ws_doc only; leave gl_doc untouched
        fs::write(ws_doc.join("a.mdx"), "# Alpha\nUpdated alpha content v2.\n").unwrap();
        fs::write(ws_doc.join("b.mdx"), "# Beta\nBrand new file.\n").unwrap();
        fs::remove_file(ws_doc.join("old.mdx")).unwrap();

        let third = update_registered_docs(Some(&mut conn), &paths, &docs).unwrap();
        assert_eq!(third[0].added, 1);
        assert_eq!(third[0].updated, 1);
        assert_eq!(third[0].removed, 1);
        assert!(!third[1].has_changes());

        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM doc_sections_meta WHERE file_path LIKE '%old.mdx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);

        drop(gconn);
        drop(conn);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_registered_doc_code_files_multilang_indexed_full() {
        let temp = std::env::temp_dir().join(format!("lee_doc_code_full_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let docs_dir = temp.join("ext_docs");
        fs::create_dir_all(&docs_dir).unwrap();

        let mut big_rs = String::from("// Long Rust doc example\npub fn compute_widget() -> usize {\n");
        for i in 1..=160 {
            big_rs.push_str(&format!("    let step_{i} = {i};\n"));
        }
        big_rs.push_str("    step_160\n}\n");
        let py_code = "def greet_user(name: str) -> str:\n    return f'hello {name}'\n";
        let cpp_code = "int calc_sum(int a, int b) {\n    return a + b;\n}\n";

        fs::write(docs_dir.join("widget.rs"), &big_rs).unwrap();
        fs::write(docs_dir.join("helper.py"), py_code).unwrap();
        fs::write(docs_dir.join("math.cpp"), cpp_code).unwrap();

        let paths = AppPaths::from_root_with_global_dir(temp.clone(), temp.join("global_cfg"));
        paths.ensure_local_dirs().unwrap();
        paths.ensure_global_dirs().unwrap();
        paths.add_doc(DocScope::Workspace, &docs_dir, "sdk").unwrap();

        let mut conn = init_connection(&paths.local_db).unwrap();
        let rs_out = crate::docs::search_docs(
            &mut conn,
            &paths,
            "compute_widget",
            None,
            Some("sdk"),
            Some("workspace"),
            400, // smaller than big_rs, yet top match must stay 100% intact
        )
        .unwrap();
        assert!(rs_out.contains("let step_1 = 1;"));
        assert!(rs_out.contains("step_160\n}"));
        assert!(!rs_out.contains("truncated to token budget"));

        let py_out = crate::docs::search_docs(
            &mut conn,
            &paths,
            "greet_user",
            None,
            Some("sdk"),
            Some("workspace"),
            800,
        )
        .unwrap();
        assert!(py_out.contains("def greet_user"));

        let cpp_out = crate::docs::search_docs(
            &mut conn,
            &paths,
            "calc_sum",
            None,
            Some("sdk"),
            Some("workspace"),
            800,
        )
        .unwrap();
        assert!(cpp_out.contains("int calc_sum"));

        drop(conn);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_pagerank_project_map_fuzzy_read_symbol_and_2hop_references() {
        let temp = std::env::temp_dir().join(format!("lee_alg_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let src_dir = temp.join("src");
        fs::create_dir_all(&src_dir).unwrap();

        // z_core.rs is alphabetically last, but referenced by both a_leaf.rs and b_mid.rs -> PageRank must rank z_core.rs first!
        fs::write(
            src_dir.join("z_core.rs"),
            "pub fn execute_core_engine() -> i32 {\n    42\n}\n",
        )
        .unwrap();
        fs::write(
            src_dir.join("b_mid.rs"),
            "use crate::z_core::execute_core_engine;\npub fn run_middle_stage() -> i32 {\n    execute_core_engine()\n}\n",
        )
        .unwrap();
        fs::write(
            src_dir.join("a_leaf.rs"),
            "use crate::z_core::execute_core_engine;\nuse crate::b_mid::run_middle_stage;\npub fn top_entry_handler() -> i32 {\n    let _ = execute_core_engine();\n    run_middle_stage()\n}\n",
        )
        .unwrap();

        let paths = AppPaths::from_root_with_global_dir(temp.clone(), temp.join("global_cfg"));
        paths.ensure_local_dirs().unwrap();
        paths.ensure_global_dirs().unwrap();

        let mut conn = init_connection(&paths.local_db).unwrap();

        // 1. PageRank puts src/z_core.rs ahead of src/a_leaf.rs despite alphabetical order
        let map_out = build_project_map(&mut conn, &paths, None, 800).unwrap();
        let pos_z = map_out.find("src/z_core.rs").unwrap();
        let pos_a = map_out.find("src/a_leaf.rs").unwrap();
        assert!(pos_z < pos_a, "Expected PageRank to rank src/z_core.rs before src/a_leaf.rs:\n{map_out}");

        // 2. Fuzzy subsequence symbol lookup resolves "exec_core_eng" -> "execute_core_engine"
        let sym_out = read_symbol_range(
            &mut conn,
            &paths,
            None,
            Some("exec_core_eng"),
            None,
            None,
            1,
            false,
            false,
            100,
        )
        .unwrap();
        assert!(sym_out.contains("pub fn execute_core_engine"));

        // 3. 2-hop call graph impact: execute_core_engine -> called by run_middle_stage -> called by top_entry_handler
        let refs_out = find_symbol_references(&mut conn, &paths, "execute_core_engine").unwrap();
        assert!(refs_out.contains("[in fn run_middle_stage] (call)"));
        assert!(refs_out.contains("2-hop caller impact:"));
        assert!(refs_out.contains("[in fn top_entry_handler] -> calls run_middle_stage"));

        drop(conn);
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_unsafe_workspace_root_and_oversized_file_guards() {
        assert!(is_unsafe_workspace_root(Path::new("/")));
        if let Some(home) = dirs::home_dir() {
            assert!(is_unsafe_workspace_root(&home));
        }
        if let Some(cfg) = dirs::config_dir() {
            assert!(is_unsafe_workspace_root(&cfg));
        }

        let temp = std::env::temp_dir().join(format!("lee_guard_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        assert!(!is_unsafe_workspace_root(&temp));

        // Oversized file (> 2 MB) must be skipped by collect_workspace_files
        fs::write(temp.join("small.rs"), "pub fn ok() {}\n").unwrap();
        let huge = "a".repeat((MAX_INDEX_FILE_BYTES as usize) + 1024);
        fs::write(temp.join("huge.json"), &huge).unwrap();

        let files = collect_workspace_files(&temp, &[]);
        assert!(files.iter().any(|p| p.ends_with("small.rs")));
        assert!(!files.iter().any(|p| p.ends_with("huge.json")));

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_read_symbol_exact_range_and_smart_grep_whole_word() {
        let temp = std::env::temp_dir().join(format!("lee_ux_fix_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(temp.join("src")).unwrap();
        fs::create_dir_all(temp.join("src2")).unwrap();

        let mut big = String::new();
        for i in 1..=220 {
            big.push_str(&format!("// line {i}\n"));
        }
        fs::write(temp.join("src/long.rs"), &big).unwrap();
        fs::write(
            temp.join("src/words.rs"),
            "use ort::Session;\nfn sort_report() { let import = 1; }\nfn real_ort() { let ort_val = ort::init(); }\n",
        )
        .unwrap();
        fs::write(temp.join("src2/other.rs"), "pub fn other() {}\n").unwrap();

        let paths = AppPaths::from_root_with_global_dir(temp.clone(), temp.join("global_cfg"));
        paths.ensure_local_dirs().unwrap();

        let mut conn = init_connection(&paths.local_db).unwrap();

        // Explicit start_line=1, end_line=180 must NOT add context_lines=2 or bogus pagination
        let exact_out = read_symbol_range(
            &mut conn,
            &paths,
            Some("src/long.rs"),
            None,
            Some(1),
            Some(180),
            2,
            false,
            false,
            180,
        )
        .unwrap();
        assert!(exact_out.contains("[1..180 of 220]"));
        assert!(!exact_out.contains("paginated at"));

        // end_line alone reads lines 1..=end_line
        let head_out = read_symbol_range(
            &mut conn,
            &paths,
            Some("src/long.rs"),
            None,
            None,
            Some(10),
            2,
            false,
            false,
            180,
        )
        .unwrap();
        assert!(head_out.contains("[1..10 of 220]"));

        // Manual start_line=1 without end_line paginates cleanly with next slice 181..220
        let paged_out = read_symbol_range(
            &mut conn,
            &paths,
            Some("src/long.rs"),
            None,
            Some(1),
            None,
            2,
            false,
            false,
            180,
        )
        .unwrap();
        assert!(paged_out.contains("start_line=181, end_line=220"));

        // whole_word=true matches `ort` without matching `sort_report` or `import`
        let grep_out = smart_grep_search(
            &conn,
            &paths,
            "ort",
            None,
            Some("src/words.rs"),
            false,
            false,
            true,
            10,
            30,
        )
        .unwrap();
        assert!(grep_out.contains("use ort::Session;"));
        assert!(grep_out.contains("ort::init()"));
        assert!(!grep_out.contains("sort_report"));

        // Invalid `path` in smart_grep must return an error instead of falling back to full workspace
        assert!(smart_grep_search(
            &conn,
            &paths,
            "ort",
            None,
            Some("does_not_exist"),
            false,
            false,
            false,
            10,
            30,
        )
        .is_err());

        // project_map subpath="src" must not match sibling "src2/other.rs", and "./" maps whole project
        let src_map = build_project_map(&mut conn, &paths, Some("src"), 800).unwrap();
        assert!(src_map.contains("src/words.rs"));
        assert!(!src_map.contains("src2/other.rs"));
        let dot_slash_map = build_project_map(&mut conn, &paths, Some("./"), 800).unwrap();
        assert!(dot_slash_map.contains("src2/other.rs"));

        // purge_source_from_db handles in-repo workspace docs and SQL LIKE wildcards (`_`)
        fs::create_dir_all(temp.join("a_b")).unwrap();
        fs::create_dir_all(temp.join("axb")).unwrap();
        fs::write(temp.join("a_b/one.md"), "# One\n").unwrap();
        fs::write(temp.join("axb/two.md"), "# Two\n").unwrap();
        index_workspace(&mut conn, &paths, None).unwrap();
        let purged = purge_source_from_db(&conn, Some(&paths.project_root), &temp.join("a_b")).unwrap();
        assert_eq!(purged, 1);
        let axb_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM file_hashes WHERE file_path = 'axb/two.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(axb_left, 1);

        drop(conn);
        let _ = fs::remove_dir_all(&temp);
    }
}

