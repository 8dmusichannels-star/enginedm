//! `unidata.scrub`: JSON list of theme directories per platform (GTK3/GTK4/Qt/Hyprland).
//! Merges the old `PlatformType` (unidata) and `ThemeType` (theme_manager) enums; the JSON shape is unchanged.

use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlatformType {
    Gtk3,
    Gtk4,
    Qt,
    Hyprland,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeTarget {
    pub platform: PlatformType,
    pub dir_path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnidataScrub {
    pub targets: Vec<ThemeTarget>,
}

/// `<state dir>/unidata/unidata.scrub`, i.e. `/var/lib/enginedm/unidata/unidata.scrub`
/// (or under `$EDM_STATE_DIR`). Not under `$HOME`.
pub fn default_scrub_path() -> PathBuf {
    crate::state::state_dir().join("unidata/unidata.scrub")
}

impl UnidataScrub {
    pub fn add_target(&mut self, platform: PlatformType, dir_path: impl Into<PathBuf>) {
        let t = ThemeTarget { platform, dir_path: dir_path.into() };
        if !self.targets.contains(&t) {
            self.targets.push(t);
        }
    }

    /// GTK theme dirs for GTK3 and GTK4, scanned in both places:
    /// `$XDG_DATA_DIRS/themes` (default `/usr/local/share:/usr/share`) and under `$HOME`
    /// (`~/.local/share/themes`, `~/.themes`).
    pub fn add_system_theme_dirs(&mut self) {
        let dirs = env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        let mut themes_dirs: Vec<PathBuf> = dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("themes"))
            .collect();
        if let Some(home) = env::var_os("HOME").filter(|h| !h.is_empty()) {
            let home = PathBuf::from(home);
            themes_dirs.push(home.join(".local/share/themes"));
            themes_dirs.push(home.join(".themes"));
        }
        for themes in themes_dirs.into_iter().filter(|t| t.is_dir()) {
            self.add_target(PlatformType::Gtk3, &themes);
            self.add_target(PlatformType::Gtk4, &themes);
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("Invalid unidata scrub: {e}"))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        fs::write(path, text).map_err(|e| format!("Failed to write {}: {e}", path.display()))
    }
}
