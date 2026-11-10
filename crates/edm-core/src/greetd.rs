//! Client for the greetd IPC protocol (`GREETD_SOCK`), the way out of the display-handover
//! problem: greetd owns PAM, the logind session, the VT and the seat. The greeter only talks to
//! it, then exits, and greetd starts the user's session once the greeter is gone.
//!
//! Wire format: every message is a 4-byte **native-endian** length followed by that many bytes of
//! JSON. Requests carry `"type"`: `create_session`, `post_auth_message_response`,
//! `start_session`, `cancel_session`. Replies are `success`, `error` or `auth_message`.
//!
//! The framing and conversation logic are generic over `Read + Write`, so the tests below talk to
//! a scripted in-process peer instead of a real greetd.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use serde_json::{json, Value};

/// A greetd reply is tiny; refuse absurd lengths instead of allocating them.
const MAX_FRAME: usize = 1 << 20;
/// A PAM stack asking more than this many questions is not a login, it is a loop.
const MAX_ROUNDS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Visible,
    Secret,
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Success,
    /// `kind` is greetd's `error_type`: `auth_error` (wrong credentials) or `error`.
    Error { kind: String, description: String },
    Auth { kind: AuthKind, message: String },
}

pub fn parse_reply(v: &Value) -> Result<Reply, String> {
    let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    match v.get("type").and_then(Value::as_str) {
        Some("success") => Ok(Reply::Success),
        Some("error") => Ok(Reply::Error { kind: text("error_type"), description: text("description") }),
        Some("auth_message") => {
            let kind = match v.get("auth_message_type").and_then(Value::as_str) {
                Some("visible") => AuthKind::Visible,
                Some("secret") => AuthKind::Secret,
                Some("info") => AuthKind::Info,
                Some("error") => AuthKind::Error,
                other => return Err(format!("unknown auth_message_type {other:?}")),
            };
            Ok(Reply::Auth { kind, message: text("auth_message") })
        }
        other => Err(format!("unknown greetd reply type {other:?}")),
    }
}

pub struct GreetdClient<S: Read + Write> {
    stream: S,
    /// A session was created and has been neither started nor cancelled.
    open: bool,
}

impl GreetdClient<UnixStream> {
    /// Connects to `$GREETD_SOCK` (set by greetd for the greeter it starts).
    pub fn connect_env() -> Result<Self, String> {
        let path = std::env::var_os("GREETD_SOCK").filter(|p| !p.is_empty()).ok_or("GREETD_SOCK is not set")?;
        let stream = UnixStream::connect(&path).map_err(|e| format!("cannot connect to greetd: {e}"))?;
        Ok(Self::new(stream))
    }
}

/// True when this process was started by greetd.
pub fn available() -> bool {
    std::env::var_os("GREETD_SOCK").is_some_and(|p| !p.is_empty())
}

impl<S: Read + Write> GreetdClient<S> {
    pub fn new(stream: S) -> Self {
        Self { stream, open: false }
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
        let len = u32::try_from(body.len()).map_err(|_| "request too large".to_string())?;
        let io = |e: std::io::Error| format!("greetd write failed: {e}");
        self.stream.write_all(&len.to_ne_bytes()).map_err(io)?;
        self.stream.write_all(&body).map_err(io)?;
        self.stream.flush().map_err(io)
    }

    fn recv(&mut self) -> Result<Reply, String> {
        let io = |e: std::io::Error| format!("greetd read failed: {e}");
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).map_err(io)?;
        let len = u32::from_ne_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err(format!("greetd reply of {len} bytes refused"));
        }
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body).map_err(io)?;
        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("bad greetd reply: {e}"))?;
        parse_reply(&v)
    }

    /// Creates a session for `username` and answers greetd's questions: secret prompts get the
    /// password, visible prompts the username, info/error messages are acknowledged. Returns
    /// `Ok(())` when greetd accepted the credentials; the session then waits for
    /// [`start_session`](Self::start_session). On any failure the session is cancelled.
    /// Errors are for logging only; never show them to the user.
    pub fn authenticate(&mut self, username: &str, password: &str) -> Result<(), String> {
        self.send(&json!({"type": "create_session", "username": username}))?;
        self.open = true;
        for _ in 0..MAX_ROUNDS {
            match self.recv() {
                Ok(Reply::Success) => return Ok(()),
                Ok(Reply::Error { kind, description }) => {
                    self.cancel();
                    return Err(format!("greetd {kind}: {description}"));
                }
                Ok(Reply::Auth { kind, message }) => {
                    let response = match kind {
                        AuthKind::Secret => Some(password),
                        AuthKind::Visible => Some(username),
                        AuthKind::Info | AuthKind::Error => {
                            eprintln!("edm: greetd: {message}");
                            None
                        }
                    };
                    if let Err(e) = self.send(&json!({"type": "post_auth_message_response", "response": response})) {
                        self.open = false; // the connection is gone; nothing to cancel
                        return Err(e);
                    }
                }
                Err(e) => {
                    self.open = false;
                    return Err(e);
                }
            }
        }
        self.cancel();
        Err("greetd asked too many questions".into())
    }

    /// Starts the authenticated session. `cmd` is the program plus arguments, `env` are
    /// `KEY=value` strings. After `Ok`, the greeter must exit so greetd can start the session.
    pub fn start_session(&mut self, cmd: &[String], env: &[String]) -> Result<(), String> {
        if !self.open {
            return Err("no authenticated greetd session".into());
        }
        self.send(&json!({"type": "start_session", "cmd": cmd, "env": env}))?;
        match self.recv()? {
            Reply::Success => {
                self.open = false;
                Ok(())
            }
            Reply::Error { kind, description } => {
                self.cancel();
                Err(format!("greetd {kind}: {description}"))
            }
            Reply::Auth { .. } => {
                self.cancel();
                Err("unexpected auth message after start_session".into())
            }
        }
    }

    /// Best-effort `cancel_session`; safe to call when nothing is open.
    pub fn cancel(&mut self) {
        if std::mem::take(&mut self.open)
            && self.send(&json!({"type": "cancel_session"})).is_ok()
        {
            let _ = self.recv();
        }
    }
}

impl<S: Read + Write> Drop for GreetdClient<S> {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;

    /// Scripted peer: replies are queued up front, everything written is recorded.
    struct Script {
        input: VecDeque<u8>,
        written: Vec<u8>,
    }

    fn frame(v: Value) -> Vec<u8> {
        let body = serde_json::to_vec(&v).unwrap();
        let mut out = (body.len() as u32).to_ne_bytes().to_vec();
        out.extend(body);
        out
    }

    fn script(replies: Vec<Value>) -> Script {
        Script { input: replies.into_iter().flat_map(frame).collect(), written: Vec::new() }
    }

    impl Read for Script {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = buf.len().min(self.input.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.input.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for Script {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Decode every request the client wrote.
    fn requests(w: &[u8]) -> Vec<Value> {
        let (mut out, mut i) = (Vec::new(), 0);
        while i + 4 <= w.len() {
            let len = u32::from_ne_bytes(w[i..i + 4].try_into().unwrap()) as usize;
            out.push(serde_json::from_slice(&w[i + 4..i + 4 + len]).unwrap());
            i += 4 + len;
        }
        out
    }

    fn kinds(w: &[u8]) -> Vec<String> {
        requests(w).iter().map(|r| r["type"].as_str().unwrap().to_string()).collect()
    }

    fn secret() -> Value {
        json!({"type": "auth_message", "auth_message_type": "secret", "auth_message": "Password: "})
    }

    #[test]
    fn password_login_and_start() {
        let mut c = GreetdClient::new(script(vec![secret(), json!({"type": "success"}), json!({"type": "success"})]));
        c.authenticate("alice", "hunter2").unwrap();
        c.start_session(&["Hyprland".into()], &["XDG_SESSION_TYPE=wayland".into()]).unwrap();
        let reqs = requests(&c.stream.written);
        assert_eq!(reqs[0], json!({"type": "create_session", "username": "alice"}));
        assert_eq!(reqs[1], json!({"type": "post_auth_message_response", "response": "hunter2"}));
        assert_eq!(reqs[2]["type"], "start_session");
        assert_eq!(reqs[2]["cmd"], json!(["Hyprland"]));
        assert_eq!(reqs[2]["env"], json!(["XDG_SESSION_TYPE=wayland"]));
    }

    #[test]
    fn started_session_is_not_cancelled_afterwards() {
        let mut c = GreetdClient::new(script(vec![json!({"type": "success"}), json!({"type": "success"})]));
        c.authenticate("alice", "x").unwrap();
        c.start_session(&["sh".into()], &[]).unwrap();
        assert_eq!(kinds(&c.stream.written), ["create_session", "start_session"]);
        c.cancel();
        assert_eq!(kinds(&c.stream.written), ["create_session", "start_session"]);
    }

    #[test]
    fn info_messages_are_acknowledged_without_the_password() {
        let info = json!({"type": "auth_message", "auth_message_type": "info", "auth_message": "hello"});
        let mut c = GreetdClient::new(script(vec![info, secret(), json!({"type": "success"})]));
        c.authenticate("alice", "pw").unwrap();
        let reqs = requests(&c.stream.written);
        assert_eq!(reqs[1], json!({"type": "post_auth_message_response", "response": null}));
        assert_eq!(reqs[2]["response"], "pw");
    }

    #[test]
    fn visible_prompt_gets_the_username() {
        let vis = json!({"type": "auth_message", "auth_message_type": "visible", "auth_message": "login: "});
        let mut c = GreetdClient::new(script(vec![vis, json!({"type": "success"})]));
        c.authenticate("alice", "pw").unwrap();
        assert_eq!(requests(&c.stream.written)[1]["response"], "alice");
    }

    #[test]
    fn wrong_password_cancels_the_session() {
        let err = json!({"type": "error", "error_type": "auth_error", "description": "Authentication failed"});
        let mut c = GreetdClient::new(script(vec![secret(), err, json!({"type": "success"})]));
        let e = c.authenticate("alice", "bad").unwrap_err();
        assert!(e.contains("auth_error"), "{e}");
        assert_eq!(kinds(&c.stream.written), ["create_session", "post_auth_message_response", "cancel_session"]);
        assert!(c.start_session(&["sh".into()], &[]).is_err());
    }

    #[test]
    fn start_without_authentication_is_refused() {
        let mut c = GreetdClient::new(script(vec![]));
        assert!(c.start_session(&["sh".into()], &[]).is_err());
        assert!(c.stream.written.is_empty());
    }

    #[test]
    fn cancel_after_authentication_sends_cancel_session() {
        let mut c = GreetdClient::new(script(vec![json!({"type": "success"}), json!({"type": "success"})]));
        c.authenticate("alice", "pw").unwrap();
        c.cancel();
        assert_eq!(kinds(&c.stream.written), ["create_session", "cancel_session"]);
    }

    #[test]
    fn endless_questions_are_cut_off() {
        let mut c = GreetdClient::new(script((0..20).map(|_| secret()).collect()));
        assert!(c.authenticate("alice", "pw").unwrap_err().contains("too many"));
    }

    #[test]
    fn oversized_and_unknown_replies_are_errors() {
        let huge = Script { input: u32::MAX.to_ne_bytes().to_vec().into(), written: Vec::new() };
        assert!(GreetdClient::new(huge).recv().is_err());
        assert!(parse_reply(&json!({"type": "weird"})).is_err());
        assert!(parse_reply(&json!({"type": "auth_message", "auth_message_type": "nope"})).is_err());
    }

    #[test]
    fn closed_connection_is_an_error() {
        let mut c = GreetdClient::new(script(vec![]));
        assert!(c.authenticate("alice", "pw").is_err());
    }
}
