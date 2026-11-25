use crate::digest::{format_exit_status, ParsedDigest};
use std::collections::BTreeMap;

pub fn is_cargo_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    trimmed.starts_with("cargo ")
        || trimmed.contains(" cargo ")
        || trimmed.contains("nextest")
        || trimmed.starts_with("rustc ")
}

pub fn has_verbose_test_flag(cmd: &str) -> bool {
    cmd.contains("--nocapture") || cmd.contains("--show-output")
}

fn extract_lint_rule(block_lines: &[&str], header_line: &str) -> Option<String> {
    // Check `error[E0308]` or `warning[unused_variables]` on header line
    if let Some(open) = header_line.find('[')
        && let Some(close) = header_line[open + 1..].find(']')
    {
        let code = header_line[open + 1..open + 1 + close].trim();
        if !code.is_empty() {
            return Some(code.to_string());
        }
    }
    // Check `= note: #[warn(clippy::...)]` / `#[deny(...)]` or `= help: for further information visit ...#lint`
    for l in block_lines {
        for marker in ["#[warn(", "#[deny(", "#[forbid("] {
            if let Some(pos) = l.find(marker) {
                let rest = &l[pos + marker.len()..];
                if let Some(end) = rest.find(')') {
                    let rule = rest[..end].trim();
                    if !rule.is_empty() {
                        return Some(rule.to_string());
                    }
                }
            }
        }
        if l.contains("for further information visit")
            && let Some(hash_pos) = l.rfind('#')
        {
            let rule = l[hash_pos + 1..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != ':');
            if !rule.is_empty() {
                return Some(if rule.contains("::") {
                    rule.to_string()
                } else {
                    format!("clippy::{rule}")
                });
            }
        }
    }
    None
}

fn extract_location(block_lines: &[&str]) -> Option<String> {
    for l in block_lines {
        let t = l.trim();
        if let Some(loc) = t.strip_prefix("--> ") {
            return Some(loc.trim().to_string());
        }
    }
    None
}

pub fn parse_cargo_output_with_cmd(cmd: &str, raw: &str, exit_code: Option<i32>) -> ParsedDigest {
    let lines: Vec<&str> = raw.lines().collect();
    let mut summary_lines = Vec::new();
    let mut error_blocks = Vec::new();
    let mut warning_blocks = Vec::new();
    let mut compiled_crates = 0usize;
    let mut warnings = 0usize;
    let keep_stdout = has_verbose_test_flag(cmd);
    let mut lint_groups: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Checking ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Downloaded ")
        {
            if trimmed.starts_with("Compiling ") || trimmed.starts_with("Checking ") {
                compiled_crates += 1;
            }
            i += 1;
            continue;
        }

        let is_stdout_header = trimmed.starts_with("---- ") && trimmed.ends_with(" stdout ----");
        let is_error = trimmed.starts_with("error[")
            || trimmed.starts_with("error:")
            || trimmed.starts_with("FAILED ")
            || is_stdout_header
            || trimmed.contains("panicked at");
        let is_warning = (trimmed.starts_with("warning:") || trimmed.starts_with("warning["))
            && !trimmed.contains("generated");

        if is_error || is_warning {
            if is_warning {
                warnings += 1;
            }
            let block_end = (i + 24).min(lines.len());
            let mut block = Vec::new();
            let mut raw_slice = Vec::new();
            for (idx, l) in lines[i..block_end].iter().enumerate() {
                let lt = l.trim();
                if idx > 0
                    && is_stdout_header
                    && ((lt.starts_with("---- ") && lt.ends_with(" stdout ----"))
                        || lt.starts_with("failures:")
                        || lt.starts_with("test result:"))
                {
                    break;
                }
                block.push(format!("{:>4} | {l}", i + idx + 1));
                raw_slice.push(*l);
                if idx > 0 && lt.is_empty() && !is_stdout_header {
                    break;
                }
            }

            if let Some(rule) = extract_lint_rule(&raw_slice, trimmed) {
                let loc = extract_location(&raw_slice).unwrap_or_else(|| format!("L{}", i + 1));
                let entry = lint_groups.entry(rule).or_insert_with(|| (0, Vec::new()));
                entry.0 += 1;
                if entry.1.len() < 4 && !entry.1.contains(&loc) {
                    entry.1.push(loc);
                }
            }

            if is_stdout_header && keep_stdout {
                summary_lines.push(block.join("\n"));
            } else if is_error {
                error_blocks.push(block.join("\n"));
                if summary_lines.len() < 20 {
                    summary_lines.push(format!("L{}: {}", i + 1, trimmed));
                }
            } else if warning_blocks.len() < 12 {
                warning_blocks.push(block.join("\n"));
            }
            i += block.len().max(1);
            continue;
        }

        if trimmed.starts_with("test result:")
            || trimmed.starts_with("Summary [")
            || (trimmed.starts_with("warning:") && trimmed.contains("generated"))
            || trimmed.starts_with("Finished ")
        {
            summary_lines.push(trimmed.to_string());
        }

        i += 1;
    }

    // Prioritize errors ahead of warnings so digest caps never hide compiler/test errors
    let mut failure_blocks = error_blocks;
    if failure_blocks.len() < 12 {
        let remaining = 12 - failure_blocks.len();
        failure_blocks.extend(warning_blocks.into_iter().take(remaining));
    }

    if !lint_groups.is_empty() {
        let mut sorted_rules: Vec<(String, usize, Vec<String>)> = lint_groups
            .into_iter()
            .map(|(r, (c, locs))| (r, c, locs))
            .collect();
        sorted_rules.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut rule_summary = Vec::new();
        for (rule, count, locs) in sorted_rules.iter().take(12) {
            rule_summary.push(format!("  - `{rule}` ({count}x): {}", locs.join(", ")));
        }
        summary_lines.insert(
            0,
            format!(
                "Grouped Lint/Compiler Rules ({} distinct):\n{}",
                sorted_rules.len(),
                rule_summary.join("\n")
            ),
        );
    }

    if compiled_crates > 0 {
        summary_lines.insert(
            0,
            format!("Compiled/checked {compiled_crates} crate(s) (warnings={warnings})."),
        );
    }

    if summary_lines.is_empty() {
        summary_lines.push(format!(
            "Cargo command completed (exit={}, total_lines={}).",
            format_exit_status(exit_code),
            lines.len()
        ));
    }

    ParsedDigest {
        summary_lines,
        failure_blocks,
    }
}
