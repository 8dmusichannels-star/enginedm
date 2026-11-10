//! Keeps one session process under watch on a dedicated thread. std only (the two libc calls it
//! needs are declared directly, so the module builds and tests without the `libc` crate).
//!
//! Why a thread: a PAM session (`pam_open_session` .. `pam_close_session`) borrows its `Context`
//! and must stay open exactly as long as the user's session runs. The thread owns both, spawns the
//! session, waits for it, and only then lets the PAM session drop (closing it and, through
//! `pam_systemd`, ending the logind session).
//!
//! The thread body is supplied by the caller as `FnOnce(Ctl)`; it sets up whatever scope it needs
//! (PAM or nothing), then calls [`Ctl::run`] with a spawn closure.

use std::io;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;

/// How long a session gets between SIGTERM and SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(100);

/// Signal the child's process group (it is a session leader, so the group holds the compositor
/// and everything it started); fall back to the pid itself if there is no such group.
fn signal(child: &Child, sig: i32) {
    let pid = child.id() as i32;
    unsafe {
        if kill(-pid, sig) != 0 {
            kill(pid, sig);
        }
    }
}

/// SIGTERM, wait up to `grace`, then SIGKILL. Always reaps the child.
pub fn terminate(child: &mut Child, grace: Duration) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    signal(child, SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    signal(child, SIGKILL);
    let _ = child.kill();
    child.wait().map(|_| ())
}

enum Cmd {
    Stop,
    Restart(Sender<Result<(), String>>),
}

/// Handed to the thread body.
pub struct Ctl {
    rx: Receiver<Cmd>,
    ready: Sender<Result<(), String>>,
    running: Arc<AtomicBool>,
    grace: Duration,
}

impl Ctl {
    /// The session could not even be set up (e.g. `open_session` failed).
    pub fn fail(self, msg: String) {
        let _ = self.ready.send(Err(msg));
    }

    /// Spawn the session, report success/failure to [`Supervisor::start`], then watch it until it
    /// exits, is stopped, or the handle is dropped. `spawn` is called again on restart.
    pub fn run<F>(self, mut spawn: F)
    where
        F: FnMut() -> io::Result<Child>,
    {
        let mut child = match spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = self.ready.send(Err(e.to_string()));
                return;
            }
        };
        self.running.store(true, Ordering::SeqCst);
        let _ = self.ready.send(Ok(()));
        loop {
            match self.rx.recv_timeout(POLL) {
                Ok(Cmd::Restart(reply)) => {
                    let _ = terminate(&mut child, self.grace);
                    match spawn() {
                        Ok(c) => {
                            child = c;
                            let _ = reply.send(Ok(()));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e.to_string()));
                            break;
                        }
                    }
                }
                // Stop, or the owner went away: the session ends with the greeter.
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    let _ = terminate(&mut child, self.grace);
                    break;
                }
                Err(RecvTimeoutError::Timeout) => match child.try_wait() {
                    Ok(None) => {}
                    // exited on its own (user logged out) or wait failed
                    _ => break,
                },
            }
        }
        self.running.store(false, Ordering::SeqCst);
    }
}

pub struct Supervisor {
    tx: Sender<Cmd>,
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub fn start<B>(body: B) -> Result<Self, String>
    where
        B: FnOnce(Ctl) + Send + 'static,
    {
        Self::start_with_grace(STOP_GRACE, body)
    }

    pub fn start_with_grace<B>(grace: Duration, body: B) -> Result<Self, String>
    where
        B: FnOnce(Ctl) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(false));
        let ctl = Ctl { rx, ready: ready_tx, running: running.clone(), grace };
        let join = thread::Builder::new()
            .name("edm-session".into())
            .spawn(move || body(ctl))
            .map_err(|e| format!("cannot start session thread: {e}"))?;
        // Blocks while PAM opens the session and the child is spawned.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, running, join: Some(join) }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err("session thread ended unexpectedly".into())
            }
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Stop the session process and start it again inside the same scope (same PAM session).
    pub fn restart(&self) -> Result<(), String> {
        if !self.is_running() {
            return Err("no session is running".into());
        }
        let (tx, rx) = mpsc::channel();
        self.tx.send(Cmd::Restart(tx)).map_err(|_| "session already ended".to_string())?;
        rx.recv().map_err(|_| "session already ended".to_string())?
    }

    /// Stop the session and wait until the thread (and with it the PAM session) is gone.
    pub fn stop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicUsize;

    fn sh(script: &str) -> io::Result<Child> {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
    }

    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn stop_terminates_a_running_session() {
        let mut s = Supervisor::start(|ctl| ctl.run(|| sh("sleep 5"))).unwrap();
        assert!(s.is_running());
        let t = Instant::now();
        s.stop();
        assert!(!s.is_running());
        assert!(t.elapsed() < Duration::from_secs(2), "SIGTERM should be enough");
    }

    #[test]
    fn session_that_exits_by_itself_is_noticed() {
        let s = Supervisor::start(|ctl| ctl.run(|| sh("exit 0"))).unwrap();
        assert!(wait_until(|| !s.is_running()));
        assert!(s.restart().is_err());
    }

    #[test]
    fn spawn_failure_is_reported_to_the_caller() {
        let e = Supervisor::start(|ctl| ctl.run(|| Command::new("/nonexistent/edm-test").spawn())).err().unwrap();
        assert!(!e.is_empty());
    }

    #[test]
    fn setup_failure_is_reported_to_the_caller() {
        let e = Supervisor::start(|ctl| ctl.fail("pam says no".into())).err().unwrap();
        assert_eq!(e, "pam says no");
        let e = Supervisor::start(|_ctl| panic!("boom")).err().unwrap();
        assert!(e.contains("unexpectedly"));
    }

    #[test]
    fn restart_respawns_in_the_same_scope() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let s = Supervisor::start(move |ctl| {
            ctl.run(|| {
                c.fetch_add(1, Ordering::SeqCst);
                sh("sleep 5")
            })
        })
        .unwrap();
        assert_eq!(s.restart(), Ok(()));
        assert!(s.is_running());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_restart_ends_the_session_and_says_why() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let s = Supervisor::start(move |ctl| {
            ctl.run(|| {
                if c.fetch_add(1, Ordering::SeqCst) == 0 {
                    sh("sleep 5")
                } else {
                    Err(io::Error::other("no more"))
                }
            })
        })
        .unwrap();
        assert_eq!(s.restart(), Err("no more".to_string()));
        assert!(wait_until(|| !s.is_running()));
    }

    #[test]
    fn scope_outlives_the_child_and_is_dropped_last() {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let closed = Arc::new(AtomicBool::new(false));
        let c = closed.clone();
        let mut s = Supervisor::start(move |ctl| {
            let _pam_session = Guard(c); // stands in for pam_client::Session
            ctl.run(|| sh("sleep 5"));
        })
        .unwrap();
        assert!(!closed.load(Ordering::SeqCst), "scope must stay open while the session runs");
        s.stop();
        assert!(closed.load(Ordering::SeqCst), "scope must be closed once the session is gone");
    }

    #[test]
    fn dropping_the_handle_ends_the_session() {
        let closed = Arc::new(AtomicBool::new(false));
        let c = closed.clone();
        let s = Supervisor::start(move |ctl| {
            ctl.run(|| sh("sleep 5"));
            c.store(true, Ordering::SeqCst);
        })
        .unwrap();
        drop(s);
        assert!(closed.load(Ordering::SeqCst));
    }

    #[test]
    fn stubborn_child_gets_sigkill_after_the_grace_period() {
        let mut c = sh("trap '' TERM; sleep 5").unwrap();
        thread::sleep(Duration::from_millis(200)); // let the shell install the trap
        let t = Instant::now();
        terminate(&mut c, Duration::from_millis(300)).unwrap();
        let e = t.elapsed();
        assert!(e >= Duration::from_millis(300) && e < Duration::from_secs(3), "{e:?}");
        assert!(c.try_wait().unwrap().is_some());
        // already dead: no error, no signal to a recycled pid
        terminate(&mut c, Duration::from_millis(10)).unwrap();
    }
}
