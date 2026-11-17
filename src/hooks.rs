use crate::security::validate_command;
use anyhow::Result;
use ignore::WalkBuilder;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const ANTIGRAVITY_PLUGIN_JSON: &str = r#"{
  "name": "lee",
  "version": "0.1.0",
  "description": "Locate Exact Elements — Antigravity PreToolUse hook plugin for transparent command digesting and token optimization using lee"
}
"#;

pub const ANTIGRAVITY_HOOKS_JSON: &str = r#"{
  "lee-rewrite": {
    "enabled": true,
    "PreToolUse": [
      {
        "matcher": "run_command",
        "hooks": [
          {
            "type": "command",
            "command": "lee hook antigravity",
            "timeout": 10
          }
        ]
      }
    ]
  }
}
"#;

pub const MANAGED_START_MARKER: &str = "<!-- lee:start -->";
pub const MANAGED_END_MARKER: &str = "<!-- lee:end -->";

/// Replaces the `<!-- lee:start --> ... <!-- lee:end -->` block inside `existing`
/// if and only if both markers exist and the block content has changed.
/// Preserves all surrounding user content outside the markers byte-for-byte.
pub fn update_managed_block_if_changed(existing: &str, latest_block: &str) -> Option<String> {
    let start_idx = existing.find(MANAGED_START_MARKER)?;
    let rel = existing[start_idx..].find(MANAGED_END_MARKER)?;
    let end_idx = start_idx + rel + MANAGED_END_MARKER.len();

    let current_slice = &existing[start_idx..end_idx];
    let target_block = latest_block.trim();

    if current_slice == target_block {
        return None;
    }

    let mut updated = String::with_capacity(existing.len() + target_block.len());
    updated.push_str(&existing[..start_idx]);
    updated.push_str(target_block);
    updated.push_str(&existing[end_idx..]);
    Some(updated)
}

/// Automatically synchronizes Antigravity plugin `rules/AGENTS.md`, `~/.config/lee/AGENTS.md` (when global),
/// and any `<!-- lee:start --> ... <!-- lee:end -->` block in `<root>/AGENTS.md`
/// or `<root>/CLAUDE.md` whenever `AGENTS_MD_RULES` changes in a new version of `lee`.
/// Also cleans up redundant `.lee/AGENTS.md` if present.
pub fn auto_sync_agents_md(paths: &crate::config::AppPaths, latest_rules: &str) -> usize {
    let mut updated_count = 0usize;
    let normalized = latest_rules.trim_start();

    // 1. Clean up redundant local `AGENTS.md` inside `.lee/` (rules live in `.agents/plugins/lee/rules/AGENTS.md`)
    let legacy_local_agents = paths.local_dir.join("AGENTS.md");
    if legacy_local_agents.is_file() {
        let _ = fs::remove_file(&legacy_local_agents);
    }

    // 2. Sync ~/.config/lee/AGENTS.md if it exists
    let global_agents = paths.global_dir.join("AGENTS.md");
    if global_agents.is_file() {
        if let Ok(curr) = fs::read_to_string(&global_agents) {
            if curr != normalized && fs::write(&global_agents, normalized).is_ok() {
                updated_count += 1;
            }
        }
    }

    // 3. Sync managed <!-- lee:start -->...<!-- lee:end --> blocks in root AGENTS.md & CLAUDE.md
    for fname in ["AGENTS.md", "CLAUDE.md"] {
        let target = paths.project_root.join(fname);
        if target.is_file() {
            if let Ok(curr) = fs::read_to_string(&target) {
                if let Some(patched) = update_managed_block_if_changed(&curr, normalized) {
                    if fs::write(&target, patched).is_ok() {
                        updated_count += 1;
                    }
                }
            }
        }
    }

    // 4. Sync Antigravity plugin rules/AGENTS.md if plugin is installed
    let mut plugin_dirs = vec![paths.project_root.join(".agents/plugins/lee")];
    if let Some(home) = dirs::home_dir() {
        plugin_dirs.push(home.join(".gemini/config/plugins/lee"));
    }
    for pdir in plugin_dirs {
        let rules_file = pdir.join("rules/AGENTS.md");
        if rules_file.is_file() {
            if let Ok(curr) = fs::read_to_string(&rules_file) {
                if curr != normalized && fs::write(&rules_file, normalized).is_ok() {
                    updated_count += 1;
                }
            }
        }
    }

    updated_count
}

/// Determines whether a raw shell command should be transparently rewritten to `lee exec '<cmd>'`.
pub fn rewrite_shell_command_for_hook(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("lee ")
        || trimmed.contains("lee exec")
        || trimmed.contains('\n')
        || trimmed.contains("<<")
        || trimmed.contains('|')
    {
        return None;
    }

    if validate_command(trimmed).is_err() {
        return None;
    }

    let prefixes = [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo build",
        "cargo nextest",
        "git status",
        "git log",
        "git diff",
        "kaye diff",
        "kaye status",
        "npm test",
        "pnpm test",
        "npx vitest",
        "npx jest",
        "npx tsc",
        "pytest",
        "go test",
    ];

    let matches_target = prefixes.iter().any(|p| {
        trimmed == *p
            || trimmed.starts_with(&format!("{p} "))
            || trimmed.contains(&format!("&& {p}"))
    });

    if !matches_target {
        return None;
    }

    let escaped = trimmed.replace('\'', "'\\''");
    Some(format!("lee exec '{escaped}'"))
}

/// Processes a Google Antigravity `PreToolUse` JSON payload from stdin.
/// Fail-open: returns `{"decision": "allow"}` on any non-matching or invalid input.
pub fn process_antigravity_hook(input: &str) -> Value {
    let trimmed = input.trim().trim_start_matches('\u{feff}');
    if trimmed.is_empty() {
        return json!({ "decision": "allow" });
    }

    let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
        return json!({ "decision": "allow" });
    };

    let tool_name = parsed
        .pointer("/toolCall/name")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if tool_name != "run_command" {
        return json!({ "decision": "allow" });
    }

    let cmd = parsed
        .pointer("/toolCall/args/CommandLine")
        .or_else(|| parsed.pointer("/toolCall/args/command"))
        .or_else(|| parsed.pointer("/toolCall/args/cmd"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if let Some(rewritten) = rewrite_shell_command_for_hook(cmd) {
        json!({
            "decision": "allow",
            "reason": "Rewritten by lee for token optimization.",
            "overwrite": {
                "CommandLine": rewritten
            }
        })
    } else {
        json!({ "decision": "allow" })
    }
}

/// Processes a Claude Code `PreToolUse` JSON payload from stdin.
/// Returns `Some(Value)` when rewriting a `Bash` command, or `None` for passthrough.
pub fn process_claude_hook(input: &str) -> Option<Value> {
    let trimmed = input.trim().trim_start_matches('\u{feff}');
    let parsed: Value = serde_json::from_str(trimmed).ok()?;

    let tool_name = parsed.get("tool_name").and_then(|v| v.as_str())?;
    if tool_name != "Bash" {
        return None;
    }

    let cmd = parsed.pointer("/tool_input/command").and_then(|v| v.as_str())?;
    let rewritten = rewrite_shell_command_for_hook(cmd)?;

    let mut updated_input = parsed
        .get("tool_input")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = updated_input.as_object_mut() {
        obj.insert("command".to_string(), Value::String(rewritten));
    }

    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecisionReason": "lee auto-rewrite",
            "updatedInput": updated_input
        }
    }))
}

/// Installs the Antigravity `PreToolUse` plugin (`plugin.json`, `hooks.json`, and `rules/AGENTS.md`).
pub fn install_antigravity_hook_plugin(
    project_root: &Path,
    global: bool,
    rules_content: &str,
) -> Result<PathBuf> {
    let plugin_dir = if global {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".gemini/config/plugins/lee")
    } else {
        project_root.join(".agents/plugins/lee")
    };

    let rules_dir = plugin_dir.join("rules");
    fs::create_dir_all(&rules_dir)?;
    fs::write(plugin_dir.join("plugin.json"), ANTIGRAVITY_PLUGIN_JSON)?;
    fs::write(plugin_dir.join("hooks.json"), ANTIGRAVITY_HOOKS_JSON)?;
    fs::write(rules_dir.join("AGENTS.md"), rules_content.trim_start())?;

    Ok(plugin_dir)
}

/// Scans AI session transcripts (`.jsonl`) to discover missed token-saving opportunities
/// where the agent used raw `read_file`/`Read` or raw `rg`/`grep`/`cargo`/`git` instead of `lee`.
pub fn discover_missed_savings(custom_path: Option<&Path>) -> Result<String> {
    let mut transcript_files: Vec<PathBuf> = Vec::new();

    if let Some(p) = custom_path {
        if p.is_file() {
            transcript_files.push(p.to_path_buf());
        } else if p.is_dir() {
            let walker = WalkBuilder::new(p).hidden(false).git_ignore(false).max_depth(Some(5)).build();
            for entry in walker.flatten() {
                if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                    && entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        == Some("jsonl")
                {
                    transcript_files.push(entry.into_path());
                }
            }
        }
    } else if let Some(home) = dirs::home_dir() {
        let agy_brain = home.join(".gemini/antigravity-cli/brain");
        if agy_brain.is_dir() {
            let walker = WalkBuilder::new(&agy_brain).hidden(false).git_ignore(false).max_depth(Some(4)).build();
            for entry in walker.flatten() {
                if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                    && entry.file_name().to_str() == Some("transcript.jsonl")
                {
                    transcript_files.push(entry.into_path());
                }
            }
        }
        let claude_projects = home.join(".claude/projects");
        if claude_projects.is_dir() {
            let walker = WalkBuilder::new(&claude_projects).hidden(false).git_ignore(false).max_depth(Some(3)).build();
            for entry in walker.flatten() {
                if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false)
                    && entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        == Some("jsonl")
                {
                    transcript_files.push(entry.into_path());
                }
            }
        }
    }

    transcript_files.sort();
    // Limit to 30 most recent transcripts for fast scanning
    if transcript_files.len() > 30 {
        let skip = transcript_files.len() - 30;
        transcript_files = transcript_files.into_iter().skip(skip).collect();
    }

    let mut missed_categories: BTreeMap<String, (usize, usize, Vec<String>)> = BTreeMap::new();
    let mut lee_calls = 0usize;
    let mut scanned_files = 0usize;

    for tf in &transcript_files {
        let Ok(content) = fs::read_to_string(tf) else {
            continue;
        };
        scanned_files += 1;

        for line in content.lines() {
            if line.contains("\"lee\"") || line.contains("mcp_lee") {
                lee_calls += 1;
            }

            let Ok(val) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(tool_calls) = val.get("tool_calls").and_then(|v| v.as_array()) else {
                continue;
            };

            for tc in tool_calls {
                let name = tc
                    .get("name")
                    .or_else(|| tc.pointer("/function/name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = tc
                    .get("args")
                    .or_else(|| tc.get("arguments"))
                    .or_else(|| tc.get("input"))
                    .cloned()
                    .unwrap_or(Value::Null);

                if matches!(name, "read_file" | "Read") {
                    let path_str = args
                        .get("Path")
                        .or_else(|| args.get("file_path"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !path_str.is_empty()
                        && (path_str.ends_with(".rs")
                            || path_str.ends_with(".ts")
                            || path_str.ends_with(".py")
                            || path_str.ends_with(".go")
                            || path_str.ends_with(".md"))
                    {
                        let ent = missed_categories
                            .entry("Raw `read_file` -> use `outline` + `read_symbol` / `search_docs`".to_string())
                            .or_insert_with(|| (0, 0, Vec::new()));
                        ent.0 += 1;
                        ent.1 += 900; // ~900 tokens est. saved per file read
                        let short = path_str.rsplit('/').next().unwrap_or(path_str).to_string();
                        if ent.2.len() < 5 && !ent.2.contains(&short) {
                            ent.2.push(short);
                        }
                    }
                } else if matches!(name, "run_command" | "Bash") {
                    let cmd_str = args
                        .get("CommandLine")
                        .or_else(|| args.get("command"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim();

                    if cmd_str.starts_with("rg ") || cmd_str.starts_with("grep ") {
                        let ent = missed_categories
                            .entry("Raw `rg`/`grep` -> use `smart_grep`".to_string())
                            .or_insert_with(|| (0, 0, Vec::new()));
                        ent.0 += 1;
                        ent.1 += 650;
                        let sample: String = cmd_str.chars().take(48).collect();
                        if ent.2.len() < 4 && !ent.2.contains(&sample) {
                            ent.2.push(sample);
                        }
                    } else if rewrite_shell_command_for_hook(cmd_str).is_some() {
                        let ent = missed_categories
                            .entry("Raw build/test/git -> use `run_digest` or `lee hook`".to_string())
                            .or_insert_with(|| (0, 0, Vec::new()));
                        ent.0 += 1;
                        ent.1 += 1_200;
                        let sample: String = cmd_str.chars().take(48).collect();
                        if ent.2.len() < 4 && !ent.2.contains(&sample) {
                            ent.2.push(sample);
                        }
                    }
                }
            }
        }
    }

    let mut out = format!(
        "# lee Discover — Session Adoption & Missed Savings Report\n\
         Scanned Transcripts : {scanned_files} session file(s)\n\
         Active `lee`        : {lee_calls} step(s) used `lee`\n\n"
    );

    if missed_categories.is_empty() {
        out.push_str("100% compliant! No bypassed raw `read_file`, `rg`, or noisy build commands detected.\n");
    } else {
        let mut total_missed_calls = 0usize;
        let mut total_est_tokens = 0usize;
        out.push_str("| Bypassed Pattern | Missed Calls | Est. Wasted Tokens | Examples |\n");
        out.push_str("|------------------|-------------:|-------------------:|----------|\n");
        for (cat, (count, est_tok, samples)) in &missed_categories {
            total_missed_calls += count;
            total_est_tokens += est_tok;
            out.push_str(&format!(
                "| {cat} | {count} | ~{est_tok} | `{}` |\n",
                samples.join("`, `")
            ));
        }
        out.push_str(&format!(
            "\n**Total Missed Opportunities:** {total_missed_calls} call(s) (~{total_est_tokens} tokens could have been saved).\n\
             Tip: Run `lee init` to automatically install the PreToolUse hook that intercepts raw `run_command` build/test calls!\n"
        ));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::cargo::parse_cargo_output_with_cmd;
    use crate::digest::generic::parse_git_status_or_log_output;

    #[test]
    fn test_antigravity_and_claude_pre_tool_use_hooks() {
        let agy_payload = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"cargo clippy --all-targets"}}}"#;
        let agy_resp = process_antigravity_hook(agy_payload);
        assert_eq!(agy_resp["decision"], "allow");
        assert_eq!(
            agy_resp["overwrite"]["CommandLine"],
            "lee exec 'cargo clippy --all-targets'"
        );

        let claude_payload =
            r#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#;
        let claude_resp = process_claude_hook(claude_payload).unwrap();
        assert_eq!(
            claude_resp["hookSpecificOutput"]["updatedInput"]["command"],
            "lee exec 'git status'"
        );

        let passthrough = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"echo hello"}}}"#;
        let pass_resp = process_antigravity_hook(passthrough);
        assert_eq!(pass_resp["decision"], "allow");
        assert!(pass_resp.get("overwrite").is_none());
    }

    #[test]
    fn test_managed_agents_md_auto_update() {
        let existing = "# Custom Project Header\n\n<!-- lee:start -->\nOLD RULES v1\n<!-- lee:end -->\n\n# Footer\n";
        let latest = "<!-- lee:start -->\nNEW RULES v2\n<!-- lee:end -->";
        let patched = update_managed_block_if_changed(existing, latest).unwrap();
        assert_eq!(
            patched,
            "# Custom Project Header\n\n<!-- lee:start -->\nNEW RULES v2\n<!-- lee:end -->\n\n# Footer\n"
        );
        // Second call with identical content returns None (no redundant write)
        assert!(update_managed_block_if_changed(&patched, latest).is_none());
    }

    #[test]
    fn test_clippy_rule_grouping_and_nocapture_flag() {
        let raw_clippy = "\
Checking foo v0.1.0
warning: unused variable: `x`
 --> src/lib.rs:10:9
  |
  = note: `#[warn(unused_variables)]` on by default

warning: unused variable: `y`
 --> src/lib.rs:20:9
  |
  = note: `#[warn(unused_variables)]` on by default

warning: this expression borrows a reference
 --> src/main.rs:42:5
  |
  = note: `#[warn(clippy::needless_borrow)]` on by default

Finished `dev` profile in 0.50s";

        let parsed = parse_cargo_output_with_cmd("cargo clippy", raw_clippy, Some(0));
        let summary = parsed.summary_lines.join("\n");
        assert!(summary.contains("Grouped Lint/Compiler Rules (2 distinct):"));
        assert!(summary.contains("`unused_variables` (2x): src/lib.rs:10:9, src/lib.rs:20:9"));
        assert!(summary.contains("`clippy::needless_borrow` (1x): src/main.rs:42:5"));

        let raw_nocapture = "\
running 1 test
---- tests::my_debug_test stdout ----
DEBUG_VALUE = 12345

test result: ok. 1 passed; 0 failed;";
        let parsed_nc =
            parse_cargo_output_with_cmd("cargo test -- --nocapture", raw_nocapture, Some(0));
        assert!(parsed_nc.summary_lines.join("\n").contains("DEBUG_VALUE = 12345"));
    }

    #[test]
    fn test_git_status_and_log_compressors() {
        let raw_status = "\
On branch main
Changes not staged for commit:
  (use \"git add <file>...\" to update what will be committed)
  (use \"git restore <file>...\" to discard changes in working directory)
\tmodified:   src/main.rs
no changes added to commit (use \"git add\" and/or \"git commit -a\")";
        let st = parse_git_status_or_log_output("git status", raw_status, Some(0));
        let joined = st.summary_lines.join("\n");
        assert!(joined.contains("modified:   src/main.rs"));
        assert!(!joined.contains("use \"git add"));
    }
}

