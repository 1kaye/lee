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
const MAX_STORED_HANDLES: i64 = 200;

#[derive(Debug, Clone)]
pub struct ParsedDigest {
    pub summary_lines: Vec<String>,
    pub failure_blocks: Vec<String>,
}

pub(crate) fn format_exit_status(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => code.to_string(),
        None => "signal".to_string(),
    }
}

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
) -> Result<String> {
    let (mut out, exit_code) = run_command_digest_with_exit(conn, paths, command, cwd)?;
    if exit_code != 0 && !out.starts_with("# Digest [") && !out.starts_with("(command exited") {
        let status = if exit_code < 0 {
            "signal".to_string()
        } else {
            exit_code.to_string()
        };
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("(exit={status})"));
    }
    Ok(out)
}

pub fn run_command_digest_with_exit(
    conn: &Connection,
    paths: &AppPaths,
    command: &str,
    cwd: Option<&str>,
) -> Result<(String, i32)> {
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
        // Return clean output directly without wrapper headers so JSON/short outputs remain intact
        let out = if compact_inline.is_empty() && code_num != 0 {
            format!("(command exited with status {})", format_exit_status(exit_code))
        } else {
            compact_inline
        };
        return Ok((out, code_num));
    }

    let digest_text = digest_raw_output(conn, command, &clean_combined, exit_code)?;
    let final_out = never_worse(&clean_combined, &digest_text).to_string();
    Ok((final_out, code_num))
}

/// Summarizes arbitrary raw output (used by `run_digest` and `lee exec`) and stores a handle `h:ID`.
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
        format_exit_status(exit_code),
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
    let _ = conn.execute(
        "DELETE FROM handles
         WHERE id NOT IN (SELECT id FROM handles ORDER BY created_at DESC LIMIT ?1)",
        params![MAX_STORED_HANDLES],
    );

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
) -> Result<String> {
    let trimmed_key = key.trim();
    if trimmed_key.is_empty() {
        bail!("Provide a non-empty handle `key` (e.g. `h:a1b2c3`).");
    }
    let normalized_key = if trimmed_key.starts_with("h:") {
        trimmed_key.to_string()
    } else {
        format!("h:{trimmed_key}")
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

    let lines: Vec<&str> = raw_content.lines().collect();

    if let Some(item_name) = item.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let extracted = extract_item_block(&lines, item_name);
        return Ok(format!(
            "# Recall [{normalized_key}] (item=`{item_name}`) — `{source_cmd}`\n{extracted}\n"
        ));
    }

    if failures_only {
        let body = failures_text
            .unwrap_or_else(|| "No extracted failures recorded for this handle.".to_string());
        return Ok(format!(
            "# Recall [{normalized_key}] (failures_only) — `{source_cmd}` (exit={})\n{body}\n",
            format_exit_status(exit_code)
        ));
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
        return Ok(out);
    }

    let line_count = lines.len();
    if line_count == 0 {
        return Ok(format!(
            "# Recall [{normalized_key}] [empty output] — `{source_cmd}`\n"
        ));
    }
    let st = start_line.unwrap_or(1).max(1);
    if st > line_count {
        return Ok(format!(
            "# Recall [{normalized_key}] [start_line={st} exceeds total lines ({line_count})] — `{source_cmd}`\n"
        ));
    }
    let default_end = st.saturating_add(79).min(line_count);
    let en = end_line.unwrap_or(default_end).max(st).min(line_count);

    let mut out = format!(
        "# Recall [{normalized_key}] [lines {st}..{en} of {total_lines}] — `{source_cmd}`\n"
    );
    for ln in st..=en {
        if let Some(l) = lines.get(ln - 1) {
            out.push_str(&format!("{ln:>4} | {l}\n"));
        }
    }

    Ok(out)
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
    fn test_digest_and_recall_handle_with_item_and_bounds() {
        let temp_dir =
            std::env::temp_dir().join(format!("lee_digest_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let temp_db = temp_dir.join("digest.db");
        let conn = init_connection(&temp_db).unwrap();

        let mut fake_log = String::new();
        for i in 1..=50 {
            if i == 22 {
                fake_log.push_str(
                    "---- auth::test_login stdout ----\nthread panicked at 'invalid token'\n",
                );
            } else {
                fake_log.push_str(&format!("Compiling crate_{i} v0.1.0\n"));
            }
        }
        fake_log.push_str("test result: FAILED. 10 passed; 1 failed;\n");

        let digest = digest_raw_output(&conn, "cargo test", &fake_log, Some(101)).unwrap();
        let start = digest.find("h:").unwrap();
        let handle_id = &digest[start..start + 8];

        let failures =
            recall_handle(&conn, &format!("  {handle_id}  "), None, true, None, None, None)
                .unwrap();
        assert!(failures.contains("test_login"));

        let item_block =
            recall_handle(&conn, handle_id, Some("test_login"), false, None, None, None).unwrap();
        assert!(item_block.contains("invalid token"));

        // Out-of-bounds start_line must not produce an inverted range or overflow
        let oob = recall_handle(&conn, handle_id, None, false, None, Some(usize::MAX), None)
            .unwrap();
        assert!(oob.contains("exceeds total lines"));

        // Signal-terminated commands report exit=signal instead of exit=0
        let sig_digest =
            digest_raw_output(&conn, "cargo test", &fake_log, None).unwrap();
        assert!(sig_digest.contains("exit=signal"));

        // ANSI-colored cargo error should be stripped and extracted cleanly
        let ansi_log = format!(
            "{}\n\x1b[1;31merror[E0425]\x1b[0m: cannot find value `x`\n",
            "Compiling dep v0.1.0\n".repeat(40)
        );
        let ansi_digest = digest_raw_output(&conn, "cargo check", &ansi_log, Some(101)).unwrap();
        assert!(ansi_digest.contains("error[E0425]"));
        assert!(!ansi_digest.contains("\x1b["));
        assert_eq!(never_worse("short", "longer wrapper text"), "short");

        drop(conn);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_cargo_clippy_grouping_errors_before_warnings_and_nocapture() {
        let raw_clippy = "\
Checking foo v0.1.0
warning: this `if` statement can be collapsed
 --> src/lib.rs:10:9
  |
  = note: `#[warn(clippy::collapsible_if)]` on by default

warning: this `if` statement can be collapsed
 --> src/lib.rs:20:9
  |
  = help: for further information visit https://rust-lang.github.io/rust-clippy/master/index.html#collapsible_if

error[E0308]: mismatched types
 --> src/main.rs:42:5
  |
  = note: expected `usize`, found `&str`

Finished `dev` profile in 0.50s";

        let parsed = cargo::parse_cargo_output_with_cmd("cargo clippy", raw_clippy, Some(101));
        let summary = parsed.summary_lines.join("\n");
        assert!(summary.contains("`clippy::collapsible_if` (2x): src/lib.rs:10:9, src/lib.rs:20:9"));
        assert!(summary.contains("`E0308` (1x): src/main.rs:42:5"));
        // Errors must appear before warnings in failure_blocks
        assert!(parsed.failure_blocks[0].contains("error[E0308]"));

        let raw_nocapture = "\
running 1 test
---- tests::my_debug_test stdout ----
DEBUG_VALUE = 12345

test result: ok. 1 passed; 0 failed;";
        let parsed_nc =
            cargo::parse_cargo_output_with_cmd("cargo test -- --nocapture", raw_nocapture, Some(0));
        assert!(parsed_nc.summary_lines.join("\n").contains("DEBUG_VALUE = 12345"));
    }

    #[test]
    fn test_diff_git_node_and_python_go_parsers() {
        // Cargo test with "diff" in name must not be hijacked as a diff command
        assert!(!generic::is_diff_command(
            "cargo test test_diff",
            "diff --git a/x b/x\n"
        ));
        assert!(generic::is_diff_command("git diff", ""));

        // b/b/file.rs must only strip one "b/" prefix, and indented context "-" must not count as removed
        let raw_diff = "\
diff --git a/b/file.rs b/b/file.rs
--- a/b/file.rs
+++ b/b/file.rs
@@ -1,3 +1,3 @@
   - indented markdown list context
-old_line
+new_line";
        let diff_parsed = generic::parse_diff_output(raw_diff, Some(0));
        let diff_summary = diff_parsed.summary_lines.join("\n");
        assert!(diff_summary.contains("- b/file.rs [L1..7] (+1 / -1)"));

        // git status strips boilerplate; git log skips Merge: header
        let raw_status = "\
On branch main
  (use \"git add <file>...\" to update what will be committed)
\tmodified:   src/main.rs";
        let st = generic::parse_git_status_or_log_output("git status", raw_status, Some(0));
        assert!(st.summary_lines.iter().any(|l| l.contains("src/main.rs")));
        assert!(!st.summary_lines.iter().any(|l| l.contains("use \"git add")));

        let raw_log = "\
commit abcdef123456
Merge: 1111111 2222222
Author: Dev <dev@example.com>
Date:   Mon Nov 25 10:00:00 2026

    Merge PR #42";
        let log = generic::parse_git_status_or_log_output("git log", raw_log, Some(0));
        assert!(log.summary_lines[0].contains("Merge PR #42"));
        assert!(!log.summary_lines[0].contains("Merge: 1111111"));

        // Node parser avoids false positive on "postscript" and advances past snippets
        assert!(!node::is_node_command("echo postscript"));
        assert!(node::is_node_command("npx tsc --noEmit"));
        let ts_err = "src/a.ts(1,1): error TS2322: bad\nsrc/a.ts(2,1): error TS2322: bad2\n";
        assert_eq!(node::parse_node_output(ts_err, Some(2)).failure_blocks.len(), 1);

        // Go compile errors and pytest collection errors are captured
        let go_err = "./main.go:12:5: undefined: foo\n";
        assert_eq!(
            python_go::parse_python_go_output(go_err, Some(1))
                .failure_blocks
                .len(),
            1
        );
        let py_err = "ERROR collecting tests/test_app.py\nImportError: boom\n";
        assert_eq!(
            python_go::parse_python_go_output(py_err, Some(2))
                .failure_blocks
                .len(),
            1
        );
    }
}
