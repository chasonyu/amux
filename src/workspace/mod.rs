//! Persist manually-added workspaces in `~/.amux/workspaces.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::AmuxConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub path: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspacesFile {
    version: u32,
    workspaces: Vec<Workspace>,
}

#[derive(Debug, Default)]
pub struct WorkspaceStore {
    workspaces: Vec<Workspace>,
    path: PathBuf,
}

impl WorkspaceStore {
    pub fn load() -> Result<Self> {
        AmuxConfig::ensure_dirs()?;
        let path = AmuxConfig::workspaces_path();
        if !path.exists() {
            return Ok(Self {
                workspaces: Vec::new(),
                path,
            });
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let file: WorkspacesFile = serde_json::from_str(&text).context("parse workspaces.json")?;
        let mut workspaces = file.workspaces;
        workspaces.sort_by_key(|w| w.order);
        Ok(Self { workspaces, path })
    }

    pub fn list(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn get(&self, id: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    pub fn add(&mut self, dir: &Path) -> Result<Workspace> {
        let abs = fs::canonicalize(dir).with_context(|| format!("canonicalize {}", dir.display()))?;
        if !abs.is_dir() {
            bail!("not a directory: {}", abs.display());
        }
        let path_str = abs.to_string_lossy().into_owned();
        if self.workspaces.iter().any(|w| w.path == path_str) {
            bail!("workspace already added: {path_str}");
        }
        let name = abs
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.clone());
        let order = self
            .workspaces
            .iter()
            .map(|w| w.order)
            .max()
            .unwrap_or(-1)
            + 1;
        let ws = Workspace {
            id: Uuid::new_v4().to_string(),
            path: path_str,
            name,
            created_at: Utc::now(),
            order,
        };
        self.workspaces.push(ws.clone());
        self.save()?;
        Ok(ws)
    }

    pub fn remove(&mut self, id: &str) -> Result<bool> {
        let before = self.workspaces.len();
        self.workspaces.retain(|w| w.id != id);
        if self.workspaces.len() != before {
            self.save()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = WorkspacesFile {
            version: 1,
            workspaces: self.workspaces.clone(),
        };
        let text = serde_json::to_string_pretty(&file)?;
        fs::write(&self.path, text).with_context(|| format!("write {}", self.path.display()))
    }
}
