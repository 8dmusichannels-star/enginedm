//! Starting a session as the authenticated user.
//!
//! The old `Session::start` ran the compositor as the greeter's own user. This looks the user up,
//! drops privileges in the child (supplementary groups, gid, uid) and builds a clean environment.
//!
//! The session runs under a [`Supervisor`] thread. When a [`PamLogin`] is passed in, that thread
//! also owns the PAM session (`pam_open_session` .. `pam_close_session`), which is what makes
//! `pam_systemd` create `/run/user/<uid>` and a logind session; the variables PAM exports are
//! merged into the session environment. The greeter process must therefore stay alive while the
//! session runs (when it exits, the session is stopped and the PAM session closed).

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::auth::PamLogin;
use crate::desktop::{merge_env, session_env, SessionEntry};
use crate::supervise::Supervisor;

#[derive(Clone, Debug)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: String,
}

impl UserInfo {
    pub fn lookup(name: &str) -> io::Result<Self> {
        let cname = CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in username"))?;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0u8; 1024];
        loop {
            let rc = unsafe {
                libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr().cast(), buf.len(), &mut result)
            };
            if rc == libc::ERANGE && buf.len() < (1 << 20) {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            break;
        }
        if result.is_null() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such user"));
        }
        let s = |p: *const libc::c_char| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
        Ok(Self {
            name: s(pwd.pw_name),
            uid: pwd.pw_uid,
            gid: pwd.pw_gid,
            home: PathBuf::from(s(pwd.pw_dir)),
            shell: s(pwd.pw_shell),
        })
    }
}

/// Supplementary groups, computed in the parent: `initgroups` is not async-signal-safe and must
/// not run between fork and exec in a multi-threaded process.
fn group_list(name: &CString, gid: u32) -> io::Result<Vec<libc::gid_t>> {
    let mut n: libc::c_int = 32;
    loop {
        let mut groups: Vec<libc::gid_t> = vec![0; n as usize];
        let mut count = n;
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), gid as libc::gid_t, groups.as_mut_ptr(), &mut count) };
        if rc >= 0 {
            groups.truncate(count.max(0) as usize);
            return Ok(groups);
        }
        if count <= n {
            return Err(io::Error::other("getgrouplist failed"));
        }
        n = count; // buffer was too small; `count` is the required size
    }
}

/// The session command with privileges dropped (when running as root) and a clean environment.
/// `extra_env` is what PAM exported; it overrides the defaults.
pub fn build_command(entry: &SessionEntry, user: &UserInfo, extra_env: &[(String, String)]) -> io::Result<Command> {
    let (program, args) = entry
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty session command"))?;

    let euid = unsafe { libc::geteuid() };
    let switch_user = euid == 0;
    if !switch_user && euid != user.uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "must run as root to start a session for another user",
        ));
    }
    let cname = CString::new(user.name.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in username"))?;
    let groups = if switch_user { group_list(&cname, user.gid)? } else { Vec::new() };
    let (uid, gid) = (user.uid, user.gid);

    // Checked now (after pam_open_session), so a runtime dir created by pam_systemd is seen.
    let runtime_dir = format!("/run/user/{uid}");
    let env = merge_env(
        session_env(entry, &user.name, uid, &user.home, &user.shell, Path::new(&runtime_dir).is_dir()),
        extra_env,
    );
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .envs(env)
        .current_dir(&user.home)
        .stdin(Stdio::null());
    // Keep the session's output: a compositor that dies at once must leave a trace somewhere.
    // Falls back to /dev/null when the log cannot be opened.
    let log = std::env::var("EDM_SESSION_LOG").unwrap_or_else(|_| "/var/log/enginedm-session.log".into());
    match std::fs::OpenOptions::new().create(true).append(true).open(&log) {
        Ok(f) => match f.try_clone() {
            Ok(f2) => {
                cmd.stdout(Stdio::from(f)).stderr(Stdio::from(f2));
            }
            Err(_) => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        },
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Only async-signal-safe calls in here.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid(); // new session; failing because we already lead one is fine
            if switch_user {
                if libc::setgroups(groups.len() as _, groups.as_ptr()) != 0
                    || libc::setgid(gid as libc::gid_t) != 0
                    || libc::setuid(uid as libc::uid_t) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if uid != 0 && libc::setuid(0) == 0 {
                    return Err(io::Error::other("privilege drop failed"));
                }
            }
            Ok(())
        });
    }
    Ok(cmd)
}

pub fn spawn_session(entry: &SessionEntry, user: &UserInfo, extra_env: &[(String, String)]) -> io::Result<Child> {
    build_command(entry, user, extra_env)?.spawn()
}

fn other(msg: String) -> io::Error {
    io::Error::other(msg)
}

#[derive(Default)]
pub struct SessionRunner {
    sup: Option<Supervisor>,
}

impl SessionRunner {
    pub fn is_running(&self) -> bool {
        self.sup.as_ref().is_some_and(Supervisor::is_running)
    }

    /// Start `entry` as `user`. With `pam`, the PAM session is opened first and closed when the
    /// session process ends. Returns once the process is spawned (or setup failed).
    pub fn start(&mut self, entry: &SessionEntry, user: &UserInfo, pam: Option<PamLogin>) -> io::Result<()> {
        if self.is_running() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "a session is already running"));
        }
        self.sup = None; // joins a finished supervisor (and closes its PAM session)
        let (entry, user) = (entry.clone(), user.clone());
        let sup = match pam {
            Some(login) => login.host_session(entry, user),
            None => Supervisor::start(move |ctl| ctl.run(|| spawn_session(&entry, &user, &[]))),
        }
        .map_err(other)?;
        self.sup = Some(sup);
        Ok(())
    }

    /// Block until the session process has ended on its own, then close the PAM session. The
    /// greeter process owns the PAM session, so it must not exit before this returns.
    pub fn wait(&mut self) {
        while self.is_running() {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        self.stop(); // joins the finished supervisor thread (closes the PAM session)
    }

    /// Like [`wait`](Self::wait), but also serves restart requests (`SIGUSR1`, see
    /// [`crate::restart`]): the session process is restarted inside the same PAM session.
    /// `on_restart` gets the result of every restart. If a restart fails the session has ended
    /// (the supervisor stops it), so this returns afterwards.
    pub fn wait_with_restart(&mut self, mut on_restart: impl FnMut(Result<(), String>)) {
        while self.is_running() {
            if crate::restart::take_request() {
                on_restart(self.restart().map_err(|e| e.to_string()));
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        self.stop();
    }

    /// Stop the session process; the PAM session is closed when this returns.
    pub fn stop(&mut self) {
        if let Some(mut s) = self.sup.take() {
            s.stop();
        }
    }

    /// Restart the session process inside the same PAM session (no new login needed).
    pub fn restart(&mut self) -> io::Result<()> {
        match &self.sup {
            Some(s) => s.restart().map_err(other),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no session was started")),
        }
    }
}
