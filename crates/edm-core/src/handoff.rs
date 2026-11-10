//! Standalone-mode handover between the greeter and the session host.
//!
//! The greeter runs inside its own Weston, which keeps the display. A user session cannot start
//! while that compositor is alive, so the greeter only authenticates, writes a small handoff
//! file here and exits. The run loop (`enginedm-run`) then sees the file, the greeter's Weston is
//! gone, and `enginedmgreeterd --run-handoff` opens the PAM session and starts the user's
//! compositor on the freed VT. std only.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const DEFAULT_HANDOFF: &str = "/run/enginedm/handoff";

pub fn handoff_path() -> PathBuf {
    std::env::var_os("EDM_HANDOFF_FILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_HANDOFF))
}

fn clean(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|c| c.is_control())
}

/// Writes `user` and the session name (0600, atomically via rename).
pub fn write(path: &Path, user: &str, session: &str) -> io::Result<()> {
    if !clean(user) || !clean(session) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "bad characters in handoff"));
    }
    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(format!("{user}\n{session}\n").as_bytes())?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

/// Reads and deletes the handoff. Refuses a file that is not ours or is readable by others.
pub fn take(path: &Path) -> Result<(String, String), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("no handoff file: {e}"))?;
    let me = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != me || meta.mode() & 0o077 != 0 {
        let _ = fs::remove_file(path);
        return Err("handoff file has wrong owner or permissions".into());
    }
    let text = fs::read_to_string(path).map_err(|e| e.to_string());
    let _ = fs::remove_file(path);
    let text = text?;
    let mut lines = text.lines();
    match (lines.next(), lines.next(), lines.next()) {
        (Some(u), Some(s), None) if clean(u) && clean(s) => Ok((u.to_string(), s.to_string())),
        _ => Err("malformed handoff file".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("edm-handoff-{}-{name}", std::process::id()));
        let _ = fs::remove_file(&d);
        d
    }

    #[test]
    fn roundtrip_and_consumed() {
        let p = tmp("rt");
        write(&p, "zaman", "Hyprland").unwrap();
        assert_eq!(take(&p).unwrap(), ("zaman".into(), "Hyprland".into()));
        assert!(take(&p).is_err(), "second take must fail: the file is consumed");
    }

    #[test]
    fn rejects_newlines_and_bad_perms() {
        let p = tmp("bad");
        assert!(write(&p, "a\nb", "x").is_err());
        fs::write(&p, "u\ns\n").unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        assert!(take(&p).is_err());
        assert!(!p.exists());
    }

    #[test]
    fn malformed() {
        let p = tmp("mal");
        write(&p, "u", "s").unwrap();
        fs::write(&p, "u\ns\nextra\n").unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        assert!(take(&p).is_err());
    }
}
