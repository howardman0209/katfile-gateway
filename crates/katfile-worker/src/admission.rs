//! Staging-space admission for new uploads (SFTPGo `pre-upload` hook).
//!
//! SFTPGo does not tell the hook how large a new upload will be, so each admitted
//! upload reserves either its declared size (when known) or a configurable default
//! until its completion event arrives. An upload is admitted only if
//! `free - reserved >= min_free + reservation`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

/// Reservations expire even if the completion event is lost (slow 10 GB uploads included).
const RESERVATION_TTL: Duration = Duration::from_secs(12 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Decision {
    Admit,
    Reject { free: u64, reserved: u64, needed: u64 },
}

#[derive(Debug)]
struct Reservation {
    bytes: u64,
    expires_at_ms: i64,
}

#[derive(Debug)]
pub struct Admission {
    min_free_bytes: u64,
    default_reservation: u64,
    inner: Mutex<HashMap<(String, String), Reservation>>,
}

impl Admission {
    pub fn new(min_free_bytes: u64, default_reservation: u64) -> Admission {
        Admission { min_free_bytes, default_reservation, inner: Mutex::new(HashMap::new()) }
    }

    /// Decide for one upload and reserve space when admitted.
    pub fn decide(&self, free: u64, username: &str, vpath: &str, declared: Option<u64>, now_ms: i64) -> Decision {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, r| r.expires_at_ms > now_ms);
        let key = (username.to_owned(), vpath.to_owned());
        // A retried pre-upload for the same path replaces its own reservation.
        map.remove(&key);
        let reserved: u64 = map.values().map(|r| r.bytes).sum();
        let bytes = declared.unwrap_or(self.default_reservation);
        let needed = self.min_free_bytes.saturating_add(bytes);
        if free.saturating_sub(reserved) < needed {
            return Decision::Reject { free, reserved, needed };
        }
        map.insert(key, Reservation { bytes, expires_at_ms: now_ms + RESERVATION_TTL.as_millis() as i64 });
        Decision::Admit
    }

    /// Release the reservation when the upload finished (successfully or not).
    pub fn release(&self, username: &str, vpath: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&(username.to_owned(), vpath.to_owned()));
    }

    pub fn reserved_bytes(&self, now_ms: i64) -> (usize, u64) {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let live: Vec<&Reservation> = map.values().filter(|r| r.expires_at_ms > now_ms).collect();
        (live.len(), live.iter().map(|r| r.bytes).sum())
    }

    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn admits_until_reserve_would_be_crossed() {
        let a = Admission::new(4 * GIB, GIB);
        assert_eq!(a.decide(6 * GIB, "alice", "/a", None, 0), Decision::Admit);
        // 6 GiB free, 1 GiB reserved: another 1 GiB upload would leave < 4 GiB.
        assert!(matches!(a.decide(6 * GIB, "bob", "/b", None, 0), Decision::Admit));
        assert!(matches!(a.decide(6 * GIB, "carol", "/c", None, 0), Decision::Reject { .. }));
        a.release("alice", "/a");
        assert_eq!(a.decide(6 * GIB, "carol", "/c", None, 0), Decision::Admit);
    }

    #[test]
    fn declared_size_is_used_for_large_uploads() {
        let a = Admission::new(4 * GIB, GIB);
        // A declared 10 GiB upload needs 14 GiB free.
        assert!(matches!(a.decide(13 * GIB, "alice", "/v.mp4", Some(10 * GIB), 0), Decision::Reject { .. }));
        assert_eq!(a.decide(15 * GIB, "alice", "/v.mp4", Some(10 * GIB), 0), Decision::Admit);
    }

    #[test]
    fn reservations_expire() {
        let a = Admission::new(0, 10);
        assert_eq!(a.decide(10, "u", "/x", None, 0), Decision::Admit);
        assert!(matches!(a.decide(10, "v", "/y", None, 1), Decision::Reject { .. }));
        let later = RESERVATION_TTL.as_millis() as i64 + 1;
        assert_eq!(a.decide(10, "v", "/y", None, later), Decision::Admit);
    }
}
