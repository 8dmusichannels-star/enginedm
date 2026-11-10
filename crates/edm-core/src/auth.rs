//! PAM password check and the `otpauth`-backed 2FA code generator.

use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use otpauth::{HOTP, TOTP};
use pam_client::{Context, ConversationHandler, ErrorCode, Flag}; // VERIFY: ConversationHandler / ErrorCode paths in pam-client 0.5

use zeroize::{Zeroize, Zeroizing};

use crate::account::{normalize_pam_service, valid_username};
use crate::desktop::{pam_session_vars, SessionEntry};
use crate::launch::{spawn_session, UserInfo};
use crate::otp::{CodeGenerator, TwoFactor};
use crate::supervise::Supervisor;

/// Answers PAM's prompts with the typed credentials. Replaces `conv_mock::Conversation` because
/// that one keeps the password inside the `Context` where it cannot be wiped (`conversation_mut()`
/// was an unverified guess). The password lives in a cell shared with [`PamAuth::authenticate`],
/// which zeroizes it as soon as authentication is over.
struct PamConv {
    username: String,
    password: Arc<Mutex<Zeroizing<String>>>,
}

impl ConversationHandler for PamConv {
    // VERIFY: trait method set/signatures of pam_client::ConversationHandler (0.5): init has a
    // default, text_info/error_msg are required, radio/binary prompts have defaults.
    fn prompt_echo_on(&mut self, _msg: &CStr) -> Result<CString, ErrorCode> {
        CString::new(self.username.as_bytes()).map_err(|_| ErrorCode::CONV_ERR)
    }

    fn prompt_echo_off(&mut self, _msg: &CStr) -> Result<CString, ErrorCode> {
        let pw = self.password.lock().map_err(|_| ErrorCode::CONV_ERR)?;
        // A wiped (empty) password is never handed out again: fail instead of sending "".
        if pw.is_empty() {
            return Err(ErrorCode::CONV_ERR);
        }
        CString::new(pw.as_bytes()).map_err(|_| ErrorCode::CONV_ERR)
    }

    fn text_info(&mut self, _msg: &CStr) {}

    fn error_msg(&mut self, msg: &CStr) {
        eprintln!("edm: pam: {}", msg.to_string_lossy());
    }
}

/// `Context` holds raw libpam pointers, which makes it `!Send` by default. The context is moved to
/// the session thread as a whole and never used from two threads at once, which libpam allows.
struct SendCtx(Context<PamConv>);

// SAFETY: see above; ownership moves, there is no sharing, and `PamConv` itself is `Send`.
unsafe impl Send for SendCtx {}

impl SendCtx {
    /// A method (not `.0`) so a `move` closure captures the whole wrapper, not the `!Send` field.
    fn into_inner(self) -> Context<PamConv> {
        self.0
    }
}

pub struct PamAuth {
    service: String,
}

impl PamAuth {
    pub fn new(service: &str) -> Self {
        Self { service: normalize_pam_service(service) }
    }

    /// Authenticates the user that was actually typed in (the old greeter always authenticated
    /// the account of the greeter process) and also runs the PAM account check
    /// (expired/locked accounts). Errors are for logging only; never show them to the user.
    ///
    /// On success the PAM transaction is kept alive in the returned [`PamLogin`] so the session
    /// can be opened on the same transaction later. Dropping it ends the transaction.
    pub fn authenticate(&self, username: &str, password: &str) -> Result<PamLogin, String> {
        if !valid_username(username) {
            return Err("invalid username".into());
        }
        let password = Arc::new(Mutex::new(Zeroizing::new(password.to_string())));
        let conv = PamConv { username: username.to_string(), password: password.clone() };
        // Init failure used to `panic!`; a login screen must survive it.
        let mut ctx = match Context::new(&self.service, Some(username), conv) {
            Ok(c) => c,
            Err(e) => {
                wipe(&password);
                return Err(format!("PAM init failed: {e}"));
            }
        };
        let result = ctx
            .authenticate(Flag::NONE)
            .map_err(|e| format!("PAM authenticate failed: {e}"))
            .and_then(|_| ctx.acct_mgmt(Flag::NONE).map_err(|e| format!("PAM account check failed: {e}")));
        // Success or not, the typed password is not needed any more. (Copies that PAM modules
        // made for themselves are out of reach.)
        wipe(&password);
        result?;
        Ok(PamLogin { username: username.to_string(), ctx: SendCtx(ctx) })
    }
}

impl PamAuth {
    /// PAM transaction for a user who was already authenticated by the greeter (standalone
    /// handover): no password is available, so any prompt fails. Only the account check runs
    /// here; `PamLogin::host_session` then opens the session. Must run as root.
    pub fn open_for(&self, username: &str) -> Result<PamLogin, String> {
        if !valid_username(username) {
            return Err("invalid username".into());
        }
        let password = Arc::new(Mutex::new(Zeroizing::new(String::new())));
        let conv = PamConv { username: username.to_string(), password };
        let mut ctx = Context::new(&self.service, Some(username), conv).map_err(|e| format!("PAM init failed: {e}"))?;
        ctx.acct_mgmt(Flag::NONE).map_err(|e| format!("PAM account check failed: {e}"))?;
        Ok(PamLogin { username: username.to_string(), ctx: SendCtx(ctx) })
    }
}

fn wipe(password: &Arc<Mutex<Zeroizing<String>>>) {
    // A poisoned lock still holds the string; wipe it anyway.
    password.lock().unwrap_or_else(|p| p.into_inner()).zeroize();
}

/// A successful PAM authentication, ready to become a PAM session.
pub struct PamLogin {
    username: String,
    ctx: SendCtx,
}

impl PamLogin {
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Opens the PAM session (`pam_setcred` + `pam_open_session`, i.e. `pam_systemd` creates the
    /// logind session and `/run/user/<uid>`), starts `entry` inside it and keeps both alive on a
    /// supervisor thread. When the session process ends, or the returned [`Supervisor`] is
    /// stopped/dropped, the PAM session is closed.
    pub fn host_session(self, entry: SessionEntry, user: UserInfo) -> Result<Supervisor, String> {
        let PamLogin { ctx, .. } = self;
        Supervisor::start(move |ctl| {
            let mut ctx = ctx.into_inner();
            let seat = std::env::var("XDG_SEAT").ok();
            let vtnr = std::env::var("XDG_VTNR").ok();
            for kv in pam_session_vars(&entry, seat.as_deref(), vtnr.as_deref()) {
                if let Err(e) = ctx.putenv(kv.as_str()) {
                    eprintln!("edm: pam putenv '{kv}' failed: {e}");
                }
            }
            if let Some(vt) = vtnr.as_deref().filter(|v| !v.is_empty()) {
                // pam_systemd falls back to the tty to find the VT.
                let _ = ctx.set_tty(Some(&format!("tty{vt}")));
            }
            let session = match ctx.open_session(Flag::NONE) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("edm: PAM open_session failed: {e}");
                    return ctl.fail("could not open the PAM session".into());
                }
            };
            let extra: Vec<(String, String)> = session
                .envlist()
                .iter_tuples()
                .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
                .collect();
            ctl.run(|| spawn_session(&entry, &user, &extra));
            // `session` drops here: pam_close_session + credential deletion.
            drop(session);
        })
    }
}

pub struct OtpAuthGenerator;

impl CodeGenerator for OtpAuthGenerator {
    fn totp(&self, secret: &str, period: u64, unix_ts: u64) -> u32 {
        TOTP::new(secret).generate(period, unix_ts)
    }
    fn hotp(&self, secret: &str, counter: u64) -> u32 {
        HOTP::new(secret).generate(counter)
    }
}

impl TwoFactor {
    /// Verify against the real clock with the `otpauth` generator.
    pub fn verify(&mut self, code: &str) -> bool {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        self.verify_with(&OtpAuthGenerator, code, now)
    }
}
