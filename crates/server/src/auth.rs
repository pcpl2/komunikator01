//! Password hashing and brute-force protection.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;

/// Argon2id with the crate's default parameters, PHC string format.
pub fn hash_password(pw: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .expect("argon2 hashing with default params cannot fail")
        .to_string()
}

pub fn verify_password(hash: &str, pw: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

/// Blocks an IP after `max_failures` failed logins within `window`.
pub struct LoginLimiter {
    max_failures: u32,
    window: Duration,
    failures: HashMap<IpAddr, Vec<Instant>>,
}

impl LoginLimiter {
    pub fn new(max_failures: u32, window: Duration) -> Self {
        Self { max_failures, window, failures: HashMap::new() }
    }

    pub fn is_blocked(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.prune(ip, now);
        self.failures.get(&ip).is_some_and(|f| f.len() as u32 >= self.max_failures)
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        // Forget every expired IP, not just this one, so scanners cannot grow the map.
        // Argon2 bounds attempts to a few dozen per second, so a full pass is cheap.
        let window = self.window;
        self.failures.retain(|_, f| {
            f.retain(|t| now.duration_since(*t) < window);
            !f.is_empty()
        });
        self.failures.entry(ip).or_default().push(now);
    }

    /// Withdraws the most recent failure, used once a pre-counted attempt succeeds.
    pub fn forgive(&mut self, ip: IpAddr) {
        if let Some(f) = self.failures.get_mut(&ip) {
            f.pop();
            if f.is_empty() {
                self.failures.remove(&ip);
            }
        }
    }

    /// IPs currently remembered.
    pub fn tracked_ips(&self) -> usize {
        self.failures.len()
    }

    fn prune(&mut self, ip: IpAddr, now: Instant) {
        let window = self.window;
        if let Some(f) = self.failures.get_mut(&ip) {
            f.retain(|t| now.duration_since(*t) < window);
            if f.is_empty() {
                self.failures.remove(&ip);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn verify_correct_and_wrong() {
        let h = hash_password("tajne");
        assert!(verify_password(&h, "tajne"));
        assert!(!verify_password(&h, "inne"));
    }

    #[test]
    fn verify_garbage_hash_is_false() {
        assert!(!verify_password("nie-hash", "x"));
    }

    #[test]
    fn limiter_blocks_after_five_and_expires() {
        let mut l = LoginLimiter::new(5, Duration::from_secs(60));
        let ip = "1.2.3.4".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..4 {
            l.record_failure(ip, t0);
        }
        assert!(!l.is_blocked(ip, t0));
        l.record_failure(ip, t0);
        assert!(l.is_blocked(ip, t0 + Duration::from_secs(1)));
        assert!(!l.is_blocked(ip, t0 + Duration::from_secs(61)));
    }

    #[test]
    fn limiter_forgets_expired_ips() {
        let mut l = LoginLimiter::new(5, Duration::from_secs(60));
        let t0 = Instant::now();
        for i in 0..100u8 {
            l.record_failure(IpAddr::from([10, 0, 0, i]), t0);
        }
        l.record_failure("1.2.3.4".parse().unwrap(), t0 + Duration::from_secs(61));
        assert_eq!(l.tracked_ips(), 1);
    }

    #[test]
    fn limiter_is_per_ip() {
        let mut l = LoginLimiter::new(5, Duration::from_secs(60));
        let t0 = Instant::now();
        for _ in 0..5 {
            l.record_failure("1.2.3.4".parse().unwrap(), t0);
        }
        assert!(!l.is_blocked("5.6.7.8".parse().unwrap(), t0));
    }
}
