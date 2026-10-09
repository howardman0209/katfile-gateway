//! Staging-space admission for new uploads.
//!
//! * WebDAV through Caddy: `forward_auth` sends the declared `Content-Length` before
//!   any byte is staged; an admitted declaration reserves that size for its path.
//! * SFTPGo `pre-upload` (all protocols, no size information): claims a pending
//!   declaration for the same path, otherwise reserves a configurable default.
//!
//! A reservation lasts until the upload's completion event (or a TTL). An upload is
//! admitted only if `free - reserved >= min_free + reservation`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

/// Reservations expire even if the completion event is lost (slow 10 GB uploads included).
const RESERVATION_TTL: Duration = Duration::from_secs(12 * 3600);
/// A declaration not claimed by SFTPGo's pre-upload within this time is dropped.
const DECLARATION_TTL: Duration = Duration::from_secs(10 * 60);
/// Username key for declarations made at the proxy, before SFTPGo authenticated the user.
const UNCLAIMED: &str = "";

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

    /// Proxy-level check of a declared upload size (WebDAV `PUT` via Caddy).
    pub fn declare(&self, free: u64, vpath: &str, declared: u64, now_ms: i64) -> Decision {
        self.reserve(
            free,
            (UNCLAIMED.to_owned(), vpath.to_owned()),
            declared,
            now_ms + DECLARATION_TTL.as_millis() as i64,
            now_ms,
        )
    }

    /// SFTPGo `pre-upload`: claim a matching declaration or reserve the default size.
    pub fn decide(&self, free: u64, username: &str, vpath: &str, declared: Option<u64>, now_ms: i64) -> Decision {
        let claimed = {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&(UNCLAIMED.to_owned(), vpath.to_owned())).filter(|r| r.expires_at_ms > now_ms).map(|r| r.bytes)
        };
        let bytes = declared.or(claimed).unwrap_or(self.default_reservation);
        let expires = now_ms + RESERVATION_TTL.as_millis() as i64;
        self.reserve(free, (username.to_owned(), vpath.to_owned()), bytes, expires, now_ms)
    }

    fn reserve(&self, free: u64, key: (String, String), bytes: u64, expires_at_ms: i64, now_ms: i64) -> Decision {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, r| r.expires_at_ms > now_ms);
        // A retried check for the same key replaces its own reservation.
        map.remove(&key);
        let reserved: u64 = map.values().map(|r| r.bytes).sum();
        let needed = self.min_free_bytes.saturating_add(bytes);
        if free.saturating_sub(reserved) < needed {
            return Decision::Reject { free, reserved, needed };
        }
        map.insert(key, Reservation { bytes, expires_at_ms });
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
    fn proxy_declarations_are_claimed_by_pre_upload() {
        let a = Admission::new(4 * GIB, GIB);
        // Two concurrent 10 GiB WebDAV PUTs with 20 GiB free and a 4 GiB reserve:
        // the second does not fit next to the first (20 - 10 < 4 + 10).
        assert_eq!(a.declare(20 * GIB, "/a.mp4", 10 * GIB, 0), Decision::Admit);
        assert!(matches!(a.declare(20 * GIB, "/b.mp4", 10 * GIB, 0), Decision::Reject { .. }));
        // SFTPGo's pre-upload for /a.mp4 takes over the 10 GiB declaration, not 1 GiB.
        assert_eq!(a.decide(20 * GIB, "alice", "/a.mp4", None, 1), Decision::Admit);
        assert_eq!(a.reserved_bytes(1), (1, 10 * GIB));
        a.release("alice", "/a.mp4");
        assert_eq!(a.reserved_bytes(1), (0, 0));
    }

    #[test]
    fn unclaimed_declarations_expire_quickly() {
        let a = Admission::new(0, 1);
        assert_eq!(a.declare(10, "/x", 10, 0), Decision::Admit);
        let later = DECLARATION_TTL.as_millis() as i64 + 1;
        assert_eq!(a.reserved_bytes(later), (0, 0));
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
