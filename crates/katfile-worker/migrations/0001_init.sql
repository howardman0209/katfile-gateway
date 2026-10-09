-- Durable state for the KatFile archival worker.
-- Times are milliseconds since the Unix epoch unless stated otherwise.

-- One row per SFTPGo account generation. SFTPGo's SQLite user id is not
-- AUTOINCREMENT and can be reused after deletion, so a principal is bound to
-- (sftpgo_user_id, sftpgo_created_at_ms) and gets its own immutable UUID.
CREATE TABLE principals (
    id                       TEXT PRIMARY KEY,
    username                 TEXT NOT NULL,
    sftpgo_user_id           INTEGER NOT NULL,
    sftpgo_created_at_ms     INTEGER NOT NULL,
    state                    TEXT NOT NULL CHECK (state IN
                                 ('provisioning', 'active', 'failed', 'blocked', 'disabled', 'deleted')),
    state_reason             TEXT,
    remote_folder_id         INTEGER,
    remote_folder_name       TEXT,
    -- Explicit administrator decisions for blocked principals.
    approved_folder_name     TEXT,
    approved_bind_folder_id  INTEGER,
    provision_attempts       INTEGER NOT NULL DEFAULT 0,
    next_provision_at_ms     INTEGER NOT NULL DEFAULT 0,
    verified_at_ms           INTEGER,
    -- Set when the deleted user's home was moved into the worker-private quarantine.
    home_quarantined_at_ms   INTEGER,
    created_at_ms            INTEGER NOT NULL,
    updated_at_ms            INTEGER NOT NULL,
    deleted_at_ms            INTEGER,
    UNIQUE (sftpgo_user_id, sftpgo_created_at_ms)
);
-- At most one live principal per username; tombstones keep history.
CREATE UNIQUE INDEX principals_live_username ON principals (username) WHERE state <> 'deleted';
-- A KatFile root folder is never shared between principals (including tombstones).
CREATE UNIQUE INDEX principals_remote_folder ON principals (remote_folder_id) WHERE remote_folder_id IS NOT NULL;

-- Crash-safe remote folder creation: the same-named folders seen before calling
-- folder/create are recorded, so an uncertain create can be resolved by diffing.
CREATE TABLE folder_intents (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    principal_id       TEXT NOT NULL REFERENCES principals (id),
    rel_path           TEXT NOT NULL,   -- '' = the principal's root folder
    parent_folder_id   INTEGER NOT NULL,
    name               TEXT NOT NULL,
    pre_existing_ids   TEXT NOT NULL,   -- JSON array of folder ids
    created_at_ms      INTEGER NOT NULL,
    UNIQUE (principal_id, rel_path)
);

-- Nested folders below a principal's root folder ('photos/2026', never '').
CREATE TABLE folder_mappings (
    principal_id       TEXT NOT NULL REFERENCES principals (id),
    rel_path           TEXT NOT NULL,
    remote_folder_id   INTEGER NOT NULL,
    remote_name        TEXT NOT NULL,
    created_at_ms      INTEGER NOT NULL,
    PRIMARY KEY (principal_id, rel_path)
);

CREATE TABLE upload_jobs (
    id                      TEXT PRIMARY KEY,   -- UUID; also the spool snapshot file name
    username                TEXT NOT NULL,
    principal_id            TEXT REFERENCES principals (id),
    virtual_path            TEXT NOT NULL,
    rel_dir                 TEXT NOT NULL,      -- '' for the home root
    file_name               TEXT NOT NULL,
    remote_name             TEXT NOT NULL,      -- sanitized name sent to KatFile
    protocol                TEXT NOT NULL,
    source                  TEXT NOT NULL CHECK (source IN ('event', 'reconcile')),
    event_ts_ms             INTEGER NOT NULL,
    size_bytes              INTEGER NOT NULL,
    -- Identity of the hard-link snapshot taken when the job was created.
    snap_dev                INTEGER NOT NULL,
    snap_ino                INTEGER NOT NULL,
    snap_mtime_ns           INTEGER NOT NULL,
    snapshot_held           INTEGER NOT NULL DEFAULT 1,
    sha256                  TEXT,
    state                   TEXT NOT NULL CHECK (state IN
                                ('pending', 'awaiting_principal', 'uploading', 'remote_uploaded',
                                 'assigning_folder', 'verifying', 'retry_waiting', 'needs_review',
                                 'failed', 'blocked', 'archived', 'cleaned', 'discarded')),
    remote_file_code        TEXT,
    remote_folder_id        INTEGER,
    upload_started_at_ms    INTEGER,
    -- Set once the whole multipart body was handed to the connection; a crash after
    -- this point leaves an ambiguous outcome that must be reconciled, not re-uploaded.
    upload_body_sent_at_ms  INTEGER,
    upload_server_time      TEXT,
    attempts                INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_ms      INTEGER NOT NULL DEFAULT 0,
    last_error_category     TEXT,
    last_error              TEXT,
    archived_at_ms          INTEGER,
    retain_until_ms         INTEGER,
    cleaned_at_ms           INTEGER,
    created_at_ms           INTEGER NOT NULL,
    updated_at_ms           INTEGER NOT NULL
);
-- While a snapshot link exists its inode cannot be reused, so (dev, ino) identifies
-- the staged content exactly; duplicate events for the same upload collapse here.
CREATE UNIQUE INDEX upload_jobs_live_snapshot ON upload_jobs (snap_dev, snap_ino) WHERE snapshot_held = 1;
CREATE INDEX upload_jobs_state_due ON upload_jobs (state, next_attempt_at_ms);
CREATE INDEX upload_jobs_principal ON upload_jobs (principal_id, state);
CREATE INDEX upload_jobs_username ON upload_jobs (username, state);

CREATE TABLE upload_attempts (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id           TEXT NOT NULL REFERENCES upload_jobs (id),
    stage            TEXT NOT NULL,
    started_at_ms    INTEGER NOT NULL,
    finished_at_ms   INTEGER,
    outcome          TEXT,   -- ok | transient | permanent | ambiguous | auth
    error_category   TEXT,
    detail           TEXT    -- sanitized, bounded
);
CREATE INDEX upload_attempts_job ON upload_attempts (job_id);

-- Small key/value store for checkpoints (last reconcile run, etc.).
CREATE TABLE kv (
    k              TEXT PRIMARY KEY,
    v              TEXT NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);
