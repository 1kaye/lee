use anyhow::Result;
use rusqlite::{params, Connection};

fn tokenize_query(raw_query: &str) -> Vec<String> {
    raw_query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s.len() >= 2)
        .take(16)
        .map(|t| format!("\"{t}\""))
        .collect()
}

/// Sanitizes an arbitrary natural-language query into a safe FTS5 MATCH expression (`OR`).
pub fn build_fts5_match_query(raw_query: &str) -> Option<String> {
    let tokens = tokenize_query(raw_query);
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" OR "))
    }
}

/// Queries an FTS5 virtual table by prioritizing `AND` (all-term) hits when applicable, then `OR` BM25 hits.
fn query_fts_ranked(conn: &Connection, table: &str, raw_query: &str, limit: usize) -> Result<Vec<i64>> {
    let tokens = tokenize_query(raw_query);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let mut ordered = Vec::new();

    if tokens.len() >= 2 && tokens.len() <= 5 {
        let and_q = tokens.join(" AND ");
        let sql_and = format!(
            "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY bm25({table}) ASC LIMIT ?2"
        );
        if let Ok(mut stmt) = conn.prepare(&sql_and) {
            if let Ok(rows) = stmt.query_map(params![and_q, limit as i64], |r| r.get::<_, i64>(0)) {
                for id in rows.flatten() {
                    if !ordered.contains(&id) {
                        ordered.push(id);
                    }
                }
            }
        }
    }

    if ordered.len() < limit {
        let or_q = tokens.join(" OR ");
        let sql_or = format!(
            "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY bm25({table}) ASC LIMIT ?2"
        );
        if let Ok(mut stmt) = conn.prepare(&sql_or) {
            if let Ok(rows) = stmt.query_map(params![or_q, limit as i64], |r| r.get::<_, i64>(0)) {
                for id in rows.flatten() {
                    if !ordered.contains(&id) {
                        ordered.push(id);
                        if ordered.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(ordered)
}

/// Queries `memories_fts` using BM25 ranking (multi-term AND boost + OR fallback).
pub fn search_memories_fts(conn: &Connection, query: &str, limit: usize) -> Result<Vec<i64>> {
    query_fts_ranked(conn, "memories_fts", query, limit)
}

/// Queries `doc_sections_fts` using BM25 ranking (multi-term AND boost + OR fallback).
pub fn search_doc_sections_fts(conn: &Connection, query: &str, limit: usize) -> Result<Vec<i64>> {
    query_fts_ranked(conn, "doc_sections_fts", query, limit)
}
