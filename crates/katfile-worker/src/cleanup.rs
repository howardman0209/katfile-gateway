//! Retention-based cleanup. Only `archived` jobs (remote copy verified in the user's
//! folder) are touched; failed, retrying, blocked and ambiguous content is never
//! deleted automatically. Under disk pressure verified files are removed early.

use std::collections::HashSet;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::app::{App, AppRef, idle};
use crate::db::{Job, JobState};
use crate::paths::{HomeRef, Identity, RelPath, VisibleCleanup};
use crate::util::now_ms;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub cleaned_jobs: u32,
    pub removed_files: u32,
    pub kept_newer_files: u32,
    pub stranded: Vec<String>,
    pub disk_pressure: bool,
    pub deleted_homes_pruned: u32,
    pub errors: Vec<String>,
}

pub async fn run(app: AppRef, mut stop: watch::Receiver<bool>) {
    let wake = tokio::sync::Notify::new();
    loop {
        let r = run_once(&app).await;
        if !r.errors.is_empty() {
            warn!(errors = ?r.errors, "cleanup finished with errors");
        }
        if idle(&wake, app.cfg.cleanup_interval, &mut stop).await {
            return;
        }
    }
}

pub async fn run_once(app: &App) -> Report {
    let mut r = Report::default();
    let staging = app.staging.clone();
    let free = match tokio::task::spawn_blocking(move || staging.free_bytes()).await {
        Ok(Ok(f)) => f,
        _ => u64::MAX,
    };
    r.disk_pressure = free < app.cfg.min_free_bytes;
    if r.disk_pressure {
        warn!(
            free,
            min_free = app.cfg.min_free_bytes,
            "disk pressure: cleaning verified archives before their retention ends"
        );
    }
    let mut touched_users = HashSet::new();
    match app.db.cleanup_candidates(now_ms(), r.disk_pressure, 500).await {
        Ok(jobs) => {
            for job in jobs {
                match clean_job(app, &job).await {
                    Ok(out) => {
                        r.cleaned_jobs += 1;
                        r.removed_files += out.removed;
                        r.kept_newer_files += out.kept_other;
                        r.stranded.extend(out.stranded);
                        touched_users.insert(job.username.clone());
                    }
                    Err(e) => r.errors.push(format!("job {}: {e:#}", job.id)),
                }
                if r.disk_pressure
                    && app.staging.free_bytes().unwrap_or(0) > app.cfg.min_free_bytes.saturating_mul(5) / 4
                {
                    break;
                }
            }
        }
        Err(e) => r.errors.push(format!("listing candidates: {e:#}")),
    }

    // SFTPGo counts quota itself; files removed behind its back need a rescan.
    if app.cfg.quota_scan_after_cleanup {
        for user in &touched_users {
            if let Ok(Some(_)) = app.db.live_principal_by_username(user).await
                && let Err(e) = app.sftpgo.start_quota_scan(user).await
            {
                warn!(user = %user, error = %format!("{e:#}"), "quota scan request failed");
            }
        }
    }

    let staging = app.staging.clone();
    match tokio::task::spawn_blocking(move || -> Result<u32> {
        let mut n = 0;
        for pid in staging.deleted_homes()? {
            if staging.prune_deleted_home(&pid)? {
                info!(principal = %pid, "deleted user's home is empty and was removed");
                n += 1;
            }
        }
        Ok(n)
    })
    .await
    {
        Ok(Ok(n)) => r.deleted_homes_pruned = n,
        Ok(Err(e)) => r.errors.push(format!("deleted homes: {e:#}")),
        Err(e) => r.errors.push(format!("deleted homes: {e}")),
    }
    if r.cleaned_jobs > 0 || r.deleted_homes_pruned > 0 {
        info!(jobs = r.cleaned_jobs, files = r.removed_files, homes = r.deleted_homes_pruned, "cleanup pass");
    }
    r
}

/// Remove the visible copies and the snapshot of one archived job, then mark it cleaned.
pub async fn clean_job(app: &App, job: &Job) -> Result<VisibleCleanup> {
    anyhow::ensure!(job.state == JobState::Archived, "only archived jobs are cleaned");
    let out = remove_local_copies(app, job).await?;
    app.db.mark_cleaned(&job.id).await?;
    info!(job = %job.id, user = %job.username, path = %job.virtual_path, removed = out.removed, "local copies removed after verified archive");
    Ok(out)
}

/// Operator decision to drop a parked job's local content (it will not be archived).
pub async fn discard_job(app: &App, job: &Job) -> Result<VisibleCleanup> {
    anyhow::ensure!(
        matches!(job.state, JobState::NeedsReview | JobState::Failed | JobState::Blocked),
        "only needs_review, failed or blocked jobs can be discarded (state is {})",
        job.state
    );
    let out = remove_local_copies(app, job).await?;
    anyhow::ensure!(app.db.mark_discarded(&job.id).await?, "job changed state while being discarded");
    warn!(job = %job.id, user = %job.username, path = %job.virtual_path, "job discarded by the operator; local copies removed");
    Ok(out)
}

/// Inode-verified removal of the visible copies (live or quarantined home) and the snapshot.
async fn remove_local_copies(app: &App, job: &Job) -> Result<VisibleCleanup> {
    let quarantined = match &job.principal_id {
        Some(pid) => app.db.principal(pid).await?.and_then(|p| p.home_quarantined_at_ms).is_some(),
        None => false,
    };
    let rel = RelPath::parse_virtual(&job.virtual_path)?;
    let ident = Identity {
        dev: job.snap_dev as u64,
        ino: job.snap_ino as u64,
        size: job.size_bytes as u64,
        mtime_ns: job.snap_mtime_ns,
        ctime_ns: 0,
        nlink: 0,
    };
    let staging = app.staging.clone();
    let (id, username, pid) = (job.id.clone(), job.username.clone(), job.principal_id.clone().unwrap_or_default());
    tokio::task::spawn_blocking(move || -> Result<VisibleCleanup> {
        let home = if quarantined { HomeRef::Deleted(&pid) } else { HomeRef::User(&username) };
        let out = staging.remove_visible_copies(&home, &rel, &ident, &id)?;
        staging.release_snapshot(&id, &ident)?;
        Ok(out)
    })
    .await?
}
