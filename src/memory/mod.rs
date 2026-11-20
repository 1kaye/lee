pub mod embedder;
pub mod fts;
pub mod hybrid;

use crate::config::{normalize_doc_name, AppPaths, DocEntry};
use crate::db::{init_connection, now_unix};
use crate::indexer::index_workspace;
use crate::memory::embedder::{
    deserialize_f32_vec, embed_memory_passage, embed_query, is_model_cached, serialize_f32_vec,
};
use crate::memory::fts::{search_doc_sections_fts, search_memories_fts};
use crate::memory::hybrid::{apply_token_budget, cosine_similarity, reciprocal_rank_fusion};
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
    pub md_path: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub created_at: i64,
}

/// Tool 8: `remember` — appends a decision/rule, session handoff, or backlog item to Markdown
/// (`.notes/` for `project`/`handoff`/`backlog` or `~/.config/lee/vault/` for `global`),
/// maintains Obsidian-compatible `[[wikilinks]]` and `.notes/INDEX.md`, and indexes into SQLite FTS5 + BGE-Small.
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

    let emb_blob = embed_memory_passage(&paths.models_dir, topic, tags_str, content)
        .map(|v| serialize_f32_vec(&v));
    let md_path_str = md_file.to_string_lossy().to_string();

    if scope_norm == "global" {
        let global_conn = init_connection(&paths.global_db)?;
        global_conn.execute(
            "INSERT INTO memories_meta (scope, topic, tags, content, md_path, embedding, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                scope_norm,
                topic,
                tags_str,
                content.trim(),
                &md_path_str,
                emb_blob,
                ts
            ],
        )?;
    } else {
        project_conn.execute(
            "INSERT INTO memories_meta (scope, topic, tags, content, md_path, embedding, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                scope_norm,
                topic,
                tags_str,
                content.trim(),
                &md_path_str,
                emb_blob,
                ts
            ],
        )?;
        let _ = update_generated_notes_index(project_conn, &paths.notes_dir);
    }

    let out = format!(
        "Saved memory [{scope_norm}] '[[{slug}|{topic}]]' (tags: [{tags_str}]) -> {}\n",
        md_file.display()
    );
    let raw_bytes = content.len().max(out.len());
    Ok((out, raw_bytes))
}

/// Regenerates `.notes/INDEX.md` between `<!-- lee-index:start -->` and `<!-- lee-index:end -->` markers
/// using standard Obsidian-compatible `[[wikilinks]]`.
pub fn update_generated_notes_index(conn: &Connection, notes_dir: &Path) -> Result<()> {
    fs::create_dir_all(notes_dir)?;
    let mut stmt = conn.prepare(
        "SELECT scope, topic, tags, created_at
         FROM memories_meta
         ORDER BY created_at DESC",
    )?;
    let rows: Vec<(String, String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let mut block = String::from("<!-- lee-index:start -->\n# Lee Vault Index\n\n");
    let mut seen = HashSet::new();
    for (scope, topic, tags, _ts) in rows {
        let slug = match scope.as_str() {
            "handoff" => "handoff".to_string(),
            "backlog" => "backlog".to_string(),
            _ => slugify(&topic),
        };
        let key = format!("{scope}:{topic}");
        if !seen.insert(key) {
            continue;
        }
        block.push_str(&format!(
            "- [[{slug}|{topic}]] (`{scope}`{}) \n",
            if tags.is_empty() {
                String::new()
            } else {
                format!(" — tags: `{tags}`")
            }
        ));
    }
    block.push_str("<!-- lee-index:end -->\n");

    let index_path = notes_dir.join("INDEX.md");
    if index_path.exists() {
        let existing = fs::read_to_string(&index_path).unwrap_or_default();
        if let (Some(st), Some(en)) = (
            existing.find("<!-- lee-index:start -->"),
            existing.find("<!-- lee-index:end -->"),
        ) {
            let end_pos = en + "<!-- lee-index:end -->".len();
            let mut updated = String::new();
            updated.push_str(&existing[..st]);
            updated.push_str(block.trim_end());
            updated.push_str(&existing[end_pos..]);
            fs::write(&index_path, updated)?;
            return Ok(());
        }
    }
    fs::write(&index_path, block)?;
    Ok(())
}

/// Backfills any `memories_meta` rows where `embedding IS NULL` when `BGESmallENV15Q` is available.
pub fn backfill_missing_memory_embeddings(conn: &Connection, models_dir: &Path) -> Result<usize> {
    if !is_model_cached(models_dir) {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT id, topic, tags, content FROM memories_meta WHERE embedding IS NULL",
    )?;
    let missing: Vec<(i64, String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let mut updated = 0usize;
    for (id, topic, tags, content) in missing {
        if let Some(vec) = embed_memory_passage(models_dir, &topic, &tags, &content) {
            let blob = serialize_f32_vec(&vec);
            conn.execute(
                "UPDATE memories_meta SET embedding = ?1 WHERE id = ?2",
                params![blob, id],
            )?;
            updated += 1;
        }
    }
    Ok(updated)
}

/// Parses YAML frontmatter (`topic`, `tags`, `scope`, `created_at`) and strips the leading `# Title`
/// from a `.notes/*.md` file so synced notes stay clean without raw frontmatter noise.
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

/// Syncs `.md` files in `.notes/` or `vault/` (excluding `INDEX.md`) with `memories_meta`,
/// pruning any DB rows whose backing `.md` file was deleted or rolled back, and healing any
/// legacy unparsed frontmatter rows.
fn sync_notes_dir_to_db(
    conn: &Connection,
    dir: &Path,
    scope: &str,
    models_dir: &Path,
) -> Result<()> {
    // 1. Prune stale DB entries whose backing .md file no longer exists on disk, or heal raw frontmatter rows
    if let Ok(mut stmt) = conn.prepare("SELECT id, md_path, content FROM memories_meta WHERE md_path IS NOT NULL") {
        let existing_rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|r| r.ok())
            .collect();
        let mut pruned = false;
        for (id, md_path, content) in existing_rows {
            if !Path::new(&md_path).exists() || content.starts_with("---\ntopic:") {
                let _ = conn.execute("DELETE FROM memories_meta WHERE id = ?1", params![id]);
                pruned = true;
            }
        }
        if pruned && scope != "global" {
            let _ = update_generated_notes_index(conn, dir);
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
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("INDEX.md") {
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
        let emb_blob = embed_memory_passage(models_dir, &topic, &tags, &clean_body)
            .map(|v| serialize_f32_vec(&v));
        conn.execute(
            "INSERT INTO memories_meta (scope, topic, tags, content, md_path, embedding, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![parsed_scope, topic, tags, clean_body, path_str, emb_blob, created_at],
        )?;
    }
    let _ = backfill_missing_memory_embeddings(conn, models_dir);
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

/// Tool 9: `recall_memory` — hybrid search across Project `.notes/` (including `handoff` & `backlog`)
/// and Global `vault/`, with 1-hop Obsidian `[[wikilinks]]` expansion.
pub fn recall_memories_hybrid(
    project_conn: &Connection,
    paths: &AppPaths,
    query: &str,
    budget_tokens: usize,
) -> Result<(String, usize)> {
    let _ = sync_notes_dir_to_db(project_conn, &paths.notes_dir, "project", &paths.models_dir);

    let query_vec = embed_query(&paths.models_dir, query);

    let mut fts_keys: Vec<(String, i64)> = Vec::new();
    let mut vec_scored: Vec<((String, i64), f32)> = Vec::new();
    let mut records_map: HashMap<(String, i64), MemoryRecord> = HashMap::new();
    let mut total_raw_bytes = 0usize;

    // 1. Collect from Project DB
    collect_db_memories(
        project_conn,
        "project",
        query,
        query_vec.as_deref(),
        &mut fts_keys,
        &mut vec_scored,
        &mut records_map,
        &mut total_raw_bytes,
    )?;

    // 2. Collect from Global DB if initialized
    if paths.global_db.exists() || paths.vault_dir.exists() {
        let _ = paths.ensure_global_dirs();
        if let Ok(global_conn) = init_connection(&paths.global_db) {
            let _ =
                sync_notes_dir_to_db(&global_conn, &paths.vault_dir, "global", &paths.models_dir);
            let _ = collect_db_memories(
                &global_conn,
                "global",
                query,
                query_vec.as_deref(),
                &mut fts_keys,
                &mut vec_scored,
                &mut records_map,
                &mut total_raw_bytes,
            );
        }
    }

    let fused = reciprocal_rank_fusion(&fts_keys, vec_scored);

    if fused.is_empty() {
        let msg = format!("No relevant project or global memories found for query: '{query}'.\n");
        return Ok((msg.clone(), total_raw_bytes.max(msg.len())));
    }

    let mut formatted_blocks = Vec::new();
    let mut included_keys: HashSet<(String, i64)> = HashSet::new();

    for (key, score) in &fused {
        if let Some(rec) = records_map.get(key) {
            included_keys.insert(key.clone());
            let slug = slugify(&rec.topic);
            let block = format!(
                "- **[{scope}] [[{slug}|{topic}]]** (tags: `{tags}`, rrf: `{score:.4}`)\n  {content}\n",
                scope = rec.scope,
                topic = rec.topic,
                tags = rec.tags,
                content = rec.content.replace('\n', "\n  ")
            );
            formatted_blocks.push(block);

            // 1-hop [[wikilink]] resolution: surface linked notes automatically
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
    Ok((out.clone(), total_raw_bytes.max(out.len())))
}

/// Formats active Session Handoffs and aging Backlog work items (`lee backlog`).
pub fn list_backlog_and_handoff(conn: &Connection) -> Result<String> {
    let mut out = String::new();

    // Latest handoff
    let mut h_stmt = conn.prepare(
        "SELECT topic, tags, content, created_at
         FROM memories_meta
         WHERE scope = 'handoff'
         ORDER BY created_at DESC, id DESC
         LIMIT 3",
    )?;
    let handoffs: Vec<(String, String, String, i64)> = h_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .filter_map(|r| r.ok())
        .collect();

    out.push_str("=== Session Handoff (Latest First) ===\n");
    if handoffs.is_empty() {
        out.push_str("(No session handoffs recorded. Use `remember` with `scope=\"handoff\"`.)\n");
    } else {
        for (topic, tags, content, ts) in handoffs {
            out.push_str(&format!(
                "- [ts:{ts}] **{topic}** (`{tags}`)\n  {}\n",
                content.replace('\n', "\n  ")
            ));
        }
    }

    // Aging backlog items (oldest items first)
    let mut b_stmt = conn.prepare(
        "SELECT id, topic, tags, content, created_at
         FROM memories_meta
         WHERE scope = 'backlog' OR LOWER(tags) LIKE '%backlog%' OR LOWER(tags) LIKE '%todo%'
         ORDER BY created_at ASC, id ASC",
    )?;
    let backlogs: Vec<(i64, String, String, String, i64)> = b_stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    out.push_str("\n=== Work Backlog Queue (Aging Items First) ===\n");
    if backlogs.is_empty() {
        out.push_str("(No backlog items queued. Use `remember` with `scope=\"backlog\"`.)\n");
    } else {
        for (idx, (_id, topic, tags, content, ts)) in backlogs.iter().enumerate() {
            out.push_str(&format!(
                "{}. [[{}|{}]] (tags: `{tags}`, created: {ts})\n   {}\n",
                idx + 1,
                slugify(topic),
                topic,
                content.replace('\n', "\n   ")
            ));
        }
    }

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn collect_db_memories(
    conn: &Connection,
    scope_prefix: &str,
    query: &str,
    query_vec: Option<&[f32]>,
    fts_keys: &mut Vec<(String, i64)>,
    vec_scored: &mut Vec<((String, i64), f32)>,
    records_map: &mut HashMap<(String, i64), MemoryRecord>,
    total_raw_bytes: &mut usize,
) -> Result<()> {
    if let Ok(fts_ids) = search_memories_fts(conn, query, 30) {
        for id in fts_ids {
            fts_keys.push((scope_prefix.to_string(), id));
        }
    }

    let mut stmt = conn.prepare(
        "SELECT id, scope, topic, tags, content, md_path, embedding, created_at
         FROM memories_meta",
    )?;
    let rows = stmt.query_map([], |r| {
        let emb_bytes: Option<Vec<u8>> = r.get(6)?;
        Ok(MemoryRecord {
            id: r.get(0)?,
            scope: r.get(1)?,
            topic: r.get(2)?,
            tags: r.get(3)?,
            content: r.get(4)?,
            md_path: r.get(5)?,
            embedding: emb_bytes.map(|b| deserialize_f32_vec(&b)),
            created_at: r.get(7)?,
        })
    })?;

    let q_lower = query.to_lowercase();
    for row in rows.flatten() {
        *total_raw_bytes += row.content.len() + row.topic.len();
        let key = (scope_prefix.to_string(), row.id);

        if let (Some(qv), Some(ev)) = (query_vec, row.embedding.as_deref()) {
            let sim = cosine_similarity(qv, ev);
            vec_scored.push((key.clone(), sim));
        }

        // Substring fallback if FTS5 tokenizer missed exact symbol/punctuation or scope match
        if !fts_keys.contains(&key)
            && (row.topic.to_lowercase().contains(&q_lower)
                || row.tags.to_lowercase().contains(&q_lower)
                || row.scope.to_lowercase().contains(&q_lower)
                || row.content.to_lowercase().contains(&q_lower))
        {
            fts_keys.push(key.clone());
        }

        records_map.insert(key, row);
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct DocSectionRecord {
    id: i64,
    file_path: String,
    heading: String,
    start_line: i64,
    end_line: i64,
    content: String,
    embedding: Option<Vec<f32>>,
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

/// Tool 4: `search_docs` — hybrid FTS5 BM25 + BGE-Small vector search across `.md` sections in workspace & global docs.
pub fn search_docs_hybrid(
    conn: &mut Connection,
    paths: &AppPaths,
    query: &str,
    file_filter: Option<&str>,
    doc_name: Option<&str>,
    scope: Option<&str>,
    budget_tokens: usize,
) -> Result<(String, usize)> {
    // Lazily index workspace & registered workspace/global docs so `.md` sections are up to date
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

    let fts_raw = search_doc_sections_fts(conn, query, 50).unwrap_or_default();
    let query_vec = embed_query(&paths.models_dir, query);

    let mut stmt = conn.prepare(
        "SELECT id, file_path, heading, start_line, end_line, content, embedding
         FROM doc_sections_meta",
    )?;
    let rows = stmt.query_map([], |r| {
        let emb_blob: Option<Vec<u8>> = r.get(6)?;
        Ok(DocSectionRecord {
            id: r.get(0)?,
            file_path: r.get(1)?,
            heading: r.get(2)?,
            start_line: r.get(3)?,
            end_line: r.get(4)?,
            content: r.get(5)?,
            embedding: emb_blob.map(|b| deserialize_f32_vec(&b)),
        })
    })?;

    let mut sections_map: HashMap<i64, DocSectionRecord> = HashMap::new();
    let mut vec_scored: Vec<(i64, f32)> = Vec::new();
    let mut fts_filtered: Vec<i64> = Vec::new();
    let fts_set: HashSet<i64> = fts_raw.iter().copied().collect();
    let q_lower = query.to_lowercase();

    for sec in rows.flatten() {
        if !matches_file(&sec.file_path) {
            continue;
        }
        if let (Some(qv), Some(ev)) = (query_vec.as_deref(), sec.embedding.as_deref()) {
            let sim = cosine_similarity(qv, ev);
            vec_scored.push((sec.id, sim));
        }
        if !fts_set.contains(&sec.id)
            && (sec.heading.to_lowercase().contains(&q_lower)
                || sec.content.to_lowercase().contains(&q_lower))
        {
            fts_filtered.push(sec.id);
        }
        sections_map.insert(sec.id, sec);
    }

    let mut ordered_fts = Vec::new();
    for id in fts_raw {
        if sections_map.contains_key(&id) {
            ordered_fts.push(id);
        }
    }
    for id in fts_filtered {
        if !ordered_fts.contains(&id) {
            ordered_fts.push(id);
        }
    }

    let fused = reciprocal_rank_fusion(&ordered_fts, vec_scored);
    if fused.is_empty() {
        let msg = format!("No documentation sections matched query '{query}'.\n");
        return Ok((msg.clone(), msg.len()));
    }

    let mut matched_files: HashSet<String> = HashSet::new();
    let mut blocks = Vec::new();
    let mut seen_ranges: Vec<(String, i64, i64)> = Vec::new();

    for (sec_id, score) in fused {
        if let Some(sec) = sections_map.get(&sec_id) {
            // Skip overlapping windows from the same file if already covered
            if seen_ranges.iter().any(|(fp, st, en)| {
                fp == &sec.file_path && sec.start_line <= *en && sec.end_line >= *st
            }) {
                continue;
            }
            seen_ranges.push((sec.file_path.clone(), sec.start_line, sec.end_line));
            matched_files.insert(sec.file_path.clone());
            let block = format!(
                "### {} [{start}..{end}] — `{heading}` (rrf: `{score:.4}`)\n{content}\n\n",
                sec.file_path,
                start = sec.start_line,
                end = sec.end_line,
                heading = sec.heading,
                content = sec.content
            );
            blocks.push(block);
        }
    }

    let mut raw_bytes = 0usize;
    for fp in &matched_files {
        let file_total: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM doc_sections_meta WHERE file_path = ?1",
                params![fp],
                |r| r.get(0),
            )
            .unwrap_or(0);
        raw_bytes += file_total as usize;
    }

    let body = apply_token_budget(&blocks, budget_tokens);
    let out = format!("# Documentation Search Results (`{query}`)\n\n{body}");
    Ok((out.clone(), raw_bytes.max(out.len())))
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
    fn test_remember_wikilinks_handoff_backlog_and_recall() {
        let temp = std::env::temp_dir().join(format!("lee_mem_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let mut paths = AppPaths::from_root(temp.clone()).unwrap();
        paths.global_dir = temp.join("global_cfg");
        paths.global_db = paths.global_dir.join("global.db");
        paths.models_dir = paths.global_dir.join("models");
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

        store_memory(
            &conn,
            &paths,
            "Add CJK Substring Test",
            "Verify smart_grep works on non-space scripts.",
            Some("backlog, test"),
            Some("backlog"),
        )
        .unwrap();

        store_memory(
            &conn,
            &paths,
            "Phase 6 Handoff",
            "Completed all 9 MCP tools; next step is running production benchmarks.",
            Some("session, handoff"),
            Some("handoff"),
        )
        .unwrap();

        // Verify 1-hop [[wikilink]] expansion: querying Indexer Transaction Rule pulls in Database WAL Mode
        let (recalled, _) =
            recall_memories_hybrid(&conn, &paths, "Indexer Transaction Rule", 600).unwrap();
        assert!(recalled.contains("Indexer Transaction Rule"));
        assert!(recalled.contains("Database WAL Mode"));

        // Verify generated INDEX.md exists and contains wikilinks
        let index_md = fs::read_to_string(paths.notes_dir.join("INDEX.md")).unwrap();
        assert!(index_md.contains("[[database-wal-mode|Database WAL Mode]]"));

        // Verify backlog & handoff report
        let report = list_backlog_and_handoff(&conn).unwrap();
        assert!(report.contains("Phase 6 Handoff"));
        assert!(report.contains("Add CJK Substring Test"));

        let _ = fs::remove_dir_all(&temp);
    }
}
