//! Two-factor (TOTP/HOTP) verification logic. std only: code generation is injected through
//! [`CodeGenerator`] (`auth.rs` plugs in the `otpauth` crate), so this is fully unit-testable.

pub const DIGITS: usize = 6;
pub const TOTP_PERIOD: u64 = 30;
/// Accept codes from this many 30s steps before/after now (clock drift). The old code accepted
/// the current step only.
pub const TOTP_WINDOW: u64 = 1;
/// HOTP look-ahead (RFC 4226 resynchronisation): codes generated on the token but never used
/// leave the server counter behind. Accept the next few counters and resume after the match.
pub const HOTP_LOOKAHEAD: u64 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TwoFactorMethod {
    None,
    Totp,
    /// `counter` is the next expected counter. The greeter persists it (see `state.rs`) after
    /// every successful verification so a restart cannot make an old code valid again.
    Hotp { counter: u64 },
}

pub trait CodeGenerator {
    fn totp(&self, secret: &str, period: u64, unix_ts: u64) -> u32;
    fn hotp(&self, secret: &str, counter: u64) -> u32;
}

/// Zero-padded 6 digits. The old code compared `code.to_string() == input`, which dropped leading
/// zeros: roughly 1 in 10 valid codes (e.g. `012345`) could never match.
pub fn format_code(code: u32) -> String {
    format!("{:0width$}", code % 1_000_000, width = DIGITS)
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Stable fingerprint of a 2FA secret (FNV-1a 64, domain-separated). It is stored next to the HOTP
/// counter so a changed secret starts again at counter 0 instead of needing a manual file delete.
/// Not a security boundary: the file is 0600 and real secrets have far more than 64 bits.
pub fn fingerprint(secret: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in b"edm-hotp:".iter().chain(secret.as_bytes()) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[derive(Clone, Debug)]
pub struct TwoFactor {
    pub method: TwoFactorMethod,
    secret: Option<String>,
    /// Set when 2FA is enabled but unusable (bad method / no secret). Logins then fail closed.
    problem: Option<String>,
}

impl TwoFactor {
    pub fn disabled() -> Self {
        Self { method: TwoFactorMethod::None, secret: None, problem: None }
    }

    pub fn from_config(enabled: bool, method: Option<&str>, secret: Option<&str>) -> Self {
        if !enabled {
            return Self::disabled();
        }
        let locked = |msg: &str| Self {
            method: TwoFactorMethod::Totp,
            secret: None,
            problem: Some(msg.to_string()),
        };
        let method = match method.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
            Some("totp") => TwoFactorMethod::Totp,
            Some("hotp") => TwoFactorMethod::Hotp { counter: 0 },
            Some(other) => return locked(&format!("unknown two_factor_method '{other}'")),
            None => return locked("two_factor_enabled=true but two_factor_method is missing"),
        };
        match secret.filter(|s| !s.is_empty()) {
            Some(s) => Self { method, secret: Some(s.to_string()), problem: None },
            None => locked("two_factor_enabled=true but two_factor_secret is missing"),
        }
    }

    /// Fingerprint of the configured secret (see [`fingerprint`]); `None` without a secret.
    pub fn secret_fingerprint(&self) -> Option<String> {
        self.secret.as_deref().map(fingerprint)
    }

    /// Next HOTP counter, `None` for TOTP / disabled.
    pub fn counter(&self) -> Option<u64> {
        match self.method {
            TwoFactorMethod::Hotp { counter } => Some(counter),
            _ => None,
        }
    }

    /// Restore a persisted HOTP counter. No effect for other methods.
    pub fn set_counter(&mut self, value: u64) {
        if let TwoFactorMethod::Hotp { counter } = &mut self.method {
            *counter = value;
        }
    }

    /// Make every login fail with `msg` (e.g. the counter file is unreadable).
    pub fn mark_problem(&mut self, msg: &str) {
        self.problem = Some(msg.to_string());
    }

    pub fn required(&self) -> bool {
        self.method != TwoFactorMethod::None
    }

    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    pub fn verify_with<G: CodeGenerator>(&mut self, g: &G, input: &str, now_unix: u64) -> bool {
        if self.problem.is_some() {
            return false;
        }
        if self.method == TwoFactorMethod::None {
            return true;
        }
        let input = input.trim();
        if input.len() != DIGITS || !input.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let Some(secret) = self.secret.clone() else { return false };
        match &mut self.method {
            TwoFactorMethod::None => true,
            TwoFactorMethod::Totp => {
                let step = now_unix / TOTP_PERIOD;
                let mut ok = false;
                // no early exit: every candidate is always compared
                for i in 0..=(2 * TOTP_WINDOW) {
                    let s = step.saturating_add(i).saturating_sub(TOTP_WINDOW);
                    let code = format_code(g.totp(&secret, TOTP_PERIOD, s * TOTP_PERIOD));
                    ok |= ct_eq(code.as_bytes(), input.as_bytes());
                }
                ok
            }
            TwoFactorMethod::Hotp { counter } => {
                let mut hit: Option<u64> = None;
                // no early exit here either
                for i in 0..=HOTP_LOOKAHEAD {
                    let c = counter.saturating_add(i);
                    let code = format_code(g.hotp(&secret, c));
                    if ct_eq(code.as_bytes(), input.as_bytes()) && hit.is_none() {
                        hit = Some(c);
                    }
                }
                match hit {
                    Some(c) => {
                        *counter = c.saturating_add(1);
                        true
                    }
                    None => false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// totp code = step * 7; hotp code = counter * 11 + 5 (small numbers => leading zeros).
    struct Fake;
    impl CodeGenerator for Fake {
        fn totp(&self, _s: &str, period: u64, ts: u64) -> u32 {
            ((ts / period) * 7) as u32
        }
        fn hotp(&self, _s: &str, c: u64) -> u32 {
            (c * 11 + 5) as u32
        }
    }

    fn totp() -> TwoFactor {
        TwoFactor::from_config(true, Some("TOTP"), Some("s3cret"))
    }

    #[test]
    fn fingerprint_is_stable_and_secret_specific() {
        // pinned: changing the algorithm would silently reset every installed counter
        assert_eq!(fingerprint("s3cret"), "4ba509f9963bbe39");
        assert_eq!(fingerprint("other"), "5395869b4a07a963");
        assert_eq!(fingerprint(""), "b977094ff33803d7");
        let t = TwoFactor::from_config(true, Some("hotp"), Some("s3cret"));
        assert_eq!(t.secret_fingerprint().as_deref(), Some("4ba509f9963bbe39"));
        assert_eq!(TwoFactor::disabled().secret_fingerprint(), None);
    }

    #[test]
    fn leading_zeros_are_padded() {
        assert_eq!(format_code(12345), "012345");
        assert_eq!(format_code(7), "000007");
        let mut t = totp();
        // step 1000 -> code 7000 -> "007000"
        assert!(t.verify_with(&Fake, "007000", 1000 * 30));
        assert!(!t.verify_with(&Fake, "7000", 1000 * 30));
    }

    #[test]
    fn totp_window_is_plus_minus_one_step() {
        let mut t = totp();
        let now = 1000 * 30 + 5;
        for step in [999u64, 1000, 1001] {
            assert!(t.verify_with(&Fake, &format_code((step * 7) as u32), now), "step {step}");
        }
        for step in [998u64, 1002] {
            assert!(!t.verify_with(&Fake, &format_code((step * 7) as u32), now), "step {step}");
        }
    }

    #[test]
    fn rejects_malformed_input() {
        let mut t = totp();
        for bad in ["", "12345", "1234567", "12345a", "      ", "abcdef"] {
            assert!(!t.verify_with(&Fake, bad, 30_000), "{bad:?}");
        }
        // surrounding whitespace is fine
        assert!(t.verify_with(&Fake, " 007000 ", 1000 * 30));
    }

    #[test]
    fn hotp_counter_advances_only_on_success() {
        let mut t = TwoFactor::from_config(true, Some("hotp"), Some("s"));
        assert!(!t.verify_with(&Fake, "000099", 0));
        assert_eq!(t.method, TwoFactorMethod::Hotp { counter: 0 });
        assert!(t.verify_with(&Fake, "000005", 0));
        assert_eq!(t.method, TwoFactorMethod::Hotp { counter: 1 });
        assert!(!t.verify_with(&Fake, "000005", 0)); // replay
        assert!(t.verify_with(&Fake, "000016", 0));
    }

    #[test]
    fn hotp_resynchronises_within_the_lookahead() {
        let mut t = TwoFactor::from_config(true, Some("hotp"), Some("s"));
        // token generated counters 0..=3 but only the 4th was typed in
        assert!(t.verify_with(&Fake, &format_code(3 * 11 + 5), 0));
        assert_eq!(t.counter(), Some(4));
        // older codes are dead now
        assert!(!t.verify_with(&Fake, &format_code(2 * 11 + 5), 0));
        // too far ahead is refused and does not move the counter
        let far = 4 + HOTP_LOOKAHEAD + 1;
        assert!(!t.verify_with(&Fake, &format_code((far * 11 + 5) as u32), 0));
        assert_eq!(t.counter(), Some(4));
    }

    #[test]
    fn counter_accessors() {
        let mut h = TwoFactor::from_config(true, Some("hotp"), Some("s"));
        h.set_counter(42);
        assert_eq!(h.counter(), Some(42));
        let mut t = totp();
        t.set_counter(42);
        assert_eq!(t.counter(), None);
        t.mark_problem("boom");
        assert_eq!(t.problem(), Some("boom"));
        assert!(!t.verify_with(&Fake, "007000", 1000 * 30));
    }

    #[test]
    fn disabled_means_not_required_and_passes() {
        let mut t = TwoFactor::from_config(false, Some("totp"), Some("x"));
        assert!(!t.required());
        assert!(t.verify_with(&Fake, "", 0));
    }

    #[test]
    fn misconfigured_2fa_fails_closed() {
        for (m, s) in [(None, Some("x")), (Some("sms"), Some("x")), (Some("totp"), None), (Some("totp"), Some(""))] {
            let mut t = TwoFactor::from_config(true, m, s);
            assert!(t.required());
            assert!(t.problem().is_some());
            assert!(!t.verify_with(&Fake, "000000", 0));
        }
    }

    #[test]
    fn constant_time_eq_basics() {
        assert!(ct_eq(b"123456", b"123456"));
        assert!(!ct_eq(b"123456", b"123457"));
        assert!(!ct_eq(b"12345", b"123456"));
    }
}
