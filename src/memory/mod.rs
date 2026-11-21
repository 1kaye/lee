pub mod fts;

use crate::config::{normalize_doc_name, AppPaths, DocEntry};
use crate::db::{init_connection, now_unix};
use crate::indexer::index_workspace;
use crate::memory::fts::{search_doc_sections_fts, search_memories_fts};
use crate::security::to_index_path;
use anyhow::{bail, Context, Result};
use globset::GlobBuilder;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct MemoryRecord {
    pub id: i64,
    pub scope: String,
    pub topic: String,
    pub tags: String,
    pub content: String,
}

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

/// Tool 9: `remember` — appends a decision/rule, session handoff, or backlog item to Markdown
/// (`.notes/` for `project`/`handoff`/`backlog` or `~/.config/lee/vault/` for `global`)
/// with Obsidian-compatible `[[wikilinks]]`, and indexes into SQLite FTS5.
pub fn store_memory(
    project_conn: &Connection,
    paths: &AppPaths,
    topic: &str,
    content: &str,
    tags: Option<&str>,
    scope: Option<&str>,
) -> Result<(String, usize)> {
    let scope_norm = match scope.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("global") => "global",
        Some("handoff") => "handoff",
        Some("backlog") => "backlog",
        _ => "project",
    };
    let tags_str = tags.unwrap_or("").trim();
    let ts = now_unix();

    let target_dir = if scope_norm == "global" {
        paths.ensure_global_dirs()?;
        &paths.vault_dir
    } else {
        paths.ensure_local_dirs()?;
        &paths.notes_dir
    };

    let slug = match scope_norm {
        "handoff" => "handoff".to_string(),
        "backlog" => "backlog".to_string(),
        _ => slugify(topic),
    };
    let md_file = target_dir.join(format!("{slug}.md"));
    let is_new = !md_file.exists();

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&md_file)
        .with_context(|| format!("Failed to open memory markdown file {}", md_file.display()))?;

    if is_new {
        writeln!(
            file,
            "---\ntopic: \"{}\"\ntags: \"{}\"\nscope: \"{}\"\ncreated_at: {}\n---\n\n# {}\n\n{}\n",
            topic.replace('"', "\\\""),
            tags_str.replace('"', "\\\""),
            scope_norm,
            ts,
            topic,
            content.trim()
        )?;
    } else {
        writeln!(
            file,
            "\n---\n<!-- appended_at: {} | scope: {} | tags: {} -->\n## {}\n\n{}\n",
            ts,
            scope_norm,
            tags_str,
            topic,
            content.trim()
        )?;
    }

    let md_path_str = md_file.to_string_lossy().to_string();
    let target_conn;
    let conn_ref = if scope_norm == "global" {
        target_conn = init_connection(&paths.global_db)?;
        &target_conn
    } else {
        project_conn
    };
    conn_ref.execute(
        "INSERT INTO memories_meta (scope, topic, tags, content, md_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![scope_norm, topic, tags_str, content.trim(), &md_path_str, ts],
    )?;

    let out = format!(
        "Saved memory [{scope_norm}] '[[{slug}|{topic}]]' (tags: [{tags_str}]) -> {}\n",
        md_file.display()
    );
    Ok((out.clone(), content.len().max(out.len())))
}

/// Parses YAML frontmatter (`topic`, `tags`, `scope`, `created_at`) and strips the leading `# Title`
/// from a `.notes/*.md` file so synced notes stay clean.
fn parse_note_markdown(raw: &str, default_topic: &str, default_scope: &str) -> (String, String, String, i64, String) {
    let mut topic = default_topic.to_string();
    let mut tags = String::new();
    let mut scope = default_scope.to_string();
    let mut created_at = now_unix();
    let mut body = raw.trim();

    if let Some(after_open) = body.strip_prefix("---\n")
        && let Some(close_idx) = after_open.find("\n---\n")
    {
        let fm = &after_open[..close_idx];
        for line in fm.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim();
                let val = v.trim().trim_matches('"').replace("\\\"", "\"");
                match key {
                    "topic" if !val.is_empty() => topic = val,
                    "tags" => tags = val,
                    "scope" if !val.is_empty() => scope = val,
                    "created_at" => {
                        if let Ok(ts) = val.parse::<i64>() {
                            created_at = ts;
                        }
                    }
                    _ => {}
                }
            }
        }
        body = after_open[close_idx + 5..].trim_start();
    }

    if let Some(first_line_end) = body.find('\n') {
        let first_line = body[..first_line_end].trim();
        if first_line.starts_with("# ") {
            body = body[first_line_end + 1..].trim_start();
        }
    }

    (topic, tags, scope, created_at, body.trim().to_string())
}

/// Syncs `.md` files in `.notes/` or `vault/` (excluding `INDEX.md`) with `memories_meta`.
fn sync_notes_dir_to_db(conn: &Connection, dir: &Path, scope: &str) -> Result<()> {
    if let Ok(mut stmt) = conn.prepare("SELECT id, md_path, content FROM memories_meta WHERE md_path IS NOT NULL") {
        let existing_rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|r| r.ok())
            .collect();
        for (id, md_path, content) in existing_rows {
            if !Path::new(&md_path).exists() || content.starts_with("---\ntopic:") {
                let _ = conn.execute("DELETE FROM memories_meta WHERE id = ?1", params![id]);
            }
        }
    }

    if !dir.is_dir() {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md")
            || path.file_name().and_then(|n| n.to_str()) == Some("INDEX.md")
        {
            continue;
        }
        let path_str = path.to_string_lossy().to_string();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_meta WHERE md_path = ?1",
                params![&path_str],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if exists > 0 {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let default_topic = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("note")
            .replace('-', " ");
        let (topic, tags, parsed_scope, created_at, clean_body) =
            parse_note_markdown(&raw, &default_topic, scope);
        conn.execute(
            "INSERT INTO memories_meta (scope, topic, tags, content, md_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![parsed_scope, topic, tags, clean_body, path_str, created_at],
        )?;
    }
    Ok(())
}

/// Extracts `[[wikilink]]` target names from note content (handles both `[[target]]` and `[[target|alias]]`).
pub fn extract_wikilinks(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find("]]") {
            let inner = &after_open[..end];
            let target = inner.split('|').next().unwrap_or(inner).trim();
            if !target.is_empty() {
                links.push(slugify(target));
            }
            rest = &after_open[end + 2..];
        } else {
            break;
        }
    }
    links
}

/// Tool 10: `recall_memory` — column-weighted FTS5 BM25 search across Project `.notes/`
/// and Global `vault/`, with 1-hop Obsidian `[[wikilinks]]` expansion.
pub fn recall_memories(
    project_conn: &Connection,
    paths: &AppPaths,
    query: &str,
    budget_tokens: usize,
) -> Result<(String, usize)> {
    let _ = sync_notes_dir_to_db(project_conn, &paths.notes_dir, "project");

    let mut matched_keys: Vec<(String, i64)> = Vec::new();
    let mut records_map: HashMap<(String, i64), MemoryRecord> = HashMap::new();

    collect_db_memories(project_conn, "project", query, &mut matched_keys, &mut records_map)?;

    let mut global_conn_opt = None;
    if paths.global_db.exists() || paths.vault_dir.exists() {
        let _ = paths.ensure_global_dirs();
        if let Ok(global_conn) = init_connection(&paths.global_db) {
            let _ = sync_notes_dir_to_db(&global_conn, &paths.vault_dir, "global");
            let _ = collect_db_memories(
                &global_conn,
                "global",
                query,
                &mut matched_keys,
                &mut records_map,
            );
            global_conn_opt = Some(global_conn);
        }
    }

    if matched_keys.is_empty() {
        let msg = format!("No relevant project or global memories found for query: '{query}'.\n");
        return Ok((msg.clone(), msg.len()));
    }

    // Load 1-hop wikilink targets on demand only when matched records actually contain [[wikilinks]]
    let mut needed_slugs: HashSet<String> = HashSet::new();
    for key in &matched_keys {
        if let Some(rec) = records_map.get(key) {
            for wl in extract_wikilinks(&rec.content) {
                needed_slugs.insert(wl);
            }
        }
    }
    if !needed_slugs.is_empty() {
        load_linked_memories(project_conn, "project", &needed_slugs, &mut records_map);
        if let Some(ref gconn) = global_conn_opt {
            load_linked_memories(gconn, "global", &needed_slugs, &mut records_map);
        }
    }

    let mut formatted_blocks = Vec::new();
    let mut included_keys: HashSet<(String, i64)> = HashSet::new();

    for key in &matched_keys {
        if included_keys.contains(key) {
            continue;
        }
        if let Some(rec) = records_map.get(key) {
            included_keys.insert(key.clone());
            let slug = slugify(&rec.topic);
            formatted_blocks.push(format!(
                "- **[{scope}] [[{slug}|{topic}]]** (tags: `{tags}`)\n  {content}\n",
                scope = rec.scope,
                topic = rec.topic,
                tags = rec.tags,
                content = rec.content.replace('\n', "\n  ")
            ));

            let wikilinks = extract_wikilinks(&rec.content);
            if !wikilinks.is_empty() {
                for (cand_key, cand_rec) in &records_map {
                    if included_keys.contains(cand_key) {
                        continue;
                    }
                    let cand_slug = slugify(&cand_rec.topic);
                    if wikilinks.contains(&cand_slug) {
                        included_keys.insert(cand_key.clone());
                        formatted_blocks.push(format!(
                            "- **[{scope}] [[{cand_slug}|{topic}]]** (linked via `[[{cand_slug}]]`)\n  {content}\n",
                            scope = cand_rec.scope,
                            topic = cand_rec.topic,
                            content = cand_rec.content.replace('\n', "\n  ")
                        ));
                    }
                }
            }
        }
    }

    let body = apply_token_budget(&formatted_blocks, budget_tokens);
    let out = format!("# Recalled Memories (query: `{query}`)\n{body}");
    Ok((out.clone(), out.len()))
}

fn collect_db_memories(
    conn: &Connection,
    scope_prefix: &str,
    query: &str,
    fts_keys: &mut Vec<(String, i64)>,
    records_map: &mut HashMap<(String, i64), MemoryRecord>,
) -> Result<()> {
    let fts_ids = search_memories_fts(conn, query, 25).unwrap_or_default();
    if !fts_ids.is_empty()
        && let Ok(mut by_id) =
            conn.prepare("SELECT id, scope, topic, tags, content FROM memories_meta WHERE id = ?1")
    {
        for id in fts_ids {
            if let Ok(rec) = by_id.query_row(params![id], |r| {
                Ok(MemoryRecord {
                    id: r.get(0)?,
                    scope: r.get(1)?,
                    topic: r.get(2)?,
                    tags: r.get(3)?,
                    content: r.get(4)?,
                })
            }) {
                let key = (scope_prefix.to_string(), rec.id);
                if !fts_keys.contains(&key) {
                    fts_keys.push(key.clone());
                }
                records_map.insert(key, rec);
            }
        }
    }

    // Targeted SQL LIKE fallback only when FTS5 returned fewer than 5 results
    if fts_keys.len() < 5 {
        let like_pat = format!("%{}%", query.trim().to_lowercase());
        let mut stmt = conn.prepare(
            "SELECT id, scope, topic, tags, content FROM memories_meta
             WHERE LOWER(topic) LIKE ?1 OR LOWER(tags) LIKE ?1 OR LOWER(scope) LIKE ?1 OR LOWER(content) LIKE ?1
             ORDER BY created_at DESC LIMIT 20",
        )?;
        let rows = stmt.query_map(params![like_pat], |r| {
            Ok(MemoryRecord {
                id: r.get(0)?,
                scope: r.get(1)?,
                topic: r.get(2)?,
                tags: r.get(3)?,
                content: r.get(4)?,
            })
        })?;
        for row in rows.flatten() {
            let key = (scope_prefix.to_string(), row.id);
            if !fts_keys.contains(&key) {
                fts_keys.push(key.clone());
            }
            records_map.insert(key, row);
        }
    }

    Ok(())
}

fn load_linked_memories(
    conn: &Connection,
    scope_prefix: &str,
    needed_slugs: &HashSet<String>,
    records_map: &mut HashMap<(String, i64), MemoryRecord>,
) {
    let Ok(mut stmt) = conn.prepare("SELECT id, scope, topic, tags, content FROM memories_meta") else {
        return;
    };
    let Ok(rows) = stmt.query_map([], |r| {
        Ok(MemoryRecord {
            id: r.get(0)?,
            scope: r.get(1)?,
            topic: r.get(2)?,
            tags: r.get(3)?,
            content: r.get(4)?,
        })
    }) else {
        return;
    };
    for rec in rows.flatten() {
        if needed_slugs.contains(&slugify(&rec.topic)) {
            records_map.entry((scope_prefix.to_string(), rec.id)).or_insert(rec);
        }
    }
}

#[derive(Debug, Clone)]
struct DocSectionRecord {
    id: i64,
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

/// Tool 5: `search_docs` — column-weighted SQLite FTS5 BM25 search across `.md`/`.mdx` sections
/// and full code files in workspace & global docs (no full-table scan).
pub fn search_docs(
    conn: &mut Connection,
    paths: &AppPaths,
    query: &str,
    file_filter: Option<&str>,
    doc_name: Option<&str>,
    scope: Option<&str>,
    budget_tokens: usize,
) -> Result<(String, usize)> {
    let _ = index_workspace(conn, paths, None)?;

    let scope_mode = match scope.map(|s| s.trim().to_lowercase()).as_deref() {
        None | Some("") | Some("all") => "all",
        Some("workspace") | Some("w") | Some("local") | Some("project") => "workspace",
        Some("global") | Some("g") => "global",
        Some(other) => bail!("Invalid `scope` '{other}'. Expected 'workspace', 'global', or 'all'."),
    };

    let ws_docs = paths.load_config().unwrap_or_default().docs;
    let global_docs = paths.load_global_config().unwrap_or_default().docs;

    let doc_trim = doc_name.map(|s| s.trim()).filter(|s| !s.is_empty());
    let allowed_doc_keys: Option<Vec<String>> = if let Some(dname) = doc_trim {
        let mut matched_paths = Vec::new();
        if scope_mode != "global" {
            for e in match_named_docs(&ws_docs, dname) {
                matched_paths.push(to_index_path(&paths.project_root, &e.path));
            }
        }
        if scope_mode != "workspace" {
            for e in match_named_docs(&global_docs, dname) {
                matched_paths.push(to_index_path(&paths.project_root, &e.path));
            }
        }
        if matched_paths.is_empty() {
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
            let msg = format!(
                "No registered doc matched '{dname}' in scope '{scope_mode}'. Available docs: {avail_str}.\n"
            );
            return Ok((msg.clone(), msg.len()));
        }
        Some(matched_paths)
    } else if scope_mode == "global" {
        if global_docs.is_empty() {
            let msg = "No global docs registered. Register one with `lee add-docs --g <path>`.\n"
                .to_string();
            return Ok((msg.clone(), msg.len()));
        }
        Some(
            global_docs
                .iter()
                .map(|e| to_index_path(&paths.project_root, &e.path))
                .collect(),
        )
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

    let filter_trim = file_filter.map(|s| s.trim()).filter(|s| !s.is_empty());
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
    let filter_substr = if !is_glob_pattern {
        filter_trim.map(|s| s.to_lowercase())
    } else {
        None
    };

    let matches_file = |fp: &str| -> bool {
        if let Some(ref keys) = allowed_doc_keys {
            let in_docs = keys.iter().any(|k| path_matches_doc_key(fp, k));
            let in_workspace_local = doc_trim.is_none() && scope_mode == "workspace" && !fp.starts_with('/');
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

    // 1. Fetch FTS5 BM25 ranked IDs and look up only those rows by primary key (O(k) index lookup)
    let fts_ids = search_doc_sections_fts(conn, query, 100).unwrap_or_default();
    let mut matched_sections: Vec<DocSectionRecord> = Vec::new();
    let mut seen_ids: HashSet<i64> = HashSet::new();

    if !fts_ids.is_empty() {
        let mut by_id = conn.prepare_cached(
            "SELECT id, file_path, heading, start_line, end_line, content
             FROM doc_sections_meta WHERE id = ?1",
        )?;
        for id in fts_ids {
            if let Ok(sec) = by_id.query_row(params![id], |r| {
                Ok(DocSectionRecord {
                    id: r.get(0)?,
                    file_path: r.get(1)?,
                    heading: r.get(2)?,
                    start_line: r.get(3)?,
                    end_line: r.get(4)?,
                    content: r.get(5)?,
                })
            }) && matches_file(&sec.file_path)
                && seen_ids.insert(sec.id)
            {
                matched_sections.push(sec);
            }
        }
    }

    // 2. Bounded SQL LIKE fallback only if FTS5 yielded fewer than 8 filtered results
    if matched_sections.len() < 8 {
        let like_pat = format!("%{}%", query.trim().to_lowercase());
        let mut like_stmt = conn.prepare(
            "SELECT id, file_path, heading, start_line, end_line, content
             FROM doc_sections_meta
             WHERE LOWER(heading) LIKE ?1 OR LOWER(file_path) LIKE ?1 OR LOWER(content) LIKE ?1
             LIMIT 80",
        )?;
        let rows = like_stmt.query_map(params![like_pat], |r| {
            Ok(DocSectionRecord {
                id: r.get(0)?,
                file_path: r.get(1)?,
                heading: r.get(2)?,
                start_line: r.get(3)?,
                end_line: r.get(4)?,
                content: r.get(5)?,
            })
        })?;
        for sec in rows.flatten() {
            if matches_file(&sec.file_path) && seen_ids.insert(sec.id) {
                matched_sections.push(sec);
            }
        }
    }

    if matched_sections.is_empty() {
        let msg = format!("No documentation sections matched query '{query}'.\n");
        return Ok((msg.clone(), msg.len()));
    }

    let mut blocks = Vec::new();
    let mut seen_ranges: Vec<(String, i64, i64)> = Vec::new();

    for sec in matched_sections {
        if seen_ranges.iter().any(|(fp, st, en)| {
            fp == &sec.file_path && sec.start_line <= *en && sec.end_line >= *st
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
    let out = format!("# Documentation Search Results (`{query}`)\n\n{body}");
    Ok((out.clone(), out.len()))
}

pub fn slugify(input: &str) -> String {
    let mut s = String::new();
    let mut last_dash = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !s.is_empty() {
            s.push('-');
            last_dash = true;
        }
    }
    let clean = s.trim_matches('-').to_string();
    if clean.is_empty() {
        "memory-note".to_string()
    } else if clean.len() > 48 {
        clean[..48].trim_matches('-').to_string()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remember_wikilinks_and_recall() {
        let temp = std::env::temp_dir().join(format!("lee_mem_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let mut paths = AppPaths::from_root(temp.clone()).unwrap();
        paths.global_dir = temp.join("global_cfg");
        paths.global_db = paths.global_dir.join("global.db");
        paths.vault_dir = paths.global_dir.join("vault");
        paths.ensure_local_dirs().unwrap();
        paths.ensure_global_dirs().unwrap();

        let conn = init_connection(&paths.local_db).unwrap();

        store_memory(
            &conn,
            &paths,
            "Database WAL Mode",
            "Always enable PRAGMA journal_mode = WAL and foreign_keys = ON in SQLite.",
            Some("sqlite, database"),
            Some("project"),
        )
        .unwrap();

        store_memory(
            &conn,
            &paths,
            "Indexer Transaction Rule",
            "Wrap bulk symbol inserts in a transaction; depends on [[database-wal-mode]].",
            Some("indexer, sqlite"),
            Some("project"),
        )
        .unwrap();

        // Verify 1-hop [[wikilink]] expansion: querying Indexer Transaction Rule pulls in Database WAL Mode
        let (recalled, _) =
            recall_memories(&conn, &paths, "Indexer Transaction Rule", 600).unwrap();
        assert!(recalled.contains("Indexer Transaction Rule"));
        assert!(recalled.contains("Database WAL Mode"));

        let _ = fs::remove_dir_all(&temp);
    }
}
