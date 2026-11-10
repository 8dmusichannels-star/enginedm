//! The login flow as a plain state machine over a [`Backend`] trait. No UI, no PAM: that is what
//! makes it testable (see the mock at the bottom).

use edm_core::desktop::SessionEntry;

pub const MSG_LOGIN_FAILED: &str = "Login failed";

pub trait Backend {
    /// PAM password check for the typed user.
    fn authenticate(&mut self, username: &str, password: &str) -> bool;
    fn two_factor_required(&self) -> bool;
    fn verify_two_factor(&mut self, code: &str) -> bool;
    /// Set when the setup is broken (e.g. 2FA enabled without secret); logins then fail with it.
    fn config_problem(&self) -> Option<String> {
        None
    }
    fn sessions(&self) -> &[SessionEntry];
    fn start_session(&mut self, username: &str, session: &SessionEntry) -> Result<(), String>;
    fn restart_session(&mut self) -> Result<(), String>;
    /// True once the greeter has nothing left to do and must exit (greetd starts the session
    /// only after the greeter is gone).
    fn should_exit(&self) -> bool {
        false
    }
    fn themes(&self) -> Vec<String>;
    fn set_theme(&mut self, name: &str) -> Result<(), String>;
}

/// State of the optional show/hide button next to the password field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reveal {
    visible: bool,
}

impl Reveal {
    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Value for the password input's `type` attribute.
    pub fn input_type(&self) -> &'static str {
        if self.visible {
            "text"
        } else {
            "password"
        }
    }

    /// Text for the button: the action it will perform (only the tests check it).
    #[cfg(test)]
    pub fn label(&self) -> &'static str {
        if self.visible {
            "Hide"
        } else {
            "Show"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Session name that was started.
    Started(String),
    /// Message that is safe to show on the login screen.
    Failed(String),
}

pub struct Controller<B: Backend> {
    pub backend: B,
    selected: usize,
}

impl<B: Backend> Controller<B> {
    pub fn new(backend: B) -> Self {
        Self { backend, selected: 0 }
    }

    pub fn selected_session(&self) -> usize {
        self.selected
    }

    pub fn select_session(&mut self, index: usize) -> bool {
        if index < self.backend.sessions().len() {
            self.selected = index;
            true
        } else {
            false
        }
    }

    /// Pre-select a session by (case-insensitive) name, e.g. `default_session` from the config.
    pub fn select_session_named(&mut self, name: &str) -> bool {
        match self.backend.sessions().iter().position(|s| s.name.eq_ignore_ascii_case(name)) {
            Some(i) => {
                self.selected = i;
                true
            }
            None => false,
        }
    }

    pub fn login(&mut self, username: &str, password: &str, code: &str) -> Outcome {
        let fail = |m: &str| Outcome::Failed(m.to_string());
        if let Some(p) = self.backend.config_problem() {
            return Outcome::Failed(p);
        }
        let username = username.trim();
        let code = code.trim();
        if username.is_empty() {
            return fail("Username missing");
        }
        if password.is_empty() {
            return fail("Password missing");
        }
        let two_factor = self.backend.two_factor_required();
        if two_factor && code.is_empty() {
            return fail("2FA code required");
        }
        if self.backend.sessions().is_empty() {
            return fail("No sessions installed");
        }
        // Password first; the 2FA code (HOTP counter!) is only consumed for a correct password.
        // Both failures show the same message so the screen doesn't say which factor was wrong.
        if !self.backend.authenticate(username, password) {
            return fail(MSG_LOGIN_FAILED);
        }
        if two_factor && !self.backend.verify_two_factor(code) {
            return fail(MSG_LOGIN_FAILED);
        }
        let Some(entry) = self.backend.sessions().get(self.selected).cloned() else {
            return fail("No session selected");
        };
        match self.backend.start_session(username, &entry) {
            Ok(()) => Outcome::Started(entry.name),
            Err(e) => Outcome::Failed(format!("Could not start session: {e}")),
        }
    }

    pub fn restart(&mut self) -> Result<(), String> {
        self.backend.restart_session()
    }

    /// Apply the theme at `index` of `backend.themes()`; returns its name.
    pub fn apply_theme(&mut self, index: usize) -> Result<String, String> {
        let name = self.backend.themes().get(index).cloned().ok_or("No such theme")?;
        self.backend.set_theme(&name)?;
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        pw_ok: bool,
        tf_required: bool,
        tf_ok: bool,
        problem: Option<String>,
        sessions: Vec<SessionEntry>,
        start_err: Option<String>,
        auth_calls: Vec<(String, String)>,
        tf_calls: usize,
        started: Vec<(String, String)>,
        theme: Option<String>,
    }

    fn sess(n: &str) -> SessionEntry {
        SessionEntry { name: n.into(), argv: vec![n.to_lowercase()] }
    }

    impl Backend for Mock {
        fn authenticate(&mut self, u: &str, p: &str) -> bool {
            self.auth_calls.push((u.into(), p.into()));
            self.pw_ok
        }
        fn two_factor_required(&self) -> bool {
            self.tf_required
        }
        fn verify_two_factor(&mut self, _c: &str) -> bool {
            self.tf_calls += 1;
            self.tf_ok
        }
        fn config_problem(&self) -> Option<String> {
            self.problem.clone()
        }
        fn sessions(&self) -> &[SessionEntry] {
            &self.sessions
        }
        fn start_session(&mut self, u: &str, s: &SessionEntry) -> Result<(), String> {
            self.started.push((u.into(), s.name.clone()));
            self.start_err.clone().map_or(Ok(()), Err)
        }
        fn restart_session(&mut self) -> Result<(), String> {
            Err("none".into())
        }
        fn themes(&self) -> Vec<String> {
            vec!["Nord".into(), "Adwaita".into()]
        }
        fn set_theme(&mut self, n: &str) -> Result<(), String> {
            self.theme = Some(n.into());
            Ok(())
        }
    }

    fn ctl() -> Controller<Mock> {
        Controller::new(Mock {
            pw_ok: true,
            tf_ok: true,
            sessions: vec![sess("Hyprland"), sess("Sway")],
            ..Default::default()
        })
    }

    #[test]
    fn happy_path_authenticates_the_typed_user() {
        let mut c = ctl();
        assert_eq!(c.login("  zaman ", "pw", ""), Outcome::Started("Hyprland".into()));
        assert_eq!(c.backend.auth_calls, [("zaman".to_string(), "pw".to_string())]);
        assert_eq!(c.backend.started, [("zaman".to_string(), "Hyprland".to_string())]);
        assert_eq!(c.backend.tf_calls, 0);
    }

    #[test]
    fn missing_fields_never_reach_pam() {
        let mut c = ctl();
        assert_eq!(c.login("  ", "pw", ""), Outcome::Failed("Username missing".into()));
        assert_eq!(c.login("u", "", ""), Outcome::Failed("Password missing".into()));
        assert!(c.backend.auth_calls.is_empty());
    }

    #[test]
    fn wrong_password_does_not_start_or_touch_2fa() {
        let mut c = ctl();
        c.backend.pw_ok = false;
        c.backend.tf_required = true;
        assert_eq!(c.login("u", "bad", "123456"), Outcome::Failed(MSG_LOGIN_FAILED.into()));
        assert_eq!(c.backend.tf_calls, 0, "HOTP counter must not advance on a bad password");
        assert!(c.backend.started.is_empty());
    }

    #[test]
    fn two_factor_required_missing_and_wrong() {
        let mut c = ctl();
        c.backend.tf_required = true;
        assert_eq!(c.login("u", "pw", " "), Outcome::Failed("2FA code required".into()));
        assert!(c.backend.auth_calls.is_empty());
        c.backend.tf_ok = false;
        // same text as a bad password
        assert_eq!(c.login("u", "pw", "000000"), Outcome::Failed(MSG_LOGIN_FAILED.into()));
        assert!(c.backend.started.is_empty());
        c.backend.tf_ok = true;
        assert_eq!(c.login("u", "pw", "123456"), Outcome::Started("Hyprland".into()));
    }

    #[test]
    fn broken_setup_blocks_login() {
        let mut c = ctl();
        c.backend.problem = Some("two_factor_secret is missing".into());
        assert_eq!(c.login("u", "pw", ""), Outcome::Failed("two_factor_secret is missing".into()));
        assert!(c.backend.auth_calls.is_empty());
    }

    #[test]
    fn session_selection() {
        let mut c = ctl();
        assert!(c.select_session_named("sway"));
        assert!(!c.select_session_named("nope"));
        assert!(!c.select_session(9));
        assert_eq!(c.login("u", "pw", ""), Outcome::Started("Sway".into()));
        assert!(c.select_session(0));
        assert_eq!(c.selected_session(), 0);
    }

    #[test]
    fn no_sessions_and_start_failure() {
        let mut c = ctl();
        c.backend.start_err = Some("boom".into());
        assert_eq!(c.login("u", "pw", ""), Outcome::Failed("Could not start session: boom".into()));
        c.backend.sessions.clear();
        assert_eq!(c.login("u", "pw", ""), Outcome::Failed("No sessions installed".into()));
    }

    #[test]
    fn reveal_toggles_between_password_and_text() {
        let mut r = Reveal::default();
        assert_eq!((r.input_type(), r.label(), r.is_visible()), ("password", "Show", false));
        r.toggle();
        assert_eq!((r.input_type(), r.label(), r.is_visible()), ("text", "Hide", true));
        r.toggle();
        assert_eq!(r.input_type(), "password");
    }

    #[test]
    fn themes() {
        let mut c = ctl();
        assert_eq!(c.apply_theme(1), Ok("Adwaita".into()));
        assert_eq!(c.backend.theme.as_deref(), Some("Adwaita"));
        assert!(c.apply_theme(5).is_err());
    }
}
