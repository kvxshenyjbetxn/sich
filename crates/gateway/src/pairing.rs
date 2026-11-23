//! One-shot pairing secrets.

use std::time::{Duration, Instant};

use base64::Engine as _;

pub const SECRET_TTL: Duration = Duration::from_secs(10 * 60);
pub const MAX_FAILURES: u32 = 5;
/// How long the desktop user has to confirm a pair request.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum SecretCheck {
    /// Valid; the secret is consumed.
    Valid,
    Invalid,
}

struct Active {
    secret: String,
    expires: Instant,
    failures: u32,
}

#[derive(Default)]
pub struct Pairing {
    active: Option<Active>,
}

/// 32 random bytes, base64url without padding.
pub fn generate_secret() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

impl Pairing {
    /// Replaces any active secret. Returns when the new one expires.
    pub fn start(&mut self, secret: String, now: Instant) -> Instant {
        let expires = now + SECRET_TTL;
        self.active = Some(Active { secret, expires, failures: 0 });
        expires
    }

    pub fn cancel(&mut self) {
        self.active = None;
    }

    pub fn is_active(&self, now: Instant) -> bool {
        self.active.as_ref().is_some_and(|a| now < a.expires)
    }

    pub fn check(&mut self, given: &str, now: Instant) -> SecretCheck {
        let Some(active) = &mut self.active else { return SecretCheck::Invalid };
        if now >= active.expires {
            self.active = None;
            return SecretCheck::Invalid;
        }
        if ct_eq(&active.secret, given) {
            self.active = None;
            return SecretCheck::Valid;
        }
        active.failures += 1;
        if active.failures >= MAX_FAILURES {
            self.active = None;
        }
        SecretCheck::Invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_has_32_bytes_of_url_safe_base64() {
        let s = generate_secret();
        assert_eq!(s.len(), 43);
        assert!(s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_ne!(s, generate_secret());
    }

    #[test]
    fn valid_secret_is_one_shot() {
        let now = Instant::now();
        let mut p = Pairing::default();
        p.start("abc".into(), now);
        assert_eq!(p.check("abc", now), SecretCheck::Valid);
        assert_eq!(p.check("abc", now), SecretCheck::Invalid);
        assert!(!p.is_active(now));
    }

    #[test]
    fn expires_after_ttl() {
        let now = Instant::now();
        let mut p = Pairing::default();
        let expires = p.start("abc".into(), now);
        assert_eq!(expires, now + SECRET_TTL);
        assert!(p.is_active(now + SECRET_TTL - Duration::from_secs(1)));
        assert_eq!(p.check("abc", now + SECRET_TTL), SecretCheck::Invalid);
        assert_eq!(p.check("abc", now), SecretCheck::Invalid, "expired secrets stay dead");
    }

    #[test]
    fn five_failures_invalidate_the_secret() {
        let now = Instant::now();
        let mut p = Pairing::default();
        p.start("abc".into(), now);
        for _ in 0..MAX_FAILURES - 1 {
            assert_eq!(p.check("bad", now), SecretCheck::Invalid);
        }
        assert!(p.is_active(now));
        assert_eq!(p.check("bad", now), SecretCheck::Invalid);
        assert!(!p.is_active(now));
        assert_eq!(p.check("abc", now), SecretCheck::Invalid);
    }

    #[test]
    fn no_active_secret_and_cancel_reject_everything() {
        let now = Instant::now();
        let mut p = Pairing::default();
        assert_eq!(p.check("", now), SecretCheck::Invalid);
        p.start("abc".into(), now);
        p.cancel();
        assert_eq!(p.check("abc", now), SecretCheck::Invalid);
    }

    #[test]
    fn restart_resets_failures_and_replaces_secret() {
        let now = Instant::now();
        let mut p = Pairing::default();
        p.start("one".into(), now);
        for _ in 0..4 {
            p.check("bad", now);
        }
        p.start("two".into(), now);
        assert_eq!(p.check("one", now), SecretCheck::Invalid);
        assert_eq!(p.check("two", now), SecretCheck::Valid);
    }

    #[test]
    fn comparison_handles_different_lengths() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", "a"));
        assert!(ct_eq("", ""));
    }
}
