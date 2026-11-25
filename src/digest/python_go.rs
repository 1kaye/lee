use crate::digest::{format_exit_status, ParsedDigest};

pub fn is_python_or_go_command(cmd: &str) -> bool {
    let c = cmd.trim();
    c.contains("pytest")
        || c.starts_with("python")
        || c.starts_with("uv run")
        || c.starts_with("go test")
        || c.starts_with("go build")
}

fn is_go_compile_error(trimmed: &str) -> bool {
    let Some((file_part, rest)) = trimmed.split_once(':') else {
        return false;
    };
    file_part.ends_with(".go") && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
}

pub fn parse_python_go_output(raw: &str, exit_code: Option<i32>) -> ParsedDigest {
    let lines: Vec<&str> = raw.lines().collect();
    let mut summary_lines = Vec::new();
    let mut failure_blocks = Vec::new();

    let mut idx = 0;
    while idx < lines.len() {
        let trimmed = lines[idx].trim();
        let is_fail = trimmed.starts_with("FAILED ")
            || trimmed.starts_with("ERROR collecting ")
            || trimmed.starts_with("E   ")
            || trimmed.starts_with("--- FAIL:")
            || trimmed.starts_with("FAIL\t")
            || trimmed.starts_with("panic:")
            || is_go_compile_error(trimmed)
            || trimmed.contains("Traceback (most recent call last)");

        let is_summary = (trimmed.starts_with("===") && trimmed.ends_with("==="))
            || trimmed.starts_with("ok  \t")
            || trimmed.starts_with("PASS");

        if is_fail {
            let start = idx.saturating_sub(1);
            let end = (idx + 7).min(lines.len());
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
            "Python/Go command completed (exit={}, total_lines={}).",
            format_exit_status(exit_code),
            lines.len()
        ));
    }

    ParsedDigest {
        summary_lines,
        failure_blocks,
    }
}
