//! Durable enqueueing: snapshot the staged file, then commit the job to SQLite.
//! The webhook only answers 200 after the commit.

use katfile_api::names::sanitize_file_name;
use tracing::{debug, info};

use crate::app::App;
use crate::db::{InsertOutcome, NewJob};
use crate::paths::{PathError, RelPath, SnapshotResult};

/// A completed upload found by an event or by a staging scan.
#[derive(Debug, Clone)]
pub struct StagedUpload {
    pub username: String,
    pub rel: RelPath,
    pub size: u64,
    pub protocol: String,
    pub event_ts_ms: i64,
    pub source: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued(String),
    /// The same staged inode is already queued (e.g. a retried hook delivery).
    Duplicate,
    /// The path no longer holds the uploaded content; a newer event covers it.
    Superseded(&'static str),
    Skipped(&'static str),
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("invalid upload: {0}")]
    Invalid(String),
    #[error(transparent)]
    Retryable(#[from] anyhow::Error),
}

pub async fn enqueue(app: &App, up: StagedUpload) -> Result<EnqueueOutcome, EnqueueError> {
    if up.size == 0 {
        // KatFile rejects empty files ("null filesize"), so they are never queued.
        debug!(user = %up.username, path = %up.rel.virtual_path(), "empty upload not archived");
        return Ok(EnqueueOutcome::Skipped("empty file"));
    }
    let job_id = uuid::Uuid::new_v4().to_string();
    let staging = app.staging.clone();
    let (username, rel, size, id) = (up.username.clone(), up.rel.clone(), up.size, job_id.clone());
    let snap = tokio::task::spawn_blocking(move || staging.snapshot(&username, &rel, size, &id))
        .await
        .map_err(|e| EnqueueError::Retryable(e.into()))?;
    let ident = match snap {
        Ok(SnapshotResult::Created(ident)) => ident,
        Ok(SnapshotResult::Missing) => return Ok(EnqueueOutcome::Superseded("file no longer present")),
        Ok(SnapshotResult::SizeMismatch(_)) => return Ok(EnqueueOutcome::Superseded("file changed after the upload")),
        Ok(SnapshotResult::NotRegularFile) => return Ok(EnqueueOutcome::Superseded("not a regular file")),
        Ok(SnapshotResult::Raced) => return Ok(EnqueueOutcome::Superseded("path changed during snapshot")),
        Err(PathError::Invalid(m)) | Err(PathError::Unsafe(m)) => return Err(EnqueueError::Invalid(m)),
        Err(e) => return Err(EnqueueError::Retryable(e.into())),
    };

    let job = NewJob {
        id: job_id.clone(),
        username: up.username.clone(),
        virtual_path: up.rel.virtual_path(),
        rel_dir: up.rel.rel_dir(),
        file_name: up.rel.file_name().to_owned(),
        remote_name: sanitize_file_name(up.rel.file_name()),
        protocol: up.protocol.clone(),
        source: up.source,
        event_ts_ms: up.event_ts_ms,
        size_bytes: ident.size as i64,
        snap_dev: ident.dev as i64,
        snap_ino: ident.ino as i64,
        snap_mtime_ns: ident.mtime_ns,
    };
    match app.db.insert_job(&job).await {
        Ok(InsertOutcome::Inserted) => {
            info!(job = %job_id, user = %up.username, path = %job.virtual_path, size = job.size_bytes, source = up.source, "upload queued");
            app.runner_wake.notify_one();
            Ok(EnqueueOutcome::Queued(job_id))
        }
        Ok(InsertOutcome::Duplicate) => {
            // Another job already holds this inode: drop our extra link.
            let staging = app.staging.clone();
            let id = job_id.clone();
            let _ = tokio::task::spawn_blocking(move || staging.release_snapshot(&id, &ident)).await;
            debug!(user = %up.username, path = %job.virtual_path, "duplicate upload event ignored");
            Ok(EnqueueOutcome::Duplicate)
        }
        Err(e) => {
            // Undo the link so a retried delivery starts clean.
            let staging = app.staging.clone();
            let id = job_id.clone();
            let _ = tokio::task::spawn_blocking(move || staging.release_snapshot(&id, &ident)).await;
            Err(EnqueueError::Retryable(e))
        }
    }
}
