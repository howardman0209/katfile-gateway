//! Operator API (bearer admin token; bind to loopback/VPN only) and readiness probe.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use katfile_api::FileCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::app::AppRef;
use crate::auth::bearer_matches;
use crate::db::{JobState, PrincipalState};
use crate::util::{human_bytes, now_ms, rfc3339};
use crate::{cleanup, reconcile};

const SEMANTICS: &str = "A finished WebDAV/SFTP upload only means the file is staged on this gateway. \
It is archived when its job reaches 'archived' (file code verified inside the user's KatFile folder). \
Local copies of archived files are removed after the retention period and then disappear from WebDAV/SFTP.";

pub fn routes() -> Router<AppRef> {
    Router::new()
        .route("/admin/status", get(status))
        .route("/admin/jobs", get(jobs))
        .route("/admin/jobs/{id}", get(job))
        .route("/admin/jobs/{id}/retry", post(retry))
        .route("/admin/jobs/{id}/adopt", post(adopt))
        .route("/admin/jobs/{id}/discard", post(discard))
        .route("/admin/jobs/{id}/expire-retention", post(expire_retention))
        .route("/admin/principals", get(principals))
        .route("/admin/principals/{id}/approve", post(approve))
        .route("/admin/reconcile", post(run_reconcile))
        .route("/admin/cleanup", post(run_cleanup))
}

pub async fn readyz(State(app): State<AppRef>) -> Response {
    match app.db.ping().await {
        Ok(()) => (StatusCode::OK, "ready").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response(),
    }
}

fn is_admin(app: &AppRef, headers: &HeaderMap) -> bool {
    bearer_matches(headers, &app.admin_token)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "unauthorized"}))).into_response()
}

fn ok(v: Value) -> Response {
    (StatusCode::OK, axum::Json(v)).into_response()
}

fn err(code: StatusCode, msg: impl ToString) -> Response {
    (code, axum::Json(json!({"error": msg.to_string()}))).into_response()
}

async fn status(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    match build_status(&app).await {
        Ok(v) => ok(v),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn build_status(app: &AppRef) -> anyhow::Result<Value> {
    let now = now_ms();
    let mut jobs = BTreeMap::new();
    for s in JobState::ALL {
        jobs.insert(s.as_str().to_owned(), json!({"count": 0, "bytes": 0}));
    }
    for (state, n, bytes) in app.db.job_counts().await? {
        jobs.insert(state, json!({"count": n, "bytes": bytes}));
    }
    let mut principals = BTreeMap::new();
    for s in PrincipalState::ALL {
        principals.insert(s.as_str().to_owned(), 0i64);
    }
    for (state, n) in app.db.principal_counts().await? {
        principals.insert(state, n);
    }
    let count = |s: &str| jobs.get(s).and_then(|v| v["count"].as_i64()).unwrap_or(0);
    let staging = app.staging.clone();
    let free = tokio::task::spawn_blocking(move || staging.free_bytes()).await?.unwrap_or(0);
    let (reservations, reserved) = app.admission.reserved_bytes(now);
    let snapshot_bytes = app.db.held_snapshot_bytes().await?;
    let oldest = app.db.oldest_unarchived_created_ms().await?;
    let auth_error = app.katfile_auth_error.lock().ok().and_then(|g| g.clone());
    Ok(json!({
        "service": "katfile-worker",
        "version": env!("CARGO_PKG_VERSION"),
        "time": rfc3339(now),
        "uptime_s": (now - app.started_at_ms) / 1000,
        "semantics": SEMANTICS,
        "processing_enabled": app.cfg.processing_enabled,
        "jobs": jobs,
        "attention": {
            "needs_review": count("needs_review"),
            "failed": count("failed"),
            "blocked": count("blocked"),
            "awaiting_principal": count("awaiting_principal"),
        },
        "queue_depth": count("pending") + count("retry_waiting") + count("uploading") + count("remote_uploaded")
            + count("assigning_folder") + count("verifying") + count("awaiting_principal"),
        "oldest_unarchived_age_s": oldest.map(|t| (now - t) / 1000),
        "principals": principals,
        "staging": {
            "free_bytes": free,
            "free": human_bytes(free),
            "min_free_bytes": app.admission.min_free_bytes(),
            "disk_pressure": free < app.admission.min_free_bytes(),
            "admitted_uploads_in_flight": reservations,
            "reserved_bytes": reserved,
            "snapshot_bytes": snapshot_bytes,
            "snapshot": human_bytes(snapshot_bytes.max(0) as u64),
        },
        "retention_hours": app.cfg.retention.as_secs() / 3600,
        "katfile_auth_error": auth_error,
        "last_reconcile": app.db.kv_get("last_reconcile").await?,
    }))
}

#[derive(Debug, Deserialize)]
struct JobsQuery {
    state: Option<String>,
    limit: Option<i64>,
}

async fn jobs(State(app): State<AppRef>, headers: HeaderMap, Query(q): Query<JobsQuery>) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let res = match q.state.as_deref() {
        Some(s) => match s.parse::<JobState>() {
            Ok(state) => app.db.jobs_in_state(state, limit).await,
            Err(e) => return err(StatusCode::BAD_REQUEST, e),
        },
        None => app.db.recent_jobs(limit).await,
    };
    match res {
        Ok(list) => ok(json!({"jobs": list})),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn job(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    match (app.db.job(&id).await, app.db.attempts(&id).await) {
        (Ok(Some(j)), Ok(attempts)) => ok(json!({"job": j, "attempts": attempts})),
        (Ok(None), _) => err(StatusCode::NOT_FOUND, "no such job"),
        (Err(e), _) | (_, Err(e)) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

/// Explicit operator approval to retry (for an ambiguous upload this allows a re-upload).
async fn retry(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    match app.db.operator_retry(&id).await {
        Ok(true) => {
            info!(job = %id, "operator requested a retry");
            app.runner_wake.notify_one();
            ok(json!({"job": id, "state": "retry_waiting"}))
        }
        Ok(false) => err(StatusCode::CONFLICT, "only needs_review, failed or blocked jobs can be retried"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Debug, Deserialize)]
struct AdoptBody {
    file_code: String,
}

/// The operator confirms which remote file belongs to an ambiguous upload.
async fn adopt(
    State(app): State<AppRef>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(b): axum::Json<AdoptBody>,
) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    let code = match FileCode::parse(&b.file_code) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    match app.db.job(&id).await {
        Ok(Some(j)) if j.state == JobState::NeedsReview && j.remote_file_code.is_none() => {}
        Ok(Some(_)) => return err(StatusCode::CONFLICT, "only needs_review jobs without a file code can adopt one"),
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such job"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
    match app.db.file_code_claimed(code.as_str()).await {
        Ok(true) => return err(StatusCode::CONFLICT, "file code already belongs to another job"),
        Ok(false) => {}
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
    match app.db.mark_remote_uploaded(&id, code.as_str(), None).await {
        Ok(()) => {
            info!(job = %id, file_code = %code, "operator adopted a remote file");
            app.runner_wake.notify_one();
            ok(json!({"job": id, "state": "remote_uploaded"}))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn discard(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    let job = match app.db.job(&id).await {
        Ok(Some(j)) => j,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such job"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    match cleanup::discard_job(&app, &job).await {
        Ok(out) => ok(json!({"job": id, "state": "discarded", "local_copies": out})),
        Err(e) => err(StatusCode::CONFLICT, format!("{e:#}")),
    }
}

/// Make an archived job eligible for cleanup now (frees staging space early).
async fn expire_retention(State(app): State<AppRef>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    match app.db.job(&id).await {
        Ok(Some(j)) if j.state == JobState::Archived => match app.db.mark_archived_retention(&id, now_ms()).await {
            Ok(()) => ok(json!({"job": id, "retain_until": rfc3339(now_ms())})),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        },
        Ok(Some(_)) => err(StatusCode::CONFLICT, "only archived jobs have a retention period"),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such job"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn principals(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    match app.db.principals().await {
        Ok(list) => ok(json!({"principals": list})),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Debug, Deserialize)]
struct ApproveBody {
    /// Create a new folder with this name (must not exist yet under the users parent).
    folder_name: Option<String>,
    /// Bind an existing folder (disaster recovery); must be a child of the users parent.
    bind_folder_id: Option<i64>,
}

async fn approve(
    State(app): State<AppRef>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(b): axum::Json<ApproveBody>,
) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    let p = match app.db.principal(&id).await {
        Ok(Some(p)) => p,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such principal"),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    if !matches!(p.state, PrincipalState::Failed | PrincipalState::Provisioning) || p.remote_folder_id.is_some() {
        return err(StatusCode::CONFLICT, "only failed or provisioning principals without a folder can be approved");
    }
    if b.folder_name.is_some() == b.bind_folder_id.is_some() {
        return err(StatusCode::BAD_REQUEST, "give exactly one of folder_name or bind_folder_id");
    }
    if let Some(name) = &b.folder_name
        && (name.is_empty() || katfile_api::names::sanitize_folder_name(name) != *name)
    {
        return err(StatusCode::BAD_REQUEST, "folder_name contains characters KatFile would rewrite");
    }
    match app.db.approve_principal(&id, b.folder_name.as_deref(), b.bind_folder_id).await {
        Ok(()) => {
            info!(principal = %id, folder_name = ?b.folder_name, bind = ?b.bind_folder_id, "provisioning approved by operator");
            app.provisioner_wake.notify_one();
            ok(json!({"principal": id, "state": "provisioning"}))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn run_reconcile(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    let report = reconcile::run_once(&app).await;
    ok(serde_json::to_value(report).unwrap_or(Value::Null))
}

async fn run_cleanup(State(app): State<AppRef>, headers: HeaderMap) -> Response {
    if !is_admin(&app, &headers) {
        return unauthorized();
    }
    let report = cleanup::run_once(&app).await;
    ok(serde_json::to_value(report).unwrap_or(Value::Null))
}
