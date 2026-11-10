//! Restarting the running session (standalone mode).
//!
//! After the handover the session is hosted by `enginedmgreeterd --run-handoff`, which owns the
//! PAM session. A restart request is therefore a signal to that process: it writes its pid to a
//! small file (0600) and listens for `SIGUSR1`; on the signal it stops the session process and
//! spawns it again inside the same PAM session (no new login). Anyone who may signal that
//! process (in practice: root) can ask for a restart with [`request_restart`] or
//! `enginedmgreeterd --restart-session`.
//!
//! Not available under greetd: there greetd owns the session, not us.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const DEFAULT_PID_FILE: &str = "/run/enginedm/session.pid";

static RESTART_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn pid_path() -> PathBuf {
    std::env::var_os("EDM_SESSION_PID_FILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(DEFAULT_PID_FILE))
}

extern "C" fn on_sigusr1(_sig: libc::c_int) {
    // Only an atomic store: async-signal-safe.
    RESTART_REQUESTED.store(true, Ordering::SeqCst);
}

/// Installs the `SIGUSR1` handler. Call this BEFORE the pid file is written: the default action
/// of `SIGUSR1` is to terminate the process.
pub fn install_handler() {
    unsafe {
        libc::signal(libc::SIGUSR1, on_sigusr1 as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
}

/// True once (and clears the flag) if a restart was requested since the last call.
pub fn take_request() -> bool {
    RESTART_REQUESTED.swap(false, Ordering::SeqCst)
}

/// Owns the pid file of the running session host; removes it on drop.
pub struct PidGuard {
    path: PathBuf,
}

impl PidGuard {
    /// Writes the current pid (0600, atomically via rename).
    pub fn create(path: &Path) -> io::Result<Self> {
        let tmp = path.with_extension("tmp");
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(Self { path: path.to_path_buf() })
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Pid of the running session host. Refuses a file that is not ours / is writable by others, and
/// a pid that no longer exists (stale file).
pub fn read_pid(path: &Path) -> Result<i32, String> {
    let meta = fs::symlink_metadata(path).map_err(|_| "no session is running".to_string())?;
    let me = unsafe { libc::geteuid() };
    if !meta.is_file() || meta.uid() != me || meta.mode() & 0o022 != 0 {
        return Err("session pid file has wrong owner or permissions".into());
    }
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let pid: i32 = text.trim().parse().map_err(|_| "malformed session pid file".to_string())?;
    if pid <= 1 {
        return Err("malformed session pid file".into());
    }
    if unsafe { libc::kill(pid, 0) } != 0 {
        return Err("no session is running".into());
    }
    Ok(pid)
}

/// Asks the session host to restart its session. Returns once the signal is sent; the restart
/// itself happens asynchronously in the host.
pub fn request_restart(path: &Path) -> Result<(), String> {
    let pid = read_pid(path)?;
    if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
        return Err(format!("cannot signal the session host: {}", io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("edm-restart-{}-{name}", std::process::id()));
        let _ = fs::remove_file(&d);
        d
    }

    #[test]
    fn pid_file_roundtrip_and_cleanup() {
        let p = tmp("rt");
        {
            let _g = PidGuard::create(&p).unwrap();
            assert_eq!(read_pid(&p).unwrap(), std::process::id() as i32);
        }
        assert!(!p.exists(), "guard removes the file");
        assert!(read_pid(&p).is_err());
    }

    #[test]
    fn stale_malformed_and_loose_files_are_refused() {
        let p = tmp("bad");
        fs::write(&p, "not a pid\n").unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        assert!(read_pid(&p).is_err());
        fs::write(&p, "1\n").unwrap(); // never signal init
        assert!(read_pid(&p).is_err());
        fs::write(&p, format!("{}\n", std::process::id())).unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o666)).unwrap();
        assert!(read_pid(&p).is_err(), "group/world-writable pid file");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn request_sets_the_flag_once() {
        install_handler();
        let p = tmp("sig");
        let _g = PidGuard::create(&p).unwrap();
        assert!(!take_request());
        request_restart(&p).unwrap();
        // The handler runs on some thread of this process; give it a moment.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < end && !RESTART_REQUESTED.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(take_request());
        assert!(!take_request(), "the flag is consumed");
    }

    #[test]
    fn no_host_means_a_clear_error() {
        assert_eq!(request_restart(&tmp("none")), Err("no session is running".to_string()));
    }
}
