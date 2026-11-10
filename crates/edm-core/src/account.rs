//! Small input checks for the login path. std only.

pub const DEFAULT_PAM_SERVICE: &str = "system-login";

/// Same mapping the old `User::new` applied: empty/"login"/"login_service"/"default" all meant
/// the `system-login` PAM service.
pub fn normalize_pam_service(service: &str) -> String {
    let s = service.trim();
    if s.is_empty() || matches!(s, "login" | "login_service" | "default") {
        DEFAULT_PAM_SERVICE.to_string()
    } else {
        s.to_string()
    }
}

/// The typed username goes to PAM and getpwnam; refuse empty, huge or control-character input.
pub fn valid_username(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && !name.chars().any(|c| c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pam_service_mapping() {
        for s in ["", " ", "login", "login_service", "default"] {
            assert_eq!(normalize_pam_service(s), "system-login");
        }
        assert_eq!(normalize_pam_service("enginedm"), "enginedm");
    }

    #[test]
    fn usernames() {
        assert!(valid_username("zaman"));
        assert!(!valid_username(""));
        assert!(!valid_username("a\0b"));
        assert!(!valid_username("a\nb"));
        assert!(!valid_username(&"x".repeat(257)));
    }
}
