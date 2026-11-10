//! The real [`Backend`]: PAM + 2FA + sessions + themes from `edm-core`.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use edm_core::auth::{PamAuth, PamLogin};
use edm_core::config::EDMConfig;
use edm_core::desktop::{available_sessions, prepare_sessions, SessionEntry};
use edm_core::greetd::{self, GreetdClient};
use edm_core::launch::{SessionRunner, UserInfo};
use edm_core::otp::TwoFactor;
use edm_core::state::{hotp_counter_path, load_counter_for, save_counter_for};
use edm_core::theme::ThemeManager;

use crate::controller::Backend;

pub struct RealBackend {
    pam: PamAuth,
    two_factor: TwoFactor,
    sessions: Vec<SessionEntry>,
    runner: SessionRunner,
    themes: ThemeManager,
    /// PAM transaction of a password that was accepted but whose login is not finished yet
    /// (2FA pending / session about to start). Dropped on any failure.
    pending: Option<PamLogin>,
    hotp_path: PathBuf,
    /// Fingerprint of the HOTP secret; stored with the counter so a new secret restarts at 0.
    hotp_fp: String,
    /// Talk to greetd instead of PAM + our own supervisor (set when `GREETD_SOCK` exists).
    use_greetd: bool,
    /// greetd session that accepted the password but has not been started yet.
    greetd_pending: Option<(String, GreetdClient<UnixStream>)>,
    exit: bool,
}

impl RealBackend {
    pub fn new(config: EDMConfig, config_path: PathBuf) -> Self {
        let mut two_factor = TwoFactor::from_config(
            config.two_factor_enabled,
            config.two_factor_method.as_deref(),
            config.two_factor_secret.as_deref(),
        );
        let hotp_path = hotp_counter_path();
        let hotp_fp = two_factor.secret_fingerprint().unwrap_or_default();
        if two_factor.counter().is_some() {
            // A corrupt/unreadable counter file must lock logins, not reset to 0 (replay). A file
            // written for a different secret is not corrupt: the secret was changed, counter 0.
            match load_counter_for(&hotp_path, &hotp_fp) {
                Ok(c) => two_factor.set_counter(c),
                Err(e) => two_factor.mark_problem(&e),
            }
        }
        let pam = PamAuth::new(config.pam_service.as_deref().unwrap_or(""));
        let dbus_wrap = config.dbus_wrap;
        let mut themes = ThemeManager::new(config, config_path);
        // No scrub file yet / no themes is normal on a fresh install.
        if let Err(e) = themes.load_and_apply_themes() {
            eprintln!("edm: themes unavailable: {e}");
        }
        Self {
            pam,
            two_factor,
            sessions: prepare_sessions(available_sessions(), dbus_wrap),
            runner: SessionRunner::default(),
            themes,
            pending: None,
            hotp_path,
            hotp_fp,
            use_greetd: greetd::available(),
            greetd_pending: None,
            exit: false,
        }
    }

    pub fn default_session_name(&self) -> &str {
        &self.themes.config.default_session
    }

    /// Ends any half-finished login (PAM transaction or greetd session).
    fn drop_pending(&mut self) {
        self.pending = None;
        if let Some((_, mut c)) = self.greetd_pending.take() {
            c.cancel();
        }
    }
}

impl Drop for RealBackend {
    /// The PAM session lives on a thread of this process. If the window goes away while the
    /// session still runs, wait for it instead of killing it. (Not needed with greetd: there the
    /// greeter exits on purpose and greetd owns the session.)
    fn drop(&mut self) {
        if self.runner.is_running() {
            eprintln!("edm: greeter window closed; staying alive until the session ends");
            self.runner.wait();
        }
    }
}

impl Backend for RealBackend {
    fn authenticate(&mut self, username: &str, password: &str) -> bool {
        self.drop_pending(); // a new attempt ends any earlier half-finished login
        if self.use_greetd {
            return match GreetdClient::connect_env().and_then(|mut c| c.authenticate(username, password).map(|()| c)) {
                Ok(c) => {
                    self.greetd_pending = Some((username.to_string(), c));
                    true
                }
                Err(e) => {
                    eprintln!("edm: greetd auth failed for '{username}': {e}");
                    false
                }
            };
        }
        match self.pam.authenticate(username, password) {
            Ok(login) => {
                self.pending = Some(login);
                true
            }
            Err(e) => {
                eprintln!("edm: auth failed for '{username}': {e}"); // never log the password
                false
            }
        }
    }

    fn two_factor_required(&self) -> bool {
        self.two_factor.required()
    }

    fn verify_two_factor(&mut self, code: &str) -> bool {
        let before = self.two_factor.counter();
        let ok = self.two_factor.verify(code);
        if !ok {
            self.drop_pending();
            return false;
        }
        // HOTP: write the advanced counter BEFORE letting the user in. If it cannot be saved the
        // same code would work again after a restart, so refuse and roll the counter back.
        if let (Some(old), Some(new)) = (before, self.two_factor.counter()) {
            if new != old {
                if let Err(e) = save_counter_for(&self.hotp_path, new, &self.hotp_fp) {
                    eprintln!("edm: HOTP counter not saved, login refused: {e}");
                    self.two_factor.set_counter(old);
                    self.drop_pending();
                    return false;
                }
            }
        }
        true
    }

    fn config_problem(&self) -> Option<String> {
        self.two_factor.problem().map(|p| format!("Two-factor setup error: {p}"))
    }

    fn sessions(&self) -> &[SessionEntry] {
        &self.sessions
    }

    fn start_session(&mut self, username: &str, session: &SessionEntry) -> Result<(), String> {
        if self.use_greetd {
            // Only reuse the greetd session that authenticated exactly this user.
            let Some((_, mut client)) = self.greetd_pending.take().filter(|(u, _)| u == username) else {
                eprintln!("edm: no greetd session for '{username}'; refusing to start");
                return Err("see the greeter log".into());
            };
            let env = vec![
                "XDG_SESSION_TYPE=wayland".to_string(),
                format!("XDG_SESSION_DESKTOP={}", session.name),
                format!("XDG_CURRENT_DESKTOP={}", session.name),
            ];
            return match client.start_session(&session.argv, &env) {
                Ok(()) => {
                    self.exit = true;
                    Ok(())
                }
                Err(e) => {
                    eprintln!("edm: greetd start_session failed: {e}");
                    Err("see the greeter log".into())
                }
            };
        }
        let user = UserInfo::lookup(username).map_err(|e| {
            eprintln!("edm: user lookup failed: {e}");
            "unknown user".to_string()
        })?;
        // Only continue if the PAM transaction authenticated exactly this user.
        let pam = self.pending.take().filter(|p| p.username() == username);
        if pam.is_none() {
            eprintln!("edm: no PAM transaction for '{username}'; refusing to start a session");
            return Err("see the greeter log".into());
        }
        drop(pam); // auth-only transaction; the session host opens its own PAM session
        // The greeter's compositor still owns the display, so the session cannot start in this
        // process. Leave a handoff and exit; `enginedm-run` starts it once the greeter's Weston is
        // gone (`enginedmgreeterd --run-handoff`).
        let _ = &user;
        edm_core::handoff::write(&edm_core::handoff::handoff_path(), username, &session.name).map_err(|e| {
            eprintln!("edm: cannot write handoff file: {e}");
            "see the greeter log".to_string()
        })?;
        self.exit = true;
        Ok(())
    }

    fn restart_session(&mut self) -> Result<(), String> {
        if self.use_greetd {
            return Err("restart is not available under greetd".into());
        }
        // Standalone: after the handover the session is hosted by `enginedmgreeterd
        // --run-handoff`, a separate process. Ask it (SIGUSR1) to restart the session; if the
        // session ran in this process (`runner`), restart it directly.
        if self.runner.is_running() {
            return self.runner.restart().map_err(|e| e.to_string());
        }
        edm_core::restart::request_restart(&edm_core::restart::pid_path())
    }

    fn should_exit(&self) -> bool {
        self.exit
    }

    fn themes(&self) -> Vec<String> {
        self.themes.names()
    }

    fn set_theme(&mut self, name: &str) -> Result<(), String> {
        self.themes.set_theme(name)
    }
}
