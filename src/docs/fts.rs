use anyhow::Result;
use rusqlite::{params, Connection};

/// Splits an identifier or natural-language query into lowercase subword tokens,
/// decomposing `snake_case`, `camelCase`, `PascalCase`, and acronyms (`HTTPServer` -> `http`, `server`).
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
        for part in trimmed.split('_') {
            if part.len() < 2 {
                continue;
            }
            let chars: Vec<char> = part.chars().collect();
            let mut cur = String::new();
            for i in 0..chars.len() {
                let ch = chars[i];
                let prev = i.checked_sub(1).map(|j| chars[j]);
                let next = chars.get(i + 1).copied();
                let split_here = !cur.is_empty()
                    && ch.is_uppercase()
                    && (prev.is_some_and(|p| p.is_lowercase())
                        || (prev.is_some_and(|p| p.is_uppercase())
                            && next.is_some_and(|n| n.is_lowercase())));
                if split_here {
                    push_unique(cur.to_lowercase());
                    cur.clear();
                }
                cur.push(ch);
            }
            if cur.len() >= 2 {
                push_unique(cur.to_lowercase());
            }
        }
    }
    terms
}

/// Queries `doc_sections_fts(file_path, heading, content)` using column-weighted BM25
/// (heading 10x, file_path 5x, content 1x): exact `AND` hits first, then prefix-boosted `OR` hits.
pub fn search_doc_sections_fts(conn: &Connection, raw_query: &str, limit: usize) -> Result<Vec<i64>> {
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
        if let Ok(mut stmt) = conn.prepare_cached(
            "SELECT rowid FROM doc_sections_fts
             WHERE doc_sections_fts MATCH ?1
             ORDER BY bm25(doc_sections_fts, 5.0, 10.0, 1.0) ASC
             LIMIT ?2",
        ) && let Ok(rows) = stmt.query_map(params![and_q, limit as i64], |r| r.get::<_, i64>(0))
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
        if let Ok(mut stmt) = conn.prepare_cached(
            "SELECT rowid FROM doc_sections_fts
             WHERE doc_sections_fts MATCH ?1
             ORDER BY bm25(doc_sections_fts, 5.0, 10.0, 1.0) ASC
             LIMIT ?2",
        ) && let Ok(rows) = stmt.query_map(params![or_q, limit as i64], |r| r.get::<_, i64>(0))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subword_tokenizer_splits_camelcase_acronyms_and_snakecase() {
        let terms = tokenize_query_terms("readSymbolRange & HTTPServer_parse_md");
        assert_eq!(
            terms,
            vec!["read", "symbol", "range", "http", "server", "parse", "md"]
        );
    }
}
