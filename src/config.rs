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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProjectConfig {
    /// Canonical paths to external documentation/reference folders registered via `add-source`
    pub sources: Vec<PathBuf>,
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
    pub models_dir: PathBuf,
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
        let models_dir = global_dir.join("models");
        let vault_dir = global_dir.join("vault");

        Ok(Self {
            project_root,
            local_dir,
            local_db,
            config_file,
            notes_dir,
            global_dir,
            global_db,
            models_dir,
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
        fs::create_dir_all(&self.models_dir)
            .with_context(|| format!("Failed to create {}", self.models_dir.display()))?;
        fs::create_dir_all(&self.vault_dir)
            .with_context(|| format!("Failed to create {}", self.vault_dir.display()))?;
        Ok(())
    }

    pub fn load_config(&self) -> Result<ProjectConfig> {
        if !self.config_file.exists() {
            return Ok(ProjectConfig::default());
        }
        let raw = fs::read_to_string(&self.config_file)
            .with_context(|| format!("Failed to read {}", self.config_file.display()))?;
        let cfg: ProjectConfig = serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse {}", self.config_file.display()))?;
        Ok(cfg)
    }

    pub fn save_config(&self, cfg: &ProjectConfig) -> Result<()> {
        fs::create_dir_all(&self.local_dir)?;
        let json = serde_json::to_string_pretty(cfg)?;
        fs::write(&self.config_file, json)
            .with_context(|| format!("Failed to write {}", self.config_file.display()))?;
        Ok(())
    }

    pub fn add_source(&self, source_path: &Path) -> Result<PathBuf> {
        let canonical = fs::canonicalize(source_path)
            .with_context(|| format!("Source path does not exist: {}", source_path.display()))?;
        let mut cfg = self.load_config()?;
        if !cfg.sources.contains(&canonical) {
            cfg.sources.push(canonical.clone());
            self.save_config(&cfg)?;
        }
        Ok(canonical)
    }

    pub fn remove_source(&self, source_path: &Path) -> Result<Option<PathBuf>> {
        let canonical = fs::canonicalize(source_path).unwrap_or_else(|_| source_path.to_path_buf());
        let raw_str = source_path.to_string_lossy();
        let mut cfg = self.load_config()?;
        let before = cfg.sources.len();
        let mut removed_path = None;
        cfg.sources.retain(|p| {
            let match_it = p == &canonical
                || p == source_path
                || p.to_string_lossy().trim_end_matches('/') == raw_str.trim_end_matches('/');
            if match_it && removed_path.is_none() {
                removed_path = Some(p.clone());
            }
            !match_it
        });
        if cfg.sources.len() != before {
            self.save_config(&cfg)?;
        }
        Ok(removed_path)
    }

    pub fn clear_sources(&self) -> Result<Vec<PathBuf>> {
        let mut cfg = self.load_config()?;
        let removed = std::mem::take(&mut cfg.sources);
        self.save_config(&cfg)?;
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
}
