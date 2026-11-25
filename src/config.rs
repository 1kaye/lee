use crate::security::expand_tilde;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const APP_NAME: &str = "lee";
pub const LEGACY_LOCAL_DIR_NAME: &str = ".lee";
pub const DB_FILE_NAME: &str = "lee.db";
const GLOBAL_DB_FILE_NAME: &str = "global.db";
pub const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocScope {
    Workspace,
    Global,
}

impl DocScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocEntry {
    pub name: String,
    pub path: PathBuf,
}

pub fn default_doc_name(path: &Path) -> String {
    let raw = if path.is_file() {
        path.file_stem()
    } else {
        path.file_name()
    }
    .and_then(|s| s.to_str())
    .unwrap_or("docs");
    let cleaned = raw.trim().replace(['_', '-'], " ");
    let collapsed: Vec<&str> = cleaned.split_whitespace().collect();
    if collapsed.is_empty() {
        "docs".to_string()
    } else {
        collapsed.join(" ")
    }
}

pub fn normalize_doc_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConfig {
    /// Named external documentation or source directories (`lee add-docs`).
    #[serde(default)]
    pub docs: Vec<DocEntry>,
    /// Legacy unnamed paths from older `add-source` configs (migrated into `docs` on load).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PathBuf>,
}

impl ProjectConfig {
    pub fn normalize_legacy(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        let legacy = std::mem::take(&mut self.sources);
        for p in legacy {
            let canonical = fs::canonicalize(&p).unwrap_or(p);
            if !self.docs.iter().any(|d| d.path == canonical) {
                self.docs.push(DocEntry {
                    name: default_doc_name(&canonical),
                    path: canonical,
                });
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub project_root: PathBuf,
    pub local_dir: PathBuf,
    pub local_db: PathBuf,
    pub config_file: PathBuf,
    pub global_dir: PathBuf,
    pub global_db: PathBuf,
    pub global_config_file: PathBuf,
}

impl AppPaths {
    pub fn discover(start_dir: &Path) -> Self {
        let canonical_start =
            fs::canonicalize(start_dir).unwrap_or_else(|_| start_dir.to_path_buf());

        let project_root = Self::find_project_root(&canonical_start)
            .unwrap_or(canonical_start);

        Self::from_root(project_root)
    }

    pub fn from_root(project_root: PathBuf) -> Self {
        let global_dir = std::env::var("LEE_GLOBAL_DIR")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| expand_tilde(Path::new(&v)))
            .or_else(|| dirs::config_dir().map(|cfg| cfg.join(APP_NAME)))
            .or_else(|| dirs::home_dir().map(|home| home.join(".config").join(APP_NAME)))
            .unwrap_or_else(|| std::env::temp_dir().join(APP_NAME));
        Self::from_root_with_global_dir(project_root, global_dir)
    }

    pub fn from_root_with_global_dir(project_root: PathBuf, global_dir: PathBuf) -> Self {
        let project_root = fs::canonicalize(&project_root).unwrap_or(project_root);
        let proj_hash = blake3::hash(project_root.to_string_lossy().as_bytes()).to_hex();
        let folder_slug = project_root
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| {
                s.chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".to_string());

        // Centralized per-project storage under `~/.config/lee/projects/<name>-<hash>/`
        // keeps project repositories 100% clean (no `.lee/` folder or `.gitignore` edits).
        let local_dir = global_dir
            .join("projects")
            .join(format!("{folder_slug}-{}", &proj_hash[..12]));
        let local_db = local_dir.join(DB_FILE_NAME);
        let config_file = local_dir.join(CONFIG_FILE_NAME);

        let global_db = global_dir.join(GLOBAL_DB_FILE_NAME);
        let global_config_file = global_dir.join(CONFIG_FILE_NAME);

        Self {
            project_root,
            local_dir,
            local_db,
            config_file,
            global_dir,
            global_db,
            global_config_file,
        }
    }

    fn find_project_root(start: &Path) -> Option<PathBuf> {
        let canon_home = dirs::home_dir().map(|h| fs::canonicalize(&h).unwrap_or(h));
        let canon_cfg = dirs::config_dir().map(|c| fs::canonicalize(&c).unwrap_or(c));

        let mut current = Some(start);
        while let Some(dir) = current {
            // Never climb into $HOME, $HOME/.config, or filesystem root (e.g. dotfiles $HOME/.git)
            if dir.parent().is_none()
                || canon_home.as_deref() == Some(dir)
                || canon_cfg.as_deref() == Some(dir)
            {
                break;
            }
            if dir.join(".git").exists()
                || dir.join(".kaye").is_dir()
                || dir.join(LEGACY_LOCAL_DIR_NAME).is_dir()
            {
                return Some(dir.to_path_buf());
            }
            current = dir.parent();
        }
        None
    }

    pub fn ensure_local_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.local_dir)
            .with_context(|| format!("Failed to create {}", self.local_dir.display()))?;
        Ok(())
    }

    pub fn ensure_global_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.global_dir)
            .with_context(|| format!("Failed to create {}", self.global_dir.display()))?;
        Ok(())
    }

    fn load_config_at(path: &Path) -> Result<ProjectConfig> {
        if !path.exists() {
            return Ok(ProjectConfig::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let mut cfg: ProjectConfig = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        cfg.normalize_legacy();
        Ok(cfg)
    }

    fn save_config_at(path: &Path, cfg: &ProjectConfig) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut to_save = cfg.clone();
        to_save.normalize_legacy();
        let data = serde_json::to_string_pretty(&to_save)?;
        // Atomic write via sibling temp file + rename to prevent corruption on concurrent access/crash
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        fs::write(&tmp, data).with_context(|| format!("Failed to write {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("Failed to replace {}", path.display()))?;
        Ok(())
    }

    pub fn load_config(&self) -> Result<ProjectConfig> {
        if self.config_file.exists() {
            return Self::load_config_at(&self.config_file);
        }
        let legacy = self
            .project_root
            .join(LEGACY_LOCAL_DIR_NAME)
            .join(CONFIG_FILE_NAME);
        if legacy.exists() {
            return Self::load_config_at(&legacy);
        }
        Ok(ProjectConfig::default())
    }

    pub fn load_global_config(&self) -> Result<ProjectConfig> {
        Self::load_config_at(&self.global_config_file)
    }

    fn load_scope_config(&self, scope: DocScope) -> Result<ProjectConfig> {
        match scope {
            DocScope::Workspace => self.load_config(),
            DocScope::Global => self.load_global_config(),
        }
    }

    fn save_scope_config(&self, scope: DocScope, cfg: &ProjectConfig) -> Result<()> {
        match scope {
            DocScope::Workspace => {
                self.ensure_local_dirs()?;
                Self::save_config_at(&self.config_file, cfg)
            }
            DocScope::Global => {
                self.ensure_global_dirs()?;
                Self::save_config_at(&self.global_config_file, cfg)
            }
        }
    }

    pub fn workspace_doc_paths(&self) -> Vec<PathBuf> {
        self.load_config()
            .unwrap_or_default()
            .docs
            .into_iter()
            .map(|e| e.path)
            .collect()
    }

    pub fn global_doc_paths(&self) -> Vec<PathBuf> {
        self.load_global_config()
            .unwrap_or_default()
            .docs
            .into_iter()
            .map(|e| e.path)
            .collect()
    }

    /// Returns all registered doc paths (workspace + global, deduplicated).
    pub fn all_doc_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for cfg in [
            self.load_config().unwrap_or_default(),
            self.load_global_config().unwrap_or_default(),
        ] {
            for entry in cfg.docs {
                if !out.contains(&entry.path) {
                    out.push(entry.path);
                }
            }
        }
        out
    }

    pub fn add_doc(&self, scope: DocScope, source_path: &Path, name: &str) -> Result<DocEntry> {
        let expanded = expand_tilde(source_path);
        let canonical = fs::canonicalize(&expanded)
            .with_context(|| format!("Docs path does not exist: {}", source_path.display()))?;
        let final_name = {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                default_doc_name(&canonical)
            } else {
                trimmed.to_string()
            }
        };

        let mut cfg = self.load_scope_config(scope)?;
        let entry = DocEntry {
            name: final_name,
            path: canonical.clone(),
        };

        if let Some(existing) = cfg.docs.iter_mut().find(|d| d.path == canonical) {
            existing.name = entry.name.clone();
        } else {
            cfg.docs.push(entry.clone());
        }

        self.save_scope_config(scope, &cfg)?;
        Ok(entry)
    }

    pub fn remove_doc(&self, scope: DocScope, target: &str) -> Result<Option<DocEntry>> {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        let mut cfg = self.load_scope_config(scope)?;
        if cfg.docs.is_empty() {
            return Ok(None);
        }

        let target_norm = normalize_doc_name(trimmed);
        let target_expanded = expand_tilde(Path::new(trimmed));
        let target_canon = fs::canonicalize(&target_expanded).ok();
        let target_trimmed_slash = target_expanded
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();

        // Support 1-based numeric index (e.g. `lee rm-docs 1`) if not matching an exact name/path
        let numeric_idx = trimmed
            .parse::<usize>()
            .ok()
            .filter(|&n| n >= 1 && n <= cfg.docs.len())
            .map(|n| n - 1);

        let idx = cfg
            .docs
            .iter()
            .position(|d| {
                let d_canon = fs::canonicalize(&d.path).unwrap_or_else(|_| d.path.clone());
                let d_str = d.path.to_string_lossy().trim_end_matches('/').to_string();
                d.name.eq_ignore_ascii_case(trimmed)
                    || (!target_norm.is_empty() && normalize_doc_name(&d.name) == target_norm)
                    || target_canon.as_ref().is_some_and(|tc| tc == &d_canon)
                    || d_str == target_trimmed_slash
            })
            .or(numeric_idx);

        let Some(pos) = idx else {
            return Ok(None);
        };

        let removed = cfg.docs.remove(pos);
        self.save_scope_config(scope, &cfg)?;
        Ok(Some(removed))
    }

    pub fn clear_docs(&self, scope: DocScope) -> Result<Vec<DocEntry>> {
        let mut cfg = self.load_scope_config(scope)?;
        if cfg.docs.is_empty() {
            return Ok(Vec::new());
        }
        let removed = std::mem::take(&mut cfg.docs);
        self.save_scope_config(scope, &cfg)?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_named_workspace_global_docs_numeric_remove_and_dot_dir_name() {
        let temp = std::env::temp_dir().join(format!("lee_docs_cfg_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let ws_doc = temp.join("gpui-kit_docs");
        let dot_dir = temp.join("next.js");
        let gl_doc = temp.join("rust_guide");
        let legacy_doc = temp.join("legacy_ref");
        fs::create_dir_all(&ws_doc).unwrap();
        fs::create_dir_all(&dot_dir).unwrap();
        fs::create_dir_all(&gl_doc).unwrap();
        fs::create_dir_all(&legacy_doc).unwrap();

        // Directory with dots keeps full name instead of truncating extension
        assert_eq!(default_doc_name(&dot_dir), "next.js");

        let repo_dir = temp.join("repo");
        fs::create_dir_all(&repo_dir).unwrap();
        let paths = AppPaths::from_root_with_global_dir(repo_dir.clone(), temp.join("global_cfg"));
        paths.ensure_local_dirs().unwrap();
        assert!(
            !repo_dir.join(".lee").exists(),
            "centralized storage must not create .lee inside repo"
        );

        // Legacy config with `"sources"` migrates cleanly into `docs`
        fs::write(
            &paths.config_file,
            serde_json::json!({ "sources": [legacy_doc] }).to_string(),
        )
        .unwrap();

        let ws_entry = paths.add_doc(DocScope::Workspace, &ws_doc, "gpui kit").unwrap();
        assert_eq!(ws_entry.name, "gpui kit");
        let gl_entry = paths.add_doc(DocScope::Global, &gl_doc, "").unwrap();
        assert_eq!(gl_entry.name, "rust guide");

        assert_eq!(paths.load_config().unwrap().docs.len(), 2);
        assert_eq!(paths.load_global_config().unwrap().docs.len(), 1);
        assert_eq!(paths.all_doc_paths().len(), 3);

        // Remove by 1-based number works on CLI path too
        let removed_num = paths.remove_doc(DocScope::Workspace, "1").unwrap().unwrap();
        assert_eq!(removed_num.name, "legacy ref");

        // Remove by normalized name
        let removed = paths.remove_doc(DocScope::Workspace, "gpui-kit").unwrap();
        assert!(removed.is_some());
        assert!(paths.load_config().unwrap().docs.is_empty());

        // Remove global doc & verify empty clear_docs returns empty without error
        assert!(paths.remove_doc(DocScope::Global, "rust guide").unwrap().is_some());
        assert!(paths.clear_docs(DocScope::Global).unwrap().is_empty());

        let _ = fs::remove_dir_all(&temp);
    }
}
