use anyhow::{Context, Result};
use rusqlite::{params, Connection};
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
         PRAGMA foreign_keys = ON;",
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

pub fn record_token_metric(
    conn: &Connection,
    tool_name: &str,
    raw_bytes: usize,
    sent_bytes: usize,
) -> Result<()> {
    let raw = raw_bytes as i64;
    let sent = sent_bytes as i64;
    let saved = (raw - sent).max(0);
    conn.execute(
        "INSERT INTO token_metrics (timestamp, tool_name, raw_bytes, sent_bytes, saved_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![now_unix(), tool_name, raw, sent, saved],
    )?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct ToolMetricRow {
    pub tool_name: String,
    pub calls: i64,
    pub raw_bytes: i64,
    pub sent_bytes: i64,
    pub saved_bytes: i64,
}

#[derive(Debug, Clone)]
pub struct DailyMetricRow {
    pub day: String,
    pub calls: i64,
    pub raw_bytes: i64,
    pub sent_bytes: i64,
    pub saved_bytes: i64,
}

#[derive(Debug, Clone)]
pub struct HistoryMetricRow {
    pub timestamp: String,
    pub tool_name: String,
    pub raw_bytes: i64,
    pub sent_bytes: i64,
    pub saved_bytes: i64,
}

pub fn query_token_metrics(conn: &Connection) -> Result<Vec<ToolMetricRow>> {
    let mut stmt = conn.prepare(
        "SELECT tool_name,
                COUNT(*) as calls,
                COALESCE(SUM(raw_bytes), 0) as total_raw,
                COALESCE(SUM(sent_bytes), 0) as total_sent,
                COALESCE(SUM(saved_bytes), 0) as total_saved
         FROM token_metrics
         GROUP BY tool_name
         ORDER BY total_saved DESC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ToolMetricRow {
                tool_name: row.get(0)?,
                calls: row.get(1)?,
                raw_bytes: row.get(2)?,
                sent_bytes: row.get(3)?,
                saved_bytes: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_daily_token_metrics(conn: &Connection) -> Result<Vec<DailyMetricRow>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(strftime('%Y-%m-%d', timestamp, 'unixepoch', 'localtime'), 'unknown') as day,
                COUNT(*) as calls,
                COALESCE(SUM(raw_bytes), 0) as total_raw,
                COALESCE(SUM(sent_bytes), 0) as total_sent,
                COALESCE(SUM(saved_bytes), 0) as total_saved
         FROM token_metrics
         GROUP BY day
         ORDER BY day DESC
         LIMIT 30",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DailyMetricRow {
                day: row.get(0)?,
                calls: row.get(1)?,
                raw_bytes: row.get(2)?,
                sent_bytes: row.get(3)?,
                saved_bytes: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn query_recent_token_history(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<HistoryMetricRow>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(strftime('%Y-%m-%d %H:%M:%S', timestamp, 'unixepoch', 'localtime'), 'unknown'),
                tool_name,
                raw_bytes,
                sent_bytes,
                saved_bytes
         FROM token_metrics
         ORDER BY id DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok(HistoryMetricRow {
                timestamp: row.get(0)?,
                tool_name: row.get(1)?,
                raw_bytes: row.get(2)?,
                sent_bytes: row.get(3)?,
                saved_bytes: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_schema_and_metrics() {
        let temp_db = std::env::temp_dir().join(format!("lee_db_test_{}.db", std::process::id()));
        let _ = fs::remove_file(&temp_db);
        let conn = init_connection(&temp_db).unwrap();

        record_token_metric(&conn, "project_map", 40_000, 1_200).unwrap();
        record_token_metric(&conn, "project_map", 20_000, 800).unwrap();

        let stats = query_token_metrics(&conn).unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].tool_name, "project_map");
        assert_eq!(stats[0].calls, 2);
        assert_eq!(stats[0].raw_bytes, 60_000);
        assert_eq!(stats[0].sent_bytes, 2_000);
        assert_eq!(stats[0].saved_bytes, 58_000);

        let _ = fs::remove_file(&temp_db);
    }
}
