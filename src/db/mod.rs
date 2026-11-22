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
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 1000;
         PRAGMA journal_size_limit = 67108864;",
    )?;

    conn.execute_batch(SCHEMA_SQL)
        .context("Failed to initialize database schema")?;

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

    #[test]
    fn test_db_schema_init() {
        let temp_db = std::env::temp_dir().join(format!("lee_db_test_{}.db", std::process::id()));
        let _ = fs::remove_file(&temp_db);
        let conn = init_connection(&temp_db).unwrap();
        let cnt: i64 = conn
            .query_row("SELECT COUNT(*) FROM file_hashes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cnt, 0);
        let _ = fs::remove_file(&temp_db);
    }
}
