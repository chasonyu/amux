//! Amux configuration under `~/.amux/`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::escape::DEFAULT_ESCAPE_BYTE;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AmuxConfig {
    #[serde(default = "default_escape_key")]
    pub escape_key: String,
    /// Override PI_* pins; empty map uses built-in table for omp ≥17.2.
    #[serde(default)]
    pub pi_pins: HashMap<String, String>,
    #[serde(default)]
    pub omp_bin: Option<String>,
    #[serde(default)]
    pub session_dir: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

fn default_escape_key() -> String {
    "ctrl-\\".to_string()
}

impl Default for AmuxConfig {
    fn default() -> Self {
        Self {
            escape_key: default_escape_key(),
            pi_pins: HashMap::new(),
            omp_bin: None,
            session_dir: None,
            profile: None,
        }
    }
}

impl AmuxConfig {
    pub fn amux_dir() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".amux")
    }

    pub fn ensure_dirs() -> Result<PathBuf> {
        let dir = Self::amux_dir();
        fs::create_dir_all(&dir).context("create ~/.amux")?;
        fs::create_dir_all(dir.join("locks")).context("create ~/.amux/locks")?;
        Ok(dir)
    }

    pub fn load() -> Result<Self> {
        let path = Self::amux_dir().join("config.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        serde_json::from_str(&text).context("parse config.json")
    }

    pub fn escape_byte(&self) -> u8 {
        parse_escape_key(&self.escape_key).unwrap_or(DEFAULT_ESCAPE_BYTE)
    }

    /// Version-gated PI_* pins validated against omp v17.2.1 (overridable).
    pub fn effective_pi_pins(&self) -> Vec<(String, String)> {
        if !self.pi_pins.is_empty() {
            return self.pi_pins.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }
        vec![
            ("PI_FORCE_IMAGE_PROTOCOL".into(), "off".into()),
            ("PI_NO_DECCARA".into(), "1".into()),
            ("PI_NO_KITTY_PLACEHOLDERS".into(), "1".into()),
            ("PI_TUI_SYNC_OUTPUT".into(), "1".into()),
        ]
    }

    pub fn omp_command(&self) -> String {
        self.omp_bin
            .clone()
            .or_else(|| which_omp())
            .unwrap_or_else(|| "omp".to_string())
    }

    pub fn workspaces_path() -> PathBuf {
        Self::amux_dir().join("workspaces.json")
    }

    pub fn locks_dir() -> PathBuf {
        Self::amux_dir().join("locks")
    }
}

fn which_omp() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("omp");
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    let home_bun = dirs::home_dir()?.join(".bun/bin/omp");
    if home_bun.is_file() {
        return Some(home_bun.to_string_lossy().into_owned());
    }
    None
}

pub fn path_exists(p: &Path) -> bool {
    p.exists()
}

/// Parse a configured escape key into its control byte.
///
/// Accepts `ctrl-<letter>` (e.g. `ctrl-z` → 0x1a), `ctrl-<symbol>`
/// (`ctrl-\` → 0x1c, `ctrl-[` → 0x1b), or a hex literal `0xNN`.
/// Unrecognized input falls back to the spec default (Ctrl+\) so a bad
/// config never silently disables the escape hatch. (§4.2.4)
fn parse_escape_key(s: &str) -> Option<u8> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_prefix("ctrl-") {
        let c = rest.chars().next()?;
        if c.is_ascii_alphabetic() {
            // ctrl-a..ctrl-z → 1..26 (case-insensitive; |0x20 lowercases)
            return Some((c as u8 | 0x20) - b'a' + 1);
        }
        return match c {
            '@' => Some(0x00),
            '[' => Some(0x1b),
            '\\' => Some(0x1c),
            ']' => Some(0x1d),
            '^' => Some(0x1e),
            '_' => Some(0x1f),
            _ => None,
        };
    }
    if let Some(rest) = s.strip_prefix("0x") {
        return u8::from_str_radix(rest, 16).ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_escape_key;

    #[test]
    fn default_ctrl_backslash_is_fs() {
        assert_eq!(parse_escape_key("ctrl-\\"), Some(0x1c));
    }

    #[test]
    fn ctrl_letter_maps_to_control_byte() {
        assert_eq!(parse_escape_key("ctrl-z"), Some(0x1a));
        assert_eq!(parse_escape_key("ctrl-a"), Some(0x01));
        assert_eq!(parse_escape_key("CTRL-Z"), Some(0x1a)); // case-insensitive
    }

    #[test]
    fn ctrl_symbol_maps_to_c0() {
        assert_eq!(parse_escape_key("ctrl-["), Some(0x1b));
        assert_eq!(parse_escape_key("ctrl-]"), Some(0x1d));
        assert_eq!(parse_escape_key("ctrl-^"), Some(0x1e));
        assert_eq!(parse_escape_key("ctrl-_"), Some(0x1f));
        assert_eq!(parse_escape_key("ctrl-@"), Some(0x00));
    }

    #[test]
    fn hex_literal_is_accepted() {
        assert_eq!(parse_escape_key("0x1c"), Some(0x1c));
        assert_eq!(parse_escape_key("0x1A"), Some(0x1a));
    }

    #[test]
    fn garbage_is_none_so_default_kicks_in() {
        assert_eq!(parse_escape_key("ctrl-9"), None); // digit, not a symbol
        assert_eq!(parse_escape_key("f1"), None);
        assert_eq!(parse_escape_key(""), None);
    }
}
