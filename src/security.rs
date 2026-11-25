use anyhow::{bail, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

const DENY_PATTERNS: &[&str] = &[
    ".env",
    ".env*",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
    "*secret*",
    "*credentials*",
    "*.p12",
    "*.pfx",
];

fn deny_globset() -> &'static GlobSet {
    static SET: OnceLock<GlobSet> = OnceLock::new();
    SET.get_or_init(|| {
        let mut builder = GlobSetBuilder::new();
        for pat in DENY_PATTERNS {
            let glob = GlobBuilder::new(pat)
                .case_insensitive(true)
                .literal_separator(false)
                .build()
                .expect("valid hardcoded deny glob");
            builder.add(glob);
        }
        builder.build().expect("valid hardcoded GlobSet")
    })
}

/// Checks if a path or any of its file/directory components matches the hardcoded sensitive file deny-list.
pub fn is_denied_path(path: &Path) -> bool {
    let set = deny_globset();
    if set.is_match(path) {
        return true;
    }
    for comp in path.components() {
        if let Component::Normal(part) = comp
            && set.is_match(Path::new(part))
        {
            return true;
        }
    }
    false
}

/// Checks `is_denied_path` only on the relative sub-path inside `base` so ancestor directory names
/// outside the project/doc root (e.g. `/home/user/secret-app/`) never cause false positives.
pub fn is_denied_relative_to(base: &Path, path: &Path) -> bool {
    if let Ok(rel) = path.strip_prefix(base) {
        if rel.as_os_str().is_empty() {
            return path.file_name().is_some_and(|n| is_denied_path(Path::new(n)));
        }
        return is_denied_path(rel);
    }
    is_denied_path(path)
}

/// Expands a leading `~/` against the user's home directory.
pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(s) = path.to_str()
        && let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Resolves a potentially non-existent path by canonicalizing its deepest existing ancestor first
/// so symlinks above non-existent nested paths cannot bypass confinement.
fn resolve_with_ancestor_canonicalization(joined: &Path) -> Result<PathBuf> {
    if joined.exists() {
        return Ok(fs::canonicalize(joined)?);
    }

    let normalized = lexical_normalize(joined)?;
    let mut existing = normalized.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Ok(normalized);
        };
        tail.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return Ok(normalized);
        };
        existing = parent;
    }

    let mut resolved = fs::canonicalize(existing)?;
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Validates that `candidate` is inside `project_root` or one of `allowed_sources`,
/// resolves symlinks canonically, and blocks any path on the security deny-list.
pub fn validate_path(
    project_root: &Path,
    allowed_sources: &[PathBuf],
    candidate: &Path,
) -> Result<PathBuf> {
    let canon_root = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canon_sources: Vec<PathBuf> = allowed_sources
        .iter()
        .map(|s| fs::canonicalize(s).unwrap_or_else(|_| s.clone()))
        .collect();

    let expanded = expand_tilde(candidate);
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        if is_denied_path(&expanded) {
            bail!(
                "Security violation: access to sensitive file '{}' is blocked by deny-list",
                candidate.display()
            );
        }
        canon_root.join(expanded)
    };

    let resolved = resolve_with_ancestor_canonicalization(&joined)?;

    let inside_root = resolved.starts_with(&canon_root);
    let inside_source = canon_sources.iter().any(|src| resolved.starts_with(src));

    if !inside_root && !inside_source {
        bail!(
            "Security violation: path '{}' escapes allowed workspace and registered sources",
            candidate.display()
        );
    }

    // Check relative components inside workspace/source so parent folder names outside root don't false-trigger
    if let Ok(rel) = resolved.strip_prefix(&canon_root) {
        if !rel.as_os_str().is_empty() && is_denied_path(rel) {
            bail!(
                "Security violation: access to sensitive file '{}' is blocked by deny-list",
                rel.display()
            );
        }
    } else {
        for src in &canon_sources {
            if let Ok(rel) = resolved.strip_prefix(src)
                && !rel.as_os_str().is_empty()
                && is_denied_path(rel)
            {
                bail!(
                    "Security violation: access to sensitive file '{}' is blocked by deny-list",
                    rel.display()
                );
            }
        }
    }

    if let Some(fname) = resolved.file_name()
        && is_denied_path(Path::new(fname))
    {
        bail!(
            "Security violation: access to sensitive file '{}' is blocked by deny-list",
            resolved.display()
        );
    }

    Ok(resolved)
}

fn lexical_normalize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("Security violation: path traversal outside root");
                }
            }
            Component::Normal(c) => out.push(c),
        }
    }
    Ok(out)
}

/// Validates that a shell command executed by `run_digest` does not read or expose denied secret files.
pub fn validate_command(command: &str) -> Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        bail!("Command cannot be empty");
    }

    for raw_token in trimmed.split(|c: char| {
        c.is_whitespace() || matches!(c, ';' | '|' | '&' | '>' | '<' | '"' | '\'' | '`' | '=' | '(' | ')')
    }) {
        let token = raw_token.trim();
        if token.is_empty() || token.starts_with('-') {
            continue;
        }
        let path = Path::new(token);
        if is_denied_path(path) {
            bail!(
                "Security violation: command references blocked sensitive file pattern '{}'",
                token
            );
        }
    }

    Ok(())
}

/// Converts a path into a clean display key (relative to already-canonical `project_root` if inside it,
/// otherwise canonical forward-slash path).
pub fn to_index_path(project_root: &Path, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(project_root) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    let canon_root = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canon_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(rel) = canon_path.strip_prefix(&canon_root) {
        rel.to_string_lossy().replace('\\', "/")
    } else {
        canon_path.to_string_lossy().replace('\\', "/")
    }
}

/// Normalizes an absolute path to forward slashes for consistent cross-DB keys.
pub fn normalize_abs_key(path: &Path) -> String {
    let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canon.to_string_lossy().replace('\\', "/")
}

/// Escapes SQL `LIKE` wildcard characters (`\`, `%`, `_`) for use with `ESCAPE '\'`.
pub fn escape_sql_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn test_deny_list_patterns() {
        let blocked = [
            ".env",
            ".env.local",
            ".env.production",
            "config/.env.development",
            "certs/server.pem",
            "tls/private.key",
            "id_rsa",
            "id_rsa.pub",
            ".ssh/id_ed25519",
            "my_secret_config.json",
            "aws_credentials.txt",
            "keystore.p12",
            "certificate.pfx",
        ];
        for p in blocked {
            assert!(
                is_denied_path(Path::new(p)),
                "Expected '{}' to be blocked by deny-list",
                p
            );
        }

        let allowed = [
            "src/main.rs",
            "Cargo.toml",
            "README.md",
            "docs/architecture.md",
            "src/environment.rs",
            "src/keyboard.rs",
        ];
        for p in allowed {
            assert!(
                !is_denied_path(Path::new(p)),
                "Expected '{}' to be allowed",
                p
            );
        }
    }

    #[test]
    fn test_path_confinement_traversal_symlinks_and_ancestor_name() {
        let temp = std::env::temp_dir().join(format!("lee_sec_test_{}", std::process::id()));
        // Ancestor folder contains "secret" to verify it does not false-trigger deny-list on valid files inside
        let root = temp.join("secret-workspace");
        let outside = temp.join("outside");
        let ext_source = temp.join("ext_docs");
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&ext_source).unwrap();

        let valid_file = root.join("main.rs");
        File::create(&valid_file).unwrap();
        let outside_file = outside.join("hack.rs");
        File::create(&outside_file).unwrap();
        let env_file = root.join(".env");
        File::create(&env_file).unwrap();
        let ext_doc = ext_source.join("guide.md");
        File::create(&ext_doc).unwrap();

        let sources = vec![ext_source.clone()];

        // Valid relative & absolute inside root (even though ancestor has "secret")
        assert!(validate_path(&root, &sources, Path::new("main.rs")).is_ok());
        assert!(validate_path(&root, &sources, &valid_file).is_ok());
        assert!(!is_denied_relative_to(&root, &valid_file));

        // Valid inside registered external source
        assert!(validate_path(&root, &sources, &ext_doc).is_ok());

        // Blocked traversal outside workspace
        assert!(validate_path(&root, &sources, Path::new("../outside/hack.rs")).is_err());
        assert!(validate_path(&root, &sources, &outside_file).is_err());

        // Blocked .env inside workspace
        assert!(validate_path(&root, &sources, Path::new(".env")).is_err());
        assert!(validate_path(&root, &sources, &env_file).is_err());

        // Blocked nested non-existent path through symlink escaping workspace
        #[cfg(unix)]
        {
            let link = root.join("escape_link");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(
                validate_path(&root, &sources, Path::new("escape_link/nonexistent/deep.rs"))
                    .is_err()
            );
        }

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_command_validation() {
        assert!(validate_command("cargo test --quiet").is_ok());
        assert!(validate_command("   ").is_err());
        assert!(validate_command("cat .env").is_err());
        assert!(validate_command("grep API_KEY config/.env.local").is_err());
        assert!(validate_command("cat ~/.ssh/id_rsa").is_err());
    }
}
