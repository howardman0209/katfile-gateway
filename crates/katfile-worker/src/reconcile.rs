//! Periodic reconciliation — the safety net for missed hooks and crashes:
//! users, staged files without jobs, ambiguous uploads, orphan snapshots,
//! homes of deleted users and stale SFTPGo temp files.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::app::{App, AppRef, idle};
use crate::db::{JobState, PrincipalState};
use crate::paths::HomeRef;
use crate::provision;
use crate::queue::{self, EnqueueOutcome, StagedUpload};
use crate::runner::reconcile_ambiguous;
use crate::util::{now_ms, rfc3339};

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub sftpgo_users: usize,
    pub enqueued_missed_uploads: u32,
    pub skipped_predating_files: u32,
    pub adopted_ambiguous: u32,
    pub orphan_snapshots_removed: u32,
    pub orphan_snapshots_kept: u32,
    pub homes_quarantined: u32,
    pub stale_temp_files_removed: u64,
    pub stale_temp_bytes_removed: u64,
    pub errors: Vec<String>,
}

pub async fn run(app: AppRef, mut stop: watch::Receiver<bool>) {
    loop {
        let report = run_once(&app).await;
        if !report.errors.is_empty() {
            warn!(errors = ?report.errors, "reconciliation finished with errors");
        }
        if idle(&app.reconcile_wake, app.cfg.reconcile_interval, &mut stop).await {
            return;
        }
    }
}

pub async fn run_once(app: &App) -> Report {
    let mut r = Report::default();
    let started = now_ms();

    match provision::sync_all(app).await {
        Ok(n) => r.sftpgo_users = n,
        Err(e) => r.errors.push(format!("user sync: {e:#}")),
    }
    if let Err(e) = retry_home_quarantine(app, &mut r).await {
        r.errors.push(format!("home quarantine: {e:#}"));
    }
    if let Err(e) = adopt_ambiguous(app, &mut r).await {
        r.errors.push(format!("ambiguous uploads: {e:#}"));
    }
    if let Err(e) = scan_staging(app, &mut r).await {
        r.errors.push(format!("staging scan: {e:#}"));
    }
    if let Err(e) = orphan_snapshots(app, &mut r).await {
        r.errors.push(format!("orphan snapshots: {e:#}"));
    }
    let staging = app.staging.clone();
    let after = app.cfg.stale_temp_after;
    match tokio::task::spawn_blocking(move || staging.sweep_stale_temp(after, now_ms())).await {
        Ok(Ok((files, bytes))) => {
            r.stale_temp_files_removed = files;
            r.stale_temp_bytes_removed = bytes;
        }
        Ok(Err(e)) => r.errors.push(format!("temp sweep: {e}")),
        Err(e) => r.errors.push(format!("temp sweep: {e}")),
    }

    let summary = serde_json::to_string(&r).unwrap_or_default();
    let _ = app.db.kv_set("last_reconcile", &format!("{} {summary}", rfc3339(started))).await;
    info!(
        users = r.sftpgo_users,
        missed = r.enqueued_missed_uploads,
        adopted = r.adopted_ambiguous,
        temp_removed = r.stale_temp_files_removed,
        errors = r.errors.len(),
        elapsed_ms = now_ms() - started,
        "reconciliation pass"
    );
    app.runner_wake.notify_one();
    r
}

/// Deleted principals whose home could not be moved earlier (no live successor).
async fn retry_home_quarantine(app: &App, r: &mut Report) -> Result<()> {
    for p in app.db.principals().await? {
        if p.state != PrincipalState::Deleted || p.home_quarantined_at_ms.is_some() {
            continue;
        }
        if app.db.live_principal_by_username(&p.username).await?.is_some() {
            continue; // handled by the split at re-creation time
        }
        let staging = app.staging.clone();
        let (u, pid) = (p.username.clone(), p.id.clone());
        let moved = tokio::task::spawn_blocking(move || -> Result<bool> {
            Ok(staging.quarantine_home(&u, &pid)? || staging.deleted_homes()?.contains(&pid))
        })
        .await??;
        if moved {
            app.db.mark_home_quarantined(&p.id, now_ms()).await?;
            r.homes_quarantined += 1;
        }
    }
    Ok(())
}

/// Retry reconciliation of ambiguous uploads; the provider may list them later.
async fn adopt_ambiguous(app: &App, r: &mut Report) -> Result<()> {
    let week_ago = now_ms() - 7 * 24 * 3_600_000;
    for job in app.db.jobs_in_state(JobState::NeedsReview, 1000).await? {
        let cat = job.last_error_category.as_deref().unwrap_or("");
        if !matches!(cat, "ambiguous_upload" | "ambiguous_upload_crash") || job.created_at_ms < week_ago {
            continue;
        }
        match reconcile_ambiguous(app, &job).await {
            Ok(Some(code)) => {
                info!(job = %job.id, file_code = %code, "ambiguous upload adopted by reconciliation");
                app.db.mark_remote_uploaded(&job.id, code.as_str(), None).await?;
                r.adopted_ambiguous += 1;
            }
            Ok(None) => {}
            Err(e) => r.errors.push(format!("job {}: {e:#}", job.id)),
        }
    }
    Ok(())
}

/// Files in live homes that no job holds (missed hook, worker downtime).
async fn scan_staging(app: &App, r: &mut Report) -> Result<()> {
    let held: HashSet<(i64, i64)> = app.db.held_inodes().await?;
    let min_age_ns = app.cfg.reconcile_min_age.as_nanos() as i64;
    let now_ns = now_ms() * 1_000_000;
    for p in app.db.live_principals().await? {
        let staging = app.staging.clone();
        let username = p.username.clone();
        let scan = tokio::task::spawn_blocking(move || staging.scan(&HomeRef::User(&username), 200_000)).await??;
        if scan.truncated {
            warn!(user = %p.username, "home scan truncated at 200000 entries");
        }
        for (rel, ident) in scan.files {
            if ident.size == 0 || held.contains(&(ident.dev as i64, ident.ino as i64)) {
                continue;
            }
            // Leave very fresh files to the hook path (the event may be in flight).
            if now_ns - ident.mtime_ns.max(ident.ctime_ns) < min_age_ns {
                continue;
            }
            // Content older than the account generation is never attributed to it.
            if ident.ctime_ns < p.sftpgo_created_at_ms * 1_000_000 {
                r.skipped_predating_files += 1;
                continue;
            }
            let up = StagedUpload {
                username: p.username.clone(),
                rel: rel.clone(),
                size: ident.size,
                protocol: "reconcile".into(),
                event_ts_ms: now_ms(),
                source: "reconcile",
            };
            match queue::enqueue(app, up).await {
                Ok(EnqueueOutcome::Queued(id)) => {
                    warn!(job = %id, user = %p.username, path = %rel.virtual_path(), "queued an upload whose hook was missed");
                    r.enqueued_missed_uploads += 1;
                }
                Ok(_) => {}
                Err(e) => r.errors.push(format!("{}{}: {e}", p.username, rel.virtual_path())),
            }
        }
    }
    Ok(())
}

/// Spool links without a job: removed when another link keeps the content, else reported.
async fn orphan_snapshots(app: &App, r: &mut Report) -> Result<()> {
    let known = app.db.job_ids_with_snapshot().await?;
    let staging = app.staging.clone();
    let orphans = tokio::task::spawn_blocking(move || staging.orphan_snapshots(&known)).await??;
    for (name, ident) in orphans {
        // Give a just-created link time to get its DB row.
        if now_ms() * 1_000_000 - ident.ctime_ns < Duration::from_secs(600).as_nanos() as i64 {
            continue;
        }
        let staging = app.staging.clone();
        let n = name.clone();
        match tokio::task::spawn_blocking(move || staging.remove_orphan_snapshot(&n, &ident)).await? {
            Ok(true) => r.orphan_snapshots_removed += 1,
            Ok(false) => {
                error!(spool_entry = %name, "orphan snapshot is the only copy of its content; kept for review");
                r.orphan_snapshots_kept += 1;
            }
            Err(e) => r.errors.push(format!("orphan {name}: {e}")),
        }
    }
    Ok(())
}
