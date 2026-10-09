//! In-process test harness: mock KatFile, mock SFTPGo admin API, temporary staging
//! volume and the real worker router on an ephemeral port.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use katfile_api::UploadMode;
use katfile_mock::MockKatFile;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::app::{App, AppRef};
use crate::config::{Config, KatFileSettings};
use crate::db::Job;
use crate::{provision, runner};

const KATFILE_KEY: &str = "testkatfilekey123";
pub const WEBHOOK_SECRET: &str = "test-webhook-secret-0123456789";
pub const ADMIN_TOKEN: &str = "test-admin-token-0123456789";
const SFTPGO_KEY: &str = "test-sftpgo-api-key-0123456789";

pub const INBOX_PERMS: &[&str] = &["list", "download", "upload", "create_dirs", "rename", "delete"];

#[derive(Clone, Default)]
pub struct SftpgoState {
    pub users: Arc<Mutex<BTreeMap<String, Value>>>,
    pub quota_scans: Arc<Mutex<Vec<String>>>,
}

/// Minimal SFTPGo admin API (users + quota scans) keyed by API key.
pub struct MockSftpgo {
    pub addr: SocketAddr,
    pub state: SftpgoState,
    users_dir: PathBuf,
    task: JoinHandle<()>,
}

impl Drop for MockSftpgo {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn key_ok(h: &HeaderMap) -> bool {
    h.get("X-SFTPGO-API-KEY").and_then(|v| v.to_str().ok()) == Some(SFTPGO_KEY)
}

async fn sg_user(State(s): State<SftpgoState>, AxPath(name): AxPath<String>, h: HeaderMap) -> Response {
    if !key_ok(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match s.users.lock().unwrap().get(&name) {
        Some(u) => axum::Json(u.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn sg_users(State(s): State<SftpgoState>, h: HeaderMap) -> Response {
    if !key_ok(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let list: Vec<Value> = s.users.lock().unwrap().values().cloned().collect();
    axum::Json(list).into_response()
}

async fn sg_scan(State(s): State<SftpgoState>, AxPath(name): AxPath<String>, h: HeaderMap) -> Response {
    if !key_ok(&h) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    s.quota_scans.lock().unwrap().push(name);
    StatusCode::ACCEPTED.into_response()
}

impl MockSftpgo {
    async fn start(users_dir: PathBuf) -> MockSftpgo {
        let state = SftpgoState::default();
        let app = Router::new()
            .route("/api/v2/users", get(sg_users))
            .route("/api/v2/users/{name}", get(sg_user))
            .route("/api/v2/quotas/users/{name}/scan", post(sg_scan))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockSftpgo { addr, state, users_dir, task }
    }

    /// Create or replace a user with the given generation (id, created_at in ms).
    pub fn put_user(&self, name: &str, id: i64, created_at_ms: i64, perms: &[&str], status: i64) {
        let user = json!({
            "id": id, "username": name, "status": status, "created_at": created_at_ms,
            "home_dir": self.users_dir.join(name).to_string_lossy(),
            "permissions": {"/": perms}, "filters": {}, "groups": null, "virtual_folders": null,
            "filesystem": {"provider": 0}
        });
        self.state.users.lock().unwrap().insert(name.to_owned(), user);
    }

    pub fn remove_user(&self, name: &str) {
        self.state.users.lock().unwrap().remove(name);
    }
}

pub struct Harness {
    pub app: AppRef,
    pub kf: MockKatFile,
    pub sg: MockSftpgo,
    pub base: String,
    pub http: reqwest::Client,
    _dir: tempfile::TempDir,
    server: JoinHandle<()>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Harness {
    pub async fn new() -> Harness {
        Harness::with(|_| {}).await
    }

    pub async fn with(tweak: impl FnOnce(&mut Config)) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for d in ["data", "spool", "uploads-tmp", "secrets"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let secret = |name: &str, value: &str| {
            let p = root.join("secrets").join(name);
            std::fs::write(&p, value).unwrap();
            p
        };
        let kf = MockKatFile::start(KATFILE_KEY).await;
        let sg = MockSftpgo::start(root.join("data")).await;
        let mut cfg = Config {
            katfile: KatFileSettings {
                base_url: kf.base_url(),
                api_key_file: secret("katfile_api_key", KATFILE_KEY),
                upload_host_regex: r"^127\.0\.0\.1$".into(),
                upload_type: "prem".into(),
                upload_mode: UploadMode::FixedLength,
                users_parent_folder_id: 0,
                insecure_test_mode: true,
                upload_stall_timeout: Duration::from_secs(10),
                api_retry_base_delay: Duration::from_millis(5),
            },
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: root.join("worker.db"),
            users_dir: root.join("data"),
            spool_dir: root.join("spool"),
            temp_dir: root.join("uploads-tmp"),
            stale_temp_after: Duration::from_secs(3600),
            retry_max: 4,
            retention: Duration::from_secs(24 * 3600),
            min_free_bytes: 0,
            unknown_upload_reservation_bytes: 1024,
            max_active_uploads: 1,
            webhook_secret_file: secret("webhook", WEBHOOK_SECRET),
            admin_token_file: secret("admin", ADMIN_TOKEN),
            sftpgo_api_url: url::Url::parse(&format!("http://{}/", sg.addr)).unwrap(),
            sftpgo_api_key_file: secret("sftpgo", SFTPGO_KEY),
            reconcile_interval: Duration::from_secs(600),
            cleanup_interval: Duration::from_secs(300),
            reconcile_min_age: Duration::ZERO,
            processing_enabled: true,
            accepted_protocols: vec!["SFTP".into(), "SCP".into(), "DAV".into(), "HTTP".into()],
            quota_scan_after_cleanup: true,
        };
        tweak(&mut cfg);
        let app = App::build(cfg).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = App::router(app.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Harness { app, kf, sg, base, http: reqwest::Client::new(), _dir: dir, server }
    }

    pub fn users_dir(&self) -> &Path {
        &self.app.cfg.users_dir
    }

    pub fn home_file(&self, user: &str, vpath: &str) -> PathBuf {
        self.users_dir().join(user).join(vpath.trim_start_matches('/'))
    }

    /// Simulate SFTPGo writing a completed upload into a home.
    pub fn put_file(&self, user: &str, vpath: &str, content: &[u8]) {
        let p = self.home_file(user, vpath);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    /// POST an SFTPGo `upload` hook; returns the HTTP status.
    pub async fn hook(&self, body: Value) -> u16 {
        self.http
            .post(format!("{}/hooks/sftpgo/fs", self.base))
            .bearer_auth(WEBHOOK_SECRET)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    pub fn upload_event(&self, user: &str, vpath: &str, size: usize, protocol: &str) -> Value {
        json!({
            "action": "upload", "username": user, "virtual_path": vpath,
            "path": self.home_file(user, vpath).to_string_lossy(),
            "file_size": size, "status": 1, "protocol": protocol, "fs_provider": 0,
            "timestamp": crate::util::now_ms() * 1_000_000
        })
    }

    /// Write the file and deliver its completion hook.
    pub async fn upload(&self, user: &str, vpath: &str, content: &[u8], protocol: &str) -> u16 {
        self.put_file(user, vpath, content);
        self.hook(self.upload_event(user, vpath, content.len(), protocol)).await
    }

    pub async fn admin(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(ADMIN_TOKEN);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// Run provisioning and job steps (skipping backoffs) until states stop changing.
    pub async fn settle(&self) {
        let mut last = self.fingerprint().await;
        for _ in 0..30 {
            self.app.db.make_everything_due().await.unwrap();
            provision::provision_due(&self.app).await;
            runner::process_due_once(&self.app).await.unwrap();
            let now = self.fingerprint().await;
            if now == last {
                return;
            }
            last = now;
        }
        panic!("jobs did not settle");
    }

    async fn fingerprint(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .app
            .db
            .recent_jobs(10_000)
            .await
            .unwrap()
            .iter()
            .map(|j| format!("{}:{}:{:?}:{}", j.id, j.state, j.remote_file_code, j.attempts))
            .collect();
        out.extend(
            self.app
                .db
                .principals()
                .await
                .unwrap()
                .iter()
                .map(|p| format!("{}:{}:{:?}", p.id, p.state, p.remote_folder_id)),
        );
        out.sort();
        out
    }

    pub async fn sync_users(&self) {
        provision::sync_all(&self.app).await.unwrap();
    }

    pub async fn job_for(&self, user: &str, vpath: &str) -> Job {
        let jobs = self.app.db.recent_jobs(1000).await.unwrap();
        jobs.into_iter()
            .find(|j| j.username == user && j.virtual_path == vpath)
            .unwrap_or_else(|| panic!("no job for {user}{vpath}"))
    }

    pub async fn jobs_for(&self, user: &str, vpath: &str) -> Vec<Job> {
        let jobs = self.app.db.recent_jobs(1000).await.unwrap();
        jobs.into_iter().filter(|j| j.username == user && j.virtual_path == vpath).collect()
    }

    pub async fn principal_folder(&self, user: &str) -> u64 {
        let p = self.app.db.live_principal_by_username(user).await.unwrap().expect("principal");
        p.remote_folder_id.expect("folder") as u64
    }
}
