pub mod cargo;
pub mod generic;
pub mod node;
pub mod python_go;

use crate::config::AppPaths;
use crate::db::now_unix;
use crate::security::{validate_command, validate_path};
use anyhow::{bail, Context, Result};
use regex::{Regex, RegexBuilder};
use rusqlite::{params, Connection, OptionalExtension};
use std::process::Command;
use std::sync::OnceLock;

pub const INLINE_LINE_THRESHOLD: usize = 35;

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
            .expect("valid ANSI strip regex")
    })
}

/// Strips ANSI color/terminal escape sequences so parsers match cleanly and no raw ANSI wastes tokens.
pub fn strip_ansi(input: &str) -> String {
    ansi_re().replace_all(input, "").into_owned()
}

/// RTK never-worse invariant: never emit a wrapper/digest longer than the stripped raw output.
pub fn never_worse<'a>(raw: &'a str, filtered: &'a str) -> &'a str {
    if filtered.len() > raw.len() {
        raw
    } else {
        filtered
    }
}

/// Tool 6: `run_digest` — executes a shell command, strips ANSI & build noise,
/// returns inline if <= 35 lines (respecting never-worse), or stores raw output in `handles` (`h:<6-char-blake3>`).
pub fn run_command_digest(
    conn: &Connection,
    paths: &AppPaths,
    command: &str,
    cwd: Option<&str>,
) -> Result<(String, usize)> {
    let (out, raw_bytes, _exit_code) = run_command_digest_with_exit(conn, paths, command, cwd)?;
    Ok((out, raw_bytes))
}

pub fn run_command_digest_with_exit(
    conn: &Connection,
    paths: &AppPaths,
    command: &str,
    cwd: Option<&str>,
) -> Result<(String, usize, i32)> {
    validate_command(command)?;

    let allowed_docs = paths.all_doc_paths();
    let work_dir = match cwd {
        Some(dir) if !dir.trim().is_empty() && dir.trim() != "." => {
            validate_path(&paths.project_root, &allowed_docs, std::path::Path::new(dir))?
        }
        _ => paths.project_root.clone(),
    };

    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&work_dir)
        .output()
        .with_context(|| format!("Failed to execute command: {command}"))?;

    let exit_code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut combined_raw = String::with_capacity(stdout.len() + stderr.len() + 2);
    if !stdout.is_empty() {
        combined_raw.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined_raw.is_empty() && !combined_raw.ends_with('\n') {
            combined_raw.push('\n');
        }
        combined_raw.push_str(&stderr);
    }

    let raw_bytes = combined_raw.len();
    let clean_combined = strip_ansi(&combined_raw);
    let total_lines = clean_combined.lines().count();
    let code_num = exit_code.unwrap_or(-1);

    if total_lines <= INLINE_LINE_THRESHOLD {
        // Strip noisy Cargo `Compiling`/`Checking` progress lines when passing cleanly
        let compact_inline = if cargo::is_cargo_command(command) && exit_code == Some(0) {
            let mut compiled = 0usize;
            let mut kept = Vec::new();
            for line in clean_combined.lines() {
                let t = line.trim_start();
                if t.starts_with("Compiling ")
                    || t.starts_with("Checking ")
                    || t.starts_with("Downloading ")
                    || t.starts_with("Downloaded ")
                {
                    compiled += 1;
                } else {
                    kept.push(line);
                }
            }
            if compiled > 0 {
                format!("(compiled {compiled} crate(s))\n{}", kept.join("\n"))
            } else {
                clean_combined.clone()
            }
        } else if generic::is_git_status_or_log_command(command) {
            generic::parse_git_status_or_log_output(command, &clean_combined, exit_code)
                .summary_lines
                .join("\n")
        } else {
            clean_combined.clone()
        };
        let out = format!(
            "# Command Output (exit={}, {} lines)\n{}",
            code_num,
            total_lines,
            compact_inline
        );
        return Ok((out, raw_bytes.max(1), code_num));
    }

    let digest_text = digest_raw_output(conn, command, &clean_combined, exit_code)?;
    let final_out = never_worse(&clean_combined, &digest_text).to_string();
    Ok((final_out, raw_bytes.max(1), code_num))
}

/// Summarizes arbitrary raw output (used by `run_digest` and `lee hook digest`) and stores a handle `h:ID`.
pub fn digest_raw_output(
    conn: &Connection,
    source_cmd: &str,
    raw_content: &str,
    exit_code: Option<i32>,
) -> Result<String> {
    let clean = strip_ansi(raw_content);
    let total_lines = clean.lines().count();
    let hash_hex = blake3::hash(clean.as_bytes()).to_hex().to_string();
    let handle_id = format!("h:{}", &hash_hex[..6]);

    let parsed = if generic::is_diff_command(source_cmd, &clean) {
        generic::parse_diff_output(&clean, exit_code)
    } else if generic::is_git_status_or_log_command(source_cmd) {
        generic::parse_git_status_or_log_output(source_cmd, &clean, exit_code)
    } else if cargo::is_cargo_command(source_cmd) {
        cargo::parse_cargo_output_with_cmd(source_cmd, &clean, exit_code)
    } else if node::is_node_command(source_cmd) {
        node::parse_node_output(&clean, exit_code)
    } else if python_go::is_python_or_go_command(source_cmd) {
        python_go::parse_python_go_output(&clean, exit_code)
    } else {
        generic::parse_generic_output(&clean, exit_code)
    };

    let failures_text = if parsed.failure_blocks.is_empty() {
        None
    } else {
        Some(parsed.failure_blocks.join("\n---\n"))
    };

    let mut digest = format!(
        "# Digest [{handle_id}] — `{source_cmd}` (exit={}, {total_lines} lines, {} bytes)\n",
        exit_code.unwrap_or(0),
        clean.len()
    );
    for line in &parsed.summary_lines {
        digest.push_str(&format!("{line}\n"));
    }
    if !parsed.failure_blocks.is_empty() {
        digest.push_str(&format!(
            "\n## Extracted Errors/Failures ({} block(s))\n",
            parsed.failure_blocks.len()
        ));
        for block in parsed.failure_blocks.iter().take(6) {
            digest.push_str(block);
            digest.push_str("\n---\n");
        }
    }
    digest.push_str(&format!(
        "\nUse `recall(key=\"{handle_id}\", item=\"NAME\")`, `recall(key=\"{handle_id}\", failures_only=true)`, or `recall(key=\"{handle_id}\", start_line=1, end_line=80)` to inspect details.\n"
    ));

    conn.execute(
        "INSERT OR REPLACE INTO handles
         (id, source_cmd, exit_code, total_lines, raw_content, digest_text, failures_text, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            &handle_id,
            source_cmd,
            exit_code,
            total_lines as i64,
            &clean,
            &digest,
            failures_text,
            now_unix(),
        ],
    )?;

    Ok(digest)
}

/// Tool 7: `recall` — retrieves specific items (test/function/diff file), failures, regex-filtered context, or line ranges from a stored handle `h:ID`.
pub fn recall_handle(
    conn: &Connection,
    key: &str,
    item: Option<&str>,
    failures_only: bool,
    filter: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<(String, usize)> {
    let normalized_key = if key.starts_with("h:") {
        key.to_string()
    } else {
        format!("h:{key}")
    };

    #[allow(clippy::type_complexity)]
    let row: Option<(String, Option<i32>, i64, String, Option<String>)> = conn
        .query_row(
            "SELECT source_cmd, exit_code, total_lines, raw_content, failures_text
             FROM handles WHERE id = ?1",
            params![&normalized_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

    let Some((source_cmd, exit_code, total_lines, raw_content, failures_text)) = row else {
        bail!("Handle '{}' not found in SQLite database.", normalized_key);
    };

    let raw_bytes = raw_content.len();
    let lines: Vec<&str> = raw_content.lines().collect();

    if let Some(item_name) = item.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let extracted = extract_item_block(&lines, item_name);
        let out = format!(
            "# Recall [{normalized_key}] (item=`{item_name}`) — `{source_cmd}`\n{extracted}\n"
        );
        return Ok((out, raw_bytes));
    }

    if failures_only {
        let body = failures_text
            .unwrap_or_else(|| "No extracted failures recorded for this handle.".to_string());
        let out = format!(
            "# Recall [{normalized_key}] (failures_only) — `{source_cmd}` (exit={:?})\n{body}\n",
            exit_code.unwrap_or(0)
        );
        return Ok((out, raw_bytes));
    }

    if let Some(pat) = filter
        && !pat.trim().is_empty()
    {
        let re = RegexBuilder::new(pat)
            .case_insensitive(true)
            .build()
            .or_else(|_| {
                RegexBuilder::new(&regex::escape(pat))
                    .case_insensitive(true)
                    .build()
            })?;

        let mut matched_ranges: Vec<(usize, usize)> = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                let st = idx.saturating_sub(3);
                let en = (idx + 3).min(lines.len().saturating_sub(1));
                if let Some(last) = matched_ranges.last_mut()
                    && st <= last.1 + 1
                {
                    last.1 = last.1.max(en);
                    continue;
                }
                matched_ranges.push((st, en));
            }
        }

        let mut out =
            format!("# Recall [{normalized_key}] (filter=`{pat}`) — `{source_cmd}`\n");
        if matched_ranges.is_empty() {
            out.push_str("(No lines matched filter)\n");
        } else {
            for (st, en) in matched_ranges.into_iter().take(25) {
                for (ln, line_txt) in lines.iter().enumerate().take(en + 1).skip(st) {
                    out.push_str(&format!("{:>4} | {}\n", ln + 1, line_txt));
                }
                out.push_str("---\n");
            }
        }
        return Ok((out, raw_bytes));
    }

    let st = start_line.unwrap_or(1).max(1);
    let en = end_line
        .unwrap_or((st + 80).min(lines.len().max(1)))
        .max(st)
        .min(lines.len().max(1));

    let mut out = format!(
        "# Recall [{normalized_key}] [lines {st}..{en} of {total_lines}] — `{source_cmd}`\n"
    );
    for ln in st..=en {
        if let Some(l) = lines.get(ln - 1) {
            out.push_str(&format!("{ln:>4} | {l}\n"));
        }
    }

    Ok((out, raw_bytes))
}

fn extract_item_block(lines: &[&str], item_name: &str) -> String {
    let item_lower = item_name.to_lowercase();

    // 1. Check if this is a unified diff or `kaye wc` output and `item_name` matches a file section
    for (idx, line) in lines.iter().enumerate() {
        let is_git_diff_hdr = line.starts_with("diff --git ");
        let is_kaye_hdr = generic::parse_kaye_wc_file_header(line).is_some();
        if (is_git_diff_hdr || is_kaye_hdr) && line.to_lowercase().contains(&item_lower) {
            let mut end_idx = (idx + 120).min(lines.len());
            for (next_idx, next_line) in lines[(idx + 1)..].iter().enumerate() {
                if next_line.starts_with("diff --git ")
                    || generic::parse_kaye_wc_file_header(next_line).is_some()
                    || next_line.starts_with("====")
                {
                    end_idx = (idx + 1 + next_idx).min(idx + 120);
                    break;
                }
            }
            return lines[idx..end_idx]
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{:>4} | {l}", idx + i + 1))
                .collect::<Vec<_>>()
                .join("\n");
        }
    }

    // 2. Check for test stdout headers (`---- <item> stdout ----` or `--- FAIL: <item>`) or general symbol/item match
    let mut blocks = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        if lines[idx].to_lowercase().contains(&item_lower) {
            let st = idx.saturating_sub(2);
            let mut en = (idx + 35).min(lines.len());
            for (rel, l) in lines[(idx + 1)..en].iter().enumerate() {
                if (l.starts_with("---- ") && l.ends_with(" stdout ----"))
                    || l.starts_with("diff --git ")
                    || l.starts_with("test result:")
                {
                    en = idx + 1 + rel;
                    break;
                }
            }
            let snippet = (st..en)
                .map(|ln| format!("{:>4} | {}", ln + 1, lines[ln]))
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(snippet);
            if blocks.len() >= 5 {
                break;
            }
            idx = en;
        } else {
            idx += 1;
        }
    }

    if blocks.is_empty() {
        format!("(No item block matching '{item_name}' found in handle)")
    } else {
        blocks.join("\n---\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_connection;

    #[test]
    fn test_digest_and_recall_handle_with_item_and_diff() {
        let temp_db =
            std::env::temp_dir().join(format!("lee_digest_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&temp_db);
        let conn = init_connection(&temp_db).unwrap();

        let mut fake_log = String::new();
        for i in 1..=50 {
            if i == 22 {
                fake_log.push_str("---- auth::test_login stdout ----\nthread panicked at 'invalid token'\n");
            } else {
                fake_log.push_str(&format!("Compiling crate_{i} v0.1.0\n"));
            }
        }
        fake_log.push_str("test result: FAILED. 10 passed; 1 failed;\n");

        let digest = digest_raw_output(&conn, "cargo test", &fake_log, Some(101)).unwrap();
        let start = digest.find("h:").unwrap();
        let handle_id = &digest[start..start + 8];

        let (failures, raw_len) =
            recall_handle(&conn, handle_id, None, true, None, None, None).unwrap();
        assert_eq!(raw_len, fake_log.len());
        assert!(failures.contains("test_login"));

        let (item_block, _) =
            recall_handle(&conn, handle_id, Some("test_login"), false, None, None, None).unwrap();
        assert!(item_block.contains("invalid token"));

        // ANSI-colored cargo error should be stripped and extracted cleanly
        let ansi_log = format!(
            "{}\n\x1b[1;31merror[E0425]\x1b[0m: cannot find value `x`\n",
            "Compiling dep v0.1.0\n".repeat(40)
        );
        let ansi_digest = digest_raw_output(&conn, "cargo check", &ansi_log, Some(101)).unwrap();
        assert!(ansi_digest.contains("error[E0425]"));
        assert!(!ansi_digest.contains("\x1b["));
        assert_eq!(never_worse("short", "longer wrapper text"), "short");

        let _ = std::fs::remove_file(&temp_db);
    }
}
