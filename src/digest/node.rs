use crate::digest::{format_exit_status, ParsedDigest};

pub fn is_node_command(cmd: &str) -> bool {
    let c = cmd.trim();
    c.split_whitespace()
        .any(|tok| matches!(tok, "vitest" | "jest" | "tsc") || tok.ends_with("/tsc"))
        || c.starts_with("npm ")
        || c.starts_with("pnpm ")
        || c.starts_with("yarn ")
        || c.starts_with("bun ")
        || c.starts_with("npx ")
}

pub fn parse_node_output(raw: &str, exit_code: Option<i32>) -> ParsedDigest {
    let lines: Vec<&str> = raw.lines().collect();
    let mut summary_lines = Vec::new();
    let mut failure_blocks = Vec::new();

    let mut idx = 0;
    while idx < lines.len() {
        let trimmed = lines[idx].trim();
        let is_ts_err = trimmed.contains("error TS")
            || trimmed.starts_with("FAIL ")
            || trimmed.starts_with("✕ ")
            || trimmed.starts_with("× ");
        let is_summary = trimmed.starts_with("Test Files")
            || trimmed.starts_with("Tests ")
            || trimmed.starts_with("Test Suites:")
            || (trimmed.starts_with("Found ") && trimmed.contains("error"));

        if is_ts_err {
            let start = idx.saturating_sub(1);
            let end = (idx + 6).min(lines.len());
            let snippet: Vec<String> = (start..end)
                .map(|ln| format!("{:>4} | {}", ln + 1, lines[ln]))
                .collect();
            failure_blocks.push(snippet.join("\n"));
            if summary_lines.len() < 20 {
                summary_lines.push(format!("L{}: {trimmed}", idx + 1));
            }
            idx = end;
            continue;
        } else if is_summary {
            summary_lines.push(trimmed.to_string());
        }
        idx += 1;
    }

    if summary_lines.is_empty() {
        summary_lines.push(format!(
            "Node/TS command completed (exit={}, total_lines={}).",
            format_exit_status(exit_code),
            lines.len()
        ));
    }

    ParsedDigest {
        summary_lines,
        failure_blocks,
    }
}
