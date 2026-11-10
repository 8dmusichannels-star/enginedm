//! Theme discovery/application (was `theme_manager.rs`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{save_config, EDMConfig};
use crate::ipc::HyprlandIpc;
use crate::unidata::{default_scrub_path, PlatformType as ThemeType, UnidataScrub};

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub path: PathBuf,
    pub kind: ThemeType,
}

pub struct ThemeManager {
    pub themes: Vec<Theme>,
    pub current: Option<Theme>,
    pub config: EDMConfig,
    pub config_path: PathBuf,
}

/// Theme names end up in an ini file; refuse anything that could add lines.
fn safe_theme_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(|c| c.is_control())
}

impl ThemeManager {
    pub fn new(config: EDMConfig, config_path: PathBuf) -> Self {
        Self { themes: Vec::new(), current: None, config, config_path }
    }

    /// Reads the scrub file and scans its directories. Returns how many themes were found.
    pub fn load_themes(&mut self) -> Result<usize, String> {
        let scrub = UnidataScrub::load(&default_scrub_path())?;
        self.themes.clear();
        for t in &scrub.targets {
            self.scan_dir(&t.dir_path, t.platform);
        }
        Ok(self.themes.len())
    }

    /// Applies the configured theme if present, otherwise the first one found
    /// (the old code always applied the first one, ignoring the config).
    pub fn load_and_apply_themes(&mut self) -> Result<(), String> {
        self.load_themes()?;
        let pick = self
            .themes
            .iter()
            .find(|t| t.name == self.config.theme)
            .or_else(|| self.themes.first())
            .cloned();
        if let Some(t) = pick {
            self.apply_theme(&t)?;
            self.current = Some(t);
        }
        Ok(())
    }

    /// Distinct theme names, in discovery order.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in &self.themes {
            if !out.contains(&t.name) {
                out.push(t.name.clone());
            }
        }
        out
    }

    pub fn set_theme(&mut self, name: &str) -> Result<(), String> {
        let theme = self
            .themes
            .iter()
            .find(|t| t.name == name)
            .cloned()
            .ok_or_else(|| format!("Theme '{name}' not found"))?;
        self.apply_theme(&theme)?;
        self.current = Some(theme);
        self.config.theme = name.to_string();
        // Persisting needs write access to /etc; a greeter running unprivileged can't, and that
        // must not make the theme switch itself fail.
        if let Err(e) = save_config(&self.config_path, &self.config) {
            eprintln!("edm: theme applied but not saved: {e}");
        }
        Ok(())
    }

    fn scan_dir(&mut self, dir: &Path, kind: ThemeType) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        let mut found: Vec<Theme> = rd
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                safe_theme_name(&name).then(|| Theme { name, path: e.path(), kind })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        self.themes.extend(found);
    }

    fn apply_theme(&self, theme: &Theme) -> Result<(), String> {
        if !safe_theme_name(&theme.name) {
            return Err("unsafe theme name".into());
        }
        match theme.kind {
            ThemeType::Gtk3 => Self::apply_gtk(&theme.name, "gtk-3.0"),
            ThemeType::Gtk4 => Self::apply_gtk(&theme.name, "gtk-4.0"),
            ThemeType::Qt => {
                env::set_var("QT_STYLE_OVERRIDE", &theme.name);
                Ok(())
            }
            ThemeType::Hyprland => HyprlandIpc.send_command(&format!("keyword general:theme {}", theme.name)),
        }
    }

    /// Writes `settings.ini` to both `$HOME/.config/<dir>` and `/etc/<dir>`.
    /// Succeeds if at least one location could be written; fails only if all fail.
    fn apply_gtk(name: &str, dir: &str) -> Result<(), String> {
        let mut dirs = Vec::new();
        if let Some(home) = env::var_os("HOME").filter(|h| !h.is_empty()) {
            dirs.push(PathBuf::from(home).join(".config").join(dir));
        }
        dirs.push(Path::new("/etc").join(dir));

        let content = format!("[Settings]\ngtk-theme-name={name}\n");
        let mut errors = Vec::new();
        let mut written = 0;
        for d in &dirs {
            let res = fs::create_dir_all(d)
                .map_err(|e| format!("Failed to create {}: {e}", d.display()))
                .and_then(|_| {
                    let file = d.join("settings.ini");
                    fs::write(&file, &content)
                        .map_err(|e| format!("Failed to write {}: {e}", file.display()))
                });
            match res {
                Ok(()) => written += 1,
                Err(e) => errors.push(e),
            }
        }
        if written > 0 {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
