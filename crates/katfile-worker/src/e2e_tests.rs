//! V2 acceptance scenarios, in process: real webhook routes, SQLite, staging volume,
//! mock KatFile (with fault injection) and a mock SFTPGo admin API.

use katfile_mock::Fault;
use reqwest::Method;
use serde_json::json;

use crate::db::{JobState, PrincipalState};
use crate::testkit::{Harness, INBOX_PERMS};
use crate::util::now_ms;
use crate::{cleanup, provision, reconcile, runner};

/// Fixed SFTPGo `created_at` so re-putting a user keeps the same account generation.
const CREATED: i64 = 1_700_000_000_000;

async fn with_users(names: &[&str]) -> Harness {
    let h = Harness::new().await;
    for (i, n) in names.iter().enumerate() {
        h.sg.put_user(n, i as i64 + 1, CREATED, INBOX_PERMS, 1);
    }
    h.sync_users().await;
    h.settle().await;
    h
}

#[tokio::test]
async fn new_users_get_distinct_root_folders_once() {
    let h = with_users(&["alice", "bob"]).await;
    let (a, b) = (h.principal_folder("alice").await, h.principal_folder("bob").await);
    assert_ne!(a, b);
    let names: Vec<String> =
        h.kf.state().children(0).iter().map(|f| String::from_utf8(f.stored_name.clone()).unwrap()).collect();
    assert_eq!(names, vec!["alice", "bob"]);
    // Hook replay and periodic reconciliation never create a second folder.
    for _ in 0..3 {
        provision::sync_user(&h.app, "alice").await.unwrap();
        h.sync_users().await;
        h.settle().await;
    }
    assert_eq!(h.kf.state().children(0).len(), 2);
    assert_eq!(h.principal_folder("alice").await, a);
}

#[tokio::test]
async fn provider_hook_requires_auth_and_triggers_sync() {
    let h = Harness::new().await;
    h.sg.put_user("carol", 7, now_ms() - 1000, INBOX_PERMS, 1);
    let unauth = h
        .http
        .post(format!("{}/hooks/sftpgo/provider?action=add&object_type=user&object_name=carol", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
    let ok = h
        .http
        .post(format!("{}/hooks/sftpgo/provider?action=add&object_type=user&object_name=carol", h.base))
        .bearer_auth(crate::testkit::WEBHOOK_SECRET)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);
    for _ in 0..50 {
        if h.app.db.live_principal_by_username("carol").await.unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    h.settle().await;
    assert!(h.principal_folder("carol").await > 0);
}

#[tokio::test]
async fn uncertain_folder_create_is_resolved_by_listing_not_retried() {
    let h = Harness::new().await;
    h.sg.put_user("dave", 1, now_ms() - 1000, INBOX_PERMS, 1);
    h.kf.inject("folder/create", Fault::CreateThenFail);
    h.sync_users().await;
    h.settle().await;
    assert_eq!(h.kf.state().call_count("folder/create"), 1, "no blind second create");
    assert_eq!(h.kf.state().children(0).len(), 1);
    assert!(h.principal_folder("dave").await > 0);
}

#[tokio::test]
async fn worker_downtime_during_user_creation_is_recovered_once() {
    let h = Harness::new().await;
    // The add hook is lost; uploads arrive before the worker knows the user.
    h.sg.put_user("erin", 3, now_ms() - 5000, INBOX_PERMS, 1);
    assert_eq!(h.upload("erin", "/early.jpg", b"early bytes", "DAV").await, 200);
    h.settle().await;
    let job = h.job_for("erin", "/early.jpg").await;
    assert_eq!(job.state, JobState::Archived, "{job:?}");
    reconcile::run_once(&h.app).await;
    h.settle().await;
    assert_eq!(h.kf.state().children(0).len(), 1, "exactly one root folder");
}

#[tokio::test]
async fn files_of_each_user_land_in_their_own_folder_over_both_protocols() {
    let h = with_users(&["alice", "bob"]).await;
    assert_eq!(h.upload("alice", "/photos/2026/a.jpg", b"alice webdav", "DAV").await, 200);
    assert_eq!(h.upload("alice", "/docs/s.pdf", b"alice sftp", "SFTP").await, 200);
    assert_eq!(h.upload("bob", "/b.txt", b"bob webdav", "DAV").await, 200);
    assert_eq!(h.upload("bob", "/x/y.bin", b"bob sftp", "SFTP").await, 200);
    h.settle().await;
    for (user, path) in
        [("alice", "/photos/2026/a.jpg"), ("alice", "/docs/s.pdf"), ("bob", "/b.txt"), ("bob", "/x/y.bin")]
    {
        let job = h.job_for(user, path).await;
        assert_eq!(job.state, JobState::Archived, "{job:?}");
        // The file's folder chain must end at the user's own root folder.
        let mut folder = job.remote_folder_id.unwrap() as u64;
        let root = h.principal_folder(user).await;
        let st = h.kf.state();
        while folder != root {
            folder = st.folders.get(&folder).expect("folder exists").parent;
            assert_ne!(folder, 0, "{user}{path} escaped its root folder");
        }
        let code = job.remote_file_code.unwrap();
        assert_eq!(st.files[&code].folder, job.remote_folder_id.map(|f| f as u64));
    }
}

#[tokio::test]
async fn forged_payloads_cannot_cross_users() {
    let h = with_users(&["alice", "bob"]).await;
    h.put_file("alice", "/secret.txt", b"alice only");
    // Bob claims Alice's file through traversal, a forged physical path, or a bare name.
    let mut ev = h.upload_event("bob", "/../alice/secret.txt", 10, "DAV");
    assert_eq!(h.hook(ev.clone()).await, 400);
    ev = h.upload_event("bob", "/secret.txt", 10, "DAV");
    ev["path"] = json!(h.home_file("alice", "/secret.txt").to_string_lossy());
    assert_eq!(h.hook(ev).await, 400, "payload path outside bob's home must be refused");
    assert_eq!(h.hook(h.upload_event("bob", "/secret.txt", 10, "DAV")).await, 200);
    h.settle().await;
    assert!(h.jobs_for("bob", "/secret.txt").await.is_empty(), "nothing exists in bob's home to archive");
    let unauth = h
        .http
        .post(format!("{}/hooks/sftpgo/fs", h.base))
        .json(&h.upload_event("alice", "/secret.txt", 10, "DAV"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
}

#[tokio::test]
async fn duplicate_events_do_not_duplicate_jobs() {
    let h = with_users(&["alice"]).await;
    h.put_file("alice", "/dup.bin", b"same upload");
    let ev = h.upload_event("alice", "/dup.bin", 11, "SFTP");
    for _ in 0..3 {
        assert_eq!(h.hook(ev.clone()).await, 200);
    }
    assert_eq!(h.jobs_for("alice", "/dup.bin").await.len(), 1);
    h.settle().await;
    assert_eq!(h.kf.state().call_count("upload.cgi"), 1);
}

#[tokio::test]
async fn folder_assignment_failure_retries_without_reupload() {
    let h = with_users(&["alice"]).await;
    for _ in 0..3 {
        h.kf.inject("file/set_folder", Fault::HttpStatus(500));
    }
    assert_eq!(h.upload("alice", "/v.mp4", b"video bytes", "DAV").await, 200);
    runner::process_due_once(&h.app).await.unwrap();
    let job = h.job_for("alice", "/v.mp4").await;
    assert_eq!(job.state, JobState::RetryWaiting);
    assert!(job.remote_file_code.is_some(), "file code committed before the folder step");
    h.settle().await;
    assert_eq!(h.job_for("alice", "/v.mp4").await.state, JobState::Archived);
    assert_eq!(h.kf.state().call_count("upload.cgi"), 1, "never re-uploaded");
}

#[tokio::test]
async fn ambiguous_upload_is_reconciled_or_flagged_never_reuploaded() {
    let h = with_users(&["alice"]).await;
    // Reply lost after the provider stored the file: adopted by exact name/size match.
    h.kf.inject("upload.cgi", Fault::StoreThenFail);
    assert_eq!(h.upload("alice", "/lost.bin", b"lost reply", "DAV").await, 200);
    h.settle().await;
    assert_eq!(h.job_for("alice", "/lost.bin").await.state, JobState::Archived);
    assert_eq!(h.kf.state().call_count("upload.cgi"), 1);

    // An unrelated, unclaimed remote file with the same name and size: a human decides.
    h.kf.add_file("same.bin", 9, 0, "2099-01-01 00:00:00");
    h.kf.inject("upload.cgi", Fault::StoreThenFail);
    assert_eq!(h.upload("alice", "/twin/same.bin", b"identical", "DAV").await, 200);
    h.settle().await;
    let b = h.job_for("alice", "/twin/same.bin").await;
    assert_eq!(b.state, JobState::NeedsReview, "{b:?}");
    assert_eq!(b.last_error_category.as_deref(), Some("ambiguous_upload"));
    assert!(b.remote_file_code.is_none());
    assert_eq!(h.kf.state().call_count("upload.cgi"), 2, "no automatic re-upload");
    // The operator picks the right remote file explicitly.
    let mine =
        h.kf.state()
            .files
            .values()
            .find(|f| f.size == 9 && f.uploaded != "2099-01-01 00:00:00" && f.folder == Some(0))
            .unwrap()
            .code
            .clone();
    let (code, _) =
        h.admin(Method::POST, &format!("/admin/jobs/{}/adopt", b.id), Some(json!({"file_code": mine}))).await;
    assert_eq!(code, 200);
    h.settle().await;
    assert_eq!(h.job_for("alice", "/twin/same.bin").await.state, JobState::Archived);
}

#[tokio::test]
async fn anonymous_storage_is_never_reported_as_archived() {
    let h = with_users(&["alice"]).await;
    h.kf.inject("upload.cgi", Fault::StoreAnonymously);
    assert_eq!(h.upload("alice", "/private.jpg", b"private photo", "DAV").await, 200);
    h.settle().await;
    let job = h.job_for("alice", "/private.jpg").await;
    assert_eq!(job.state, JobState::NeedsReview);
    assert_eq!(job.last_error_category.as_deref(), Some("remote_ownership_unverified"));
}

#[tokio::test]
async fn restart_mid_upload_recovers_without_silent_loss() {
    let h = with_users(&["alice"]).await;
    // Crash before the body was sent: safe to upload again.
    assert_eq!(h.upload("alice", "/a.bin", b"interrupted early", "DAV").await, 200);
    let a = h.job_for("alice", "/a.bin").await;
    h.app.db.mark_uploading(&a.id, None).await.unwrap();
    // Crash after the whole body was sent and the provider stored it.
    assert_eq!(h.upload("alice", "/b.bin", b"interrupted late", "DAV").await, 200);
    let b = h.job_for("alice", "/b.bin").await;
    h.kf.inject("upload.cgi", Fault::StoreThenFail);
    h.settle().await; // stores b remotely, reply lost -> reconciled
    let b_after = h.job_for("alice", "/b.bin").await;
    assert_eq!(b_after.state, JobState::Archived);
    let _ = b;
    runner::recover_interrupted(&h.app).await.unwrap();
    assert_eq!(h.job_for("alice", "/a.bin").await.state, JobState::RetryWaiting);
    h.settle().await;
    assert_eq!(h.job_for("alice", "/a.bin").await.state, JobState::Archived);

    // Crash after body sent, nothing found remotely yet: review, not a blind re-upload.
    assert_eq!(h.upload("alice", "/c.bin", b"unknown outcome", "DAV").await, 200);
    let c = h.job_for("alice", "/c.bin").await;
    h.app.db.mark_uploading(&c.id, Some("2099-01-01 00:00:00")).await.unwrap();
    h.app.db.mark_body_sent(&c.id).await.unwrap();
    runner::recover_interrupted(&h.app).await.unwrap();
    let c = h.job_for("alice", "/c.bin").await;
    assert_eq!(c.state, JobState::NeedsReview);
    assert_eq!(c.last_error_category.as_deref(), Some("ambiguous_upload_crash"));
    // The operator may approve a re-upload explicitly.
    let (code, _) = h.admin(Method::POST, &format!("/admin/jobs/{}/retry", c.id), None).await;
    assert_eq!(code, 200);
    h.settle().await;
    assert_eq!(h.job_for("alice", "/c.bin").await.state, JobState::Archived);
}

#[tokio::test]
async fn user_deletes_or_renames_file_before_archival_content_is_still_archived() {
    let h = with_users(&["alice"]).await;
    assert_eq!(h.upload("alice", "/gone.txt", b"deleted by user", "SFTP").await, 200);
    std::fs::remove_file(h.home_file("alice", "/gone.txt")).unwrap();
    assert_eq!(h.upload("alice", "/moved.txt", b"renamed by user", "SFTP").await, 200);
    std::fs::rename(h.home_file("alice", "/moved.txt"), h.home_file("alice", "/elsewhere.txt")).unwrap();
    h.settle().await;
    let gone = h.job_for("alice", "/gone.txt").await;
    assert_eq!(gone.state, JobState::Archived);
    assert_eq!(h.kf.state().files[gone.remote_file_code.as_deref().unwrap()].size, 15);
    let moved = h.job_for("alice", "/moved.txt").await;
    assert_eq!(moved.state, JobState::Archived);
    // Cleanup later removes the renamed visible copy by inode.
    h.admin(Method::POST, &format!("/admin/jobs/{}/expire-retention", moved.id), None).await;
    cleanup::run_once(&h.app).await;
    assert!(!h.home_file("alice", "/elsewhere.txt").exists());
}

#[tokio::test]
async fn failed_and_unfinished_files_are_never_auto_deleted() {
    let h = Harness::with(|c| c.min_free_bytes = u64::MAX).await; // permanent disk pressure
    h.sg.put_user("alice", 1, now_ms() - 60_000, INBOX_PERMS, 1);
    h.sync_users().await;
    h.settle().await;
    h.kf.inject("upload.cgi", Fault::Reject("file type not allowed".into()));
    // Admission would refuse new uploads under this pressure, so stage directly.
    h.put_file("alice", "/rejected.exe", b"rejected content");
    assert_eq!(h.hook(h.upload_event("alice", "/rejected.exe", 16, "DAV")).await, 200);
    h.kf.inject("file/set_folder", Fault::HttpStatus(500));
    h.kf.inject("file/set_folder", Fault::HttpStatus(500));
    h.kf.inject("file/set_folder", Fault::HttpStatus(500));
    h.put_file("alice", "/retrying.bin", b"retrying content");
    assert_eq!(h.hook(h.upload_event("alice", "/retrying.bin", 16, "DAV")).await, 200);
    runner::process_due_once(&h.app).await.unwrap();
    assert_eq!(h.job_for("alice", "/rejected.exe").await.state, JobState::Failed);
    assert_eq!(h.job_for("alice", "/retrying.bin").await.state, JobState::RetryWaiting);
    let r = cleanup::run_once(&h.app).await;
    assert!(r.disk_pressure);
    assert_eq!(r.cleaned_jobs, 0);
    assert!(h.home_file("alice", "/rejected.exe").exists());
    assert!(h.home_file("alice", "/retrying.bin").exists());
}

#[tokio::test]
async fn retention_cleanup_removes_only_verified_copies() {
    let h = with_users(&["alice"]).await;
    assert_eq!(h.upload("alice", "/old.jpg", b"archived photo", "DAV").await, 200);
    h.settle().await;
    let job = h.job_for("alice", "/old.jpg").await;
    assert_eq!(job.state, JobState::Archived);
    assert_eq!(cleanup::run_once(&h.app).await.cleaned_jobs, 0, "retention not elapsed");
    // The user replaces the file with new content under the same name.
    std::fs::remove_file(h.home_file("alice", "/old.jpg")).unwrap();
    h.put_file("alice", "/old.jpg", b"brand new upload");
    let (code, _) = h.admin(Method::POST, &format!("/admin/jobs/{}/expire-retention", job.id), None).await;
    assert_eq!(code, 200);
    let r = cleanup::run_once(&h.app).await;
    assert_eq!(r.cleaned_jobs, 1);
    assert_eq!(std::fs::read(h.home_file("alice", "/old.jpg")).unwrap(), b"brand new upload");
    assert_eq!(h.job_for("alice", "/old.jpg").await.state, JobState::Cleaned);
    assert_eq!(h.sg.state.quota_scans.lock().unwrap().as_slice(), ["alice"]);
}

#[tokio::test]
async fn disabled_user_blocks_new_archives_until_reenabled() {
    let h = with_users(&["bob"]).await;
    h.sg.put_user("bob", 1, CREATED, INBOX_PERMS, 0);
    provision::sync_user(&h.app, "bob").await.unwrap();
    assert_eq!(h.upload("bob", "/later.txt", b"while disabled", "DAV").await, 200);
    h.settle().await;
    assert_eq!(h.job_for("bob", "/later.txt").await.state, JobState::Blocked);
    h.sg.put_user("bob", 1, CREATED, INBOX_PERMS, 1);
    provision::sync_user(&h.app, "bob").await.unwrap();
    h.settle().await;
    assert_eq!(h.job_for("bob", "/later.txt").await.state, JobState::Archived);
}

#[tokio::test]
async fn deleted_user_home_is_cleared_and_recreated_name_gets_a_new_folder() {
    let h = with_users(&["alice"]).await;
    let old_folder = h.principal_folder("alice").await;
    assert_eq!(h.upload("alice", "/kept/archived.jpg", b"archived before delete", "DAV").await, 200);
    h.settle().await;
    let archived = h.job_for("alice", "/kept/archived.jpg").await;
    assert_eq!(archived.state, JobState::Archived);
    // An upload that is still queued when the account is deleted.
    h.kf.inject("upload/server", Fault::HttpStatus(503));
    h.kf.inject("upload/server", Fault::HttpStatus(503));
    h.kf.inject("upload/server", Fault::HttpStatus(503));
    assert_eq!(h.upload("alice", "/pending.jpg", b"not archived yet", "DAV").await, 200);
    runner::process_due_once(&h.app).await.unwrap();

    h.sg.remove_user("alice");
    provision::sync_user(&h.app, "alice").await.unwrap();
    assert!(!h.users_dir().join("alice").exists(), "home moved out of SFTPGo's reach");
    let pending = h.job_for("alice", "/pending.jpg").await;
    assert_eq!(pending.state, JobState::Blocked, "{pending:?}");

    // Archived files of the deleted account are removed at once; the pending one stays.
    let r = cleanup::run_once(&h.app).await;
    assert_eq!(r.cleaned_jobs, 1);
    assert!(h.app.staging.deleted_homes().unwrap().len() == 1, "quarantine keeps the unarchived file");

    // Re-created username: new principal, new folder, the old folder is untouched.
    h.sg.put_user("alice", 9, now_ms(), INBOX_PERMS, 1);
    h.sync_users().await;
    h.settle().await;
    let new_folder = h.principal_folder("alice").await;
    assert_ne!(new_folder, old_folder);
    let new_name = {
        let st = h.kf.state();
        String::from_utf8(st.folders[&new_folder].stored_name.clone()).unwrap()
    };
    assert_eq!(new_name, "alice-2");
    assert_eq!(
        h.job_for("alice", "/pending.jpg").await.state,
        JobState::Blocked,
        "old uploads never move to the new account"
    );
    let principals = h.app.db.principals().await.unwrap();
    assert_eq!(principals.iter().filter(|p| p.state == PrincipalState::Deleted).count(), 1);

    // Operator discards the old account's unarchived file; the quarantine empties.
    let (code, _) = h.admin(Method::POST, &format!("/admin/jobs/{}/discard", pending.id), None).await;
    assert_eq!(code, 200);
    cleanup::run_once(&h.app).await;
    assert!(h.app.staging.deleted_homes().unwrap().is_empty());
}

#[tokio::test]
async fn username_recreated_before_worker_noticed_splits_the_home() {
    let h = with_users(&["frank"]).await;
    h.put_file("frank", "/old-account.txt", b"previous owner");
    std::thread::sleep(std::time::Duration::from_millis(20));
    // Deleted and re-created while the worker was down.
    h.sg.put_user("frank", 2, now_ms(), INBOX_PERMS, 1);
    std::thread::sleep(std::time::Duration::from_millis(20));
    h.put_file("frank", "/new-account.txt", b"new owner");
    h.sync_users().await;
    assert!(!h.home_file("frank", "/old-account.txt").exists());
    assert!(h.home_file("frank", "/new-account.txt").exists());
}

#[tokio::test]
async fn unsafe_permissions_block_archival_until_fixed() {
    let h = Harness::new().await;
    h.sg.put_user("gina", 1, CREATED, &["*"], 1);
    h.sync_users().await;
    let p = h.app.db.live_principal_by_username("gina").await.unwrap().unwrap();
    assert_eq!(p.state, PrincipalState::Blocked);
    assert!(p.state_reason.unwrap().contains("not allowed"));
    assert_eq!(h.upload("gina", "/x.txt", b"waiting", "DAV").await, 200);
    h.settle().await;
    assert_eq!(h.job_for("gina", "/x.txt").await.state, JobState::AwaitingPrincipal);
    h.sg.put_user("gina", 1, CREATED, INBOX_PERMS, 1);
    h.sync_users().await;
    h.settle().await;
    assert_eq!(h.job_for("gina", "/x.txt").await.state, JobState::Archived);
}

#[tokio::test]
async fn failed_provisioning_keeps_uploads_queued_until_approved() {
    let h = Harness::new().await;
    h.sg.put_user("hank", 1, now_ms() - 60_000, INBOX_PERMS, 1);
    h.kf.inject("folder/create", Fault::AppError(403, "Folder not exist or not yours".into()));
    h.sync_users().await;
    h.settle().await;
    let p = h.app.db.live_principal_by_username("hank").await.unwrap().unwrap();
    assert_eq!(p.state, PrincipalState::Failed);
    assert_eq!(h.upload("hank", "/q.txt", b"queued", "SFTP").await, 200);
    h.settle().await;
    assert_eq!(h.job_for("hank", "/q.txt").await.state, JobState::AwaitingPrincipal);
    let (s, body) = h.admin(Method::GET, "/admin/status", None).await;
    assert_eq!(s, 200);
    assert_eq!(body["principals"]["failed"], 1);
    let (code, _) = h
        .admin(Method::POST, &format!("/admin/principals/{}/approve", p.id), Some(json!({"folder_name": "hank-files"})))
        .await;
    assert_eq!(code, 200);
    h.settle().await;
    assert_eq!(h.job_for("hank", "/q.txt").await.state, JobState::Archived);
}

#[tokio::test]
async fn missed_hook_is_found_by_the_staging_scan() {
    let h = with_users(&["ivan"]).await;
    h.put_file("ivan", "/nested/dir/missed.jpg", b"hook never arrived");
    let r = reconcile::run_once(&h.app).await;
    assert_eq!(r.enqueued_missed_uploads, 1);
    assert_eq!(reconcile::run_once(&h.app).await.enqueued_missed_uploads, 0, "not rediscovered");
    h.settle().await;
    assert_eq!(h.job_for("ivan", "/nested/dir/missed.jpg").await.state, JobState::Archived);
}

#[tokio::test]
async fn non_ascii_nested_and_colliding_names() {
    let h = with_users(&["jane"]).await;
    assert_eq!(h.upload("jane", "/相片/2026/假期 \"一\".jpg", b"unicode", "DAV").await, 200);
    assert_eq!(h.upload("jane", "/a\"b/x.txt", b"quote dir", "DAV").await, 200);
    assert_eq!(h.upload("jane", "/a'b/y.txt", b"apostrophe dir", "DAV").await, 200);
    h.settle().await;
    for p in ["/相片/2026/假期 \"一\".jpg", "/a\"b/x.txt", "/a'b/y.txt"] {
        assert_eq!(h.job_for("jane", p).await.state, JobState::Archived, "{p}");
    }
    let job = h.job_for("jane", "/相片/2026/假期 \"一\".jpg").await;
    assert_eq!(job.remote_name, "假期 _一_.jpg");
}

#[tokio::test]
async fn empty_files_and_unaccepted_protocols_are_not_queued() {
    let h = with_users(&["kate"]).await;
    assert_eq!(h.upload("kate", "/empty.txt", b"", "DAV").await, 200);
    assert_eq!(h.upload("kate", "/ftp.txt", b"via ftp", "FTP").await, 200);
    let mut failed = h.upload_event("kate", "/failed.txt", 5, "DAV");
    failed["status"] = json!(2);
    h.put_file("kate", "/failed.txt", b"12345");
    assert_eq!(h.hook(failed).await, 200);
    assert!(h.app.db.recent_jobs(100).await.unwrap().is_empty());
}

#[tokio::test]
async fn admission_rejects_uploads_when_staging_space_is_short() {
    let h = Harness::with(|c| c.min_free_bytes = u64::MAX / 2).await;
    let pre = json!({"action": "pre-upload", "username": "alice", "virtual_path": "/big.mp4", "status": 1, "protocol": "DAV"});
    assert_eq!(h.hook(pre).await, 403);
    let h = Harness::new().await;
    let pre =
        json!({"action": "pre-upload", "username": "alice", "virtual_path": "/ok.mp4", "status": 1, "protocol": "DAV"});
    assert_eq!(h.hook(pre).await, 200);
}

#[tokio::test]
async fn sqlite_write_failure_returns_503_and_leaves_no_orphan_snapshot() {
    let h = with_users(&["leo"]).await;
    h.app
        .db
        .exec_raw("CREATE TRIGGER fail_insert BEFORE INSERT ON upload_jobs BEGIN SELECT RAISE(ABORT, 'simulated'); END")
        .await
        .unwrap();
    assert_eq!(h.upload("leo", "/x.bin", b"retry me", "DAV").await, 503);
    let orphans = h.app.staging.orphan_snapshots(&Default::default()).unwrap();
    assert!(orphans.is_empty(), "{orphans:?}");
    h.app.db.exec_raw("DROP TRIGGER fail_insert").await.unwrap();
    // SFTPGo retries the hook.
    assert_eq!(h.hook(h.upload_event("leo", "/x.bin", 8, "DAV")).await, 200);
    h.settle().await;
    assert_eq!(h.job_for("leo", "/x.bin").await.state, JobState::Archived);
}

#[tokio::test]
async fn sqlite_backup_restores_the_queue() {
    let h = with_users(&["mia"]).await;
    assert_eq!(h.upload("mia", "/a.txt", b"one", "DAV").await, 200);
    let dest = h.app.cfg.db_path.with_file_name("backup.db");
    h.app.db.backup_to(&dest).await.unwrap();
    let restored = crate::db::Db::open(&dest).await.unwrap();
    let jobs = restored.recent_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(restored.principals().await.unwrap().len(), 1);
}

#[tokio::test]
async fn admin_api_requires_token_and_reports_semantics() {
    let h = with_users(&["nina"]).await;
    let unauth = h.http.get(format!("{}/admin/status", h.base)).send().await.unwrap();
    assert_eq!(unauth.status().as_u16(), 401);
    let (code, body) = h.admin(Method::GET, "/admin/status", None).await;
    assert_eq!(code, 200);
    assert!(body["semantics"].as_str().unwrap().contains("only means the file is staged"));
    assert_eq!(body["principals"]["active"], 1);
}
