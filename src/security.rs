use anyhow::{bail, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

const DENY_PATTERNS: &[&str] = &[
    ".env",
    ".env*",
    "**/.env",
    "**/.env*",
    "*.pem",
    "**/*.pem",
    "*.key",
    "**/*.key",
    "id_rsa*",
    "**/id_rsa*",
    "id_ed25519*",
    "**/id_ed25519*",
    "*secret*",
    "**/*secret*",
    "*credentials*",
    "**/*credentials*",
    "*.p12",
    "**/*.p12",
    "*.pfx",
    "**/*.pfx",
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

/// Checks if a path or any of its file/directory names matches the hardcoded sensitive file deny-list.
pub fn is_denied_path(path: &Path) -> bool {
    let set = deny_globset();
    if set.is_match(path) {
        return true;
    }
    if let Some(name) = path.file_name() {
        if set.is_match(Path::new(name)) {
            return true;
        }
    }
    for comp in path.components() {
        if let Component::Normal(part) = comp {
            if set.is_match(Path::new(part)) {
                return true;
            }
        }
    }
    false
}

/// Validates that `candidate` is inside `project_root` or one of `allowed_sources`,
/// resolves symlinks canonically, and blocks any path on the security deny-list.
pub fn validate_path(
    project_root: &Path,
    allowed_sources: &[PathBuf],
    candidate: &Path,
) -> Result<PathBuf> {
    if is_denied_path(candidate) {
        bail!(
            "Security violation: access to sensitive file '{}' is blocked by deny-list",
            candidate.display()
        );
    }

    let canon_root = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canon_sources: Vec<PathBuf> = allowed_sources
        .iter()
        .map(|s| fs::canonicalize(s).unwrap_or_else(|_| s.clone()))
        .collect();

    let expanded: PathBuf = if let Some(s) = candidate.to_str() {
        if let Some(rest) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(rest)
            } else {
                candidate.to_path_buf()
            }
        } else {
            candidate.to_path_buf()
        }
    } else {
        candidate.to_path_buf()
    };

    let joined = if expanded.is_absolute() {
        expanded
    } else {
        canon_root.join(expanded)
    };

    // Lexical normalization for non-existent paths, canonical resolution for existing ones
    let resolved = if joined.exists() {
        fs::canonicalize(&joined)?
    } else if let Some(parent) = joined.parent() {
        if parent.exists() {
            let canon_parent = fs::canonicalize(parent)?;
            if let Some(name) = joined.file_name() {
                // Ensure no parent dir traversal in filename
                if name == ".." || name == "." {
                    bail!("Security violation: invalid path traversal in '{}'", candidate.display());
                }
                canon_parent.join(name)
            } else {
                canon_parent
            }
        } else {
            lexical_normalize(&joined)?
        }
    } else {
        lexical_normalize(&joined)?
    };

    let inside_root = resolved.starts_with(&canon_root);
    let inside_source = canon_sources.iter().any(|src| resolved.starts_with(src));

    if !inside_root && !inside_source {
        bail!(
            "Security violation: path '{}' escapes allowed workspace and registered sources",
            candidate.display()
        );
    }

    // Check relative components inside workspace against deny-list so parent folder names don't false-trigger
    if let Ok(rel) = resolved.strip_prefix(&canon_root) {
        if is_denied_path(rel) {
            bail!(
                "Security violation: access to sensitive file '{}' is blocked by deny-list",
                rel.display()
            );
        }
    } else {
        for src in &canon_sources {
            if let Ok(rel) = resolved.strip_prefix(src) {
                if is_denied_path(rel) {
                    bail!(
                        "Security violation: access to sensitive file '{}' is blocked by deny-list",
                        rel.display()
                    );
                }
            }
        }
    }

    if let Some(fname) = resolved.file_name() {
        if is_denied_path(Path::new(fname)) {
            bail!(
                "Security violation: access to sensitive file '{}' is blocked by deny-list",
                resolved.display()
            );
        }
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

/// Converts a validated path into a clean display key (relative to project_root if inside it, otherwise canonical path).
pub fn to_index_path(project_root: &Path, path: &Path) -> String {
    let canon_root = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let canon_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(rel) = canon_path.strip_prefix(&canon_root) {
        rel.to_string_lossy().replace('\\', "/")
    } else {
        canon_path.to_string_lossy().replace('\\', "/")
    }
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
    fn test_path_confinement_and_traversal() {
        let temp = std::env::temp_dir().join(format!("lee_sec_test_{}", std::process::id()));
        let root = temp.join("workspace");
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

        // Valid relative & absolute inside root
        assert!(validate_path(&root, &sources, Path::new("main.rs")).is_ok());
        assert!(validate_path(&root, &sources, &valid_file).is_ok());

        // Valid inside registered external source
        assert!(validate_path(&root, &sources, &ext_doc).is_ok());

        // Blocked traversal outside workspace
        assert!(validate_path(&root, &sources, Path::new("../outside/hack.rs")).is_err());
        assert!(validate_path(&root, &sources, &outside_file).is_err());

        // Blocked .env inside workspace
        assert!(validate_path(&root, &sources, Path::new(".env")).is_err());
        assert!(validate_path(&root, &sources, &env_file).is_err());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_command_validation() {
        assert!(validate_command("cargo test --quiet").is_ok());
        assert!(validate_command("cat .env").is_err());
        assert!(validate_command("grep API_KEY config/.env.local").is_err());
        assert!(validate_command("cat ~/.ssh/id_rsa").is_err());
    }
}
