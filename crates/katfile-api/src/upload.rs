//! Two-step upload: `upload/server` returns a CGI URL plus session, then the file is
//! streamed as `multipart/form-data` (`sess_id`, `utype`, `file_0`).
//!
//! The body is produced by [`UploadBody`], which reads the local file in bounded
//! chunks (never `read_to_end`), hashes it on the fly, and either advertises an exact
//! length (`Content-Length`, the default) or streams with chunked encoding.
//!
//! Outcome classification matters more than speed here: once the whole body has been
//! handed to the connection, any failure is reported as [`KatFileError::UploadAmbiguous`]
//! because the provider may already have stored the file.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame, SizeHint};
use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, Take};
use tracing::{debug, info};

use crate::client::{KatFileClient, read_limited, validate_upload_url};
use crate::error::{KatFileError, sanitize_text};
use crate::models::{FileCode, UploadServer, value_string};
use crate::names::sanitize_file_name;

/// Largest reply accepted from the upload CGI.
const MAX_UPLOAD_REPLY_BYTES: usize = 64 * 1024;

/// How the multipart body is framed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadMode {
    /// Exact `Content-Length` computed up front (preamble + file size + epilogue). Default.
    FixedLength,
    /// `Transfer-Encoding: chunked`; verified to work on katfile.biz for small files.
    Chunked,
}

/// Live upload counters, shared between the body producer and observers.
#[derive(Debug)]
pub struct UploadProgress {
    started: Instant,
    bytes_sent: AtomicU64,
    file_bytes_sent: AtomicU64,
    last_activity_ms: AtomicU64,
    /// Milliseconds since `started` when the last body byte was handed over; 0 = not yet.
    body_complete_ms: AtomicU64,
    sha256: Mutex<Option<String>>,
    local_error: Mutex<Option<String>>,
}

impl UploadProgress {
    pub fn new() -> Arc<Self> {
        Arc::new(UploadProgress {
            started: Instant::now(),
            bytes_sent: AtomicU64::new(0),
            file_bytes_sent: AtomicU64::new(0),
            last_activity_ms: AtomicU64::new(0),
            body_complete_ms: AtomicU64::new(0),
            sha256: Mutex::new(None),
            local_error: Mutex::new(None),
        })
    }

    /// Total multipart bytes handed to the connection so far.
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    /// File payload bytes handed to the connection so far.
    pub fn file_bytes_sent(&self) -> u64 {
        self.file_bytes_sent.load(Ordering::Relaxed)
    }

    /// True once the complete multipart body was handed to the connection.
    pub fn body_complete(&self) -> bool {
        self.body_complete_ms.load(Ordering::Acquire) != 0
    }

    /// SHA-256 of the file payload, available once the whole file was read.
    pub fn sha256_hex(&self) -> Option<String> {
        self.sha256.lock().ok().and_then(|g| g.clone())
    }

    fn now_ms(&self) -> u64 {
        // +1 so that a completion in the first millisecond is still distinguishable from "not yet".
        self.started.elapsed().as_millis() as u64 + 1
    }

    fn record_sent(&self, total: usize, file: usize) {
        self.bytes_sent.fetch_add(total as u64, Ordering::Relaxed);
        self.file_bytes_sent.fetch_add(file as u64, Ordering::Relaxed);
        self.last_activity_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        Duration::from_millis(self.now_ms().saturating_sub(last))
    }

    fn mark_body_complete(&self) {
        self.body_complete_ms.store(self.now_ms(), Ordering::Release);
    }

    fn since_body_complete(&self) -> Option<Duration> {
        match self.body_complete_ms.load(Ordering::Acquire) {
            0 => None,
            done => Some(Duration::from_millis(self.now_ms().saturating_sub(done))),
        }
    }

    fn set_local_error(&self, msg: String) {
        if let Ok(mut g) = self.local_error.lock() {
            g.get_or_insert(msg);
        }
    }

    fn local_error(&self) -> Option<String> {
        self.local_error.lock().ok().and_then(|g| g.clone())
    }
}

/// A file to upload. The caller opens the file (typically an immutable snapshot) and
/// states its expected size; at most `size` bytes are read.
pub struct UploadRequest {
    pub file: tokio::fs::File,
    pub size: u64,
    /// Remote file name; must already be sanitized with [`sanitize_file_name`].
    pub remote_name: String,
    pub mode: UploadMode,
}

/// Successful upload result. The file may still be anonymous if the session was not
/// honoured; callers must confirm ownership with an account-scoped listing.
#[derive(Debug, Clone)]
pub struct UploadReceipt {
    pub file_code: FileCode,
    pub sha256_hex: String,
    pub bytes_sent: u64,
}

impl KatFileClient {
    /// Step 1: obtain a validated upload CGI URL and a fresh session.
    pub async fn request_upload_server(&self) -> Result<UploadServer, KatFileError> {
        let reply = self.post_api("upload/server", &[], true).await?;
        let raw_url =
            reply.result.as_str().ok_or_else(|| KatFileError::Malformed("upload/server result is not a URL".into()))?;
        let url = validate_upload_url(raw_url, &self.upload_host_re, self.cfg.insecure_test_mode)?;
        let sess_id = reply
            .root
            .get("sess_id")
            .and_then(value_string)
            .filter(|s| (8..=256).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()))
            .ok_or_else(|| KatFileError::Malformed("upload/server returned no usable sess_id".into()))?;
        debug!(host = url.host_str().unwrap_or(""), "obtained KatFile upload server");
        Ok(UploadServer { url, sess_id, server_time: reply.root.get("server_time").and_then(value_string) })
    }

    /// Step 2: stream the file to the upload CGI and return the new file code.
    pub async fn upload(
        &self,
        server: &UploadServer,
        req: UploadRequest,
        progress: Arc<UploadProgress>,
    ) -> Result<UploadReceipt, KatFileError> {
        if req.size == 0 {
            // Verified: the CGI answers "null filesize or wrong file path" for empty files.
            return Err(KatFileError::UploadRejected("KatFile does not accept empty files".into()));
        }
        if req.remote_name.is_empty() || sanitize_file_name(&req.remote_name) != req.remote_name {
            return Err(KatFileError::InvalidArgument("remote file name is not sanitized".into()));
        }
        let utype = &self.cfg.upload_type;
        if utype.is_empty() || !utype.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(KatFileError::InvalidArgument("invalid upload type".into()));
        }

        let boundary = new_boundary();
        let preamble = build_preamble(&boundary, &server.sess_id, utype, &req.remote_name);
        let epilogue = Bytes::from(format!("\r\n--{boundary}--\r\n"));
        let fixed = req.mode == UploadMode::FixedLength;
        let total_len = preamble.len() as u64 + req.size + epilogue.len() as u64;
        let body = UploadBody::new(
            preamble,
            epilogue,
            req.file,
            req.size,
            self.cfg.upload_chunk_bytes,
            fixed,
            progress.clone(),
        );

        info!(
            host = server.url.host_str().unwrap_or(""),
            size = req.size,
            mode = ?req.mode,
            "starting KatFile upload"
        );
        let send = self
            .http
            .post(server.url.clone())
            .header(CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(reqwest::Body::wrap(body))
            .send();
        tokio::pin!(send);

        // Generous reply deadline: the CGI may post-process large files before answering.
        let response_timeout = self.cfg.upload_response_timeout + Duration::from_secs(req.size / (100 * 1024 * 1024));
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let sent = loop {
            tokio::select! {
                res = &mut send => break res,
                _ = ticker.tick() => {
                    if let Some(since) = progress.since_body_complete() {
                        if since > response_timeout {
                            return Err(KatFileError::UploadAmbiguous(format!(
                                "no reply within {}s after the body was sent", response_timeout.as_secs())));
                        }
                    } else if progress.idle_for() > self.cfg.upload_stall_timeout {
                        return Err(KatFileError::Stalled(self.cfg.upload_stall_timeout));
                    }
                }
            }
        };

        let resp = match sent {
            Ok(resp) => resp,
            Err(err) => return Err(self.classify_send_error(err, &progress)),
        };
        let status = resp.status();
        let body_complete = progress.body_complete();
        let reply = match tokio::time::timeout(response_timeout, read_limited(resp, MAX_UPLOAD_REPLY_BYTES)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) if body_complete => {
                return Err(KatFileError::UploadAmbiguous(format!("reading the reply failed: {err}")));
            }
            Ok(Err(err)) => return Err(err),
            Err(_) if body_complete => {
                return Err(KatFileError::UploadAmbiguous("timed out reading the reply".into()));
            }
            Err(_) => return Err(KatFileError::Timeout),
        };

        if !status.is_success() {
            let code = status.as_u16();
            return Err(if body_complete && (status.is_server_error() || status.is_redirection()) {
                KatFileError::UploadAmbiguous(format!("HTTP {code} after the body was sent"))
            } else if status.is_client_error() {
                KatFileError::UploadRejected(format!("HTTP {code}"))
            } else if status.is_redirection() {
                KatFileError::Redirect { status: code }
            } else {
                KatFileError::HttpStatus { status: code }
            });
        }

        let file_code = parse_upload_reply(&reply)?;
        let sha256_hex = match (body_complete, progress.sha256_hex()) {
            (true, Some(h)) => h,
            // A success reply before we finished sending cannot be trusted.
            _ => return Err(KatFileError::UploadAmbiguous("success reply before the body was complete".into())),
        };
        info!(file_code = %file_code, bytes = progress.bytes_sent(), total_len, "KatFile upload accepted");
        Ok(UploadReceipt { file_code, sha256_hex, bytes_sent: progress.bytes_sent() })
    }

    fn classify_send_error(&self, err: reqwest::Error, progress: &UploadProgress) -> KatFileError {
        if let Some(local) = progress.local_error() {
            return if local.contains("shorter than declared") {
                KatFileError::LocalFileChanged
            } else {
                KatFileError::LocalIo(local)
            };
        }
        let mapped = KatFileError::from_reqwest(err);
        if progress.body_complete() {
            KatFileError::UploadAmbiguous(format!("connection ended after the body was sent: {mapped}"))
        } else {
            mapped
        }
    }
}

/// Interpret the CGI reply, e.g. `[{"file_code":"79cpmm7q3pmg","file_status":"OK"}]`.
pub(crate) fn parse_upload_reply(body: &[u8]) -> Result<FileCode, KatFileError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| KatFileError::UploadAmbiguous("upload reply is not JSON".into()))?;
    let item = value
        .as_array()
        .and_then(|items| items.first())
        .ok_or_else(|| KatFileError::UploadAmbiguous("upload reply has no items".into()))?;
    let file_status = item.get("file_status").and_then(value_string).unwrap_or_default();
    if file_status != "OK" {
        return Err(KatFileError::UploadRejected(sanitize_text(&file_status)));
    }
    let code = item.get("file_code").and_then(value_string).unwrap_or_default();
    FileCode::parse(&code).map_err(|_| KatFileError::UploadAmbiguous("OK reply without a valid file_code".into()))
}

fn build_preamble(boundary: &str, sess_id: &str, utype: &str, file_name: &str) -> Bytes {
    let mut s = String::with_capacity(512);
    for (name, value) in [("sess_id", sess_id), ("utype", utype)] {
        s.push_str(&format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"));
    }
    // `file_name` is sanitized (no quotes, backslashes or control characters).
    s.push_str(&format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file_0\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    ));
    Bytes::from(s)
}

/// Unpredictable multipart boundary (96 bits from a hashed time/counter/pid seed).
fn new_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::digest(format!("{nanos}:{n}:{}", std::process::id()).as_bytes());
    format!("kfgw{}", hex::encode(&digest[..12]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Preamble,
    File,
    Epilogue,
    Done,
}

/// Streaming multipart body: preamble, file bytes (bounded chunks), epilogue.
pub(crate) struct UploadBody {
    phase: Phase,
    preamble: Bytes,
    epilogue: Bytes,
    file: Take<tokio::fs::File>,
    remaining: u64,
    chunk_bytes: usize,
    buf: BytesMut,
    hasher: Option<Sha256>,
    fixed_len: bool,
    progress: Arc<UploadProgress>,
}

impl UploadBody {
    pub(crate) fn new(
        preamble: Bytes,
        epilogue: Bytes,
        file: tokio::fs::File,
        size: u64,
        chunk_bytes: usize,
        fixed_len: bool,
        progress: Arc<UploadProgress>,
    ) -> Self {
        let chunk_bytes = chunk_bytes.clamp(8 * 1024, 4 * 1024 * 1024);
        UploadBody {
            phase: Phase::Preamble,
            preamble,
            epilogue,
            file: file.take(size),
            remaining: size,
            chunk_bytes,
            buf: BytesMut::new(),
            hasher: Some(Sha256::new()),
            fixed_len,
            progress,
        }
    }

    fn remaining_len(&self) -> u64 {
        match self.phase {
            Phase::Preamble => self.preamble.len() as u64 + self.remaining + self.epilogue.len() as u64,
            Phase::File => self.remaining + self.epilogue.len() as u64,
            Phase::Epilogue => self.epilogue.len() as u64,
            Phase::Done => 0,
        }
    }

    fn finish_hash(&mut self) {
        if let Some(h) = self.hasher.take()
            && let Ok(mut slot) = self.progress.sha256.lock()
        {
            *slot = Some(hex::encode(h.finalize()));
        }
    }
}

impl Body for UploadBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        loop {
            match this.phase {
                Phase::Preamble => {
                    this.phase = Phase::File;
                    let pre = std::mem::take(&mut this.preamble);
                    this.progress.record_sent(pre.len(), 0);
                    return Poll::Ready(Some(Ok(Frame::data(pre))));
                }
                Phase::File => {
                    if this.remaining == 0 {
                        this.finish_hash();
                        this.phase = Phase::Epilogue;
                        continue;
                    }
                    if this.buf.capacity() - this.buf.len() < 8 * 1024 {
                        this.buf.reserve(this.chunk_bytes);
                    }
                    match tokio_util::io::poll_read_buf(Pin::new(&mut this.file), cx, &mut this.buf) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => {
                            this.progress.set_local_error(format!("local read failed: {}", e.kind()));
                            return Poll::Ready(Some(Err(e)));
                        }
                        Poll::Ready(Ok(0)) => {
                            let msg = "local file is shorter than declared size".to_owned();
                            this.progress.set_local_error(msg.clone());
                            return Poll::Ready(Some(Err(io::Error::new(io::ErrorKind::UnexpectedEof, msg))));
                        }
                        Poll::Ready(Ok(n)) => {
                            let chunk = this.buf.split().freeze();
                            this.remaining -= n as u64;
                            if let Some(h) = this.hasher.as_mut() {
                                h.update(&chunk);
                            }
                            this.progress.record_sent(n, n);
                            return Poll::Ready(Some(Ok(Frame::data(chunk))));
                        }
                    }
                }
                Phase::Epilogue => {
                    this.phase = Phase::Done;
                    let epi = std::mem::take(&mut this.epilogue);
                    this.progress.record_sent(epi.len(), 0);
                    this.progress.mark_body_complete();
                    return Poll::Ready(Some(Ok(Frame::data(epi))));
                }
                Phase::Done => return Poll::Ready(None),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.phase == Phase::Done
    }

    fn size_hint(&self) -> SizeHint {
        if self.fixed_len { SizeHint::with_exact(self.remaining_len()) } else { SizeHint::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_observed_success_reply() {
        let code = parse_upload_reply(br#"[{"file_status":"OK","file_code":"79cpmm7q3pmg"}]"#).unwrap();
        assert_eq!(code.as_str(), "79cpmm7q3pmg");
    }

    #[test]
    fn empty_file_reply_is_a_rejection() {
        let err = parse_upload_reply(br#"[{"file_status":"null filesize or wrong file path","file_code":"undef"}]"#)
            .unwrap_err();
        assert!(matches!(err, KatFileError::UploadRejected(_)));
    }

    #[test]
    fn garbage_reply_is_ambiguous() {
        assert!(matches!(parse_upload_reply(b"<html>oops</html>"), Err(KatFileError::UploadAmbiguous(_))));
        assert!(matches!(parse_upload_reply(b"[]"), Err(KatFileError::UploadAmbiguous(_))));
        assert!(matches!(
            parse_upload_reply(br#"[{"file_status":"OK","file_code":"undef"}]"#),
            Err(KatFileError::UploadAmbiguous(_))
        ));
    }

    #[test]
    fn preamble_contains_expected_fields() {
        let p = build_preamble("BOUND", "sess123", "prem", "相片.jpg");
        let s = std::str::from_utf8(&p).unwrap();
        assert!(s.contains("name=\"sess_id\"\r\n\r\nsess123\r\n"));
        assert!(s.contains("name=\"utype\"\r\n\r\nprem\r\n"));
        assert!(s.contains("name=\"file_0\"; filename=\"相片.jpg\""));
        assert!(s.ends_with("\r\n\r\n"));
    }

    #[test]
    fn boundaries_are_unique() {
        assert_ne!(new_boundary(), new_boundary());
    }
}
