//! Shared-secret checks for internal endpoints (webhooks and admin API).

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;

/// True when the request carries `Authorization: Bearer <secret>`.
pub fn bearer_matches(headers: &HeaderMap, secret: &str) -> bool {
    let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.trim().as_bytes(), secret.as_bytes())
}

/// Compare without early exit so response timing does not leak the secret prefix.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_check() {
        let mut h = HeaderMap::new();
        assert!(!bearer_matches(&h, "s3cret-value-123456"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer s3cret-value-123456"));
        assert!(bearer_matches(&h, "s3cret-value-123456"));
        assert!(!bearer_matches(&h, "s3cret-value-1234567"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Basic s3cret-value-123456"));
        assert!(!bearer_matches(&h, "s3cret-value-123456"));
    }

    #[test]
    fn constant_time_eq_lengths() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
