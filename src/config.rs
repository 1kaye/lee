use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "lee";
pub const LOCAL_DIR_NAME: &str = ".lee";
pub const NOTES_DIR_NAME: &str = ".notes";
pub const DB_FILE_NAME: &str = "lee.db";
pub const GLOBAL_DB_FILE_NAME: &str = "global.db";
pub const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocScope {
    Workspace,
    Global,
}

impl DocScope {
    pub fn as_str(self) -> &'static str {
        match self {
            DocScope::Workspace => "workspace",
            DocScope::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocEntry {
    pub name: String,
    pub path: PathBuf,
}

/// Derives a clean default doc name from a file or directory path.
pub fn default_doc_name(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("docs")
        .to_string()
}

/// Normalizes a doc name for flexible matching (case-insensitive, `-`/`_`/spaces collapsed).
pub fn normalize_doc_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.trim().chars() {
        if ch.is_whitespace() || ch == '-' || ch == '_' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProjectConfig {
    /// Named external documentation/code directories registered via `add-docs`
    pub docs: Vec<DocEntry>,
    /// Legacy unnamed paths from older `add-source` configs (migrated into `docs` on load)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<PathBuf>,
}

impl ProjectConfig {
    fn normalize_legacy(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        for path in std::mem::take(&mut self.sources) {
            if !self.docs.iter().any(|d| d.path == path) {
                let name = default_doc_name(&path);
                self.docs.push(DocEntry { name, path });
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
    pub notes_dir: PathBuf,
    pub global_dir: PathBuf,
    pub global_db: PathBuf,
    pub global_config_file: PathBuf,
    pub vault_dir: PathBuf,
}

impl AppPaths {
    pub fn discover(start_dir: &Path) -> Result<Self> {
        let canonical_start = fs::canonicalize(start_dir)
            .unwrap_or_else(|_| start_dir.to_path_buf());

        let project_root = Self::find_project_root(&canonical_start)
            .unwrap_or_else(|| canonical_start.clone());

        Self::from_root(project_root)
    }

    pub fn from_root(project_root: PathBuf) -> Result<Self> {
        let project_root = fs::canonicalize(&project_root).unwrap_or(project_root);
        let local_dir = project_root.join(LOCAL_DIR_NAME);
        let local_db = local_dir.join(DB_FILE_NAME);
        let config_file = local_dir.join(CONFIG_FILE_NAME);
        let notes_dir = local_dir.join(NOTES_DIR_NAME);

        let global_dir = if let Ok(custom) = std::env::var("LEE_GLOBAL_DIR") {
            PathBuf::from(custom)
        } else if let Some(home) = dirs::home_dir() {
            home.join(".config").join(APP_NAME)
        } else if let Some(cfg) = dirs::config_dir() {
            cfg.join(APP_NAME)
        } else {
            PathBuf::from(".config").join(APP_NAME)
        };

        let global_db = global_dir.join(GLOBAL_DB_FILE_NAME);
        let global_config_file = global_dir.join(CONFIG_FILE_NAME);
        let vault_dir = global_dir.join("vault");

        Ok(Self {
            project_root,
            local_dir,
            local_db,
            config_file,
            notes_dir,
            global_dir,
            global_db,
            global_config_file,
            vault_dir,
        })
    }

    fn find_project_root(start: &Path) -> Option<PathBuf> {
        let mut current = Some(start);
        while let Some(dir) = current {
            if dir.join(LOCAL_DIR_NAME).is_dir()
                || dir.join(".git").exists()
                || dir.join(".kaye").is_dir()
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
        fs::create_dir_all(&self.notes_dir)
            .with_context(|| format!("Failed to create {}", self.notes_dir.display()))?;

        // Migrate legacy `<project_root>/.notes` into `.lee/.notes`
        let legacy_notes = self.project_root.join(NOTES_DIR_NAME);
        if legacy_notes.is_dir() && legacy_notes != self.notes_dir {
            if let Ok(entries) = fs::read_dir(&legacy_notes) {
                for entry in entries.flatten() {
                    let src = entry.path();
                    if let Some(name) = src.file_name() {
                        let dst = self.notes_dir.join(name);
                        if !dst.exists() {
                            let _ = fs::rename(&src, &dst);
                        }
                    }
                }
            }
            let is_empty = fs::read_dir(&legacy_notes)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = fs::remove_dir(&legacy_notes);
            }
        }

        // Automatically ensure .lee/ is in .gitignore whenever local dirs are created/used
        self.ensure_gitignore(&[".lee/"])?;
        Ok(())
    }

    /// Appends each entry to `.gitignore` (if `.gitignore` exists) only when no equivalent pattern is already present.
    pub fn ensure_gitignore(&self, entries: &[&str]) -> Result<()> {
        let gitignore_path = self.project_root.join(".gitignore");
        if !gitignore_path.exists() {
            return Ok(());
        }
        let Ok(mut gi) = fs::read_to_string(&gitignore_path) else {
            return Ok(());
        };
        let mut changed = false;
        for &entry in entries {
            let norm_entry = entry.trim().trim_start_matches('/').trim_end_matches('/');
            let already_present = gi.lines().any(|l| {
                let norm = l.trim().trim_start_matches('/').trim_end_matches('/');
                norm == norm_entry
                    || (norm_entry.starts_with(".agents/") && norm == ".agents")
            });
            if !already_present {
                if !gi.is_empty() && !gi.ends_with('\n') {
                    gi.push('\n');
                }
                gi.push_str(entry);
                gi.push('\n');
                changed = true;
            }
        }
        if changed {
            let _ = fs::write(&gitignore_path, gi);
        }
        Ok(())
    }

    pub fn ensure_global_dirs(&self) -> Result<()> {
        fs::create_dir_all(&self.global_dir)
            .with_context(|| format!("Failed to create {}", self.global_dir.display()))?;
        fs::create_dir_all(&self.vault_dir)
            .with_context(|| format!("Failed to create {}", self.vault_dir.display()))?;
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
        let json = serde_json::to_string_pretty(cfg)?;
        fs::write(path, json)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn load_config(&self) -> Result<ProjectConfig> {
        Self::load_config_at(&self.config_file)
    }

    pub fn save_config(&self, cfg: &ProjectConfig) -> Result<()> {
        Self::save_config_at(&self.config_file, cfg)
    }

    pub fn load_global_config(&self) -> Result<ProjectConfig> {
        Self::load_config_at(&self.global_config_file)
    }

    pub fn save_global_config(&self, cfg: &ProjectConfig) -> Result<()> {
        Self::save_config_at(&self.global_config_file, cfg)
    }

    pub fn load_docs(&self, scope: DocScope) -> Result<Vec<DocEntry>> {
        match scope {
            DocScope::Workspace => Ok(self.load_config()?.docs),
            DocScope::Global => Ok(self.load_global_config()?.docs),
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
        let canonical = fs::canonicalize(source_path)
            .with_context(|| format!("Docs path does not exist: {}", source_path.display()))?;
        let final_name = {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                default_doc_name(&canonical)
            } else {
                trimmed.to_string()
            }
        };

        let mut cfg = match scope {
            DocScope::Workspace => self.load_config()?,
            DocScope::Global => self.load_global_config()?,
        };

        let entry = DocEntry {
            name: final_name,
            path: canonical.clone(),
        };

        if let Some(existing) = cfg.docs.iter_mut().find(|d| d.path == canonical) {
            existing.name = entry.name.clone();
        } else {
            cfg.docs.push(entry.clone());
        }

        match scope {
            DocScope::Workspace => self.save_config(&cfg)?,
            DocScope::Global => self.save_global_config(&cfg)?,
        }
        Ok(entry)
    }

    pub fn remove_doc(&self, scope: DocScope, target: &str) -> Result<Option<DocEntry>> {
        let target_trimmed = target.trim();
        let target_norm = normalize_doc_name(target_trimmed);
        let target_path = Path::new(target_trimmed);
        let canonical = fs::canonicalize(target_path).unwrap_or_else(|_| target_path.to_path_buf());

        let mut cfg = match scope {
            DocScope::Workspace => self.load_config()?,
            DocScope::Global => self.load_global_config()?,
        };

        let before = cfg.docs.len();
        let mut removed = None;
        cfg.docs.retain(|d| {
            let match_it = removed.is_none()
                && (d.path == canonical
                    || d.path == target_path
                    || d.path.to_string_lossy().trim_end_matches('/')
                        == target_trimmed.trim_end_matches('/')
                    || d.name.eq_ignore_ascii_case(target_trimmed)
                    || (!target_norm.is_empty() && normalize_doc_name(&d.name) == target_norm));
            if match_it {
                removed = Some(d.clone());
            }
            !match_it
        });

        if cfg.docs.len() != before {
            match scope {
                DocScope::Workspace => self.save_config(&cfg)?,
                DocScope::Global => self.save_global_config(&cfg)?,
            }
        }
        Ok(removed)
    }

    pub fn clear_docs(&self, scope: DocScope) -> Result<Vec<DocEntry>> {
        let mut cfg = match scope {
            DocScope::Workspace => self.load_config()?,
            DocScope::Global => self.load_global_config()?,
        };
        let removed = std::mem::take(&mut cfg.docs);
        match scope {
            DocScope::Workspace => self.save_config(&cfg)?,
            DocScope::Global => self.save_global_config(&cfg)?,
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notes_inside_local_dir_and_legacy_migration() {
        let temp = std::env::temp_dir().join(format!("lee_cfg_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(temp.join(".notes")).unwrap();
        fs::write(temp.join(".notes/rule.md"), "# Rule\n").unwrap();

        let paths = AppPaths::from_root(temp.clone()).unwrap();
        assert_eq!(paths.notes_dir, paths.local_dir.join(".notes"));
        paths.ensure_local_dirs().unwrap();

        assert!(paths.notes_dir.join("rule.md").exists());
        assert!(!temp.join(".notes").exists());

        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_named_workspace_global_docs_and_legacy_sources_migration() {
        let temp = std::env::temp_dir().join(format!("lee_docs_cfg_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        let ws_doc = temp.join("gpui-kit_docs");
        let gl_doc = temp.join("rust_guide");
        let legacy_doc = temp.join("legacy_ref");
        fs::create_dir_all(&ws_doc).unwrap();
        fs::create_dir_all(&gl_doc).unwrap();
        fs::create_dir_all(&legacy_doc).unwrap();

        let mut paths = AppPaths::from_root(temp.join("repo")).unwrap();
        fs::create_dir_all(&paths.local_dir).unwrap();
        paths.global_dir = temp.join("global_cfg");
        paths.global_config_file = paths.global_dir.join(CONFIG_FILE_NAME);

        // Legacy config with `"sources"` migrates cleanly into `docs`
        fs::write(
            &paths.config_file,
            serde_json::json!({ "sources": [legacy_doc] }).to_string(),
        )
        .unwrap();
        let migrated = paths.load_config().unwrap();
        assert_eq!(migrated.docs.len(), 1);
        assert_eq!(migrated.docs[0].name, "legacy_ref");
        assert!(migrated.sources.is_empty());

        // Add workspace & global named docs
        let e1 = paths.add_doc(DocScope::Workspace, &ws_doc, "gpui kit").unwrap();
        assert_eq!(e1.name, "gpui kit");
        let e2 = paths.add_doc(DocScope::Global, &gl_doc, "rust guide").unwrap();
        assert_eq!(e2.name, "rust guide");
        assert_eq!(paths.all_doc_paths().len(), 3);

        // Remove by normalized name
        let removed = paths.remove_doc(DocScope::Workspace, "gpui-kit").unwrap();
        assert_eq!(removed.unwrap().name, "gpui kit");
        assert_eq!(paths.load_docs(DocScope::Workspace).unwrap().len(), 1);
        assert_eq!(paths.load_docs(DocScope::Global).unwrap().len(), 1);

        let _ = fs::remove_dir_all(&temp);
    }
}
