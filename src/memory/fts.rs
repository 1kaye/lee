use anyhow::Result;
use rusqlite::{params, Connection};

/// Splits an identifier or natural-language query into lowercase subword tokens,
/// decomposing both `snake_case` and `camelCase`/`PascalCase` boundaries.
pub fn tokenize_query_terms(raw_query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut push_unique = |s: String| {
        if s.len() >= 2 && !terms.contains(&s) && terms.len() < 16 {
            terms.push(s);
        }
    };

    for word in raw_query.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let trimmed = word.trim_matches('_');
        if trimmed.len() < 2 {
            continue;
        }
        // Push subwords from snake_case and camelCase
        for part in trimmed.split('_') {
            if part.len() < 2 {
                continue;
            }
            let mut cur = String::new();
            let mut prev_lower = false;
            for ch in part.chars() {
                if ch.is_uppercase() && prev_lower && cur.len() >= 2 {
                    push_unique(cur.to_lowercase());
                    cur.clear();
                }
                prev_lower = ch.is_lowercase();
                cur.push(ch);
            }
            if cur.len() >= 2 {
                push_unique(cur.to_lowercase());
            }
            push_unique(part.to_lowercase());
        }
    }
    terms
}

/// Queries an FTS5 virtual table with column-weighted BM25, prioritizing exact `AND` hits,
/// then prefix-boosted `OR` hits (`"token"*` for terms >= 4 chars).
fn query_fts_ranked(
    conn: &Connection,
    table: &str,
    bm25_weights: &str,
    raw_query: &str,
    limit: usize,
) -> Result<Vec<i64>> {
    let terms = tokenize_query_terms(raw_query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    let mut ordered = Vec::new();

    if (2..=6).contains(&terms.len()) {
        let and_q: String = terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql_and = format!(
            "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY bm25({table}, {bm25_weights}) ASC LIMIT ?2"
        );
        if let Ok(mut stmt) = conn.prepare(&sql_and)
            && let Ok(rows) = stmt.query_map(params![and_q, limit as i64], |r| r.get::<_, i64>(0))
        {
            for id in rows.flatten() {
                if !ordered.contains(&id) {
                    ordered.push(id);
                }
            }
        }
    }

    if ordered.len() < limit {
        let or_q: String = terms
            .iter()
            .map(|t| {
                if t.len() >= 4 {
                    format!("\"{t}\"*")
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql_or = format!(
            "SELECT rowid FROM {table} WHERE {table} MATCH ?1 ORDER BY bm25({table}, {bm25_weights}) ASC LIMIT ?2"
        );
        if let Ok(mut stmt) = conn.prepare(&sql_or)
            && let Ok(rows) = stmt.query_map(params![or_q, limit as i64], |r| r.get::<_, i64>(0))
        {
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

    Ok(ordered)
}

/// Queries `doc_sections_fts(file_path, heading, content)` with heading weighted 10x and file_path 5x.
pub fn search_doc_sections_fts(conn: &Connection, query: &str, limit: usize) -> Result<Vec<i64>> {
    query_fts_ranked(conn, "doc_sections_fts", "5.0, 10.0, 1.0", query, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subword_tokenizer_splits_camelcase_and_snakecase() {
        let terms = tokenize_query_terms("readSymbolRange & parse_markdown_doc");
        assert!(terms.contains(&"read".to_string()));
        assert!(terms.contains(&"symbol".to_string()));
        assert!(terms.contains(&"range".to_string()));
        assert!(terms.contains(&"parse".to_string()));
        assert!(terms.contains(&"markdown".to_string()));
        assert!(terms.contains(&"doc".to_string()));
    }
}
