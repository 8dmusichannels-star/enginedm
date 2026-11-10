//! Small persistent state the greeter must keep across restarts (currently: the HOTP counter).
//! std only.
//!
//! The file is replaced atomically (write temp file, fsync, rename) with mode 0600, so a crash or
//! power loss leaves either the old or the new counter, never a torn file.

use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_STATE_DIR: &str = "/var/lib/enginedm";
pub const HOTP_COUNTER_FILE: &str = "hotp_counter";

/// `$EDM_STATE_DIR` if set, else `/var/lib/enginedm`.
pub fn state_dir() -> PathBuf {
    env::var_os("EDM_STATE_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR))
}

pub fn hotp_counter_path() -> PathBuf {
    state_dir().join(HOTP_COUNTER_FILE)
}

/// Parsed counter file: `<counter>\n` (legacy) or `<counter>\n<16 hex digits fingerprint>\n`.
struct Stored {
    counter: u64,
    fingerprint: Option<String>,
}

fn read_stored(path: &Path) -> Result<Option<Stored>, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let corrupt = || format!("{} is corrupt (expected a counter and optionally a fingerprint)", path.display());
    let mut lines = text.lines().map(str::trim);
    let counter = lines.next().and_then(|l| l.parse::<u64>().ok()).ok_or_else(corrupt)?;
    let fingerprint = lines.next().filter(|l| !l.is_empty()).map(str::to_string);
    if lines.any(|l| !l.is_empty()) {
        return Err(corrupt());
    }
    if let Some(f) = &fingerprint {
        if f.len() != 16 || !f.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(corrupt());
        }
    }
    Ok(Some(Stored { counter, fingerprint }))
}

/// A missing file means "never used" (counter 0). An unreadable or corrupt file is an error:
/// guessing 0 would make every already-used code valid again.
pub fn load_counter(path: &Path) -> Result<u64, String> {
    Ok(read_stored(path)?.map_or(0, |s| s.counter))
}

/// Like [`load_counter`] for a known secret fingerprint: a file written for a *different* secret
/// means the secret was changed, so nothing was ever used with the new one and the counter is 0.
/// A legacy file without a fingerprint is accepted as is.
pub fn load_counter_for(path: &Path, fingerprint: &str) -> Result<u64, String> {
    Ok(match read_stored(path)? {
        None => 0,
        Some(Stored { fingerprint: Some(f), .. }) if !f.eq_ignore_ascii_case(fingerprint) => 0,
        Some(s) => s.counter,
    })
}

pub fn save_counter(path: &Path, value: u64) -> Result<(), String> {
    write_atomic(path, &format!("{value}\n"))
}

/// Saves the counter together with the fingerprint of the secret it belongs to.
pub fn save_counter_for(path: &Path, value: u64, fingerprint: &str) -> Result<(), String> {
    write_atomic(path, &format!("{value}\n{fingerprint}\n"))
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let err = |what: &str, e: std::io::Error| format!("{what} {}: {e}", path.display());
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| err("cannot create directory for", e))?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(err("cannot save", e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let base = env::var_os("EDM_TEST_TMP").map(PathBuf::from).unwrap_or_else(env::temp_dir);
        let p = base.join(format!("edm-test-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_file_is_zero() {
        assert_eq!(load_counter(&tmp("missing").join("nope")), Ok(0));
    }

    #[test]
    fn round_trip_is_private_and_leaves_no_temp_file() {
        let d = tmp("rt");
        let p = d.join("deep/dir/hotp_counter");
        save_counter(&p, 41).unwrap();
        save_counter(&p, 42).unwrap();
        assert_eq!(load_counter(&p), Ok(42));
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let names: Vec<_> = fs::read_dir(p.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn corrupt_file_is_an_error_not_zero() {
        let d = tmp("bad");
        let p = d.join("c");
        for junk in ["", "abc", "-1", "1 2", "99999999999999999999999"] {
            fs::write(&p, junk).unwrap();
            assert!(load_counter(&p).is_err(), "{junk:?}");
        }
        fs::write(&p, " 7 \n").unwrap();
        assert_eq!(load_counter(&p), Ok(7));
    }

    const FP_A: &str = "4ba509f9963bbe39";
    const FP_B: &str = "5395869b4a07a963";

    #[test]
    fn counter_is_bound_to_the_secret() {
        let d = tmp("fp");
        let p = d.join("hotp_counter");
        save_counter_for(&p, 9, FP_A).unwrap();
        assert_eq!(load_counter_for(&p, FP_A), Ok(9));
        // secret changed -> start again at 0, no manual file delete
        assert_eq!(load_counter_for(&p, FP_B), Ok(0));
        // the plain loader still sees the number
        assert_eq!(load_counter(&p), Ok(9));
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn legacy_file_without_fingerprint_is_accepted() {
        let d = tmp("legacy");
        let p = d.join("c");
        fs::write(&p, "12\n").unwrap();
        assert_eq!(load_counter_for(&p, FP_A), Ok(12));
        assert_eq!(load_counter_for(&d.join("missing"), FP_A), Ok(0));
    }

    #[test]
    fn bad_fingerprint_line_or_extra_lines_are_corrupt() {
        let d = tmp("badfp");
        let p = d.join("c");
        for junk in ["5\nnothex\n", "5\n5ca6151c8f5b895\n", "5\n4ba509f9963bbe39\nmore\n"] {
            fs::write(&p, junk).unwrap();
            assert!(load_counter_for(&p, FP_A).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn unwritable_location_reports_error() {
        let d = tmp("ro");
        let file = d.join("a_file");
        fs::write(&file, "x").unwrap();
        // parent is a regular file -> cannot create the directory
        assert!(save_counter(&file.join("sub/c"), 1).is_err());
    }
}
