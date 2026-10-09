//! SQLite persistence: principals, folder mappings, upload jobs and attempts.
//!
//! Every state transition is an explicit, static SQL statement so the set of
//! possible transitions stays reviewable. Times are milliseconds since the epoch.

use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::Row;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};

use crate::util::now_ms;

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> Result<Self> {
                match s { $($text => Ok($name::$variant),)+ other => bail!("unknown {} {other:?}", stringify!($name)) }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }
    };
}

string_enum!(PrincipalState {
    Provisioning => "provisioning",
    Active => "active",
    Failed => "failed",
    Blocked => "blocked",
    Disabled => "disabled",
    Deleted => "deleted",
});

string_enum!(JobState {
    Pending => "pending",
    AwaitingPrincipal => "awaiting_principal",
    Uploading => "uploading",
    RemoteUploaded => "remote_uploaded",
    AssigningFolder => "assigning_folder",
    Verifying => "verifying",
    RetryWaiting => "retry_waiting",
    NeedsReview => "needs_review",
    Failed => "failed",
    Blocked => "blocked",
    Archived => "archived",
    Cleaned => "cleaned",
    Discarded => "discarded",
});

#[derive(Debug, Clone, serde::Serialize)]
pub struct Principal {
    pub id: String,
    pub username: String,
    pub sftpgo_user_id: i64,
    pub sftpgo_created_at_ms: i64,
    pub state: PrincipalState,
    pub state_reason: Option<String>,
    pub remote_folder_id: Option<i64>,
    pub remote_folder_name: Option<String>,
    pub approved_folder_name: Option<String>,
    pub approved_bind_folder_id: Option<i64>,
    pub provision_attempts: i64,
    pub next_provision_at_ms: i64,
    pub verified_at_ms: Option<i64>,
    pub home_quarantined_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub deleted_at_ms: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Job {
    pub id: String,
    pub username: String,
    pub principal_id: Option<String>,
    pub virtual_path: String,
    pub rel_dir: String,
    pub file_name: String,
    pub remote_name: String,
    pub protocol: String,
    pub source: String,
    pub event_ts_ms: i64,
    pub size_bytes: i64,
    pub snap_dev: i64,
    pub snap_ino: i64,
    pub snap_mtime_ns: i64,
    pub snapshot_held: bool,
    pub sha256: Option<String>,
    pub state: JobState,
    pub remote_file_code: Option<String>,
    pub remote_folder_id: Option<i64>,
    pub upload_started_at_ms: Option<i64>,
    pub upload_body_sent_at_ms: Option<i64>,
    pub upload_server_time: Option<String>,
    pub attempts: i64,
    pub next_attempt_at_ms: i64,
    pub last_error_category: Option<String>,
    pub last_error: Option<String>,
    pub archived_at_ms: Option<i64>,
    pub retain_until_ms: Option<i64>,
    pub cleaned_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Fields of a job created from an upload event or a staging scan.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub id: String,
    pub username: String,
    pub virtual_path: String,
    pub rel_dir: String,
    pub file_name: String,
    pub remote_name: String,
    pub protocol: String,
    pub source: &'static str,
    pub event_ts_ms: i64,
    pub size_bytes: i64,
    pub snap_dev: i64,
    pub snap_ino: i64,
    pub snap_mtime_ns: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Attempt {
    pub id: i64,
    pub stage: String,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub outcome: Option<String>,
    pub error_category: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FolderIntent {
    pub parent_folder_id: i64,
    pub name: String,
    pub pre_existing_ids: Vec<i64>,
}

/// Result of inserting a job.
#[derive(Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// Another live job already holds a snapshot of the same inode.
    Duplicate,
}

#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
}

/// `SELECT <all job columns> FROM upload_jobs <tail>` as a compile-time constant.
macro_rules! job_sql {
    ($tail:literal) => {
        concat!(
            "SELECT id, username, principal_id, virtual_path, rel_dir, file_name, remote_name, protocol, source, ",
            "event_ts_ms, size_bytes, snap_dev, snap_ino, snap_mtime_ns, snapshot_held, sha256, state, remote_file_code, ",
            "remote_folder_id, upload_started_at_ms, upload_body_sent_at_ms, upload_server_time, attempts, ",
            "next_attempt_at_ms, last_error_category, last_error, archived_at_ms, retain_until_ms, cleaned_at_ms, ",
            "created_at_ms, updated_at_ms FROM upload_jobs ",
            $tail
        )
    };
}

impl Db {
    /// Open (creating if needed) and migrate the database.
    pub async fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // Commit = fsync of the WAL: a 200 OK to SFTPGo means the job is on disk.
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new().max_connections(4).connect_with(opts).await.context("opening SQLite")?;
        sqlx::migrate!("./migrations").run(&pool).await.context("running migrations")?;
        Ok(Db { pool })
    }

    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Consistent online copy (`VACUUM INTO`) for backups.
    pub async fn backup_to(&self, dest: &Path) -> Result<()> {
        let dest = dest.to_str().context("backup path is not UTF-8")?.to_owned();
        sqlx::query("VACUUM INTO ?1").bind(dest).execute(&self.pool).await.context("VACUUM INTO")?;
        Ok(())
    }

    // ---- principals -------------------------------------------------------

    pub async fn insert_principal(&self, p: &Principal) -> Result<()> {
        sqlx::query(
            "INSERT INTO principals (id, username, sftpgo_user_id, sftpgo_created_at_ms, state, state_reason, \
             verified_at_ms, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        )
        .bind(&p.id)
        .bind(&p.username)
        .bind(p.sftpgo_user_id)
        .bind(p.sftpgo_created_at_ms)
        .bind(p.state.as_str())
        .bind(&p.state_reason)
        .bind(p.verified_at_ms)
        .bind(p.created_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn principal(&self, id: &str) -> Result<Option<Principal>> {
        let row = sqlx::query("SELECT * FROM principals WHERE id = ?1").bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(principal_from_row).transpose()
    }

    pub async fn live_principal_by_username(&self, username: &str) -> Result<Option<Principal>> {
        let row = sqlx::query("SELECT * FROM principals WHERE username = ?1 AND state <> 'deleted'")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(principal_from_row).transpose()
    }

    /// The account generation that owned `username` at `event_ts_ms` (SFTPGo usernames are
    /// unique at any instant, so the newest generation created before the event owns it).
    pub async fn principal_for_event(&self, username: &str, event_ts_ms: i64) -> Result<Option<Principal>> {
        let row = sqlx::query(
            "SELECT * FROM principals WHERE username = ?1 AND sftpgo_created_at_ms <= ?2 \
             ORDER BY sftpgo_created_at_ms DESC LIMIT 1",
        )
        .bind(username)
        .bind(event_ts_ms)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(principal_from_row).transpose()
    }

    pub async fn principals(&self) -> Result<Vec<Principal>> {
        let rows =
            sqlx::query("SELECT * FROM principals ORDER BY username, created_at_ms").fetch_all(&self.pool).await?;
        rows.iter().map(principal_from_row).collect()
    }

    pub async fn live_principals(&self) -> Result<Vec<Principal>> {
        let rows = sqlx::query("SELECT * FROM principals WHERE state <> 'deleted' ORDER BY username")
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(principal_from_row).collect()
    }

    pub async fn principals_due_provisioning(&self, now: i64) -> Result<Vec<Principal>> {
        let rows = sqlx::query(
            "SELECT * FROM principals WHERE state = 'provisioning' AND next_provision_at_ms <= ?1 ORDER BY created_at_ms",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(principal_from_row).collect()
    }

    pub async fn set_principal_state(&self, id: &str, state: PrincipalState, reason: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE principals SET state = ?2, state_reason = ?3, updated_at_ms = ?4 WHERE id = ?1")
            .bind(id)
            .bind(state.as_str())
            .bind(reason)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_principal_verified(&self, id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE principals SET verified_at_ms = ?2, updated_at_ms = ?2 WHERE id = ?1")
            .bind(id)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record a provisioning failure and when to try again.
    pub async fn schedule_provisioning(&self, id: &str, next_at: i64, reason: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE principals SET provision_attempts = provision_attempts + 1, next_provision_at_ms = ?2, \
             state_reason = ?3, updated_at_ms = ?4 WHERE id = ?1",
        )
        .bind(id)
        .bind(next_at)
        .bind(reason)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Bind the principal's KatFile root folder and activate it.
    pub async fn activate_principal(&self, id: &str, folder_id: i64, folder_name: &str) -> Result<()> {
        sqlx::query(
            "UPDATE principals SET remote_folder_id = ?2, remote_folder_name = ?3, state = 'active', \
             state_reason = NULL, updated_at_ms = ?4 WHERE id = ?1",
        )
        .bind(id)
        .bind(folder_id)
        .bind(folder_name)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Administrator decision for a blocked principal.
    pub async fn approve_principal(
        &self,
        id: &str,
        folder_name: Option<&str>,
        bind_folder_id: Option<i64>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE principals SET approved_folder_name = ?2, approved_bind_folder_id = ?3, state = 'provisioning', \
             state_reason = 'approved by administrator', next_provision_at_ms = 0, updated_at_ms = ?4 WHERE id = ?1",
        )
        .bind(id)
        .bind(folder_name)
        .bind(bind_folder_id)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn tombstone_principal(&self, id: &str, at: i64, reason: &str) -> Result<()> {
        sqlx::query(
            "UPDATE principals SET state = 'deleted', state_reason = ?3, deleted_at_ms = ?2, updated_at_ms = ?2 \
             WHERE id = ?1",
        )
        .bind(id)
        .bind(at)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_home_quarantined(&self, id: &str, at: i64) -> Result<()> {
        sqlx::query("UPDATE principals SET home_quarantined_at_ms = ?2, updated_at_ms = ?2 WHERE id = ?1")
            .bind(id)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Is this KatFile folder id already bound to any principal (live or tombstoned)?
    pub async fn remote_folder_owner(&self, folder_id: i64) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM principals WHERE remote_folder_id = ?1")
            .bind(folder_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<String, _>("id")))
    }

    // ---- folders ---------------------------------------------------------

    pub async fn folder_mapping(&self, principal_id: &str, rel_path: &str) -> Result<Option<i64>> {
        let row = sqlx::query("SELECT remote_folder_id FROM folder_mappings WHERE principal_id = ?1 AND rel_path = ?2")
            .bind(principal_id)
            .bind(rel_path)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<i64, _>("remote_folder_id")))
    }

    pub async fn put_folder_mapping(
        &self,
        principal_id: &str,
        rel_path: &str,
        folder_id: i64,
        name: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO folder_mappings (principal_id, rel_path, remote_folder_id, remote_name, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (principal_id, rel_path) DO NOTHING",
        )
        .bind(principal_id)
        .bind(rel_path)
        .bind(folder_id)
        .bind(name)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn folder_intent(&self, principal_id: &str, rel_path: &str) -> Result<Option<FolderIntent>> {
        let row = sqlx::query(
            "SELECT parent_folder_id, name, pre_existing_ids FROM folder_intents WHERE principal_id = ?1 AND rel_path = ?2",
        )
        .bind(principal_id)
        .bind(rel_path)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let ids: Vec<i64> = serde_json::from_str(&r.get::<String, _>("pre_existing_ids"))?;
            Ok(FolderIntent { parent_folder_id: r.get("parent_folder_id"), name: r.get("name"), pre_existing_ids: ids })
        })
        .transpose()
    }

    /// Persist the pre-create snapshot before calling `folder/create`.
    pub async fn put_folder_intent(&self, principal_id: &str, rel_path: &str, intent: &FolderIntent) -> Result<()> {
        sqlx::query(
            "INSERT INTO folder_intents (principal_id, rel_path, parent_folder_id, name, pre_existing_ids, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (principal_id, rel_path) DO UPDATE SET \
             parent_folder_id = excluded.parent_folder_id, name = excluded.name, \
             pre_existing_ids = excluded.pre_existing_ids, created_at_ms = excluded.created_at_ms",
        )
        .bind(principal_id)
        .bind(rel_path)
        .bind(intent.parent_folder_id)
        .bind(&intent.name)
        .bind(serde_json::to_string(&intent.pre_existing_ids)?)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_folder_intent(&self, principal_id: &str, rel_path: &str) -> Result<()> {
        sqlx::query("DELETE FROM folder_intents WHERE principal_id = ?1 AND rel_path = ?2")
            .bind(principal_id)
            .bind(rel_path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- jobs ------------------------------------------------------------

    pub async fn insert_job(&self, j: &NewJob) -> Result<InsertOutcome> {
        let now = now_ms();
        let res = sqlx::query(
            "INSERT INTO upload_jobs (id, username, virtual_path, rel_dir, file_name, remote_name, protocol, source, \
             event_ts_ms, size_bytes, snap_dev, snap_ino, snap_mtime_ns, snapshot_held, state, next_attempt_at_ms, \
             created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1, 'pending', 0, ?14, ?14)",
        )
        .bind(&j.id)
        .bind(&j.username)
        .bind(&j.virtual_path)
        .bind(&j.rel_dir)
        .bind(&j.file_name)
        .bind(&j.remote_name)
        .bind(&j.protocol)
        .bind(j.source)
        .bind(j.event_ts_ms)
        .bind(j.size_bytes)
        .bind(j.snap_dev)
        .bind(j.snap_ino)
        .bind(j.snap_mtime_ns)
        .bind(now)
        .execute(&self.pool)
        .await;
        match res {
            Ok(_) => Ok(InsertOutcome::Inserted),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(InsertOutcome::Duplicate),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn job(&self, id: &str) -> Result<Option<Job>> {
        let row = sqlx::query(job_sql!("WHERE id = ?1")).bind(id).fetch_optional(&self.pool).await?;
        row.as_ref().map(job_from_row).transpose()
    }

    /// Inodes currently held by live snapshots (for staging scans).
    pub async fn held_inodes(&self) -> Result<std::collections::HashSet<(i64, i64)>> {
        let rows = sqlx::query("SELECT snap_dev, snap_ino FROM upload_jobs WHERE snapshot_held = 1")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| (r.get::<i64, _>("snap_dev"), r.get::<i64, _>("snap_ino"))).collect())
    }

    pub async fn job_ids_with_snapshot(&self) -> Result<std::collections::HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM upload_jobs WHERE snapshot_held = 1").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    /// Runnable jobs whose retry time has come, oldest first.
    pub async fn due_jobs(&self, now: i64, limit: i64) -> Result<Vec<Job>> {
        let rows = sqlx::query(job_sql!(
            "WHERE state IN ('pending', 'awaiting_principal', 'retry_waiting', \
             'remote_uploaded', 'assigning_folder', 'verifying') AND next_attempt_at_ms <= ?1 \
             ORDER BY next_attempt_at_ms, created_at_ms LIMIT ?2"
        ))
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(job_from_row).collect()
    }

    pub async fn jobs_in_state(&self, state: JobState, limit: i64) -> Result<Vec<Job>> {
        let rows = sqlx::query(job_sql!("WHERE state = ?1 ORDER BY created_at_ms LIMIT ?2"))
            .bind(state.as_str())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(job_from_row).collect()
    }

    pub async fn recent_jobs(&self, limit: i64) -> Result<Vec<Job>> {
        let rows =
            sqlx::query(job_sql!("ORDER BY created_at_ms DESC LIMIT ?1")).bind(limit).fetch_all(&self.pool).await?;
        rows.iter().map(job_from_row).collect()
    }

    /// Archived jobs whose local copies may be removed now (or all archived jobs under disk pressure).
    pub async fn cleanup_candidates(&self, now: i64, pressure: bool, limit: i64) -> Result<Vec<Job>> {
        let rows = sqlx::query(job_sql!(
            "WHERE state = 'archived' AND (retain_until_ms <= ?1 OR ?2) \
             ORDER BY archived_at_ms LIMIT ?3"
        ))
        .bind(now)
        .bind(pressure)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(job_from_row).collect()
    }

    pub async fn bind_job_principal(&self, job_id: &str, principal_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE upload_jobs SET principal_id = ?2, updated_at_ms = ?3 WHERE id = ?1 AND principal_id IS NULL",
        )
        .bind(job_id)
        .bind(principal_id)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Generic waiting transition (awaiting_principal / retry_waiting / blocked / needs_review / failed).
    pub async fn park_job(
        &self,
        id: &str,
        state: JobState,
        next_at: i64,
        bump_attempts: bool,
        category: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE upload_jobs SET state = ?2, next_attempt_at_ms = ?3, attempts = attempts + ?4, \
             last_error_category = ?5, last_error = ?6, updated_at_ms = ?7 WHERE id = ?1",
        )
        .bind(id)
        .bind(state.as_str())
        .bind(next_at)
        .bind(i64::from(bump_attempts))
        .bind(category)
        .bind(error)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persisted before any byte is sent, so a crash leaves a recoverable marker.
    pub async fn mark_uploading(&self, id: &str, server_time: Option<&str>) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE upload_jobs SET state = 'uploading', upload_started_at_ms = ?2, upload_body_sent_at_ms = NULL, \
             upload_server_time = ?3, updated_at_ms = ?2 WHERE id = ?1",
        )
        .bind(id)
        .bind(now)
        .bind(server_time)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_body_sent(&self, id: &str) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE upload_jobs SET upload_body_sent_at_ms = ?2, updated_at_ms = ?2 WHERE id = ?1 AND state = 'uploading'",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The remote file code is committed before any folder operation (never re-upload after this).
    pub async fn mark_remote_uploaded(&self, id: &str, code: &str, sha256: Option<&str>) -> Result<()> {
        sqlx::query(
            "UPDATE upload_jobs SET state = 'remote_uploaded', remote_file_code = ?2, sha256 = COALESCE(?3, sha256), \
             next_attempt_at_ms = 0, last_error_category = NULL, last_error = NULL, updated_at_ms = ?4 WHERE id = ?1",
        )
        .bind(id)
        .bind(code)
        .bind(sha256)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_assigning(&self, id: &str, folder_id: i64) -> Result<()> {
        sqlx::query(
            "UPDATE upload_jobs SET state = 'assigning_folder', remote_folder_id = ?2, updated_at_ms = ?3 WHERE id = ?1",
        )
        .bind(id)
        .bind(folder_id)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_verifying(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE upload_jobs SET state = 'verifying', updated_at_ms = ?2 WHERE id = ?1")
            .bind(id)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_archived(&self, id: &str, retain_until: i64) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE upload_jobs SET state = 'archived', archived_at_ms = ?2, retain_until_ms = ?3, \
             last_error_category = NULL, last_error = NULL, updated_at_ms = ?2 WHERE id = ?1",
        )
        .bind(id)
        .bind(now)
        .bind(retain_until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Operator request: make one archived job eligible for cleanup at `at`.
    pub async fn mark_archived_retention(&self, id: &str, at: i64) -> Result<()> {
        sqlx::query(
            "UPDATE upload_jobs SET retain_until_ms = ?2, updated_at_ms = ?2 WHERE id = ?1 AND state = 'archived'",
        )
        .bind(id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Shorten retention (e.g. the owner was deleted and nobody can see the file any more).
    pub async fn expire_retention_for_principal(&self, principal_id: &str, now: i64) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE upload_jobs SET retain_until_ms = ?2, updated_at_ms = ?2 WHERE principal_id = ?1 AND state = 'archived'",
        )
        .bind(principal_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Final transition after local copies were removed.
    pub async fn mark_cleaned(&self, id: &str) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            "UPDATE upload_jobs SET state = 'cleaned', snapshot_held = 0, cleaned_at_ms = ?2, updated_at_ms = ?2 \
             WHERE id = ?1 AND state = 'archived'",
        )
        .bind(id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Operator decision: give up on a job and release its snapshot.
    pub async fn mark_discarded(&self, id: &str) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE upload_jobs SET state = 'discarded', snapshot_held = 0, updated_at_ms = ?2 \
             WHERE id = ?1 AND state IN ('needs_review', 'failed', 'blocked')",
        )
        .bind(id)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Operator decision: retry a parked job from scratch (attempt counter reset).
    pub async fn operator_retry(&self, id: &str) -> Result<bool> {
        let res = sqlx::query(
            "UPDATE upload_jobs SET state = 'retry_waiting', attempts = 0, next_attempt_at_ms = 0, \
             last_error_category = 'operator_retry', updated_at_ms = ?2 \
             WHERE id = ?1 AND state IN ('needs_review', 'failed', 'blocked')",
        )
        .bind(id)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Requeue every job of a principal that was parked for `category` (e.g. re-enabled user).
    pub async fn requeue_principal_jobs(&self, principal_id: &str, state: JobState, category: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE upload_jobs SET state = 'pending', next_attempt_at_ms = 0, updated_at_ms = ?4 \
             WHERE principal_id = ?1 AND state = ?2 AND last_error_category = ?3",
        )
        .bind(principal_id)
        .bind(state.as_str())
        .bind(category)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Block jobs of a principal that have not reached the provider yet.
    pub async fn block_unstarted_jobs(&self, principal_id: &str, category: &str, reason: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE upload_jobs SET state = 'blocked', last_error_category = ?2, last_error = ?3, updated_at_ms = ?4 \
             WHERE principal_id = ?1 AND remote_file_code IS NULL \
             AND state IN ('pending', 'awaiting_principal', 'retry_waiting')",
        )
        .bind(principal_id)
        .bind(category)
        .bind(reason)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Is this remote file code already attached to another job?
    pub async fn file_code_claimed(&self, code: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM upload_jobs WHERE remote_file_code = ?1 LIMIT 1")
            .bind(code)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    // ---- attempts --------------------------------------------------------

    pub async fn start_attempt(&self, job_id: &str, stage: &str) -> Result<i64> {
        let res = sqlx::query("INSERT INTO upload_attempts (job_id, stage, started_at_ms) VALUES (?1, ?2, ?3)")
            .bind(job_id)
            .bind(stage)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(res.last_insert_rowid())
    }

    pub async fn finish_attempt(
        &self,
        id: i64,
        outcome: &str,
        category: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE upload_attempts SET finished_at_ms = ?2, outcome = ?3, error_category = ?4, detail = ?5 WHERE id = ?1",
        )
        .bind(id)
        .bind(now_ms())
        .bind(outcome)
        .bind(category)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn attempts(&self, job_id: &str) -> Result<Vec<Attempt>> {
        let rows = sqlx::query("SELECT * FROM upload_attempts WHERE job_id = ?1 ORDER BY id")
            .bind(job_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .iter()
            .map(|r| Attempt {
                id: r.get("id"),
                stage: r.get("stage"),
                started_at_ms: r.get("started_at_ms"),
                finished_at_ms: r.get("finished_at_ms"),
                outcome: r.get("outcome"),
                error_category: r.get("error_category"),
                detail: r.get("detail"),
            })
            .collect())
    }

    // ---- status ----------------------------------------------------------

    pub async fn job_counts(&self) -> Result<Vec<(String, i64, i64)>> {
        let rows = sqlx::query(
            "SELECT state, COUNT(*) AS n, COALESCE(SUM(size_bytes), 0) AS bytes FROM upload_jobs GROUP BY state",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("state"), r.get("n"), r.get("bytes"))).collect())
    }

    pub async fn principal_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows =
            sqlx::query("SELECT state, COUNT(*) AS n FROM principals GROUP BY state").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| (r.get("state"), r.get("n"))).collect())
    }

    /// Bytes still pinned by snapshot links (space that cleanup will eventually free).
    pub async fn held_snapshot_bytes(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COALESCE(SUM(size_bytes), 0) AS b FROM upload_jobs WHERE snapshot_held = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.get("b"))
    }

    pub async fn oldest_unarchived_created_ms(&self) -> Result<Option<i64>> {
        let row = sqlx::query(
            "SELECT MIN(created_at_ms) AS m FROM upload_jobs WHERE state NOT IN ('archived', 'cleaned', 'discarded')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("m"))
    }

    pub async fn kv_set(&self, k: &str, v: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO kv (k, v, updated_at_ms) VALUES (?1, ?2, ?3) \
             ON CONFLICT (k) DO UPDATE SET v = excluded.v, updated_at_ms = excluded.updated_at_ms",
        )
        .bind(k)
        .bind(v)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn kv_get(&self, k: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT v FROM kv WHERE k = ?1").bind(k).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| r.get("v")))
    }
}

#[cfg(test)]
impl Db {
    /// Tests: pretend every backoff has elapsed.
    pub async fn make_everything_due(&self) -> Result<()> {
        sqlx::query("UPDATE upload_jobs SET next_attempt_at_ms = 0").execute(&self.pool).await?;
        sqlx::query("UPDATE principals SET next_provision_at_ms = 0").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn exec_raw(&self, sql: &'static str) -> Result<()> {
        sqlx::query(sql).execute(&self.pool).await?;
        Ok(())
    }
}

fn principal_from_row(r: &SqliteRow) -> Result<Principal> {
    Ok(Principal {
        id: r.try_get("id")?,
        username: r.try_get("username")?,
        sftpgo_user_id: r.try_get("sftpgo_user_id")?,
        sftpgo_created_at_ms: r.try_get("sftpgo_created_at_ms")?,
        state: r.try_get::<String, _>("state")?.parse()?,
        state_reason: r.try_get("state_reason")?,
        remote_folder_id: r.try_get("remote_folder_id")?,
        remote_folder_name: r.try_get("remote_folder_name")?,
        approved_folder_name: r.try_get("approved_folder_name")?,
        approved_bind_folder_id: r.try_get("approved_bind_folder_id")?,
        provision_attempts: r.try_get("provision_attempts")?,
        next_provision_at_ms: r.try_get("next_provision_at_ms")?,
        verified_at_ms: r.try_get("verified_at_ms")?,
        home_quarantined_at_ms: r.try_get("home_quarantined_at_ms")?,
        created_at_ms: r.try_get("created_at_ms")?,
        updated_at_ms: r.try_get("updated_at_ms")?,
        deleted_at_ms: r.try_get("deleted_at_ms")?,
    })
}

fn job_from_row(r: &SqliteRow) -> Result<Job> {
    Ok(Job {
        id: r.try_get("id")?,
        username: r.try_get("username")?,
        principal_id: r.try_get("principal_id")?,
        virtual_path: r.try_get("virtual_path")?,
        rel_dir: r.try_get("rel_dir")?,
        file_name: r.try_get("file_name")?,
        remote_name: r.try_get("remote_name")?,
        protocol: r.try_get("protocol")?,
        source: r.try_get("source")?,
        event_ts_ms: r.try_get("event_ts_ms")?,
        size_bytes: r.try_get("size_bytes")?,
        snap_dev: r.try_get("snap_dev")?,
        snap_ino: r.try_get("snap_ino")?,
        snap_mtime_ns: r.try_get("snap_mtime_ns")?,
        snapshot_held: r.try_get::<i64, _>("snapshot_held")? != 0,
        sha256: r.try_get("sha256")?,
        state: r.try_get::<String, _>("state")?.parse()?,
        remote_file_code: r.try_get("remote_file_code")?,
        remote_folder_id: r.try_get("remote_folder_id")?,
        upload_started_at_ms: r.try_get("upload_started_at_ms")?,
        upload_body_sent_at_ms: r.try_get("upload_body_sent_at_ms")?,
        upload_server_time: r.try_get("upload_server_time")?,
        attempts: r.try_get("attempts")?,
        next_attempt_at_ms: r.try_get("next_attempt_at_ms")?,
        last_error_category: r.try_get("last_error_category")?,
        last_error: r.try_get("last_error")?,
        archived_at_ms: r.try_get("archived_at_ms")?,
        retain_until_ms: r.try_get("retain_until_ms")?,
        cleaned_at_ms: r.try_get("cleaned_at_ms")?,
        created_at_ms: r.try_get("created_at_ms")?,
        updated_at_ms: r.try_get("updated_at_ms")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("w.db")).await.unwrap();
        (dir, db)
    }

    fn new_job(id: &str, ino: i64) -> NewJob {
        NewJob {
            id: id.into(),
            username: "alice".into(),
            virtual_path: "/a.txt".into(),
            rel_dir: String::new(),
            file_name: "a.txt".into(),
            remote_name: "a.txt".into(),
            protocol: "DAV".into(),
            source: "event",
            event_ts_ms: 1,
            size_bytes: 10,
            snap_dev: 1,
            snap_ino: ino,
            snap_mtime_ns: 5,
        }
    }

    #[tokio::test]
    async fn duplicate_snapshot_inode_is_rejected_while_held() {
        let (_d, db) = test_db().await;
        assert_eq!(db.insert_job(&new_job("j1", 42)).await.unwrap(), InsertOutcome::Inserted);
        assert_eq!(db.insert_job(&new_job("j2", 42)).await.unwrap(), InsertOutcome::Duplicate);
        // Once the snapshot is released the inode number may be reused.
        sqlx::query("UPDATE upload_jobs SET snapshot_held = 0 WHERE id = 'j1'").execute(&db.pool).await.unwrap();
        assert_eq!(db.insert_job(&new_job("j3", 42)).await.unwrap(), InsertOutcome::Inserted);
    }

    #[tokio::test]
    async fn principal_for_event_picks_owning_generation() {
        let (_d, db) = test_db().await;
        let mk = |id: &str, created: i64, state: PrincipalState| Principal {
            id: id.into(),
            username: "alice".into(),
            sftpgo_user_id: 1,
            sftpgo_created_at_ms: created,
            state,
            state_reason: None,
            remote_folder_id: None,
            remote_folder_name: None,
            approved_folder_name: None,
            approved_bind_folder_id: None,
            provision_attempts: 0,
            next_provision_at_ms: 0,
            verified_at_ms: None,
            home_quarantined_at_ms: None,
            created_at_ms: created,
            updated_at_ms: created,
            deleted_at_ms: None,
        };
        db.insert_principal(&mk("old", 100, PrincipalState::Deleted)).await.unwrap();
        db.insert_principal(&mk("new", 200, PrincipalState::Active)).await.unwrap();
        assert_eq!(db.principal_for_event("alice", 150).await.unwrap().unwrap().id, "old");
        assert_eq!(db.principal_for_event("alice", 250).await.unwrap().unwrap().id, "new");
        assert!(db.principal_for_event("alice", 50).await.unwrap().is_none());
        assert_eq!(db.live_principal_by_username("alice").await.unwrap().unwrap().id, "new");
    }

    #[tokio::test]
    async fn only_one_live_principal_per_username() {
        let (_d, db) = test_db().await;
        let p = Principal {
            id: "a".into(),
            username: "bob".into(),
            sftpgo_user_id: 1,
            sftpgo_created_at_ms: 1,
            state: PrincipalState::Active,
            state_reason: None,
            remote_folder_id: None,
            remote_folder_name: None,
            approved_folder_name: None,
            approved_bind_folder_id: None,
            provision_attempts: 0,
            next_provision_at_ms: 0,
            verified_at_ms: None,
            home_quarantined_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            deleted_at_ms: None,
        };
        db.insert_principal(&p).await.unwrap();
        let mut q = p.clone();
        q.id = "b".into();
        q.sftpgo_user_id = 2;
        assert!(db.insert_principal(&q).await.is_err(), "second live principal must be refused");
        db.tombstone_principal("a", 5, "deleted").await.unwrap();
        db.insert_principal(&q).await.unwrap();
    }
}
