pub mod fts;

use crate::config::{normalize_doc_name, AppPaths, DocEntry};
use crate::db::init_connection;
use crate::docs::fts::search_doc_sections_fts;
use crate::indexer::{index_global_docs, index_workspace};
use crate::security::{escape_sql_like, normalize_abs_key, to_index_path};
use anyhow::{bail, Result};
use globset::GlobBuilder;
use rusqlite::{params, Connection};
use std::collections::HashSet;

/// Selects formatted blocks within `budget_tokens` (1 token ≈ 4 bytes).
/// Always keeps the top block intact (never slices mid-code); subsequent blocks stop once budget is met.
fn apply_token_budget(blocks: &[String], budget_tokens: usize) -> String {
    let max_bytes = budget_tokens.saturating_mul(4).max(240);
    let mut out = String::new();

    for block in blocks {
        if !out.is_empty() && out.len() + block.len() > max_bytes {
            break;
        }
        out.push_str(block);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if out.len() >= max_bytes {
            break;
        }
    }

    out
}

#[derive(Debug, Clone)]
struct DocSectionRecord {
    file_path: String,
    heading: String,
    start_line: i64,
    end_line: i64,
    content: String,
}

fn path_matches_doc_key(file_path: &str, doc_key: &str) -> bool {
    let key = doc_key.trim_end_matches('/');
    if key.is_empty() {
        return !file_path.starts_with('/');
    }
    file_path == key || file_path.starts_with(&format!("{key}/"))
}

fn match_named_docs<'a>(entries: &'a [DocEntry], query_name: &str) -> Vec<&'a DocEntry> {
    let q_norm = normalize_doc_name(query_name);
    if q_norm.is_empty() {
        return Vec::new();
    }
    let exact: Vec<&DocEntry> = entries
        .iter()
        .filter(|e| normalize_doc_name(&e.name) == q_norm)
        .collect();
    if !exact.is_empty() {
        return exact;
    }
    entries
        .iter()
        .filter(|e| normalize_doc_name(&e.name).contains(&q_norm))
        .collect()
}

fn collect_doc_sections_from_db<F>(
    conn: &Connection,
    query: &str,
    matches_file: &F,
    matched_sections: &mut Vec<DocSectionRecord>,
    seen_keys: &mut HashSet<(String, i64, i64)>,
) -> Result<()>
where
    F: Fn(&str) -> bool,
{
    // Fetch a generous FTS pool so post-filtering by `doc`/`file_filter` does not starve smaller docs
    let fts_ids = search_doc_sections_fts(conn, query, 300).unwrap_or_default();
    if !fts_ids.is_empty() {
        let mut by_id = conn.prepare_cached(
            "SELECT file_path, heading, start_line, end_line, content
             FROM doc_sections_meta WHERE id = ?1",
        )?;
        for id in fts_ids {
            if let Ok(sec) = by_id.query_row(params![id], |r| {
                Ok(DocSectionRecord {
                    file_path: r.get(0)?,
                    heading: r.get(1)?,
                    start_line: r.get(2)?,
                    end_line: r.get(3)?,
                    content: r.get(4)?,
                })
            }) && matches_file(&sec.file_path)
                && seen_keys.insert((sec.file_path.clone(), sec.start_line, sec.end_line))
            {
                matched_sections.push(sec);
            }
        }
    }

    if matched_sections.len() < 8 {
        let escaped = escape_sql_like(&query.trim().to_lowercase());
        let like_pat = format!("%{escaped}%");
        let mut like_stmt = conn.prepare(
            "SELECT file_path, heading, start_line, end_line, content
             FROM doc_sections_meta
             WHERE LOWER(heading) LIKE ?1 ESCAPE '\\'
                OR LOWER(file_path) LIKE ?1 ESCAPE '\\'
                OR LOWER(content) LIKE ?1 ESCAPE '\\'
             LIMIT 200",
        )?;
        let rows = like_stmt.query_map(params![like_pat], |r| {
            Ok(DocSectionRecord {
                file_path: r.get(0)?,
                heading: r.get(1)?,
                start_line: r.get(2)?,
                end_line: r.get(3)?,
                content: r.get(4)?,
            })
        })?;
        for sec in rows.flatten() {
            if matches_file(&sec.file_path)
                && seen_keys.insert((sec.file_path.clone(), sec.start_line, sec.end_line))
            {
                matched_sections.push(sec);
            }
        }
    }
    Ok(())
}

/// Tool 5: `search_docs` — column-weighted SQLite FTS5 BM25 search across `.md`/`.mdx` sections
/// and full code files in workspace (`local_db`) & global (`global_db`) docs.
pub fn search_docs(
    conn: &mut Connection,
    paths: &AppPaths,
    query: &str,
    file_filter: Option<&str>,
    doc_name: Option<&str>,
    scope: Option<&str>,
    budget_tokens: usize,
) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        bail!("Provide a non-empty `query` to `search_docs`");
    }

    let scope_mode = match scope.map(|s| s.trim().to_lowercase()).as_deref() {
        None | Some("") | Some("all") => "all",
        Some("workspace") | Some("w") | Some("local") | Some("project") => "workspace",
        Some("global") | Some("g") => "global",
        Some(other) => {
            bail!("Invalid `scope` '{other}'. Expected 'workspace', 'global', or 'all'.")
        }
    };

    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;

    let doc_trim = doc_name.map(str::trim).filter(|s| !s.is_empty());
    let mut query_local = scope_mode != "global";
    let mut query_global = scope_mode != "workspace" && !global_docs.is_empty();

    let allowed_doc_keys: Option<Vec<String>> = if let Some(dname) = doc_trim {
        let mut ws_matched = Vec::new();
        let mut gl_matched = Vec::new();
        if scope_mode != "global" {
            for e in match_named_docs(&ws_docs, dname) {
                ws_matched.push(to_index_path(&paths.project_root, &e.path));
            }
        }
        if scope_mode != "workspace" {
            for e in match_named_docs(&global_docs, dname) {
                gl_matched.push(normalize_abs_key(&e.path));
            }
        }
        if ws_matched.is_empty() && gl_matched.is_empty() {
            let mut avail = Vec::new();
            for d in &ws_docs {
                avail.push(format!("`{}` (workspace)", d.name));
            }
            for d in &global_docs {
                avail.push(format!("`{}` (global)", d.name));
            }
            let avail_str = if avail.is_empty() {
                "none registered (use `lee add-docs <path>`)".to_string()
            } else {
                avail.join(", ")
            };
            return Ok(format!(
                "No registered doc matched '{dname}' in scope '{scope_mode}'. Available docs: {avail_str}.\n"
            ));
        }
        // Only scan the DB(s) that actually own the matched doc(s) so global docs work even from $HOME
        query_local = !ws_matched.is_empty();
        query_global = !gl_matched.is_empty();
        ws_matched.extend(gl_matched);
        Some(ws_matched)
    } else if scope_mode == "global" {
        if global_docs.is_empty() {
            return Ok(
                "No global docs registered. Register one with `lee add-docs --g <path>`.\n"
                    .to_string(),
            );
        }
        Some(global_docs.iter().map(|e| normalize_abs_key(&e.path)).collect())
    } else if scope_mode == "workspace" {
        Some(
            ws_docs
                .iter()
                .map(|e| to_index_path(&paths.project_root, &e.path))
                .collect(),
        )
    } else {
        None
    };

    if query_local {
        let _ = index_workspace(conn, paths, None)?;
    }
    if query_global {
        let _ = index_global_docs(paths)?;
    }

    let filter_trim = file_filter.map(str::trim).filter(|s| !s.is_empty());
    let is_glob_pattern = filter_trim
        .map(|s| s.contains('*') || s.contains('?') || s.contains('['))
        .unwrap_or(false);
    let filter_glob = if is_glob_pattern {
        filter_trim.and_then(|f| {
            GlobBuilder::new(f)
                .literal_separator(false)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        })
    } else {
        None
    };
    // Fall back to substring matching when glob compilation fails so malformed globs never disable filtering
    let filter_substr = if filter_glob.is_none() {
        filter_trim.map(|s| s.to_lowercase())
    } else {
        None
    };

    let matches_file = |fp: &str| -> bool {
        if let Some(ref keys) = allowed_doc_keys {
            let in_docs = keys.iter().any(|k| path_matches_doc_key(fp, k));
            let in_workspace_local =
                doc_trim.is_none() && scope_mode == "workspace" && !fp.starts_with('/');
            if !in_docs && !in_workspace_local {
                return false;
            }
        }
        if let Some(ref matcher) = filter_glob {
            return matcher.is_match(fp);
        }
        if let Some(ref sub) = filter_substr {
            return fp.to_lowercase().contains(sub);
        }
        true
    };

    let mut matched_sections: Vec<DocSectionRecord> = Vec::new();
    let mut seen_keys: HashSet<(String, i64, i64)> = HashSet::new();

    if query_local {
        collect_doc_sections_from_db(
            conn,
            query,
            &matches_file,
            &mut matched_sections,
            &mut seen_keys,
        )?;
    }

    if query_global && paths.global_db.exists() {
        let global_conn = init_connection(&paths.global_db)?;
        collect_doc_sections_from_db(
            &global_conn,
            query,
            &matches_file,
            &mut matched_sections,
            &mut seen_keys,
        )?;
    }

    if matched_sections.is_empty() {
        return Ok(format!("No documentation sections matched query '{query}'.\n"));
    }

    let mut blocks = Vec::new();
    let mut seen_ranges: Vec<(String, i64, i64)> = Vec::new();

    for sec in matched_sections {
        // Only skip a window if it is largely duplicated (>50% overlap), preserving adjacent 15-line overlap windows
        let sec_len = (sec.end_line - sec.start_line + 1).max(1);
        if seen_ranges.iter().any(|(fp, st, en)| {
            if fp != &sec.file_path {
                return false;
            }
            let overlap = (sec.end_line.min(*en) - sec.start_line.max(*st) + 1).max(0);
            overlap * 2 > sec_len
        }) {
            continue;
        }
        seen_ranges.push((sec.file_path.clone(), sec.start_line, sec.end_line));
        blocks.push(format!(
            "### {} [{start}..{end}] — `{heading}`\n{content}\n\n",
            sec.file_path,
            start = sec.start_line,
            end = sec.end_line,
            heading = sec.heading,
            content = sec.content
        ));
    }

    let body = apply_token_budget(&blocks, budget_tokens);
    Ok(format!("# Documentation Search Results (`{query}`)\n\n{body}"))
}
