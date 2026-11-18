pub mod languages;
pub mod markdown;
pub mod parser;

use crate::config::AppPaths;
use crate::db::now_unix;
use crate::indexer::languages::CodeLanguage;
use crate::indexer::markdown::ExtractedSymbol;
use crate::indexer::parser::{is_non_doc_comment_line, parse_file_content, ConfigFormat, IndexedFileType};
use crate::memory::embedder::{
    cap_passage_chars, embed_doc_section_passage, is_model_cached, serialize_f32_vec,
    try_embed_batch, MAX_PASSAGE_CHARS,
};
use crate::security::{is_denied_path, to_index_path, validate_path};
use anyhow::{bail, Context, Result};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub struct IndexStats {
    pub files_scanned: usize,
    pub files_reindexed: usize,
    pub files_cached: usize,
    pub code_symbols: usize,
    pub md_headings: usize,
    pub doc_sections: usize,
    pub total_raw_bytes: usize,
    pub elapsed_ms: u128,
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".kaye" | ".lee" | ".notes" | "target" | "node_modules" | "dist" | "build"
    )
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
            if !is_denied_path(&root) && IndexedFileType::classify(&root).is_some() {
                found.lock().unwrap().push(root);
            }
            continue;
        }
        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(true)
            .filter_entry(|entry| {
                if let Some(name) = entry.file_name().to_str() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                        && should_skip_dir(name)
                    {
                        return false;
                    }
                }
                !is_denied_path(entry.path())
            })
            .build_parallel();

        walker.run(|| {
            let found_ref = &found;
            Box::new(move |res| {
                if let Ok(entry) = res {
                    if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                        let p = entry.into_path();
                        if !is_denied_path(&p) && IndexedFileType::classify(&p).is_some() {
                            if let Ok(mut guard) = found_ref.lock() {
                                guard.push(p);
                            }
                        }
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

/// Removes all indexed files, symbols, and doc sections belonging to a removed external source path.
pub fn purge_source_from_db(conn: &Connection, source_path: &Path) -> Result<usize> {
    let prefix = source_path.to_string_lossy().replace('\\', "/");
    let like_pat = format!("{}/%", prefix.trim_end_matches('/'));
    let deleted = conn.execute(
        "DELETE FROM file_hashes WHERE file_path = ?1 OR file_path LIKE ?2",
        params![prefix, like_pat],
    )?;
    Ok(deleted)
}

/// Scans and lazily indexes all supported code and Markdown files in `subpath` + `sources`.
pub fn index_workspace(
    conn: &mut Connection,
    paths: &AppPaths,
    subpath: Option<&str>,
) -> Result<IndexStats> {
    let start_instant = Instant::now();
    let cfg = paths.load_config().unwrap_or_default();

    let scan_root = match subpath {
        Some(sp) if !sp.is_empty() && sp != "." => {
            validate_path(&paths.project_root, &cfg.sources, Path::new(sp))?
        }
        _ => paths.project_root.clone(),
    };

    let include_sources = subpath.is_none() || subpath == Some(".");
    let extra_sources = if include_sources {
        cfg.sources.as_slice()
    } else {
        &[]
    };

    let files = collect_workspace_files(&scan_root, extra_sources);
    let mut stats = IndexStats::default();
    let mut seen_keys: HashSet<String> = HashSet::with_capacity(files.len());

    let tx = conn.transaction()?;

    for file_path in &files {
        let Ok(raw_content) = fs::read_to_string(file_path) else {
            continue;
        };
        stats.files_scanned += 1;
        stats.total_raw_bytes += raw_content.len();

        let index_key = to_index_path(&paths.project_root, file_path);
        seen_keys.insert(index_key.clone());
        let raw_hash = blake3::hash(raw_content.as_bytes()).to_hex().to_string();
        let new_hash = match IndexedFileType::classify(file_path) {
            Some(IndexedFileType::Markdown) => format!("{raw_hash}:md2"),
            Some(IndexedFileType::Code(_)) => format!("{raw_hash}:r1"),
            _ => raw_hash,
        };

        let existing_hash: Option<String> = tx
            .query_row(
                "SELECT blake3_hash FROM file_hashes WHERE file_path = ?1",
                params![&index_key],
                |row| row.get(0),
            )
            .optional()?;

        if existing_hash.as_deref() == Some(new_hash.as_str()) {
            stats.files_cached += 1;
            continue;
        }

        if let Some(parsed) = parse_file_content(file_path, &raw_content)? {
            stats.files_reindexed += 1;
            upsert_parsed_file(&tx, &index_key, &new_hash, &parsed)?;
        }
    }

    // If this was a full workspace scan, prune any files from SQLite that were deleted/reverted on disk
    if include_sources {
        let existing_db_paths: Vec<String> = {
            let mut stmt = tx.prepare("SELECT file_path FROM file_hashes")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            rows
        };
        for db_path in existing_db_paths {
            if !seen_keys.contains(&db_path) {
                tx.execute(
                    "DELETE FROM file_hashes WHERE file_path = ?1",
                    params![&db_path],
                )?;
            }
        }
    }

    tx.commit()?;

    if is_model_cached(&paths.models_dir) {
        let _ = backfill_missing_doc_embeddings(conn, &paths.models_dir);
    }

    // Populate total counts from DB
    stats.code_symbols = conn
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE kind != 'md_heading'",
            [],
            |r| r.get::<_, i64>(0),
        )
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

/// Computes BGE embeddings for all `doc_sections_meta` rows where `embedding IS NULL`.
/// Uses length-sorted batching (`batch_size = 32`, `max_length = 128`) and CUDA->CPU ExecutionProvider
/// to eliminate padding overhead and maximize throughput.
pub fn backfill_missing_doc_embeddings(conn: &mut Connection, models_dir: &Path) -> Result<usize> {
    if !is_model_cached(models_dir) {
        return Ok(0);
    }
    let mut missing: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, file_path, heading, content FROM doc_sections_meta WHERE embedding IS NULL",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let id: i64 = r.get(0)?;
                let fp: String = r.get(1)?;
                let heading: String = r.get(2)?;
                let content: String = r.get(3)?;
                let snippet = cap_passage_chars(&content, MAX_PASSAGE_CHARS);
                let passage = format!("passage: {fp} > {heading} - {snippet}");
                Ok((id, passage))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    if missing.is_empty() {
        return Ok(0);
    }

    // Sort by byte length so every batch of 32 has near-identical token length (zero padding waste)
    missing.sort_by_key(|(_, text)| text.len());

    let mut updated = 0usize;
    for super_chunk in missing.chunks(256) {
        let mut computed_pairs: Vec<(i64, Vec<u8>)> = Vec::with_capacity(super_chunk.len());
        for batch in super_chunk.chunks(32) {
            let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
            let Some(vecs) = try_embed_batch(models_dir, texts) else {
                break;
            };
            for ((id, _), vec) in batch.iter().zip(vecs.into_iter()) {
                computed_pairs.push((*id, serialize_f32_vec(&vec)));
            }
        }
        if computed_pairs.is_empty() {
            break;
        }
        let tx = conn.transaction()?;
        for (id, blob) in computed_pairs {
            tx.execute(
                "UPDATE doc_sections_meta SET embedding = ?1 WHERE id = ?2",
                params![blob, id],
            )?;
            updated += 1;
        }
        tx.commit()?;
    }
    Ok(updated)
}

fn upsert_parsed_file(
    conn: &Connection,
    index_key: &str,
    blake3_hash: &str,
    parsed: &parser::ParsedFileResult,
) -> Result<()> {
    conn.execute(
        "DELETE FROM file_hashes WHERE file_path = ?1",
        params![index_key],
    )?;

    conn.execute(
        "INSERT INTO file_hashes (file_path, blake3_hash, line_count, file_type, last_indexed)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            index_key,
            blake3_hash,
            parsed.line_count as i64,
            parsed.file_type.as_db_str(),
            now_unix()
        ],
    )?;

    for sym in &parsed.symbols {
        conn.execute(
            "INSERT INTO symbols (file_path, name, kind, signature, start_line, end_line, parent_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                index_key,
                sym.name,
                sym.kind,
                sym.signature,
                sym.start_line as i64,
                sym.end_line as i64,
                sym.parent_name,
            ],
        )?;
    }

    for r in &parsed.symbol_refs {
        conn.execute(
            "INSERT INTO symbol_refs (symbol_name, file_path, caller_symbol, line_number, ref_kind)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                r.symbol_name,
                index_key,
                r.caller_symbol,
                r.line_number as i64,
                r.ref_kind,
            ],
        )?;
    }

    for sec in &parsed.doc_sections {
        conn.execute(
            "INSERT INTO doc_sections_meta (file_path, heading, start_line, end_line, content, embedding)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![
                index_key,
                sec.heading,
                sec.start_line as i64,
                sec.end_line as i64,
                sec.content,
            ],
        )?;
    }

    Ok(())
}

/// Lazily indexes a single file if its `blake3` hash is missing or changed.
/// Returns `(index_key, raw_content, raw_bytes)`.
pub fn ensure_single_file_indexed(
    conn: &Connection,
    paths: &AppPaths,
    file_arg: &str,
) -> Result<(String, String, usize)> {
    let mut cfg = paths.load_config().unwrap_or_default();
    if let Ok(mut stmt) = conn.prepare("SELECT file_path FROM file_hashes WHERE file_path LIKE '/%'")
    {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for fp in rows.flatten() {
                if let Some(parent) = Path::new(&fp).parent() {
                    let pb = parent.to_path_buf();
                    if !cfg.sources.contains(&pb) {
                        cfg.sources.push(pb);
                    }
                }
            }
        }
    }

    let resolved = resolve_file_with_sources(conn, &paths.project_root, &cfg.sources, file_arg)?;
    let raw_content = fs::read_to_string(&resolved)
        .with_context(|| format!("Failed to read file '{}'", resolved.display()))?;
    let raw_bytes = raw_content.len();
    let index_key = to_index_path(&paths.project_root, &resolved);
    let raw_hash = blake3::hash(raw_content.as_bytes()).to_hex().to_string();
    let new_hash = match IndexedFileType::classify(&resolved) {
        Some(IndexedFileType::Markdown) => format!("{raw_hash}:md2"),
        Some(IndexedFileType::Code(_)) => format!("{raw_hash}:r1"),
        _ => raw_hash,
    };

    let existing_hash: Option<String> = conn
        .query_row(
            "SELECT blake3_hash FROM file_hashes WHERE file_path = ?1",
            params![&index_key],
            |row| row.get(0),
        )
        .optional()?;

    if existing_hash.as_deref() != Some(new_hash.as_str()) {
        if let Some(parsed) = parse_file_content(&resolved, &raw_content)? {
            upsert_parsed_file(conn, &index_key, &new_hash, &parsed)?;
            if parsed.file_type == IndexedFileType::Markdown && is_model_cached(&paths.models_dir) {
                for sec in &parsed.doc_sections {
                    if let Some(vec) = embed_doc_section_passage(
                        &paths.models_dir,
                        &index_key,
                        &sec.heading,
                        &sec.content,
                    ) {
                        let blob = serialize_f32_vec(&vec);
                        let _ = conn.execute(
                            "UPDATE doc_sections_meta SET embedding = ?1 WHERE file_path = ?2 AND start_line = ?3",
                            params![blob, &index_key, sec.start_line as i64],
                        );
                    }
                }
            }
        }
    }

    Ok((index_key, raw_content, raw_bytes))
}

fn resolve_file_with_sources(
    conn: &Connection,
    project_root: &Path,
    sources: &[PathBuf],
    file_arg: &str,
) -> Result<PathBuf> {
    let candidate = Path::new(file_arg);
    if let Ok(valid) = validate_path(project_root, sources, candidate) {
        if valid.exists() {
            return Ok(valid);
        }
    }
    for src in sources {
        let joined = src.join(candidate);
        if joined.exists() {
            return validate_path(project_root, sources, &joined);
        }
    }
    let clean_arg = file_arg.trim_start_matches("./").replace('\\', "/");
    if let Ok(Some(matched_fp)) = conn
        .query_row(
            "SELECT file_path FROM file_hashes WHERE file_path = ?1 OR file_path LIKE ?2 ORDER BY LENGTH(file_path) ASC LIMIT 1",
            params![&clean_arg, format!("%/{clean_arg}")],
            |r| r.get::<_, String>(0),
        )
        .optional()
    {
        let matched_path = Path::new(&matched_fp);
        if let Ok(valid) = validate_path(project_root, sources, matched_path) {
            if valid.exists() {
                return Ok(valid);
            }
        }
    }
    validate_path(project_root, sources, candidate)
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

/// Tool 1: `project_map` — builds a compact directory & symbol/heading map within `max_tokens`.
/// Prioritizes root + core code folders (`src/`, `crates/`, `lib/`, `app/`), collapses overflow folders
/// into 1-line directory summaries, and summarizes external `sources`.
pub fn build_project_map(
    conn: &mut Connection,
    paths: &AppPaths,
    subpath: Option<&str>,
    max_tokens: usize,
) -> Result<(String, usize)> {
    let stats = index_workspace(conn, paths, subpath)?;
    let max_bytes = max_tokens.saturating_mul(4).max(400);

    let prefix_filter = match subpath {
        Some(sp) if !sp.is_empty() && sp != "." => {
            let cfg = paths.load_config().unwrap_or_default();
            let resolved = validate_path(&paths.project_root, &cfg.sources, Path::new(sp))?;
            Some(to_index_path(&paths.project_root, &resolved))
        }
        _ => None,
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
            if !file_path.starts_with(pref) {
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
    }

    let mut ordered: Vec<(String, i64, String, Vec<String>)> = local_map
        .into_iter()
        .map(|(f, (l, ft, s))| (f, l, ft, s))
        .collect();
    ordered.sort_by(|a, b| map_file_priority(&a.0).cmp(&map_file_priority(&b.0)).then(a.0.cmp(&b.0)));

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

    if ext_file_count > 0 {
        let cfg = paths.load_config().unwrap_or_default();
        out.push_str(&format!(
            "\n# External Sources ({} files, {} symbols/headings — query via `search_docs`, `smart_grep`, `outline`, or pass `subpath`)\n",
            ext_file_count, ext_symbol_count
        ));
        for src in &cfg.sources {
            out.push_str(&format!("- {}\n", src.display()));
        }
    }

    let raw_bytes = stats.total_raw_bytes.max(out.len());
    Ok((out, raw_bytes))
}

/// Tool 2: `outline` — returns AST symbols (code), Heading TOC (Markdown), or key/section TOC (YAML/TOML/JSON).
/// Collapses `mod tests` children by default and enforces `max_tokens` so outlines stay compact.
pub fn build_outline(
    conn: &Connection,
    paths: &AppPaths,
    file_arg: &str,
    max_tokens: usize,
    include_tests: bool,
) -> Result<(String, usize)> {
    let (index_key, _content, raw_bytes) = ensure_single_file_indexed(conn, paths, file_arg)?;

    let mut stmt = conn.prepare(
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

    Ok((out, raw_bytes))
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
) -> Result<(String, usize)> {
    let mut other_matches_note = String::new();
    let resolved_file_owned: String;
    let effective_file_arg = if let Some(fa) = file_arg.filter(|s| !s.trim().is_empty()) {
        fa
    } else if let Some(sym_query) = symbol {
        let clean_query = sym_query.trim().trim_start_matches('#').trim();
        let find_matches = |c: &Connection| -> Result<Vec<(String, String, String, i64, i64)>> {
            let mut stmt = c.prepare(
                "SELECT file_path, kind, name, start_line, end_line
                 FROM symbols
                 WHERE name = ?1 OR signature = ?2 OR LOWER(name) LIKE LOWER(?3)
                 ORDER BY
                   CASE WHEN name = ?1 THEN 0 WHEN LOWER(name) = LOWER(?1) THEN 1 ELSE 2 END,
                   CASE WHEN file_path LIKE '/%' THEN 1 ELSE 0 END ASC,
                   CASE WHEN parent_name = 'tests' THEN 1 ELSE 0 END ASC,
                   LENGTH(name) ASC,
                   file_path ASC,
                   start_line ASC
                 LIMIT 6",
            )?;
            let rows = stmt
                .query_map(
                    params![clean_query, sym_query.trim(), format!("%{clean_query}%")],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )?
                .filter_map(|r| r.ok())
                .collect();
            Ok(rows)
        };

        let mut matches = find_matches(conn)?;
        if matches.is_empty() {
            let _ = index_workspace(conn, paths, None);
            matches = find_matches(conn)?;
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

    let (index_key, raw_content, raw_bytes) =
        ensure_single_file_indexed(conn, paths, effective_file_arg)?;
    let lines: Vec<&str> = raw_content.lines().collect();
    let total_lines = lines.len().max(1);

    let (target_start, target_end, header_label) = if let Some(sym_query) = symbol {
        let clean_query = sym_query.trim().trim_start_matches('#').trim();
        let row: Option<(String, String, i64, i64)> = conn
            .query_row(
                "SELECT name, signature, start_line, end_line
                 FROM symbols
                 WHERE file_path = ?1
                   AND (name = ?2 OR signature = ?3 OR LOWER(name) LIKE LOWER(?4) OR LOWER(signature) LIKE LOWER(?4))
                 ORDER BY CASE WHEN name = ?2 THEN 0 WHEN LOWER(name) = LOWER(?2) THEN 1 ELSE 2 END, start_line ASC
                 LIMIT 1",
                params![
                    &index_key,
                    clean_query,
                    sym_query.trim(),
                    format!("%{clean_query}%")
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        let Some((name, sig, st, en)) = row else {
            bail!(
                "Symbol or heading '{}' not found in '{}'. Run `outline` first to see available symbols.",
                sym_query,
                index_key
            );
        };
        (st as usize, en as usize, format!("{name} ({sig})"))
    } else if let (Some(st), Some(en)) = (start_line, end_line) {
        (
            st.max(1),
            en.max(st).min(total_lines),
            format!("lines {st}..{en}"),
        )
    } else if let Some(st) = start_line {
        let en = (st + 60).min(total_lines);
        (st.max(1), en, format!("lines {st}..{en}"))
    } else {
        bail!("Provide either `symbol` or `start_line`/`end_line` to read_symbol");
    };

    let actual_start = target_start.saturating_sub(context_lines).max(1);
    let requested_end = (target_end + context_lines).min(total_lines);
    let max_cap = max_lines.max(20);
    let actual_end = if signatures_only {
        requested_end.min(actual_start + max_cap.saturating_mul(4) - 1)
    } else {
        requested_end.min(actual_start + max_cap - 1)
    };

    let file_type = IndexedFileType::classify(Path::new(&index_key));
    let is_code_file = matches!(file_type, Some(IndexedFileType::Code(_)));
    let hash_comments = matches!(
        file_type,
        Some(IndexedFileType::Config(ConfigFormat::Toml | ConfigFormat::Yaml))
            | Some(IndexedFileType::Code(CodeLanguage::Python))
    );

    // Collect AST function/method spans if `signatures_only` is active on a code file
    let fn_spans: Vec<(usize, usize)> = if signatures_only && is_code_file {
        let mut stmt = conn.prepare(
            "SELECT start_line, end_line FROM symbols
             WHERE file_path = ?1 AND kind IN ('fn', 'method') AND end_line > start_line + 2
             ORDER BY start_line ASC",
        )?;
        let rows = stmt
            .query_map(params![&index_key], |r| {
                Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows
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
                let sig_limit = (fn_start + 5).min(fn_end).min(actual_end);
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
                    if !hash_comments {
                        if let Some(close_line) = lines.get(fn_end - 1) {
                            if close_line.trim().starts_with('}') {
                                out.push_str(&format!("{fn_end:>4} | {close_line}\n"));
                            }
                        }
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
        out.push_str(&format!(
            "... [paginated at {max_cap} lines; call read_symbol(file=\"{index_key}\", start_line={}, end_line={requested_end}) for next slice]\n",
            actual_end + 1
        ));
    }
    if !other_matches_note.is_empty() {
        out.push_str(&other_matches_note);
    }

    Ok((out, raw_bytes))
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

/// Tool 5: `smart_grep` — RTK-style file-grouped regex search with enclosing AST/Heading context,
/// `path` directory/file filter, `files_only` (`rg -l`), `case_insensitive`, and `max_per_file` cap.
#[allow(clippy::too_many_arguments)]
pub fn smart_grep_search(
    conn: &Connection,
    paths: &AppPaths,
    pattern: &str,
    glob_filter: Option<&str>,
    path_filter: Option<&str>,
    files_only: bool,
    case_insensitive: bool,
    max_per_file: usize,
    max_results: usize,
) -> Result<(String, usize)> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
        .or_else(|_| {
            RegexBuilder::new(&regex::escape(pattern))
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

    let cfg = paths.load_config().unwrap_or_default();
    let mut local_files = Vec::new();
    let mut ext_files = Vec::new();

    // Determine explicit path roots if `path_filter` is given (supports comma/space-separated paths)
    let explicit_roots: Vec<PathBuf> = match path_filter {
        Some(pf) if !pf.trim().is_empty() => {
            let mut list = Vec::new();
            for token in pf.split(|c: char| c == ',' || c.is_whitespace()) {
                let t = token.trim();
                if t.is_empty() {
                    continue;
                }
                if let Ok(valid) = validate_path(&paths.project_root, &cfg.sources, Path::new(t)) {
                    if valid.exists() {
                        list.push(valid);
                    }
                }
            }
            list
        }
        _ => Vec::new(),
    };

    if !explicit_roots.is_empty() {
        for root in explicit_roots {
            if root.is_file() {
                if !is_denied_path(&root) {
                    local_files.push(root);
                }
                continue;
            }
            let walker = WalkBuilder::new(&root)
                .hidden(false)
                .git_ignore(true)
                .filter_entry(|entry| {
                    if let Some(name) = entry.file_name().to_str() {
                        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                            && should_skip_dir(name)
                        {
                            return false;
                        }
                    }
                    !is_denied_path(entry.path())
                })
                .build();
            for entry in walker.flatten() {
                if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    let p = entry.into_path();
                    if !is_denied_path(&p) {
                        local_files.push(p);
                    }
                }
            }
        }
        local_files.sort();
        local_files.dedup();
    } else {
        let walker = WalkBuilder::new(&paths.project_root)
            .hidden(false)
            .git_ignore(true)
            .filter_entry(|entry| {
                if let Some(name) = entry.file_name().to_str() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                        && should_skip_dir(name)
                    {
                        return false;
                    }
                }
                !is_denied_path(entry.path())
            })
            .build();

        for entry in walker.flatten() {
            if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                let p = entry.into_path();
                if !is_denied_path(&p) {
                    local_files.push(p);
                }
            }
        }
        local_files.sort();
        local_files.dedup();

        for src in &cfg.sources {
            if !src.exists() {
                continue;
            }
            let walker = WalkBuilder::new(src)
                .hidden(false)
                .git_ignore(true)
                .filter_entry(|entry| {
                    if let Some(name) = entry.file_name().to_str() {
                        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                            && should_skip_dir(name)
                        {
                            return false;
                        }
                    }
                    !is_denied_path(entry.path())
                })
                .build();

            for entry in walker.flatten() {
                if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    let p = entry.into_path();
                    if !is_denied_path(&p) {
                        ext_files.push(p);
                    }
                }
            }
        }
        ext_files.sort();
        ext_files.dedup();
    }

    let all_files = local_files.into_iter().chain(ext_files);

    // Group matches by file (RTK-style) with per-file cap
    let mut grouped: Vec<(String, usize, Vec<String>)> = Vec::new();
    let mut total_matched_file_bytes = 0usize;
    let mut total_matches = 0usize;
    let mut total_shown = 0usize;
    let per_file_cap = max_per_file.max(1);

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

        for (idx, line) in raw_content.lines().enumerate() {
            if re.is_match(line) {
                if file_match_count == 0 {
                    total_matched_file_bytes += raw_content.len();
                    if !files_only && IndexedFileType::classify(&file_path).is_some() {
                        let _ = ensure_single_file_indexed(conn, paths, &index_key);
                    }
                }
                file_match_count += 1;
                total_matches += 1;

                if files_only || file_lines.len() >= per_file_cap || total_shown >= max_results {
                    continue;
                }

                let line_no = (idx + 1) as i64;
                let enclosing: Option<(String, String, String, i64, i64)> = conn
                    .query_row(
                        "SELECT kind, name, signature, start_line, end_line
                         FROM symbols
                         WHERE file_path = ?1 AND start_line <= ?2 AND end_line >= ?2
                         ORDER BY (end_line - start_line) ASC
                         LIMIT 1",
                        params![&index_key, line_no],
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
                s.push_str(&format!(
                    "  +{} more in {fp} (use `path=\"{fp}\"`)\n",
                    cnt - lines.len()
                ));
            }
        }
        s
    };

    let raw_bytes = total_matched_file_bytes.max(out.len());
    Ok((out, raw_bytes))
}

/// Finds all AST references (`call`, `type_use`, `import`) to `symbol` across the workspace.
pub fn find_symbol_references(
    conn: &mut Connection,
    paths: &AppPaths,
    symbol: &str,
) -> Result<(String, usize)> {
    let clean = symbol.trim();
    if clean.is_empty() {
        bail!("Provide a non-empty `symbol` to `find_references`");
    }

    let stats = index_workspace(conn, paths, None)?;

    let mut stmt = conn.prepare(
        "SELECT r.file_path, r.line_number, r.caller_symbol, r.ref_kind, COALESCE(f.line_count, 50)
         FROM symbol_refs r
         LEFT JOIN file_hashes f ON r.file_path = f.file_path
         WHERE r.symbol_name = ?1
         ORDER BY
           CASE WHEN r.file_path LIKE '/%' THEN 1 ELSE 0 END ASC,
           CASE WHEN r.ref_kind = 'import' THEN 0 ELSE 1 END ASC,
           r.file_path ASC,
           r.line_number ASC",
    )?;

    let rows: Vec<(String, i64, Option<String>, String, i64)> = stmt
        .query_map(params![clean], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        let msg = format!("No references found for symbol '{clean}'.");
        return Ok((msg.clone(), stats.total_raw_bytes.max(msg.len())));
    }

    let mut seen_files = HashSet::new();
    let mut raw_bytes = 0usize;
    let mut out = format!("{clean} dipakai di {} lokasi:\n", rows.len());

    for (fp, line, caller, kind, line_cnt) in &rows {
        if seen_files.insert(fp.clone()) {
            raw_bytes += (*line_cnt as usize).saturating_mul(45);
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

    Ok((out.clone(), raw_bytes.max(out.len())))
}

