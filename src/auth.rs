use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub const COOKIE_NAME: &str = "dot_auth";
pub const COOKIE_TTL_SECS: u64 = 60 * 60 * 24 * 30; // 30 days

pub struct AuthThrottle {
    window: Duration,
    max_failures: usize,
    failures: Mutex<VecDeque<Instant>>,
}

impl AuthThrottle {
    pub fn new(window: Duration, max_failures: usize) -> Self {
        Self {
            window,
            max_failures,
            failures: Mutex::new(VecDeque::new()),
        }
    }

    pub fn allow(&self) -> bool {
        let now = Instant::now();
        let mut guard = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        while guard
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            guard.pop_front();
        }
        guard.len() < self.max_failures
    }

    pub fn record_failure(&self) {
        let now = Instant::now();
        let mut guard = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        while guard
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            guard.pop_front();
        }
        if guard.len() < self.max_failures.saturating_mul(2) {
            guard.push_back(now);
        }
    }
}

#[derive(Clone)]
pub struct Auth {
    access_key: String,
    cookie_secret: [u8; 32],
}

impl Auth {
    pub fn new(access_key: String, custom_secret: Option<String>) -> Self {
        let mut hasher = Sha256::new();
        match custom_secret {
            Some(s) if !s.is_empty() => hasher.update(s.as_bytes()),
            _ => {
                hasher.update(b"dot-proxy-session:");
                hasher.update(access_key.as_bytes());
            }
        }
        let cookie_secret: [u8; 32] = hasher.finalize().into();
        Self {
            access_key,
            cookie_secret,
        }
    }

    pub fn verify_key(&self, candidate: &str) -> bool {
        let a = candidate.as_bytes();
        let b = self.access_key.as_bytes();
        a.len() == b.len() && bool::from(a.ct_eq(b))
    }

    pub fn issue_cookie_header(&self) -> String {
        let exp = now_secs().saturating_add(COOKIE_TTL_SECS);
        let token = self.sign_exp(exp);
        format!(
            "{COOKIE_NAME}={token}; Path=/; Max-Age={COOKIE_TTL_SECS}; HttpOnly; SameSite=Lax; Secure"
        )
    }

    pub fn clear_cookie_header() -> &'static str {
        "dot_auth=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
    }

    pub fn verify_cookie_header(&self, cookie_header: Option<&str>) -> bool {
        let Some(raw) = cookie_header else {
            return false;
        };
        for part in raw.split(';') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            if k.trim() == COOKIE_NAME {
                return self.verify_token(v.trim());
            }
        }
        false
    }

    fn sign_exp(&self, exp: u64) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.cookie_secret).expect("HMAC accepts any key size");
        mac.update(exp.to_string().as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        format!("{exp}.{sig}")
    }

    fn verify_token(&self, token: &str) -> bool {
        let Some((exp_str, _sig)) = token.split_once('.') else {
            return false;
        };
        let Ok(exp) = exp_str.parse::<u64>() else {
            return false;
        };
        if exp < now_secs() {
            return false;
        }
        let expected = self.sign_exp(exp);
        let a = token.as_bytes();
        let b = expected.as_bytes();
        a.len() == b.len() && bool::from(a.ct_eq(b))
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_throttle_blocks_after_limit() {
        let throttle = AuthThrottle::new(Duration::from_secs(60), 3);
        assert!(throttle.allow());
        throttle.record_failure();
        throttle.record_failure();
        assert!(throttle.allow());
        throttle.record_failure();
        assert!(!throttle.allow());
    }
}
