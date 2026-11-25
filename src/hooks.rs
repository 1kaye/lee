use crate::security::validate_command;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

fn antigravity_plugin_json() -> String {
    serde_json::to_string_pretty(&json!({
        "name": "lee",
        "version": env!("CARGO_PKG_VERSION"),
        "description": "Locate Exact Elements — Antigravity PreToolUse hook plugin for transparent command digesting and token optimization using lee"
    }))
    .unwrap_or_default()
        + "\n"
}

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

/// Synchronizes Antigravity plugin `rules/AGENTS.md`, `~/.config/lee/AGENTS.md`,
/// and any `<!-- lee:start --> ... <!-- lee:end -->` block in `<root>/AGENTS.md`
/// or `<root>/CLAUDE.md` whenever `AGENTS_MD_RULES` changes.
pub fn auto_sync_agents_md(paths: &crate::config::AppPaths, latest_rules: &str) -> usize {
    let mut updated_count = 0usize;
    let normalized = latest_rules.trim_start();

    let global_agents = paths.global_dir.join("AGENTS.md");
    if global_agents.is_file()
        && let Ok(curr) = fs::read_to_string(&global_agents)
        && curr != normalized
        && fs::write(&global_agents, normalized).is_ok()
    {
        updated_count += 1;
    }

    for fname in ["AGENTS.md", "CLAUDE.md"] {
        let target = paths.project_root.join(fname);
        if target.is_file()
            && let Ok(curr) = fs::read_to_string(&target)
            && let Some(patched) = update_managed_block_if_changed(&curr, normalized)
            && fs::write(&target, patched).is_ok()
        {
            updated_count += 1;
        }
    }

    let mut plugin_dirs = vec![paths.project_root.join(".agents/plugins/lee")];
    if let Some(home) = dirs::home_dir() {
        plugin_dirs.push(home.join(".gemini/config/plugins/lee"));
    }
    for pdir in plugin_dirs {
        let rules_file = pdir.join("rules/AGENTS.md");
        if rules_file.is_file()
            && let Ok(curr) = fs::read_to_string(&rules_file)
            && curr != normalized
            && fs::write(&rules_file, normalized).is_ok()
        {
            updated_count += 1;
        }
    }

    updated_count
}

fn has_machine_readable_flag(cmd: &str) -> bool {
    cmd.split_whitespace().any(|tok| {
        matches!(
            tok,
            "--json"
                | "-json"
                | "--porcelain"
                | "--short"
                | "-s"
                | "--name-only"
                | "--oneline"
        ) || tok.starts_with("--porcelain=")
            || tok.starts_with("--format=")
            || tok.starts_with("--pretty=")
            || tok.starts_with("--message-format=")
    })
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
        || trimmed.contains('>')
        || trimmed.contains('<')
        || trimmed.contains(';')
        || has_machine_readable_flag(trimmed)
    {
        return None;
    }

    // Reject single `&` (backgrounding) while permitting `&&`
    let bytes = trimmed.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'&' {
            let prev_amp = i > 0 && bytes[i - 1] == b'&';
            let next_amp = i + 1 < bytes.len() && bytes[i + 1] == b'&';
            if !prev_amp && !next_amp {
                return None;
            }
        }
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
        "kaye wc",
        "kaye what change",
        "kaye w c",
        "kaye status",
        "npm test",
        "pnpm test",
        "npx vitest",
        "npx jest",
        "npx tsc",
        "pytest",
        "go test",
    ];

    let matches_target = trimmed.split("&&").any(|seg| {
        let s = seg.trim();
        prefixes
            .iter()
            .any(|p| s == *p || s.starts_with(&format!("{p} ")))
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

    let Some(args_obj) = parsed.pointer("/toolCall/args").and_then(|v| v.as_object()) else {
        return json!({ "decision": "allow" });
    };

    let Some((key, cmd)) = ["CommandLine", "command", "cmd"]
        .into_iter()
        .find_map(|k| args_obj.get(k).and_then(|v| v.as_str()).map(|s| (k, s)))
    else {
        return json!({ "decision": "allow" });
    };

    if let Some(rewritten) = rewrite_shell_command_for_hook(cmd) {
        let mut overwrite = serde_json::Map::new();
        overwrite.insert(key.to_string(), Value::String(rewritten));
        json!({
            "decision": "allow",
            "reason": "Rewritten by lee for token optimization.",
            "overwrite": overwrite
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

/// Installs the Antigravity `PreToolUse` plugin globally at `~/.gemini/config/plugins/lee/`
/// (`plugin.json`, `hooks.json`, and `rules/AGENTS.md`) so every project works with zero repo files.
pub fn install_antigravity_hook_plugin(rules_content: &str) -> Result<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        bail!("Could not determine home directory to install Antigravity plugin");
    };
    let plugin_dir = home.join(".gemini/config/plugins/lee");

    let rules_dir = plugin_dir.join("rules");
    fs::create_dir_all(&rules_dir)?;
    fs::write(plugin_dir.join("plugin.json"), antigravity_plugin_json())?;
    fs::write(plugin_dir.join("hooks.json"), ANTIGRAVITY_HOOKS_JSON)?;
    fs::write(rules_dir.join("AGENTS.md"), rules_content.trim_start())?;

    Ok(plugin_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_antigravity_and_claude_pre_tool_use_hooks() {
        let agy_payload = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"cargo clippy --all-targets"}}}"#;
        let agy_resp = process_antigravity_hook(agy_payload);
        assert_eq!(agy_resp["decision"], "allow");
        assert_eq!(
            agy_resp["overwrite"]["CommandLine"],
            "lee exec 'cargo clippy --all-targets'"
        );

        // Overwrites the exact input key (`command` / `cmd`), not always `CommandLine`
        let agy_cmd = r#"{"toolCall":{"name":"run_command","args":{"command":"git diff"}}}"#;
        let agy_cmd_resp = process_antigravity_hook(agy_cmd);
        assert_eq!(agy_cmd_resp["overwrite"]["command"], "lee exec 'git diff'");
        assert!(agy_cmd_resp["overwrite"].get("CommandLine").is_none());

        let claude_payload = r#"{"tool_name":"Bash","tool_input":{"command":"git status"}}"#;
        let claude_resp = process_claude_hook(claude_payload).unwrap();
        assert_eq!(
            claude_resp["hookSpecificOutput"]["updatedInput"]["command"],
            "lee exec 'git status'"
        );

        let passthrough = r#"{"toolCall":{"name":"run_command","args":{"CommandLine":"echo hello"}}}"#;
        let pass_resp = process_antigravity_hook(passthrough);
        assert_eq!(pass_resp["decision"], "allow");
        assert!(pass_resp.get("overwrite").is_none());

        // Single-quote escaping, machine-readable flag skipping, redirections, and denied commands
        assert_eq!(
            rewrite_shell_command_for_hook("git diff -- 'a b'").as_deref(),
            Some("lee exec 'git diff -- '\\''a b'\\'''")
        );
        assert!(rewrite_shell_command_for_hook("kaye status --json").is_none());
        assert!(rewrite_shell_command_for_hook("git status --porcelain").is_none());
        assert!(rewrite_shell_command_for_hook("cargo test > out.txt").is_none());
        assert!(rewrite_shell_command_for_hook("cargo test &").is_none());
        assert!(rewrite_shell_command_for_hook("cargo test && cat .env").is_none());
        assert!(rewrite_shell_command_for_hook("echo hi && cargo tester").is_none());
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
}

