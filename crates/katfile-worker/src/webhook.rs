//! SFTPGo hook receivers.
//!
//! * `POST /hooks/sftpgo/fs` (custom actions `pre-upload`, `upload`): staging-space
//!   admission and durable enqueueing of completed uploads.
//! * `POST /hooks/sftpgo/provider` (user add/update/delete): triggers an authoritative
//!   re-read of the user from the SFTPGo API; the payload itself is never trusted.
//!
//! Status codes matter: SFTPGo retries 5xx (and connection errors) but not 4xx, and
//! any non-200 answer to `pre-upload` denies the upload.

use std::path::Path;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::admission::Decision;
use crate::app::AppRef;
use crate::auth::bearer_matches;
use crate::paths::{RelPath, validate_username};
use crate::queue::{self, EnqueueError, EnqueueOutcome, StagedUpload};
use crate::util::now_ms;

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/hooks/sftpgo/fs", post(fs_hook))
        .route("/hooks/sftpgo/provider", post(provider_hook))
        .layer(DefaultBodyLimit::max(256 * 1024))
}

/// Fields of SFTPGo's custom-action JSON (see tests/fixtures/sftpgo_fs_*.json).
#[derive(Debug, Deserialize)]
pub struct FsEvent {
    pub action: String,
    pub username: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub virtual_path: String,
    #[serde(default)]
    pub file_size: Option<i64>,
    #[serde(default)]
    pub status: i64,
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub fs_provider: i64,
    /// Nanoseconds since the epoch.
    #[serde(default)]
    pub timestamp: i64,
}

fn reply(code: StatusCode, msg: &str) -> Response {
    (code, axum::Json(json!({ "message": msg }))).into_response()
}

async fn fs_hook(State(app): State<AppRef>, headers: HeaderMap, body: Bytes) -> Response {
    if !bearer_matches(&headers, &app.webhook_secret) {
        warn!("rejected unauthenticated filesystem hook");
        return reply(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let ev: FsEvent = match serde_json::from_slice(&body) {
        Ok(ev) => ev,
        Err(e) => return reply(StatusCode::BAD_REQUEST, &format!("invalid payload: {e}")),
    };
    if let Err(e) = validate_username(&ev.username) {
        warn!(error = %e, "hook for an unsupported username");
        return reply(StatusCode::BAD_REQUEST, "unsupported username");
    }
    let rel = match RelPath::parse_virtual(&ev.virtual_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(user = %ev.username, error = %e, "hook with an invalid virtual path");
            return reply(StatusCode::BAD_REQUEST, "invalid virtual path");
        }
    };
    match ev.action.as_str() {
        "pre-upload" => admit(&app, &ev, &rel).await,
        "upload" => completed_upload(&app, &ev, rel).await,
        other => {
            debug!(action = other, "ignoring filesystem action");
            reply(StatusCode::OK, "ignored")
        }
    }
}

async fn admit(app: &AppRef, ev: &FsEvent, rel: &RelPath) -> Response {
    let staging = app.staging.clone();
    let free = match tokio::task::spawn_blocking(move || staging.free_bytes()).await {
        Ok(Ok(f)) => f,
        _ => {
            // Fail closed: without a reading the upload might fill the disk.
            warn!(user = %ev.username, "cannot read staging free space; denying upload");
            return reply(StatusCode::FORBIDDEN, "staging space unknown");
        }
    };
    match app.admission.decide(free, &ev.username, &rel.virtual_path(), None, now_ms()) {
        Decision::Admit => reply(StatusCode::OK, "admitted"),
        Decision::Reject { free, reserved, needed } => {
            warn!(user = %ev.username, path = %rel.virtual_path(), free, reserved, needed, "upload denied: staging space below reserve");
            // 403 (not 507): SFTPGo would retry a 5xx for minutes before denying.
            reply(StatusCode::FORBIDDEN, "insufficient staging space")
        }
    }
}

async fn completed_upload(app: &AppRef, ev: &FsEvent, rel: RelPath) -> Response {
    app.admission.release(&ev.username, &rel.virtual_path());
    if ev.status != 1 {
        debug!(user = %ev.username, path = %rel.virtual_path(), status = ev.status, "upload did not complete; nothing to archive");
        return reply(StatusCode::OK, "not completed");
    }
    if ev.fs_provider != 0 {
        return reply(StatusCode::OK, "non-local filesystem ignored");
    }
    if !app.cfg.accepted_protocols.iter().any(|p| p == &ev.protocol) {
        info!(user = %ev.username, protocol = %ev.protocol, "protocol not accepted for archival");
        return reply(StatusCode::OK, "protocol ignored");
    }
    // The physical path is derived, never taken from the payload; mismatches are suspicious.
    let derived = app.cfg.users_dir.join(&ev.username).join(rel.virtual_path().trim_start_matches('/'));
    if !ev.path.is_empty() && Path::new(&ev.path) != derived {
        warn!(user = %ev.username, payload_path = %ev.path, "payload path does not match the user's home; refusing");
        return reply(StatusCode::BAD_REQUEST, "path mismatch");
    }
    let size = ev.file_size.unwrap_or(0).max(0) as u64;
    let up = StagedUpload {
        username: ev.username.clone(),
        rel,
        size,
        protocol: ev.protocol.clone(),
        event_ts_ms: if ev.timestamp > 0 { ev.timestamp / 1_000_000 } else { now_ms() },
        source: "event",
    };
    match queue::enqueue(app, up).await {
        Ok(EnqueueOutcome::Queued(id)) => reply(StatusCode::OK, &format!("queued {id}")),
        Ok(other) => reply(StatusCode::OK, &format!("{other:?}")),
        Err(EnqueueError::Invalid(msg)) => reply(StatusCode::BAD_REQUEST, &msg),
        Err(EnqueueError::Retryable(e)) => {
            warn!(error = %format!("{e:#}"), "could not record upload; SFTPGo will retry");
            reply(StatusCode::SERVICE_UNAVAILABLE, "temporarily unable to record the upload")
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProviderQuery {
    #[serde(default)]
    action: String,
    #[serde(default)]
    object_type: String,
    #[serde(default)]
    object_name: String,
}

async fn provider_hook(State(app): State<AppRef>, Query(q): Query<ProviderQuery>, headers: HeaderMap) -> Response {
    if !bearer_matches(&headers, &app.webhook_secret) {
        warn!("rejected unauthenticated provider hook");
        return reply(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if q.object_type != "user" {
        return reply(StatusCode::OK, "ignored");
    }
    if validate_username(&q.object_name).is_err() {
        warn!(name = %q.object_name.escape_debug(), "provider event for an unsupported username");
        return reply(StatusCode::OK, "unsupported username ignored");
    }
    info!(action = %q.action, user = %q.object_name, "provider event; re-reading the user from SFTPGo");
    // Answer at once; the periodic reconciler is the safety net if this sync fails.
    let app2 = app.clone();
    let username = q.object_name.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::provision::sync_user(&app2, &username).await {
            warn!(user = %username, error = %format!("{e:#}"), "user sync after provider event failed");
        }
    });
    reply(StatusCode::OK, "accepted")
}
