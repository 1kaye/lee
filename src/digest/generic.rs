use crate::digest::cargo::ParsedDigest;
use regex::Regex;
use std::sync::OnceLock;

fn err_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(error|panic|failed|failure|fatal|exception)\b")
            .expect("valid generic error regex")
    })
}

pub fn is_diff_command(cmd: &str, raw: &str) -> bool {
    let c = cmd.trim();
    c.contains("diff")
        || c.contains("what change")
        || c.contains("w c")
        || raw.contains("\ndiff --git ")
        || raw.starts_with("diff --git ")
}

pub fn is_git_status_or_log_command(cmd: &str) -> bool {
    let c = cmd.trim();
    c.starts_with("git status")
        || c.starts_with("git log")
        || c.starts_with("kaye status")
        || c.starts_with("kaye log")
        || c.starts_with("kaye history")
}

/// Compacts verbose `git status` / `git log` / `kaye status` output by stripping instructional
/// `(use "git add..." to ...)` boilerplate and collapsing multi-line commit blocks into 1-line entries.
pub fn parse_git_status_or_log_output(cmd: &str, raw: &str, exit_code: Option<i32>) -> ParsedDigest {
    let mut summary_lines = Vec::new();
    let failure_blocks = Vec::new();

    if cmd.contains("log") || cmd.contains("history") {
        let mut current_sha = String::new();
        let mut current_author = String::new();
        let mut current_date = String::new();
        let mut msg_collected = false;

        for line in raw.lines() {
            let trimmed = line.trim();
            if let Some(sha) = trimmed.strip_prefix("commit ") {
                let short_sha = sha.split_whitespace().next().unwrap_or(sha);
                current_sha = short_sha.chars().take(8).collect();
                current_author.clear();
                current_date.clear();
                msg_collected = false;
            } else if let Some(auth) = trimmed.strip_prefix("Author:") {
                current_author = auth
                    .split('<')
                    .next()
                    .unwrap_or(auth)
                    .trim()
                    .to_string();
            } else if let Some(dt) = trimmed.strip_prefix("Date:") {
                current_date = dt.trim().chars().take(16).collect();
            } else if !trimmed.is_empty() && ! current_sha.is_empty() && !msg_collected {
                summary_lines.push(format!(
                    "- {current_sha} ({current_author}, {current_date}): {trimmed}"
                ));
                msg_collected = true;
            } else if current_sha.is_empty() && !trimmed.is_empty() && summary_lines.len() < 40 {
                summary_lines.push(trimmed.to_string());
            }
        }
    } else {
        for line in raw.lines() {
            let t = line.trim();
            if t.is_empty()
                || t.starts_with("(use \"git ")
                || t.starts_with("no changes added to commit")
                || t.starts_with("nothing added to commit")
            {
                continue;
            }
            summary_lines.push(t.to_string());
        }
    }

    if summary_lines.is_empty() {
        summary_lines.push(format!(
            "VCS command finished (exit={}).",
            exit_code.unwrap_or(0)
        ));
    }

    ParsedDigest {
        summary_lines,
        failure_blocks,
    }
}

/// Parses unified diff output (`kaye diff`, `kaye what change`, `git diff`, `gh pr diff`)
/// into a per-file summary with `+added/-removed` counts and line ranges.
pub fn parse_diff_output(raw: &str, exit_code: Option<i32>) -> ParsedDigest {
    let lines: Vec<&str> = raw.lines().collect();
    let mut summary_lines = Vec::new();
    let failure_blocks = Vec::new();

    summary_lines.push(format!(
        "Diff summary (exit_code={:?}, total_lines={}):",
        exit_code,
        lines.len()
    ));

    let mut current_file: Option<(String, usize, usize, usize)> = None; // (file, start_line, added, removed)

    for (idx, line) in lines.iter().enumerate() {
        let ln = idx + 1;
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some((f, st, add, rem)) = current_file.take() {
                summary_lines.push(format!(
                    "- {f} [L{st}..{}] (+{add} / -{rem}) -> recall with `item=\"{f}\"`",
                    ln.saturating_sub(1)
                ));
            }
            let file_name = rest
                .split_whitespace()
                .last()
                .unwrap_or(rest)
                .trim_start_matches("b/")
                .to_string();
            current_file = Some((file_name, ln, 0, 0));
        } else if let Some((_, _, ref mut add, ref mut rem)) = current_file {
            if line.starts_with('+') && !line.starts_with("+++") {
                *add += 1;
            } else if line.starts_with('-') && !line.starts_with("---") {
                *rem += 1;
            }
        }
    }

    if let Some((f, st, add, rem)) = current_file.take() {
        summary_lines.push(format!(
            "- {f} [L{st}..{}] (+{add} / -{rem}) -> recall with `item=\"{f}\"`",
            lines.len()
        ));
    }

    ParsedDigest {
        summary_lines,
        failure_blocks,
    }
}

/// Fallback parser: extracts first 10 lines, last 15 lines, and lines matching error/panic/fail regex.
pub fn parse_generic_output(raw: &str, exit_code: Option<i32>) -> ParsedDigest {
    let lines: Vec<&str> = raw.lines().collect();
    let total = lines.len();
    let re = err_regex();

    let mut summary_lines = Vec::new();
    let mut failure_blocks = Vec::new();

    summary_lines.push(format!(
        "Command finished (exit_code={:?}, total_lines={total}).",
        exit_code
    ));

    // First 10 lines
    let head_end = 10.min(total);
    if head_end > 0 {
        summary_lines.push("--- Head (first 10 lines) ---".to_string());
        for (i, l) in lines[..head_end].iter().enumerate() {
            summary_lines.push(format!("L{}: {}", i + 1, l));
        }
    }

    // Error / panic / fail matches
    for (idx, line) in lines.iter().enumerate() {
        if re.is_match(line) && failure_blocks.len() < 25 {
            failure_blocks.push(format!("{:>4} | {}", idx + 1, line));
        }
    }

    // Last 15 lines
    if total > 10 {
        let tail_start = total.saturating_sub(15).max(head_end);
        summary_lines.push("--- Tail (last 15 lines) ---".to_string());
        for (idx, l) in lines[tail_start..].iter().enumerate() {
            summary_lines.push(format!("L{}: {}", tail_start + idx + 1, l));
        }
    }

    ParsedDigest {
        summary_lines,
        failure_blocks,
    }
}
