//! Typed, sanitized errors for the KatFile client.
//!
//! No variant ever carries a request URL, API key, upload session ID or
//! request body; reqwest errors are stripped of their URL before formatting.

use std::time::Duration;

/// How a caller should react to an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Safe to retry later with backoff (network, 429, 5xx, timeouts before the body completed).
    Transient,
    /// Retrying will not help without operator intervention (bad input, policy, rejected upload).
    Permanent,
    /// The API key was rejected; every call will fail until the secret is fixed.
    Auth,
    /// The provider may or may not have accepted an upload; never blindly re-upload.
    Ambiguous,
}

#[derive(Debug, thiserror::Error)]
pub enum KatFileError {
    #[error("KatFile rejected the API key")]
    InvalidKey,
    #[error("KatFile API error (status {status}): {msg}")]
    Api { status: i64, msg: String },
    #[error("unexpected HTTP status {status} from KatFile")]
    HttpStatus { status: u16 },
    #[error("unexpected HTTP redirect (status {status}) refused")]
    Redirect { status: u16 },
    #[error("request timed out")]
    Timeout,
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("malformed KatFile response: {0}")]
    Malformed(String),
    #[error("upload endpoint rejected by policy: {0}")]
    UntrustedEndpoint(String),
    #[error("upload rejected by KatFile: {0}")]
    UploadRejected(String),
    #[error("upload outcome is ambiguous: {0}")]
    UploadAmbiguous(String),
    #[error("upload stalled: no progress for {0:?}")]
    Stalled(Duration),
    #[error("local file changed while it was being uploaded")]
    LocalFileChanged,
    #[error("local I/O error: {0}")]
    LocalIo(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl KatFileError {
    /// Classify the error for retry decisions.
    pub fn class(&self) -> ErrorClass {
        match self {
            KatFileError::InvalidKey => ErrorClass::Auth,
            KatFileError::Api { status, .. } => match status {
                // KatFile reports server-side trouble with 5xx-like application codes.
                500..=599 | 429 => ErrorClass::Transient,
                _ => ErrorClass::Permanent,
            },
            KatFileError::HttpStatus { status } => match status {
                408 | 429 | 500..=599 => ErrorClass::Transient,
                _ => ErrorClass::Permanent,
            },
            KatFileError::Timeout | KatFileError::Connect(_) | KatFileError::Transport(_) => ErrorClass::Transient,
            KatFileError::Stalled(_) => ErrorClass::Transient,
            KatFileError::UploadAmbiguous(_) => ErrorClass::Ambiguous,
            KatFileError::Redirect { .. }
            | KatFileError::Malformed(_)
            | KatFileError::UntrustedEndpoint(_)
            | KatFileError::UploadRejected(_)
            | KatFileError::LocalFileChanged
            | KatFileError::LocalIo(_)
            | KatFileError::InvalidArgument(_) => ErrorClass::Permanent,
        }
    }

    /// Short machine-readable category for persistence and metrics.
    pub fn category(&self) -> &'static str {
        match self {
            KatFileError::InvalidKey => "auth_invalid_key",
            KatFileError::Api { .. } => "api_error",
            KatFileError::HttpStatus { .. } => "http_status",
            KatFileError::Redirect { .. } => "redirect_refused",
            KatFileError::Timeout => "timeout",
            KatFileError::Connect(_) => "connect",
            KatFileError::Transport(_) => "transport",
            KatFileError::Malformed(_) => "malformed_response",
            KatFileError::UntrustedEndpoint(_) => "untrusted_endpoint",
            KatFileError::UploadRejected(_) => "upload_rejected",
            KatFileError::UploadAmbiguous(_) => "upload_ambiguous",
            KatFileError::Stalled(_) => "upload_stalled",
            KatFileError::LocalFileChanged => "local_file_changed",
            KatFileError::LocalIo(_) => "local_io",
            KatFileError::InvalidArgument(_) => "invalid_argument",
        }
    }

    /// Map a reqwest error without leaking its URL (which may embed secrets).
    pub(crate) fn from_reqwest(err: reqwest::Error) -> Self {
        let err = err.without_url();
        if err.is_timeout() {
            KatFileError::Timeout
        } else if err.is_connect() {
            KatFileError::Connect(sanitize_text(&error_chain(&err)))
        } else if err.is_redirect() {
            KatFileError::Redirect { status: 0 }
        } else if err.is_decode() {
            KatFileError::Malformed(sanitize_text(&error_chain(&err)))
        } else {
            KatFileError::Transport(sanitize_text(&error_chain(&err)))
        }
    }
}

/// Render an error with its source chain on one line.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// Bound and strip provider- or library-supplied text before it reaches logs or the DB.
pub(crate) fn sanitize_text(input: &str) -> String {
    const MAX: usize = 300;
    let mut out: String = input.chars().map(|c| if c.is_control() { ' ' } else { c }).take(MAX).collect();
    if input.chars().count() > MAX {
        out.push('…');
    }
    // Defensive: never echo anything that looks like a key=... query parameter.
    redact_query_secrets(&out)
}

/// Replace values of `key=` / `sess_id=` parameters, should a library ever include them.
pub(crate) fn redact_query_secrets(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = find_secret_param(rest) {
        let (head, tail) = rest.split_at(pos.0);
        out.push_str(head);
        out.push_str(&tail[..pos.1]);
        out.push_str("<redacted>");
        let after = &tail[pos.1..];
        let end = after.find(['&', ' ', '"', '\'']).unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Returns (start offset, length of the `name=` prefix) of the next secret parameter.
fn find_secret_param(s: &str) -> Option<(usize, usize)> {
    ["key=", "sess_id="]
        .iter()
        .filter_map(|p| {
            s.match_indices(p)
                .find(|(i, _)| *i == 0 || !s.as_bytes()[i - 1].is_ascii_alphanumeric())
                .map(|(i, _)| (i, p.len()))
        })
        .min_by_key(|(i, _)| *i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_key_and_session_parameters() {
        let s = "GET https://x/api/file/list?fld_id=1&key=abc123&sess_id=zzz other";
        let r = redact_query_secrets(s);
        assert!(!r.contains("abc123"));
        assert!(!r.contains("zzz"));
        assert!(r.contains("fld_id=1"));
        assert!(r.contains("key=<redacted>"));
    }

    #[test]
    fn sanitize_strips_control_chars_and_bounds_length() {
        let long = "a\n".repeat(500);
        let s = sanitize_text(&long);
        assert!(!s.contains('\n'));
        assert!(s.chars().count() <= 301);
    }

    #[test]
    fn classification_is_conservative() {
        assert_eq!(KatFileError::Timeout.class(), ErrorClass::Transient);
        assert_eq!(KatFileError::InvalidKey.class(), ErrorClass::Auth);
        assert_eq!(KatFileError::UploadAmbiguous("x".into()).class(), ErrorClass::Ambiguous);
        assert_eq!(KatFileError::HttpStatus { status: 503 }.class(), ErrorClass::Transient);
        assert_eq!(KatFileError::Api { status: 403, msg: "no".into() }.class(), ErrorClass::Permanent);
    }
}
