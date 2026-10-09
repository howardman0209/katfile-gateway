//! Principal lifecycle: SFTPGo user record -> worker principal -> KatFile root folder.
//!
//! * SFTPGo is the source of truth; hooks only trigger a re-read through its API.
//! * A principal is one account *generation* (SFTPGo id + created_at), so a deleted
//!   and re-created username is a new principal with a new KatFile folder.
//! * Remote folder creation is not idempotent on KatFile, so every create is
//!   preceded by a persisted intent (the same-named folders that existed before).
//!   An uncertain outcome is resolved by listing again, never by blind retries.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use katfile_api::names::sanitize_folder_name;
use katfile_api::{ErrorClass, FolderId, FolderListing, KatFileError};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::app::{App, AppRef, idle};
use crate::db::{FolderIntent, JobState, Principal, PrincipalState};
use crate::paths::validate_username;
use crate::sftpgo::{Policy, SftpgoUser, evaluate_policy};
use crate::util::now_ms;

/// Re-read one user from SFTPGo and reconcile the local principal.
pub async fn sync_user(app: &App, username: &str) -> Result<()> {
    validate_username(username)?;
    let user = app.sftpgo.user(username).await?;
    apply_user(app, username, user.as_ref()).await
}

/// Full user reconciliation (startup and periodic): catches missed hooks.
pub async fn sync_all(app: &App) -> Result<usize> {
    let users = app.sftpgo.users().await?;
    let mut seen = HashSet::new();
    for u in &users {
        if validate_username(&u.username).is_err() {
            warn!(user = %u.username.escape_debug(), "SFTPGo user with an unsupported name is ignored");
            continue;
        }
        seen.insert(u.username.clone());
        if let Err(e) = apply_user(app, &u.username, Some(u)).await {
            warn!(user = %u.username, error = %format!("{e:#}"), "user reconciliation failed");
        }
    }
    for p in app.db.live_principals().await? {
        if !seen.contains(&p.username)
            && let Err(e) = apply_user(app, &p.username, None).await
        {
            warn!(user = %p.username, error = %format!("{e:#}"), "deleted-user reconciliation failed");
        }
    }
    Ok(users.len())
}

async fn apply_user(app: &App, username: &str, user: Option<&SftpgoUser>) -> Result<()> {
    let now = now_ms();
    let live = app.db.live_principal_by_username(username).await?;
    match (user, live) {
        (None, None) => Ok(()),
        (None, Some(p)) => on_deleted(app, &p, None).await,
        (Some(u), Some(p)) if p.sftpgo_user_id == u.id && p.sftpgo_created_at_ms == u.created_at => {
            refresh(app, &p, u, now).await
        }
        (Some(u), Some(p)) => {
            // Same username, different account generation: the old one was deleted.
            on_deleted(app, &p, Some(u.created_at)).await?;
            create(app, u, now).await
        }
        (Some(u), None) => create(app, u, now).await,
    }
}

fn policy_state(policy: &Policy) -> (PrincipalState, Option<String>) {
    match policy {
        Policy::Ok => (PrincipalState::Provisioning, None),
        Policy::Disabled(r) => (PrincipalState::Disabled, Some(r.clone())),
        Policy::Unsupported(r) => (PrincipalState::Blocked, Some(r.clone())),
    }
}

async fn create(app: &App, u: &SftpgoUser, now: i64) -> Result<()> {
    let (state, reason) = policy_state(&evaluate_policy(u, &app.cfg.users_dir, now));
    let p = Principal {
        id: uuid::Uuid::new_v4().to_string(),
        username: u.username.clone(),
        sftpgo_user_id: u.id,
        sftpgo_created_at_ms: u.created_at,
        state,
        state_reason: reason.clone(),
        remote_folder_id: None,
        remote_folder_name: None,
        approved_folder_name: None,
        approved_bind_folder_id: None,
        provision_attempts: 0,
        next_provision_at_ms: 0,
        verified_at_ms: Some(now),
        home_quarantined_at_ms: None,
        created_at_ms: now,
        updated_at_ms: now,
        deleted_at_ms: None,
    };
    if let Err(e) = app.db.insert_principal(&p).await {
        // A concurrent sync created it first; nothing else to do.
        if app.db.live_principal_by_username(&u.username).await?.is_some() {
            return Ok(());
        }
        return Err(e);
    }
    info!(user = %u.username, principal = %p.id, state = %state, reason = reason.as_deref().unwrap_or(""), "new principal");
    app.provisioner_wake.notify_one();
    Ok(())
}

async fn refresh(app: &App, p: &Principal, u: &SftpgoUser, now: i64) -> Result<()> {
    app.db.mark_principal_verified(&p.id, now).await?;
    let policy = evaluate_policy(u, &app.cfg.users_dir, now);
    let (target, reason) = match &policy {
        Policy::Ok => match (p.state, p.remote_folder_id) {
            // Provisioning failures that need an operator stay failed until approved.
            (PrincipalState::Failed, _) => (PrincipalState::Failed, p.state_reason.clone()),
            (_, Some(_)) => (PrincipalState::Active, None),
            (PrincipalState::Provisioning, None) => (PrincipalState::Provisioning, p.state_reason.clone()),
            (_, None) => (PrincipalState::Provisioning, None),
        },
        other => policy_state(other),
    };
    if target != p.state || reason != p.state_reason {
        app.db.set_principal_state(&p.id, target, reason.as_deref()).await?;
        info!(user = %p.username, from = %p.state, to = %target, reason = reason.as_deref().unwrap_or(""), "principal state changed");
        if target == PrincipalState::Active {
            let n = app.db.requeue_principal_jobs(&p.id, JobState::Blocked, "principal_disabled").await?;
            if n > 0 {
                info!(user = %p.username, jobs = n, "re-enabled user: blocked uploads requeued");
            }
            app.runner_wake.notify_one();
        }
        if target == PrincipalState::Provisioning {
            app.provisioner_wake.notify_one();
        }
    }
    Ok(())
}

/// The SFTPGo account behind `p` is gone (`recreated_at`: a new generation already exists).
async fn on_deleted(app: &App, p: &Principal, recreated_at: Option<i64>) -> Result<()> {
    let now = now_ms();
    app.db.tombstone_principal(&p.id, now, "user deleted in SFTPGo").await?;
    let blocked = app
        .db
        .block_unstarted_jobs(&p.id, "principal_deleted", "the account was deleted before this upload was archived")
        .await?;
    // Nobody can see archived files of a deleted account any more: clean them now.
    let expired = app.db.expire_retention_for_principal(&p.id, now).await?;
    info!(user = %p.username, principal = %p.id, blocked, expired, "principal tombstoned; remote folder and files are kept");

    let staging = app.staging.clone();
    let (username, pid) = (p.username.clone(), p.id.clone());
    let moved = tokio::task::spawn_blocking(move || -> Result<bool> {
        match recreated_at {
            // Only files that predate the new account leave the shared home.
            Some(created_ms) => {
                staging.split_recreated_home(&username, &pid, created_ms * 1_000_000)?;
                Ok(true)
            }
            None => Ok(staging.quarantine_home(&username, &pid)? || staging.deleted_homes()?.contains(&pid)),
        }
    })
    .await??;
    if moved {
        app.db.mark_home_quarantined(&p.id, now).await?;
    }
    Ok(())
}

/// Provisioner loop: create KatFile root folders for principals in `provisioning`.
pub async fn run(app: AppRef, mut stop: watch::Receiver<bool>) {
    loop {
        provision_due(&app).await;
        if idle(&app.provisioner_wake, Duration::from_secs(30), &mut stop).await {
            return;
        }
    }
}

/// One provisioning pass over all due principals.
pub async fn provision_due(app: &App) {
    match app.db.principals_due_provisioning(now_ms()).await {
        Ok(due) => {
            for p in due {
                if let Err(e) = provision_root(app, &p).await {
                    let permanent =
                        e.downcast_ref::<KatFileError>().is_some_and(|k| k.class() == ErrorClass::Permanent);
                    if permanent {
                        // Retrying cannot help (e.g. the configured parent folder is missing).
                        let _ = fail(app, &p, &format!("{e:#}")).await;
                    } else {
                        error!(user = %p.username, error = %format!("{e:#}"), "provisioning error; will retry");
                        let next = now_ms() + backoff_ms(p.provision_attempts);
                        let _ = app.db.schedule_provisioning(&p.id, next, Some(&format!("{e:#}"))).await;
                    }
                }
            }
        }
        Err(e) => error!(error = %format!("{e:#}"), "cannot list principals to provision"),
    }
}

fn backoff_ms(attempts: i64) -> i64 {
    let base: i64 = 30_000;
    base.saturating_mul(1 << attempts.clamp(0, 6)).min(30 * 60_000)
}

async fn provision_root(app: &App, p: &Principal) -> Result<()> {
    let parent = FolderId(app.cfg.katfile.users_parent_folder_id);

    if let Some(bind) = p.approved_bind_folder_id {
        return bind_existing(app, p, parent, bind).await;
    }
    // Resume an interrupted create first.
    if let Some(intent) = app.db.folder_intent(&p.id, "").await? {
        match resolve_intent(app, &intent).await? {
            IntentResolution::Created(id) => return activate(app, p, id, &intent.name).await,
            IntentResolution::NotCreated => app.db.delete_folder_intent(&p.id, "").await?,
            IntentResolution::Ambiguous(n) => {
                return fail(
                    app,
                    p,
                    &format!(
                        "{n} new folders named {:?} appeared under {parent}; approve with bind_folder_id to choose one",
                        intent.name
                    ),
                )
                .await;
            }
        }
    }

    let base = sanitize_folder_name(p.approved_folder_name.as_deref().unwrap_or(&p.username));
    let listing = app.katfile.list_folder(parent).await.map_err(|e| katfile_error(app, e))?;
    // Never attach an existing folder by name: choose a name nobody uses (case-insensitive).
    let Some(name) = choose_free_name(&base, &listing) else {
        return fail(app, p, &format!("no free folder name for {base:?} under {parent}")).await;
    };
    if name != base {
        info!(user = %p.username, folder = %name, "folder name {base:?} is taken; using a suffixed name");
    }
    create_with_intent(app, p, "", parent, &name).await.map(|_| ())
}

/// Create `name` under `parent` with a crash-safe intent; returns the new folder id.
async fn create_with_intent(
    app: &App,
    p: &Principal,
    rel_path: &str,
    parent: FolderId,
    name: &str,
) -> Result<FolderId> {
    let listing = app.katfile.list_folder(parent).await.map_err(|e| katfile_error(app, e))?;
    let pre: Vec<i64> = listing.folders.iter().filter(|f| f.name == name).map(|f| f.id.0 as i64).collect();
    let intent = FolderIntent { parent_folder_id: parent.0 as i64, name: name.to_owned(), pre_existing_ids: pre };
    app.db.put_folder_intent(&p.id, rel_path, &intent).await?;
    match app.katfile.create_folder(parent, name).await {
        Ok(id) => {
            // Confirm through a listing before trusting the id.
            let after = app.katfile.list_folder(parent).await.map_err(|e| katfile_error(app, e))?;
            if !after.folders.iter().any(|f| f.id == id && f.name == name) {
                return Err(anyhow!("created folder {id} is not listed under {parent}"));
            }
            finish_create(app, p, rel_path, id, name).await?;
            Ok(id)
        }
        Err(e) if matches!(e.class(), ErrorClass::Transient) => {
            warn!(user = %p.username, error = %e, "folder create outcome uncertain; re-listing before any retry");
            match resolve_intent(app, &intent).await? {
                IntentResolution::Created(id) => {
                    finish_create(app, p, rel_path, id, name).await?;
                    Ok(id)
                }
                IntentResolution::NotCreated => {
                    app.db.delete_folder_intent(&p.id, rel_path).await?;
                    Err(anyhow::Error::new(e).context("folder create failed; will retry"))
                }
                IntentResolution::Ambiguous(n) => {
                    Err(anyhow!("{n} folders named {name:?} appeared under {parent}; manual resolution required"))
                }
            }
        }
        Err(e) => Err(katfile_error(app, e)),
    }
}

async fn finish_create(app: &App, p: &Principal, rel_path: &str, id: FolderId, name: &str) -> Result<()> {
    if rel_path.is_empty() {
        activate(app, p, id, name).await?;
    } else {
        app.db.put_folder_mapping(&p.id, rel_path, id.0 as i64, name).await?;
    }
    app.db.delete_folder_intent(&p.id, rel_path).await
}

enum IntentResolution {
    Created(FolderId),
    NotCreated,
    Ambiguous(usize),
}

/// Compare the parent's listing with the pre-create snapshot.
async fn resolve_intent(app: &App, intent: &FolderIntent) -> Result<IntentResolution> {
    let listing =
        app.katfile.list_folder(FolderId(intent.parent_folder_id as u64)).await.map_err(|e| katfile_error(app, e))?;
    let fresh: Vec<FolderId> = listing
        .folders
        .iter()
        .filter(|f| f.name == intent.name && !intent.pre_existing_ids.contains(&(f.id.0 as i64)))
        .map(|f| f.id)
        .collect();
    Ok(match fresh.len() {
        0 => IntentResolution::NotCreated,
        1 => IntentResolution::Created(fresh[0]),
        n => IntentResolution::Ambiguous(n),
    })
}

async fn activate(app: &App, p: &Principal, id: FolderId, name: &str) -> Result<()> {
    if let Some(owner) = app.db.remote_folder_owner(id.0 as i64).await?
        && owner != p.id
    {
        return fail(app, p, &format!("folder {id} already belongs to principal {owner}")).await;
    }
    app.db.activate_principal(&p.id, id.0 as i64, name).await?;
    info!(user = %p.username, principal = %p.id, folder = id.0, name, "KatFile root folder provisioned");
    // Uploads waiting for this account can go now.
    for job in app.db.jobs_in_state(JobState::AwaitingPrincipal, 10_000).await? {
        if job.username == p.username {
            app.db
                .park_job(&job.id, JobState::AwaitingPrincipal, 0, false, job.last_error_category.as_deref(), None)
                .await?;
        }
    }
    app.runner_wake.notify_one();
    Ok(())
}

/// Administrator-approved binding to an existing folder (e.g. after restoring a backup).
async fn bind_existing(app: &App, p: &Principal, parent: FolderId, folder: i64) -> Result<()> {
    let listing = app.katfile.list_folder(parent).await.map_err(|e| katfile_error(app, e))?;
    let Some(f) = listing.folders.iter().find(|f| f.id.0 as i64 == folder) else {
        return fail(app, p, &format!("approved folder {folder} is not a child of {parent}")).await;
    };
    activate(app, p, f.id, &f.name.clone()).await
}

async fn fail(app: &App, p: &Principal, reason: &str) -> Result<()> {
    warn!(user = %p.username, reason, "provisioning needs an administrator");
    app.db.set_principal_state(&p.id, PrincipalState::Failed, Some(reason)).await
}

/// `base`, then `base-2` ... `base-99`, skipping names used by any child (case-insensitive).
fn choose_free_name(base: &str, listing: &FolderListing) -> Option<String> {
    let taken: HashSet<String> = listing.folders.iter().map(|f| f.name.to_lowercase()).collect();
    std::iter::once(base.to_owned())
        .chain((2..100).map(|n| format!("{base}-{n}")))
        .map(|n| sanitize_folder_name(&n))
        .find(|n| !taken.contains(&n.to_lowercase()))
}

/// Record authentication problems for the status page and pass the error on.
fn katfile_error(app: &App, e: KatFileError) -> anyhow::Error {
    if e.class() == ErrorClass::Auth {
        app.note_katfile_auth_error(Some(e.to_string()));
    }
    anyhow::Error::new(e)
}

/// KatFile folder for a job directory (`rel_dir` relative to the user's home), creating
/// missing levels below the principal's root folder.
pub async fn ensure_folder(app: &App, p: &Principal, rel_dir: &str) -> Result<FolderId> {
    let root = FolderId(p.remote_folder_id.context("principal has no root folder")? as u64);
    if rel_dir.is_empty() {
        return Ok(root);
    }
    let mut parent = root;
    let mut prefix = String::new();
    for comp in rel_dir.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(comp);
        if let Some(id) = app.db.folder_mapping(&p.id, &prefix).await? {
            parent = FolderId(id as u64);
            continue;
        }
        let name = sanitize_folder_name(comp);
        if let Some(intent) = app.db.folder_intent(&p.id, &prefix).await? {
            match resolve_intent(app, &intent).await? {
                IntentResolution::Created(id) => {
                    finish_create(app, p, &prefix, id, &intent.name).await?;
                    parent = id;
                    continue;
                }
                IntentResolution::NotCreated => app.db.delete_folder_intent(&p.id, &prefix).await?,
                IntentResolution::Ambiguous(n) => {
                    return Err(anyhow!("{n} folders named {:?} appeared; manual resolution required", intent.name));
                }
            }
        }
        // Inside the principal's own folder only the gateway creates sub-folders, so an
        // existing child with this name is reused (lowest id if a past race left two).
        let listing = app.katfile.list_folder(parent).await.map_err(|e| katfile_error(app, e))?;
        if let Some(existing) = listing.folders.iter().filter(|f| f.name == name).min_by_key(|f| f.id) {
            debug!(user = %p.username, path = %prefix, folder = existing.id.0, "reusing existing sub-folder");
            app.db.put_folder_mapping(&p.id, &prefix, existing.id.0 as i64, &name).await?;
            parent = existing.id;
            continue;
        }
        parent = create_with_intent(app, p, &prefix, parent, &name).await?;
    }
    Ok(parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use katfile_api::FolderEntry;

    fn listing(names: &[&str]) -> FolderListing {
        FolderListing {
            folders: names
                .iter()
                .enumerate()
                .map(|(i, n)| FolderEntry { id: FolderId(i as u64 + 1), name: (*n).into(), raw_name: (*n).into() })
                .collect(),
            files: vec![],
        }
    }

    #[test]
    fn free_name_skips_taken_names_case_insensitively() {
        assert_eq!(choose_free_name("alice", &listing(&["bob"])).as_deref(), Some("alice"));
        assert_eq!(choose_free_name("alice", &listing(&["Alice"])).as_deref(), Some("alice-2"));
        assert_eq!(choose_free_name("alice", &listing(&["alice", "alice-2"])).as_deref(), Some("alice-3"));
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff_ms(0), 30_000);
        assert_eq!(backoff_ms(2), 120_000);
        assert_eq!(backoff_ms(50), 30 * 60_000);
    }
}
