use anyhow::{Context, Result};
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_SQL: &str = include_str!("schema.sql");

pub fn init_connection(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create DB directory {}", parent.display()))?;
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open SQLite DB at {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = 67108864;",
    )?;

    conn.execute_batch(SCHEMA_SQL)
        .context("Failed to initialize database schema")?;

    Ok(conn)
}

/// Opens an ephemeral in-memory SQLite connection (used when cache dir is read-only inside sandboxes).
pub fn init_in_memory_connection() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("Failed to open in-memory SQLite DB")?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    conn.execute_batch(SCHEMA_SQL)
        .context("Failed to initialize in-memory database schema")?;
    Ok(conn)
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    #[test]
    fn test_db_schema_wal_cascade_and_fts_triggers() {
        let temp_dir = std::env::temp_dir().join(format!("lee_db_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let temp_db = temp_dir.join("lee.db");

        {
            let conn = init_connection(&temp_db).unwrap();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");

            let timeout: i64 = conn
                .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap();
            assert!(timeout >= 5000);

            conn.execute(
                "INSERT INTO file_hashes (file_path, blake3_hash, line_count, file_type, last_indexed)
                 VALUES ('doc.md', 'abc', 10, 'markdown', 100)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO doc_sections_meta (file_path, heading, start_line, end_line, content)
                 VALUES ('doc.md', '# Guide', 1, 10, 'alpha token')",
                [],
            )
            .unwrap();
            let fts_hits: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM doc_sections_fts WHERE doc_sections_fts MATCH '\"alpha\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(fts_hits, 1);

            // UPDATE trigger keeps FTS synced
            conn.execute(
                "UPDATE doc_sections_meta SET content = 'omega token' WHERE file_path = 'doc.md'",
                [],
            )
            .unwrap();
            let omega_hits: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM doc_sections_fts WHERE doc_sections_fts MATCH '\"omega\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(omega_hits, 1);

            // ON DELETE CASCADE cleans doc_sections_meta + FTS via AD trigger
            conn.execute("DELETE FROM file_hashes WHERE file_path = ?1", params!["doc.md"])
                .unwrap();
            let remaining: i64 = conn
                .query_row("SELECT COUNT(*) FROM doc_sections_meta", [], |r| r.get(0))
                .unwrap();
            let fts_remaining: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM doc_sections_fts WHERE doc_sections_fts MATCH '\"omega\"'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 0);
            assert_eq!(fts_remaining, 0);
        }

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
