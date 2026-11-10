//! `/etc/enginedm/enginedm.conf`: plain `key=value` lines, `#` comments. std only.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/enginedm/enginedm.conf";
/// File every interface directory must contain.
pub const INTERFACE_ENTRY: &str = "main.html";

/// One line of the `user_dirs { ... }` block: `Label=/absolute/path` (one directory, files directly
/// in it) or `Label=/absolute/path/*` (that directory and everything below it).
///
/// `path` is stored normalised (`file:`/`//` spellings folded to a plain `/...`). `$HOME`, `${HOME}`
/// and a leading `~` are kept as written and expanded by the greeter when it scans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserDir {
    pub label: String,
    pub path: String,
}

impl UserDir {
    /// True for `.../*`: the directory and all sub-directories.
    pub fn recursive(&self) -> bool {
        self.path.ends_with("/*")
    }

    /// The directory part, without the trailing `/*`.
    pub fn base(&self) -> &str {
        self.path.strip_suffix("/*").unwrap_or(&self.path)
    }
}

/// Cuts a `# comment` (the `#` must start the text or follow whitespace, so `a#b` stays intact).
fn strip_comment(line: &str) -> &str {
    let mut prev_ws = true;
    for (i, c) in line.char_indices() {
        if c == '#' && prev_ws {
            return line[..i].trim();
        }
        prev_ws = c.is_whitespace();
    }
    line.trim()
}

/// `Label=/path`, `Label:/path`, and the URL-looking spellings `Label://home/x`, `Label:///home/x`,
/// `Label=file:///home/x`. Only the label is a name; there is no real URL scheme here.
pub fn parse_user_dir(line: &str) -> Result<UserDir, String> {
    let line = strip_comment(line);
    let label_len = line.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(line.len());
    let (label, rest) = line.split_at(label_len);
    if label.is_empty() {
        return Err("a label (like Pictures) must come first".into());
    }
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) else {
        return Err("expected `Label=/path`".into());
    };
    let mut path = rest.trim();
    path = path.strip_prefix("file:").unwrap_or(path);
    // `//home/x` and `///home/x` both mean `/home/x` (a typed `file://home/x` is not a real host).
    let path = if path.starts_with("//") { format!("/{}", path.trim_start_matches('/')) } else { path.to_string() };
    if !(path.starts_with('/') || path == "~" || path.starts_with("~/") || path.starts_with('$')) {
        return Err("the path must be absolute (or start with $HOME or ~)".into());
    }
    let (base, recursive) = match path.strip_suffix("/*") {
        Some(b) => (b, true),
        None => (path.as_str(), false),
    };
    let base = if base.len() > 1 { base.trim_end_matches('/') } else { base };
    if base.contains('*') {
        return Err("`*` is only allowed as the last part, like /dir/*".into());
    }
    if base.split('/').any(|c| c == "..") {
        return Err("`..` is not allowed".into());
    }
    if base.is_empty() || base == "/" {
        return Err("refusing to index the whole file system".into());
    }
    let path = if recursive { format!("{base}/*") } else { base.to_string() };
    Ok(UserDir { label: label.to_string(), path })
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub struct EDMConfig {
    pub theme: String,
    pub tiling: bool,
    pub default_session: String,
    /// Interface (UI directory name) to load. Falls back to `default_session`, which is what the
    /// old QML loader did, so existing configs keep working.
    pub interface: Option<String>,
    /// PAM service for the login (`/etc/pam.d/<name>`). Empty/None = `system-login`.
    pub pam_service: Option<String>,
    /// Wallpaper image: absolute path, or relative to the interface directory.
    pub wallpaper: Option<String>,
    /// Start sessions under `dbus-run-session` when their `Exec=` does not already (elogind/OpenRC
    /// systems have no `systemd --user` to provide a session bus). Default true; ignored when the
    /// system is booted by systemd, which provides the bus itself.
    pub dbus_wrap: bool,
    pub autologin: bool,
    pub two_factor_enabled: bool,
    pub two_factor_method: Option<String>,
    pub two_factor_secret: Option<String>,
    pub systemctl_usedefine: bool,
    /// Folders the picture inspector indexes (`user_dirs { ... }`). Empty = `$HOME/Pictures/*`
    /// (or `$EDM_PICTURES_DIR`).
    pub user_dirs: Vec<UserDir>,
    /// The `user_settings { ... }` block (personal choices; more keys will be added over time).
    pub user_settings: UserSettings,
}

/// `user_settings { ... }`: one `key=value` per line, `#` starts a comment.
///
/// ```text
/// user_settings {
///     filter=Pictures/a.png /home/me/walls/b.jpg "/home/me/my walls/c.webp"
///     filter_known=2
/// }
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserSettings {
    /// `filter=`: pictures the inspector lists while its search box is empty, in this order. Paths
    /// are separated by spaces (use "double quotes" around a path that contains spaces); absolute,
    /// `~`/`$HOME`-relative, or relative to the `user_dirs` folders / `$HOME`. At most
    /// `USER_FILTER_MAX` entries (every listed picture is a real DOM node in the greeter).
    pub filter: Vec<String>,
    /// `filter_known=N`: how many pictures the inspector lists while its search box is empty.
    /// With `filter=` it limits that list (the first N entries); without `filter=` it lists the first
    /// N pictures alphabetically. Unset: all `filter=` entries, or nothing when there is no `filter=`.
    /// At most `USER_FILTER_MAX`.
    pub filter_known: Option<usize>,
}

/// Most `filter=` entries (and `filter_known`) that are kept.
pub const USER_FILTER_MAX: usize = 500;

/// Splits a `filter=` value into words: whitespace separates, `"..."` keeps spaces inside one word.
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// A word as it is written back: quoted when it contains whitespace.
fn quote_word(w: &str) -> String {
    if w.chars().any(char::is_whitespace) { format!("\"{w}\"") } else { w.to_string() }
}

/// True for the line that opens a `{ ... }` block this file knows (`user_dirs {`, `user_settings {`).
fn opens_block(line: &str) -> Option<&'static str> {
    let head = strip_comment(line).strip_suffix('{')?.trim();
    ["user_dirs", "user_settings"].into_iter().find(|n| head == *n)
}

impl Default for EDMConfig {
    fn default() -> Self {
        Self {
            theme: "Adwaita-dark".into(),
            tiling: true,
            default_session: "Hyprland".into(),
            interface: None,
            pam_service: None,
            wallpaper: None,
            dbus_wrap: true,
            // Same defaults as before (autologin=true when the key is missing).
            autologin: true,
            two_factor_enabled: false,
            two_factor_method: None,
            two_factor_secret: None,
            systemctl_usedefine: false,
            user_dirs: Vec::new(),
            user_settings: UserSettings::default(),
        }
    }
}

impl EDMConfig {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        let mut user_dirs: Vec<UserDir> = Vec::new();
        let mut in_user_dirs = false;
        let mut settings = UserSettings::default();
        let mut in_user_settings = false;
        for line in text.lines() {
            let line = line.trim();
            if in_user_settings {
                let entry = strip_comment(line);
                if entry.starts_with('}') {
                    in_user_settings = false;
                } else if let Some((k, v)) = entry.split_once('=') {
                    match k.trim() {
                        "filter" => {
                            for w in split_words(v) {
                                if settings.filter.len() < USER_FILTER_MAX && !settings.filter.contains(&w) {
                                    settings.filter.push(w);
                                }
                            }
                        }
                        "filter_known" => match v.trim().parse::<usize>() {
                            Ok(n) => settings.filter_known = Some(n.min(USER_FILTER_MAX)),
                            Err(_) => eprintln!("edm: user_settings: filter_known '{}' is not a number, ignored", v.trim()),
                        },
                        other => eprintln!("edm: user_settings: unknown key '{other}' ignored"),
                    }
                } else if !entry.is_empty() {
                    eprintln!("edm: user_settings: ignored '{entry}' (expected key=value)");
                }
                continue;
            }
            if in_user_dirs {
                let entry = strip_comment(line);
                if entry.starts_with('}') {
                    in_user_dirs = false;
                } else if !entry.is_empty() {
                    // A bad picture folder must not stop the login screen: report and skip.
                    match parse_user_dir(entry) {
                        Ok(d) if !user_dirs.contains(&d) => user_dirs.push(d),
                        Ok(_) => {}
                        Err(e) => eprintln!("edm: user_dirs: ignored '{entry}': {e}"),
                    }
                }
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match opens_block(line) {
                Some("user_dirs") => {
                    in_user_dirs = true;
                    continue;
                }
                Some(_) => {
                    in_user_settings = true;
                    continue;
                }
                None => {}
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let d = Self::default();
        let text_of = |k: &str| map.get(k).filter(|v| !v.is_empty()).cloned();
        let bool_of = |k: &str, default: bool| map.get(k).map(|v| v == "true").unwrap_or(default);

        Self {
            theme: text_of("theme").unwrap_or(d.theme),
            tiling: bool_of("tiling", d.tiling),
            default_session: text_of("default_session").unwrap_or(d.default_session),
            interface: text_of("interface"),
            pam_service: text_of("pam_service"),
            wallpaper: text_of("wallpaper"),
            dbus_wrap: bool_of("dbus_wrap", d.dbus_wrap),
            autologin: bool_of("autologin", d.autologin),
            two_factor_enabled: bool_of("two_factor_enabled", d.two_factor_enabled),
            two_factor_method: text_of("two_factor_method"),
            two_factor_secret: text_of("two_factor_secret"),
            systemctl_usedefine: bool_of("systemctl_usedefine", d.systemctl_usedefine),
            user_dirs,
            user_settings: settings,
        }
        .validate()
    }

    /// The old code `panic!`ed here; a login screen should report instead of crashing.
    fn validate(self) -> Result<Self, String> {
        if self.autologin && self.systemctl_usedefine {
            return Err("autologin and systemctl_usedefine cannot both be true".into());
        }
        Ok(self)
    }

    pub fn to_text(&self) -> String {
        let mut l = vec![
            format!("theme={}", self.theme),
            format!("tiling={}", self.tiling),
            format!("default_session={}", self.default_session),
        ];
        if let Some(i) = &self.interface {
            l.push(format!("interface={i}"));
        }
        if let Some(p) = &self.pam_service {
            l.push(format!("pam_service={p}"));
        }
        if let Some(w) = &self.wallpaper {
            l.push(format!("wallpaper={w}"));
        }
        l.push(format!("dbus_wrap={}", self.dbus_wrap));
        l.push(format!("autologin={}", self.autologin));
        l.push(format!("two_factor_enabled={}", self.two_factor_enabled));
        if let Some(m) = &self.two_factor_method {
            l.push(format!("two_factor_method={m}"));
        }
        if let Some(s) = &self.two_factor_secret {
            l.push(format!("two_factor_secret={s}"));
        }
        l.push(if self.systemctl_usedefine {
            "systemctl_usedefine=true".to_string()
        } else {
            "# systemctl_usedefine=true".to_string()
        });
        if !self.user_settings.filter.is_empty() || self.user_settings.filter_known.is_some() {
            l.push("user_settings {".to_string());
            if !self.user_settings.filter.is_empty() {
                l.push(format!(
                    "    filter={}",
                    self.user_settings.filter.iter().map(|w| quote_word(w)).collect::<Vec<_>>().join(" ")
                ));
            }
            if let Some(n) = self.user_settings.filter_known {
                l.push(format!("    filter_known={n}"));
            }
            l.push("}".to_string());
        }
        if !self.user_dirs.is_empty() {
            l.push("user_dirs {".to_string());
            l.extend(self.user_dirs.iter().map(|d| format!("    {}={}", d.label, d.path)));
            l.push("}".to_string());
        }
        l.join("\n") + "\n"
    }

    /// Name of the UI directory to load.
    pub fn interface_name(&self) -> &str {
        self.interface.as_deref().unwrap_or(&self.default_session)
    }

    /// Directory (containing `main.html`) of the selected interface.
    pub fn find_interface(&self) -> Option<PathBuf> {
        self.find_interface_in(&interface_search_dirs())
    }

    pub fn find_interface_in(&self, dirs: &[PathBuf]) -> Option<PathBuf> {
        let name = self.interface_name();
        // The name comes from a config file; never let it climb out of the search dirs.
        if !is_safe_component(name) {
            return None;
        }
        dirs.iter()
            .map(|d| d.join(name))
            .find(|dir| dir.join(INTERFACE_ENTRY).is_file())
    }
}

pub fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
}

/// Interface search order. Nothing under `$HOME`: everything lives under `/`.
pub fn interface_search_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = env::var_os("EDM_INTERFACE_DIR").filter(|d| !d.is_empty()) {
        v.push(PathBuf::from(d));
    }
    v.push(PathBuf::from("/etc/enginedm/interfaces"));
    v.push(PathBuf::from("/usr/local/share/enginedm/interfaces"));
    v.push(PathBuf::from("/usr/share/enginedm/interfaces"));
    v
}

pub fn load_config(path: &Path) -> Result<EDMConfig, String> {
    if !path.exists() {
        return Ok(EDMConfig::default());
    }
    let text = fs::read_to_string(path).map_err(|e| format!("Failed to read config: {e}"))?;
    EDMConfig::parse(&text)
}

/// Writes the config. A file that holds a 2FA secret is created/forced to mode 0600
/// (it used to be written world-readable).
pub fn save_config(path: &Path, config: &EDMConfig) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    let mode = if config.two_factor_secret.is_some() { 0o600 } else { 0o644 };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|e| format!("Failed to save config: {e}"))?;
    // `mode` only applies to new files; fix existing ones before any secret is written.
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("Failed to set config permissions: {e}"))?;
    file.write_all(config.to_text().as_bytes())
        .map_err(|e| format!("Failed to save config: {e}"))
}

/// `text` with `key=value` set, everything else byte for byte as it was (comments, order, line
/// endings). The first active `key=` line outside `user_dirs { }` is replaced; without one the first
/// commented `#key=` line is replaced in place (so `#wallpaper=/x.png` becomes the real setting);
/// without either the line is appended.
pub fn set_key_in_text(text: &str, key: &str, value: &str) -> String {
    // `rest` starts at the key name; true when `key` is followed by optional spaces and `=`.
    let is_key = |rest: &str| rest.strip_prefix(key).is_some_and(|r| r.trim_start().starts_with('='));
    let mut active = None;
    let mut commented = None;
    let mut in_block = false;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let t = line.trim();
        if in_block {
            in_block = !t.starts_with('}');
            continue;
        }
        if opens_block(t).is_some() {
            in_block = true;
        } else if is_key(t) {
            active.get_or_insert(i);
        } else if let Some(c) = t.strip_prefix('#') {
            if is_key(c.trim_start()) {
                commented.get_or_insert(i);
            }
        }
    }
    let new_line = format!("{key}={value}");
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_string).collect();
    match active.or(commented) {
        Some(i) => {
            let eol = if lines[i].ends_with("\r\n") { "\r\n" } else if lines[i].ends_with('\n') { "\n" } else { "" };
            lines[i] = format!("{new_line}{eol}");
        }
        None => {
            if lines.last().is_some_and(|l| !l.ends_with('\n')) {
                lines.push("\n".into());
            }
            lines.push(format!("{new_line}\n"));
        }
    }
    lines.concat()
}

/// Sets one `key=value` in the config file (see `set_key_in_text`). The file is replaced through a
/// temp file + rename, and keeps its owner, group and mode (a config holding a 2FA secret stays
/// 0600, one shared with the greeter group stays 0640). A missing file is created as 0644.
pub fn set_config_value(path: &Path, key: &str, value: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if value.contains(['\n', '\r']) || value != value.trim() {
        return Err("value has a line break or surrounding spaces".into());
    }
    let (text, meta) = match fs::read_to_string(path) {
        Ok(t) => (t, fs::metadata(path).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (String::new(), None),
        Err(e) => return Err(format!("Failed to read config: {e}")),
    };
    let new_text = set_key_in_text(&text, key, value);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".enginedm.conf.tmp.{}", std::process::id()));
    let mode = meta.as_ref().map(|m| m.mode() & 0o7777).unwrap_or(0o644);
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        // Mode and ownership are set before the content goes in, so a secret is never readable by more.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        if let Some(m) = &meta {
            std::os::unix::fs::chown(&tmp, Some(m.uid()), Some(m.gid()))?;
        }
        f.write_all(new_text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Failed to save config: {e}"));
    }
    Ok(())
}

pub fn load_or_create_config(path: &Path) -> Result<EDMConfig, String> {
    if path.exists() {
        load_config(path)
    } else {
        let cfg = EDMConfig::default();
        save_config(path, &cfg)?;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let base = env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(env::temp_dir);
        let p = base.join(format!("edm-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parses_keys_and_defaults() {
        let c = EDMConfig::parse("# c\ntheme = Nord\ndefault_session=Hyproman\nautologin=false\n").unwrap();
        assert_eq!(c.theme, "Nord");
        assert_eq!(c.default_session, "Hyproman");
        assert!(!c.autologin);
        assert!(c.tiling);
        assert_eq!(c.interface_name(), "Hyproman");
        let d = EDMConfig::parse("").unwrap();
        assert_eq!(d, EDMConfig::default());
    }

    #[test]
    fn interface_key_wins_over_default_session() {
        let c = EDMConfig::parse("default_session=Hyprland\ninterface=Hyproman\n").unwrap();
        assert_eq!(c.interface_name(), "Hyproman");
    }

    #[test]
    fn user_settings_filter() {
        assert!(EDMConfig::default().user_settings.filter.is_empty());
        let t = "theme=Nord\n# c\nuser_settings {\n    # comment\n    filter=Pictures/a.png /home/me/b.jpg  \"/home/me/my walls/c.webp\" # tail\n    filter = Pictures/a.png d.gif\n    future_key=1\n}\ntiling=false\n";
        let c = EDMConfig::parse(t).unwrap();
        assert_eq!(c.user_settings.filter, ["Pictures/a.png", "/home/me/b.jpg", "/home/me/my walls/c.webp", "d.gif"]);
        assert_eq!(c.theme, "Nord");
        assert!(!c.tiling, "keys after the block are still read");
        // a `filter=` OUTSIDE the block is not the setting
        assert!(EDMConfig::parse("filter=/x.png\n").unwrap().user_settings.filter.is_empty());
        // round trip, including a path with spaces
        assert_eq!(EDMConfig::parse(&c.to_text()).unwrap().user_settings, c.user_settings);
        assert_eq!(split_words(r#"a "b c" d"#), ["a", "b c", "d"]);
        assert_eq!(split_words("   "), Vec::<String>::new());
        // filter_known lives in the block too; a bad value is ignored, a big one is capped
        assert_eq!(c.user_settings.filter_known, None);
        let k = |v: &str| EDMConfig::parse(&format!("user_settings {{\nfilter_known={v}\n}}\n")).unwrap().user_settings.filter_known;
        assert_eq!((k("25"), k("0"), k("abc"), k("-3"), k("99999")), (Some(25), Some(0), None, None, Some(USER_FILTER_MAX)));
        assert_eq!(EDMConfig::parse("filter_known=5\n").unwrap().user_settings.filter_known, None, "outside the block it is not the setting");
        let both = EDMConfig::parse("user_settings {\nfilter=/a.png\nfilter_known=1\n}\n").unwrap();
        assert_eq!(EDMConfig::parse(&both.to_text()).unwrap().user_settings, both.user_settings);
        let only_n = EDMConfig::parse("user_settings {\nfilter_known=7\n}\n").unwrap();
        assert_eq!(EDMConfig::parse(&only_n.to_text()).unwrap().user_settings.filter_known, Some(7));
        // capped
        let many: String = (0..USER_FILTER_MAX + 20).map(|i| format!("/p/{i}.png ")).collect();
        assert_eq!(EDMConfig::parse(&format!("user_settings {{\nfilter={many}\n}}\n")).unwrap().user_settings.filter.len(), USER_FILTER_MAX);
    }

    #[test]
    fn user_settings_block_is_left_alone_when_saving_a_key() {
        let t = "user_settings {\n    wallpaper=/inside\n}\n#wallpaper=/x.png\n";
        assert_eq!(set_key_in_text(t, "wallpaper", "/n.png"), "user_settings {\n    wallpaper=/inside\n}\nwallpaper=/n.png\n");
    }

    #[test]
    fn pam_service_and_wallpaper_keys() {
        let c = EDMConfig::parse("pam_service=enginedm\nwallpaper=/usr/share/backgrounds/a b.png\n").unwrap();
        assert_eq!(c.pam_service.as_deref(), Some("enginedm"));
        assert_eq!(c.wallpaper.as_deref(), Some("/usr/share/backgrounds/a b.png"));
        assert_eq!(EDMConfig::parse(&c.to_text()).unwrap(), c);
        assert_eq!(EDMConfig::parse("pam_service=\nwallpaper=\n").unwrap().wallpaper, None);
    }

    #[test]
    fn dbus_wrap_key() {
        assert!(EDMConfig::parse("").unwrap().dbus_wrap);
        let off = EDMConfig::parse("dbus_wrap=false\n").unwrap();
        assert!(!off.dbus_wrap);
        assert_eq!(EDMConfig::parse(&off.to_text()).unwrap(), off);
        assert!(EDMConfig::parse(&EDMConfig::default().to_text()).unwrap().dbus_wrap);
    }

    fn ud(label: &str, path: &str) -> UserDir {
        UserDir { label: label.into(), path: path.into() }
    }

    #[test]
    fn user_dirs_block_in_the_users_own_spelling() {
        let text = "theme=Nord\n\
            user_dirs {\n\
            \n\
            Pictures://home/goychay23/Pictures/*   # every folder below\n\
            Pictures://home/goychay23/Pictures/sha\n\
            Pictures://home/goychay23/Pictures/picture # only this folder\n\
            }\n\
            tiling=false\n";
        let c = EDMConfig::parse(text).unwrap();
        assert_eq!(
            c.user_dirs,
            [
                ud("Pictures", "/home/goychay23/Pictures/*"),
                ud("Pictures", "/home/goychay23/Pictures/sha"),
                ud("Pictures", "/home/goychay23/Pictures/picture"),
            ]
        );
        assert!(c.user_dirs[0].recursive() && !c.user_dirs[1].recursive());
        assert_eq!(c.user_dirs[0].base(), "/home/goychay23/Pictures");
        // keys on both sides of the block still apply
        assert_eq!(c.theme, "Nord");
        assert!(!c.tiling);
    }

    #[test]
    fn user_dir_spellings_fold_to_one_form() {
        for s in [
            "Pictures=/home/a/P/*",
            "Pictures: /home/a/P/*",
            "Pictures://home/a/P/*",
            "Pictures:///home/a/P/*",
            "Pictures=file:///home/a/P/*",
            "Pictures = //home/a/P/*",
            "Pictures=/home/a/P//*",
        ] {
            assert_eq!(parse_user_dir(s).unwrap(), ud("Pictures", "/home/a/P/*"), "{s}");
        }
        assert_eq!(parse_user_dir("Photos=$HOME/Pictures/*").unwrap(), ud("Photos", "$HOME/Pictures/*"));
        assert_eq!(parse_user_dir("Photos=~/Pictures").unwrap(), ud("Photos", "~/Pictures"));
        assert_eq!(parse_user_dir("Photos=/x/y/").unwrap(), ud("Photos", "/x/y"));
    }

    #[test]
    fn bad_user_dir_lines_are_skipped_not_fatal() {
        for bad in [
            "",
            "=/x",
            "Pictures",
            "Pictures=relative/path",
            "Pictures=/a/*/b",
            "Pictures=/a/../b",
            "Pictures=/*",
            "Pictures=/",
            "Pictures=*",
        ] {
            assert!(parse_user_dir(bad).is_err(), "{bad:?}");
        }
        let c = EDMConfig::parse("user_dirs {\nPictures=nope\nPictures=/ok/*\nPictures=/ok/*\n}\n").unwrap();
        assert_eq!(c.user_dirs, [ud("Pictures", "/ok/*")], "bad line skipped, duplicate dropped");
    }

    #[test]
    fn user_dirs_round_trip_and_default_is_empty() {
        assert!(EDMConfig::default().user_dirs.is_empty());
        assert!(!EDMConfig::default().to_text().contains("user_dirs"));
        let c = EDMConfig {
            user_dirs: vec![ud("Pictures", "$HOME/Pictures/*"), ud("Walls", "/usr/share/backgrounds")],
            ..Default::default()
        };
        assert_eq!(EDMConfig::parse(&c.to_text()).unwrap(), c);
    }

    const SAMPLE: &str = "# Example\ntheme=Nord\n\n# Background image: absolute path\n# png/jpg, max 32 MiB.\n#wallpaper=/usr/share/backgrounds/enginedm.png\n\nuser_dirs {\n    wallpaper=/in/the/block\n}\ntiling=false\n";

    #[test]
    fn set_key_replaces_the_commented_example_in_place() {
        let out = set_key_in_text(SAMPLE, "wallpaper", "/home/a/Pictures/me.jpg");
        let expect = SAMPLE.replace("#wallpaper=/usr/share/backgrounds/enginedm.png", "wallpaper=/home/a/Pictures/me.jpg");
        assert_eq!(out, expect, "only that line changed; the user_dirs `wallpaper=` is untouched");
        let c = EDMConfig::parse(&out).unwrap();
        assert_eq!(c.wallpaper.as_deref(), Some("/home/a/Pictures/me.jpg"));
        assert_eq!(c.theme, "Nord");
        assert!(!c.tiling);
        // picking again replaces the active line, no duplicates
        let again = set_key_in_text(&out, "wallpaper", "/b.png");
        assert_eq!(again.matches("wallpaper=").count(), 2, "{again}"); // the setting + the one in the block
        assert_eq!(EDMConfig::parse(&again).unwrap().wallpaper.as_deref(), Some("/b.png"));
    }

    #[test]
    fn set_key_active_line_beats_comment_and_append_is_last_resort() {
        let t = "#wallpaper=/old\nwallpaper = /active\n";
        assert_eq!(set_key_in_text(t, "wallpaper", "/n.png"), "#wallpaper=/old\nwallpaper=/n.png\n");
        assert_eq!(set_key_in_text("theme=Nord", "wallpaper", "/n.png"), "theme=Nord\nwallpaper=/n.png\n");
        assert_eq!(set_key_in_text("", "wallpaper", "/n.png"), "wallpaper=/n.png\n");
        // CRLF line endings survive, a prose comment that merely mentions the key is not touched
        assert_eq!(set_key_in_text("# set wallpaper= below\r\nwallpaper=/a\r\n", "wallpaper", "/b"), "# set wallpaper= below\r\nwallpaper=/b\r\n");
        // a longer key name is not the key
        assert_eq!(set_key_in_text("wallpaper_dir=/x\n", "wallpaper", "/b"), "wallpaper_dir=/x\nwallpaper=/b\n");
    }

    #[test]
    fn set_config_value_keeps_mode_and_leaves_no_temp_file() {
        let dir = tmp("setval");
        let p = dir.join("enginedm.conf");
        fs::write(&p, SAMPLE).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        set_config_value(&p, "wallpaper", "/home/a/Pictures/x y.png").unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        assert_eq!(load_config(&p).unwrap().wallpaper.as_deref(), Some("/home/a/Pictures/x y.png"));
        assert!(fs::read_to_string(&p).unwrap().contains("# png/jpg, max 32 MiB.\n"), "comments kept");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["enginedm.conf"]);
        // a missing file is created
        let n = dir.join("new/enginedm.conf");
        set_config_value(&n, "wallpaper", "/a.png").unwrap();
        assert_eq!(fs::read_to_string(&n).unwrap(), "wallpaper=/a.png\n");
        assert_eq!(fs::metadata(&n).unwrap().permissions().mode() & 0o777, 0o644);
        // values that would corrupt the file are refused and the file is unchanged
        let before = fs::read_to_string(&p).unwrap();
        assert!(set_config_value(&p, "wallpaper", "/a\nautologin=true").is_err());
        assert!(set_config_value(&p, "wallpaper", " /a.png").is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), before);
    }

    #[test]
    fn invalid_combo_is_an_error_not_a_panic() {
        assert!(EDMConfig::parse("autologin=true\nsystemctl_usedefine=true\n").is_err());
    }

    #[test]
    fn text_round_trip() {
        let c = EDMConfig {
            autologin: false,
            interface: Some("Hyproman".into()),
            two_factor_enabled: true,
            two_factor_method: Some("totp".into()),
            two_factor_secret: Some("JBSWY3DPEHPK3PXP".into()),
            ..Default::default()
        };
        assert_eq!(EDMConfig::parse(&c.to_text()).unwrap(), c);
    }

    #[test]
    fn secret_file_is_0600_even_if_it_existed_as_0644() {
        let dir = tmp("perm");
        let p = dir.join("sub/enginedm.conf");
        let mut c = EDMConfig { autologin: false, ..Default::default() };
        save_config(&p, &c).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        c.two_factor_secret = Some("SECRET".into());
        save_config(&p, &c).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(load_config(&p).unwrap().two_factor_secret.as_deref(), Some("SECRET"));
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tmp("missing");
        assert_eq!(load_config(&dir.join("nope.conf")).unwrap(), EDMConfig::default());
    }

    #[test]
    fn interface_lookup_and_traversal_guard() {
        let root = tmp("iface");
        fs::create_dir_all(root.join("Hyproman")).unwrap();
        fs::write(root.join("Hyproman/main.html"), "<html></html>").unwrap();
        let mut c = EDMConfig { interface: Some("Hyproman".into()), ..Default::default() };
        assert_eq!(c.find_interface_in(std::slice::from_ref(&root)), Some(root.join("Hyproman")));
        c.interface = Some("Missing".into());
        assert_eq!(c.find_interface_in(std::slice::from_ref(&root)), None);
        for bad in ["..", ".", "../Hyproman", "a/b", ""] {
            c.interface = Some(bad.into());
            assert_eq!(c.find_interface_in(&[root.join("Hyproman")]), None, "{bad:?}");
        }
    }
}
