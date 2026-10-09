//! Job processing state machine:
//!
//! ```text
//! pending -> uploading -> remote_uploaded -> assigning_folder -> verifying -> archived
//!    ^          |  (transient)                     |  (transient)      |
//!    +-- retry_waiting <---------------------------+-------------------+
//! uploading --(ambiguous)--> needs_review   (reconciled by name/size, never re-uploaded blindly)
//! ```
//!
//! Invariants: `uploading` is persisted before the first byte is sent; the moment the
//! whole body was handed to the connection is persisted too; a `file_code` is committed
//! before any folder operation and is never discarded by a retry.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use katfile_api::{ErrorClass, FileCode, FolderId, KatFileError, UploadProgress, UploadRequest};
use tokio::sync::{Semaphore, watch};
use tracing::{error, info, warn};

use crate::app::{App, AppRef, idle};
use crate::db::{Job, JobState, Principal, PrincipalState};
use crate::paths::{Identity, PathError};
use crate::provision;
use crate::util::now_ms;

/// How long a principal verification stays fresh.
const PRINCIPAL_FRESHNESS_MS: i64 = 10 * 60_000;
/// Verification misses tolerated before the job needs review.
const MAX_VERIFY_MISSES: i64 = 3;

pub async fn run(app: AppRef, mut stop: watch::Receiver<bool>) {
    if let Err(e) = recover_interrupted(&app).await {
        error!(error = %format!("{e:#}"), "startup recovery failed");
    }
    let permits = Arc::new(Semaphore::new(app.cfg.max_active_uploads));
    let running: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        match app.db.due_jobs(now_ms(), 32).await {
            Ok(due) => {
                for job in due {
                    if running.lock().map(|r| r.contains(&job.id)).unwrap_or(true) {
                        continue;
                    }
                    let Ok(permit) = permits.clone().try_acquire_owned() else { break };
                    if let Ok(mut r) = running.lock() {
                        r.insert(job.id.clone());
                    }
                    let (app2, running2) = (app.clone(), running.clone());
                    tokio::spawn(async move {
                        let id = job.id.clone();
                        if let Err(e) = step(&app2, job).await {
                            error!(job = %id, error = %format!("{e:#}"), "job step failed unexpectedly");
                            let next = now_ms() + 60_000;
                            let _ = app2
                                .db
                                .park_job(
                                    &id,
                                    JobState::RetryWaiting,
                                    next,
                                    true,
                                    Some("internal"),
                                    Some(&format!("{e:#}")),
                                )
                                .await;
                        }
                        if let Ok(mut r) = running2.lock() {
                            r.remove(&id);
                        }
                        drop(permit);
                        app2.runner_wake.notify_one();
                    });
                }
            }
            Err(e) => error!(error = %format!("{e:#}"), "cannot read due jobs"),
        }
        if idle(&app.runner_wake, Duration::from_secs(5), &mut stop).await {
            return;
        }
    }
}

/// Test driver: process every due job sequentially; returns how many were stepped.
#[cfg(test)]
pub async fn process_due_once(app: &App) -> Result<usize> {
    let due = app.db.due_jobs(now_ms(), 1000).await?;
    let n = due.len();
    for job in due {
        step(app, job).await?;
    }
    Ok(n)
}

/// Jobs left in `uploading` by a crash or restart.
pub async fn recover_interrupted(app: &App) -> Result<()> {
    for job in app.db.jobs_in_state(JobState::Uploading, 10_000).await? {
        if job.upload_body_sent_at_ms.is_none() {
            // The provider cannot have stored an incomplete body: safe to upload again.
            info!(job = %job.id, "upload interrupted before the body was sent; will retry");
            app.db
                .park_job(
                    &job.id,
                    JobState::RetryWaiting,
                    0,
                    true,
                    Some("interrupted"),
                    Some("worker stopped mid-upload"),
                )
                .await?;
        } else {
            warn!(job = %job.id, "upload interrupted after the body was sent; reconciling instead of re-uploading");
            app.db
                .park_job(
                    &job.id,
                    JobState::NeedsReview,
                    0,
                    false,
                    Some("ambiguous_upload_crash"),
                    Some("worker stopped after sending the whole body; awaiting reconciliation"),
                )
                .await?;
        }
    }
    Ok(())
}

enum Resolved {
    Ready(Box<Principal>),
    Parked,
}

/// Advance one job as far as possible.
async fn step(app: &App, mut job: Job) -> Result<()> {
    let p = match resolve_principal(app, &job).await? {
        Resolved::Ready(p) => p,
        Resolved::Parked => return Ok(()),
    };
    let folder = match provision::ensure_folder(app, &p, &job.rel_dir).await {
        Ok(f) => f,
        Err(e) => return park_error(app, &job, "folder", e).await,
    };

    if job.remote_file_code.is_none() {
        match upload(app, &job).await? {
            Some(code) => job.remote_file_code = Some(code),
            None => return Ok(()),
        }
    }
    let code = FileCode::parse(job.remote_file_code.as_deref().context("file code")?)?;

    app.db.mark_assigning(&job.id, folder.0 as i64).await?;
    let attempt = app.db.start_attempt(&job.id, "assign_folder").await?;
    if let Err(e) = app.katfile.set_file_folder(&code, folder).await {
        app.db.finish_attempt(attempt, class_name(&e), Some(e.category()), Some(&e.to_string())).await?;
        return park_error(app, &job, "assign_folder", e.into()).await;
    }
    app.db.finish_attempt(attempt, "ok", None, None).await?;

    app.db.mark_verifying(&job.id).await?;
    verify(app, &job, &p, &code, folder).await
}

/// Archived only after the account-scoped listing shows the file in the user's folder
/// (set_folder answers OK even for anonymous files; see docs/API_COMPATIBILITY.md).
async fn verify(app: &App, job: &Job, p: &Principal, code: &FileCode, folder: FolderId) -> Result<()> {
    let attempt = app.db.start_attempt(&job.id, "verify").await?;
    let found = match app.katfile.find_file_in_folder(folder, code, &job.remote_name, 50).await {
        Ok(found) => found,
        Err(e) => {
            app.db.finish_attempt(attempt, class_name(&e), Some(e.category()), Some(&e.to_string())).await?;
            return park_error(app, job, "verify", e.into()).await;
        }
    };
    match found {
        Some(entry) if entry.size == Some(job.size_bytes as u64) => {
            app.db.finish_attempt(attempt, "ok", None, None).await?;
            let retain_until = if p.state == PrincipalState::Deleted {
                now_ms()
            } else {
                now_ms() + app.cfg.retention.as_millis() as i64
            };
            app.db.mark_archived(&job.id, retain_until).await?;
            app.note_katfile_auth_error(None);
            info!(job = %job.id, user = %job.username, path = %job.virtual_path, file_code = %code, folder = folder.0, "archived and verified");
            Ok(())
        }
        Some(entry) => {
            let msg = format!("remote size {:?} differs from local {}", entry.size, job.size_bytes);
            app.db.finish_attempt(attempt, "permanent", Some("remote_size_mismatch"), Some(&msg)).await?;
            app.db.park_job(&job.id, JobState::NeedsReview, 0, false, Some("remote_size_mismatch"), Some(&msg)).await
        }
        None => {
            let misses =
                if job.last_error_category.as_deref() == Some("remote_not_listed") { job.attempts + 1 } else { 1 };
            let msg = "file not listed in the user's KatFile folder";
            app.db.finish_attempt(attempt, "transient", Some("remote_not_listed"), Some(msg)).await?;
            if misses >= MAX_VERIFY_MISSES {
                warn!(job = %job.id, file_code = %code, "ownership not verified; the provider may have stored the file anonymously");
                app.db
                    .park_job(
                        &job.id,
                        JobState::NeedsReview,
                        0,
                        false,
                        Some("remote_ownership_unverified"),
                        Some("file code not found in the account-scoped listing after several checks"),
                    )
                    .await
            } else {
                app.db
                    .park_job(
                        &job.id,
                        JobState::RetryWaiting,
                        now_ms() + 60_000,
                        true,
                        Some("remote_not_listed"),
                        Some(msg),
                    )
                    .await
            }
        }
    }
}

async fn resolve_principal(app: &App, job: &Job) -> Result<Resolved> {
    let mut p = match &job.principal_id {
        Some(pid) => app.db.principal(pid).await?.context("bound principal vanished")?,
        None => match app.db.principal_for_event(&job.username, job.event_ts_ms).await? {
            Some(p) => p,
            None => {
                // Perhaps the provider event was missed: ask SFTPGo now.
                if let Err(e) = provision::sync_user(app, &job.username).await {
                    warn!(user = %job.username, error = %format!("{e:#}"), "user lookup failed");
                }
                match app.db.principal_for_event(&job.username, job.event_ts_ms).await? {
                    Some(p) => p,
                    None => {
                        return park(
                            app,
                            job,
                            JobState::AwaitingPrincipal,
                            60_000,
                            "no_principal",
                            "no SFTPGo account owns this upload yet",
                        )
                        .await;
                    }
                }
            }
        },
    };
    if job.principal_id.is_none() {
        app.db.bind_job_principal(&job.id, &p.id).await?;
    }

    // Confirm with SFTPGo that this generation still owns the name after the event.
    let stale = p.verified_at_ms.unwrap_or(0) < job.event_ts_ms.max(now_ms() - PRINCIPAL_FRESHNESS_MS);
    if p.state != PrincipalState::Deleted && stale {
        if let Err(e) = provision::sync_user(app, &p.username).await {
            return park(app, job, JobState::RetryWaiting, 60_000, "sftpgo_unreachable", &format!("{e:#}")).await;
        }
        p = app.db.principal(&p.id).await?.context("principal vanished")?;
    }

    // Remote work already in flight (file code known) is completed regardless, and an
    // operator retry may archive into a disabled/deleted account's own folder.
    let in_flight = job.remote_file_code.is_some() || job.operator_override;
    let reason = p.state_reason.clone().unwrap_or_default();
    match p.state {
        PrincipalState::Deleted if !in_flight => {
            park(
                app,
                job,
                JobState::Blocked,
                0,
                "principal_deleted",
                "the account was deleted before this upload was archived",
            )
            .await
        }
        PrincipalState::Disabled if !in_flight => {
            park(app, job, JobState::Blocked, 0, "principal_disabled", &reason).await
        }
        PrincipalState::Blocked | PrincipalState::Failed | PrincipalState::Provisioning if !in_flight => {
            park(
                app,
                job,
                JobState::AwaitingPrincipal,
                60_000,
                "principal_not_ready",
                &format!("{}: {reason}", p.state),
            )
            .await
        }
        _ if p.remote_folder_id.is_none() => {
            park(app, job, JobState::AwaitingPrincipal, 60_000, "principal_not_ready", "no KatFile folder yet").await
        }
        _ => Ok(Resolved::Ready(Box::new(p))),
    }
}

async fn park(app: &App, job: &Job, state: JobState, delay_ms: i64, category: &str, reason: &str) -> Result<Resolved> {
    let next = if delay_ms > 0 { now_ms() + delay_ms } else { 0 };
    app.db.park_job(&job.id, state, next, false, Some(category), Some(reason)).await?;
    Ok(Resolved::Parked)
}

/// Upload the snapshot; `Some(code)` once the file code is committed.
async fn upload(app: &App, job: &Job) -> Result<Option<String>> {
    let staging = app.staging.clone();
    let id = job.id.clone();
    let (file, ident) = match tokio::task::spawn_blocking(move || staging.open_snapshot(&id)).await? {
        Ok(x) => x,
        Err(PathError::NotFound) => {
            review(app, job, "snapshot_missing", "the spool snapshot disappeared").await?;
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let recorded = recorded_identity(job);
    if !ident.unchanged(&recorded) {
        review(app, job, "snapshot_changed", "the staged content changed after it was queued").await?;
        return Ok(None);
    }

    let server = match app.katfile.request_upload_server().await {
        Ok(s) => s,
        Err(e) => {
            park_error(app, job, "upload_server", e.into()).await?;
            return Ok(None);
        }
    };
    app.db.mark_uploading(&job.id, server.server_time.as_deref()).await?;
    let attempt = app.db.start_attempt(&job.id, "upload").await?;
    let progress = UploadProgress::new();

    // Persist "whole body sent" as soon as it happens: it decides crash recovery.
    let watcher = {
        let (db, id, progress) = (app.db.clone(), job.id.clone(), progress.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if progress.body_complete() {
                    let _ = db.mark_body_sent(&id).await;
                    return;
                }
            }
        })
    };
    let req = UploadRequest {
        file: tokio::fs::File::from_std(file),
        size: job.size_bytes as u64,
        remote_name: job.remote_name.clone(),
        mode: app.cfg.katfile.upload_mode,
    };
    let result = app.katfile.upload(&server, req, progress.clone()).await;
    watcher.abort();
    if progress.body_complete() {
        app.db.mark_body_sent(&job.id).await?;
    }

    // Post-upload tamper check on the snapshot.
    let staging = app.staging.clone();
    let id = job.id.clone();
    let after = tokio::task::spawn_blocking(move || staging.snapshot_identity(&id)).await??;
    let changed = after.is_none_or(|a| !a.unchanged(&recorded));

    match result {
        Ok(receipt) => {
            // Commit the remote identity before anything else can fail.
            app.db.mark_remote_uploaded(&job.id, receipt.file_code.as_str(), Some(&receipt.sha256_hex)).await?;
            app.db.finish_attempt(attempt, "ok", None, Some(&format!("{} bytes", receipt.bytes_sent))).await?;
            if changed {
                review(app, job, "snapshot_changed_during_upload", "the staged file changed while uploading").await?;
                return Ok(None);
            }
            info!(job = %job.id, file_code = %receipt.file_code, "uploaded; assigning folder");
            Ok(Some(receipt.file_code.to_string()))
        }
        Err(e) => {
            app.db.finish_attempt(attempt, class_name(&e), Some(e.category()), Some(&e.to_string())).await?;
            if e.class() == ErrorClass::Ambiguous {
                return ambiguous(app, job, &e, progress.sha256_hex()).await;
            }
            if matches!(e, KatFileError::LocalFileChanged) || changed {
                review(app, job, "snapshot_changed", "the staged file changed while uploading").await?;
                return Ok(None);
            }
            park_error(app, job, "upload", e.into()).await?;
            Ok(None)
        }
    }
}

/// The provider may have stored the file: adopt a unique match, otherwise ask a human.
async fn ambiguous(app: &App, job: &Job, err: &KatFileError, sha256: Option<String>) -> Result<Option<String>> {
    let job = app.db.job(&job.id).await?.context("job vanished")?;
    match reconcile_ambiguous(app, &job).await {
        Ok(Some(code)) => {
            info!(job = %job.id, file_code = %code, "ambiguous upload reconciled with a unique remote match");
            app.db.mark_remote_uploaded(&job.id, code.as_str(), sha256.as_deref()).await?;
            Ok(Some(code.to_string()))
        }
        Ok(None) => {
            review(app, &job, "ambiguous_upload", &format!("{err}; no unique remote match yet")).await?;
            Ok(None)
        }
        Err(e) => {
            review(app, &job, "ambiguous_upload", &format!("{err}; reconciliation failed: {e:#}")).await?;
            Ok(None)
        }
    }
}

/// Find exactly one unclaimed remote file with this name and size, uploaded after the
/// session was issued (provider clock strings sort chronologically).
pub async fn reconcile_ambiguous(app: &App, job: &Job) -> Result<Option<FileCode>> {
    let hits = app.katfile.find_account_files_named(&job.remote_name, 20).await?;
    let mut candidates = Vec::new();
    for f in hits {
        if f.size != Some(job.size_bytes as u64) {
            continue;
        }
        if let (Some(since), Some(uploaded)) = (&job.upload_server_time, &f.uploaded)
            && uploaded < since
        {
            continue;
        }
        if app.db.file_code_claimed(f.file_code.as_str()).await? {
            continue;
        }
        candidates.push(f.file_code);
    }
    Ok(if candidates.len() == 1 { candidates.pop() } else { None })
}

async fn review(app: &App, job: &Job, category: &str, msg: &str) -> Result<()> {
    warn!(job = %job.id, user = %job.username, category, msg, "job needs review");
    app.db.park_job(&job.id, JobState::NeedsReview, 0, false, Some(category), Some(msg)).await
}

/// Map an error to retry / failure according to its class.
async fn park_error(app: &App, job: &Job, stage: &str, err: anyhow::Error) -> Result<()> {
    let msg = format!("{stage}: {err:#}");
    let class = err.downcast_ref::<KatFileError>().map(|k| (k.class(), k.category()));
    match class {
        Some((ErrorClass::Auth, cat)) => {
            app.note_katfile_auth_error(Some(err.to_string()));
            error!(job = %job.id, "KatFile rejected the API key; pausing this job for 15 minutes");
            app.db.park_job(&job.id, JobState::RetryWaiting, now_ms() + 15 * 60_000, false, Some(cat), Some(&msg)).await
        }
        Some((ErrorClass::Permanent, cat)) => {
            error!(job = %job.id, error = %msg, "permanent failure; local copy kept");
            app.db.park_job(&job.id, JobState::Failed, 0, true, Some(cat), Some(&msg)).await
        }
        other => {
            let category = other.map(|(_, c)| c).unwrap_or("internal");
            if job.attempts + 1 >= i64::from(app.cfg.retry_max) {
                error!(job = %job.id, attempts = job.attempts + 1, error = %msg, "retry limit reached; local copy kept");
                app.db.park_job(&job.id, JobState::Failed, 0, true, Some(category), Some(&msg)).await
            } else {
                let delay = retry_delay_ms(job.attempts);
                warn!(job = %job.id, attempt = job.attempts + 1, delay_ms = delay, error = %msg, "transient failure; will retry");
                app.db
                    .park_job(&job.id, JobState::RetryWaiting, now_ms() + delay, true, Some(category), Some(&msg))
                    .await
            }
        }
    }
}

/// Exponential backoff with +-20% jitter: 30 s, 60 s, ... capped at one hour.
fn retry_delay_ms(attempts: i64) -> i64 {
    let base = (30_000i64 << attempts.clamp(0, 7)).min(3_600_000);
    let jitter = (base / 5) as u64;
    base - (jitter as i64) + fastrand::u64(0..=2 * jitter) as i64
}

fn class_name(e: &KatFileError) -> &'static str {
    match e.class() {
        ErrorClass::Transient => "transient",
        ErrorClass::Permanent => "permanent",
        ErrorClass::Auth => "auth",
        ErrorClass::Ambiguous => "ambiguous",
    }
}

fn recorded_identity(job: &Job) -> Identity {
    Identity {
        dev: job.snap_dev as u64,
        ino: job.snap_ino as u64,
        size: job.size_bytes as u64,
        mtime_ns: job.snap_mtime_ns,
        ctime_ns: 0,
        nlink: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_grows_and_is_capped() {
        for _ in 0..50 {
            let d0 = retry_delay_ms(0);
            assert!((24_000..=36_000).contains(&d0), "{d0}");
            let d10 = retry_delay_ms(10);
            assert!(d10 <= 3_600_000 + 720_000, "{d10}");
        }
    }
}
