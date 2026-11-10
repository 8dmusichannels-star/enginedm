//! Wayland session discovery (`*.desktop` files) and the environment a session is started with.
//! std only.

use std::fs;
use std::path::{Path, PathBuf};

pub const SESSION_DIRS: [&str; 2] = ["/usr/share/wayland-sessions", "/usr/share/wayland-session"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    /// Program followed by its arguments. The old code kept only the first word of `Exec=`, which
    /// broke entries like `Exec=dbus-run-session Hyprland`.
    pub argv: Vec<String>,
}

/// Split an `Exec=` value (double quotes + backslash escapes inside quotes) and drop the
/// `%f %u ...` field codes that make no sense for a session.
pub fn parse_exec(exec: &str) -> Vec<String> {
    let mut raw: Vec<String> = Vec::new();
    let (mut cur, mut in_q, mut started) = (String::new(), false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_q = !in_q;
                started = true;
            }
            '\\' if in_q => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !in_q => {
                if started || !cur.is_empty() {
                    raw.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        raw.push(cur);
    }
    raw.into_iter()
        .filter_map(|t| {
            let b = t.as_bytes();
            if t == "%%" {
                Some("%".to_string())
            } else if b.len() == 2 && b[0] == b'%' && b[1].is_ascii_alphabetic() {
                None
            } else {
                Some(t)
            }
        })
        .collect()
}

pub fn parse_desktop_entry(content: &str) -> Option<SessionEntry> {
    let (mut in_entry, mut hidden) = (false, false);
    let (mut name, mut exec) = (None, None);
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            match k.trim() {
                "Name" => name = Some(v.to_string()),
                "Exec" => exec = Some(v.to_string()),
                "Hidden" | "NoDisplay" if v.eq_ignore_ascii_case("true") => hidden = true,
                _ => {}
            }
        }
    }
    if hidden {
        return None;
    }
    let name = name.filter(|n| !n.is_empty())?;
    let argv = parse_exec(&exec?);
    if argv.is_empty() {
        return None;
    }
    Some(SessionEntry { name, argv })
}

pub fn available_sessions_in(dirs: &[PathBuf]) -> Vec<SessionEntry> {
    let mut out: Vec<SessionEntry> = Vec::new();
    for dir in dirs {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        let mut paths: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("desktop"))
            .collect();
        paths.sort();
        for p in paths {
            if let Some(e) = fs::read_to_string(&p).ok().as_deref().and_then(parse_desktop_entry) {
                if !out.contains(&e) {
                    out.push(e);
                }
            }
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

pub fn available_sessions() -> Vec<SessionEntry> {
    let dirs: Vec<PathBuf> = SESSION_DIRS.iter().map(PathBuf::from).collect();
    available_sessions_in(&dirs)
}

/// Programs that already start a session bus; a session using one is left alone.
const DBUS_LAUNCHERS: [&str; 2] = ["dbus-run-session", "dbus-launch"];

/// Same as the `PATH` sessions are started with.
const DBUS_SEARCH_DIRS: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

/// First executable regular file called `name` in `dirs`.
pub fn find_executable(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    dirs.iter().map(|d| d.join(name)).find(|p| {
        fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    })
}

/// Run the session under `launcher -- ...` so it gets a D-Bus session bus, unless its command is
/// already `dbus-run-session`/`dbus-launch`. Only the command's first word is checked: wrappers like
/// `sh -c "dbus-run-session ..."` are not detected (turn `dbus_wrap` off for those).
pub fn with_session_bus(mut entry: SessionEntry, launcher: &Path) -> SessionEntry {
    let Some(first) = entry.argv.first() else { return entry };
    let base = Path::new(first).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if DBUS_LAUNCHERS.contains(&base) {
        return entry;
    }
    let mut argv = vec![launcher.display().to_string(), "--".to_string()];
    argv.append(&mut entry.argv);
    entry.argv = argv;
    entry
}

/// Same check as sd_booted(3): under systemd, `systemd --user` already provides the session bus
/// (`$XDG_RUNTIME_DIR/bus`), and a second bus from `dbus-run-session` would shadow it.
fn systemd_booted() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

/// Applies `with_session_bus` to every session when `wrap` is set and the system is not booted by
/// systemd. If `dbus-run-session` is not installed the sessions are returned unchanged (a missing
/// wrapper must not break logins).
pub fn prepare_sessions(sessions: Vec<SessionEntry>, wrap: bool) -> Vec<SessionEntry> {
    if !wrap || systemd_booted() {
        return sessions;
    }
    let dirs: Vec<PathBuf> = DBUS_SEARCH_DIRS.iter().map(PathBuf::from).collect();
    match find_executable("dbus-run-session", &dirs) {
        Some(l) => sessions.into_iter().map(|e| with_session_bus(e, &l)).collect(),
        None => {
            eprintln!("edm: dbus_wrap is on but dbus-run-session was not found; sessions start without a session bus");
            sessions
        }
    }
}

/// Environment for a freshly logged-in session (the child starts from an empty environment).
pub fn session_env(
    entry: &SessionEntry,
    user: &str,
    uid: u32,
    home: &Path,
    shell: &str,
    runtime_dir_exists: bool,
) -> Vec<(String, String)> {
    let mut e = vec![
        ("HOME".to_string(), home.display().to_string()),
        ("USER".into(), user.into()),
        ("LOGNAME".into(), user.into()),
        ("SHELL".into(), shell.into()),
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("XDG_SESSION_TYPE".into(), "wayland".into()),
        ("XDG_SESSION_DESKTOP".into(), entry.name.clone()),
        ("XDG_CURRENT_DESKTOP".into(), entry.name.clone()),
    ];
    if let Some(lang) = std::env::var_os("LANG").and_then(|l| l.into_string().ok()) {
        e.push(("LANG".into(), lang));
    }
    if runtime_dir_exists {
        e.push(("XDG_RUNTIME_DIR".into(), format!("/run/user/{uid}")));
    }
    e
}

/// Variables handed to PAM (`pam_putenv`) before `open_session`, so `pam_systemd` registers a
/// proper logind session: wayland user session on the greeter's seat/VT.
pub fn pam_session_vars(entry: &SessionEntry, seat: Option<&str>, vtnr: Option<&str>) -> Vec<String> {
    let mut v = vec![
        "XDG_SESSION_TYPE=wayland".to_string(),
        "XDG_SESSION_CLASS=user".to_string(),
        format!("XDG_SESSION_DESKTOP={}", entry.name),
        format!("XDG_SEAT={}", seat.filter(|s| !s.is_empty()).unwrap_or("seat0")),
    ];
    if let Some(vt) = vtnr.filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
        v.push(format!("XDG_VTNR={vt}"));
    }
    v
}

/// `extra` (what PAM modules exported, e.g. `XDG_SESSION_ID`, `XDG_RUNTIME_DIR`) overrides `base`.
/// Entries that are not valid `KEY` (empty, containing `=` or NUL) are dropped.
pub fn merge_env(base: Vec<(String, String)>, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = base;
    for (k, v) in extra {
        if k.is_empty() || k.contains(['=', '\0']) || v.contains('\0') {
            continue;
        }
        match out.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = v.clone(),
            None => out.push((k.clone(), v.clone())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pam_vars_and_env_merge() {
        let e = SessionEntry { name: "Hyprland".into(), argv: vec!["Hyprland".into()] };
        let v = pam_session_vars(&e, None, Some("2"));
        assert!(v.contains(&"XDG_SEAT=seat0".to_string()));
        assert!(v.contains(&"XDG_VTNR=2".to_string()));
        assert!(v.contains(&"XDG_SESSION_DESKTOP=Hyprland".to_string()));
        assert!(!pam_session_vars(&e, Some("seat1"), Some("x;y")).iter().any(|s| s.starts_with("XDG_VTNR")));
        assert!(pam_session_vars(&e, Some("seat1"), None).contains(&"XDG_SEAT=seat1".to_string()));

        let base = vec![("A".to_string(), "1".to_string()), ("B".to_string(), "2".to_string())];
        let extra = vec![
            ("B".to_string(), "pam".to_string()),
            ("C".to_string(), "3".to_string()),
            ("".to_string(), "x".to_string()),
            ("D=E".to_string(), "x".to_string()),
            ("F".to_string(), "a\0b".to_string()),
        ];
        let m = merge_env(base, &extra);
        assert_eq!(
            m,
            [("A".into(), "1".into()), ("B".into(), "pam".into()), ("C".into(), "3".into())] as [(String, String); 3]
        );
    }

    #[test]
    fn exec_keeps_arguments_and_drops_field_codes() {
        assert_eq!(parse_exec("dbus-run-session Hyprland"), ["dbus-run-session", "Hyprland"]);
        assert_eq!(parse_exec("sway %u --unsupported-gpu %%"), ["sway", "--unsupported-gpu", "%"]);
        assert_eq!(parse_exec(r#""/opt/my wm/start" --x "a b""#), ["/opt/my wm/start", "--x", "a b"]);
        assert_eq!(parse_exec(r#"x "" y"#), ["x", "", "y"]);
        assert!(parse_exec("   ").is_empty());
    }

    #[test]
    fn desktop_entry_parsing() {
        let ok = "[Desktop Entry]\nName=Hyprland\nName[tr]=Hypr\nExec=Hyprland\n";
        assert_eq!(
            parse_desktop_entry(ok),
            Some(SessionEntry { name: "Hyprland".into(), argv: vec!["Hyprland".into()] })
        );
        assert!(parse_desktop_entry("[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true\n").is_none());
        assert!(parse_desktop_entry("[Desktop Entry]\nName=A\nHidden=TRUE\nExec=a\n").is_none());
        assert!(parse_desktop_entry("[Desktop Entry]\nExec=a\n").is_none());
        assert!(parse_desktop_entry("[Desktop Entry]\nName=A\n").is_none());
        // keys outside [Desktop Entry] are ignored
        assert!(parse_desktop_entry("[Other]\nName=A\nExec=a\n").is_none());
        let action = "[Desktop Entry]\nName=A\nExec=a\n[Desktop Action x]\nName=B\nExec=b\nNoDisplay=true\n";
        assert_eq!(parse_desktop_entry(action).unwrap().argv, ["a"]);
    }

    #[test]
    fn lists_sorted_deduped_sessions() {
        let base = std::env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-sessions-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("b.desktop"), "[Desktop Entry]\nName=zeta\nExec=zeta\n").unwrap();
        fs::write(d.join("a.desktop"), "[Desktop Entry]\nName=Alpha\nExec=alpha --x\n").unwrap();
        fs::write(d.join("dup.desktop"), "[Desktop Entry]\nName=Alpha\nExec=alpha --x\n").unwrap();
        fs::write(d.join("hidden.desktop"), "[Desktop Entry]\nName=H\nExec=h\nHidden=true\n").unwrap();
        fs::write(d.join("note.txt"), "x").unwrap();
        let names: Vec<_> = available_sessions_in(&[d.clone(), d.join("missing")])
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["Alpha", "zeta"]);
    }

    #[test]
    fn dbus_wrapping() {
        let l = Path::new("/usr/bin/dbus-run-session");
        let plain = SessionEntry { name: "Mango".into(), argv: vec!["mango".into(), "-x".into()] };
        let w = with_session_bus(plain, l);
        assert_eq!(w.name, "Mango");
        assert_eq!(w.argv, ["/usr/bin/dbus-run-session", "--", "mango", "-x"]);
        for first in ["dbus-run-session", "/usr/bin/dbus-run-session", "dbus-launch"] {
            let e = SessionEntry { name: "H".into(), argv: vec![first.into(), "Hyprland".into()] };
            assert_eq!(with_session_bus(e.clone(), l), e);
        }
        let empty = SessionEntry { name: "E".into(), argv: vec![] };
        assert_eq!(with_session_bus(empty.clone(), l), empty);
        let one = vec![SessionEntry { name: "A".into(), argv: vec!["a".into()] }];
        assert_eq!(prepare_sessions(one.clone(), false), one);
    }

    #[test]
    fn finds_only_executable_files() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let d = base.join(format!("edm-test-exec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("tool"), "x").unwrap();
        fs::set_permissions(d.join("tool"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(find_executable("tool", std::slice::from_ref(&d)), None);
        fs::set_permissions(d.join("tool"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_executable("tool", &[d.join("missing"), d.clone()]), Some(d.join("tool")));
        assert_eq!(find_executable("none", &[d]), None);
    }

    #[test]
    fn env_contents() {
        let e = SessionEntry { name: "Hyprland".into(), argv: vec!["Hyprland".into()] };
        let env = session_env(&e, "zaman", 1000, Path::new("/home/zaman"), "/bin/zsh", true);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("HOME"), Some("/home/zaman"));
        assert_eq!(get("XDG_SESSION_TYPE"), Some("wayland"));
        assert_eq!(get("XDG_RUNTIME_DIR"), Some("/run/user/1000"));
        let env = session_env(&e, "zaman", 1000, Path::new("/home/zaman"), "/bin/zsh", false);
        assert!(!env.iter().any(|(n, _)| n == "XDG_RUNTIME_DIR"));
    }
}
