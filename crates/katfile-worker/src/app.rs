//! Shared state and the `serve` entry point: HTTP server plus the background loops
//! (runner, provisioner, reconciler, cleanup), with graceful shutdown.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::get;
use katfile_api::KatFileClient;
use tokio::sync::{Notify, watch};
use tracing::{info, warn};

use crate::admission::Admission;
use crate::config::{Config, read_secret};
use crate::db::Db;
use crate::paths::Staging;
use crate::sftpgo::SftpgoClient;
use crate::util::now_ms;
use crate::{cleanup, provision, reconcile, runner, status, webhook};

pub struct App {
    pub cfg: Config,
    pub db: Db,
    pub staging: Staging,
    pub katfile: KatFileClient,
    pub sftpgo: SftpgoClient,
    pub admission: Admission,
    pub webhook_secret: String,
    pub admin_token: String,
    pub runner_wake: Notify,
    pub provisioner_wake: Notify,
    pub reconcile_wake: Notify,
    pub started_at_ms: i64,
    /// Last KatFile authentication failure (shown in /admin/status until a call succeeds).
    pub katfile_auth_error: Mutex<Option<String>>,
}

pub type AppRef = Arc<App>;

impl App {
    /// Build the application from configuration and secret files.
    pub async fn build(cfg: Config) -> Result<AppRef> {
        let staging = Staging::new(cfg.users_dir.clone(), cfg.spool_dir.clone(), cfg.temp_dir.clone());
        let prepared = staging.clone();
        tokio::task::spawn_blocking(move || prepared.prepare())
            .await?
            .context("staging layout check failed (see docs/DEPLOYMENT.md)")?;
        let db = Db::open(&cfg.db_path).await?;
        let katfile = cfg.katfile.client()?;
        let sftpgo = SftpgoClient::new(
            cfg.sftpgo_api_url.clone(),
            read_secret(&cfg.sftpgo_api_key_file, "SFTPGo API key (run scripts/sftpgo-bootstrap.sh)")?,
        )?;
        Ok(Arc::new(App {
            admission: Admission::new(cfg.min_free_bytes, cfg.unknown_upload_reservation_bytes),
            webhook_secret: read_secret(&cfg.webhook_secret_file, "webhook secret")?,
            admin_token: read_secret(&cfg.admin_token_file, "admin token")?,
            cfg,
            db,
            staging,
            katfile,
            sftpgo,
            runner_wake: Notify::new(),
            provisioner_wake: Notify::new(),
            reconcile_wake: Notify::new(),
            started_at_ms: now_ms(),
            katfile_auth_error: Mutex::new(None),
        }))
    }

    pub fn router(app: AppRef) -> Router {
        Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/readyz", get(status::readyz))
            .merge(webhook::routes())
            .merge(status::routes())
            .with_state(app)
    }

    pub fn note_katfile_auth_error(&self, msg: Option<String>) {
        if let Ok(mut g) = self.katfile_auth_error.lock() {
            *g = msg;
        }
    }
}

/// Run until SIGTERM/SIGINT.
pub async fn serve(cfg: Config) -> Result<()> {
    let bind = cfg.bind;
    let processing = cfg.processing_enabled;
    let app = App::build(cfg).await?;
    let (stop_tx, stop_rx) = watch::channel(false);

    let mut tasks = Vec::new();
    if processing {
        tasks.push(tokio::spawn(runner::run(app.clone(), stop_rx.clone())));
        tasks.push(tokio::spawn(provision::run(app.clone(), stop_rx.clone())));
        tasks.push(tokio::spawn(reconcile::run(app.clone(), stop_rx.clone())));
        tasks.push(tokio::spawn(cleanup::run(app.clone(), stop_rx.clone())));
    } else {
        warn!("WORKER_PROCESSING_ENABLED=false: events are recorded but nothing is archived or cleaned");
    }

    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("binding {bind}"))?;
    info!(%bind, processing, "katfile-worker listening");
    axum::serve(listener, App::router(app.clone())).with_graceful_shutdown(shutdown_signal()).await?;

    info!("shutting down background tasks");
    let _ = stop_tx.send(true);
    app.runner_wake.notify_waiters();
    app.provisioner_wake.notify_waiters();
    app.reconcile_wake.notify_waiters();
    for t in tasks {
        // An upload in progress is abandoned; its job is recovered on the next start.
        if tokio::time::timeout(Duration::from_secs(10), t).await.is_err() {
            warn!("background task did not stop within 10s");
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// Sleep until woken, the interval elapses or shutdown is requested; true means stop.
pub async fn idle(wake: &Notify, interval: Duration, stop: &mut watch::Receiver<bool>) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        _ = wake.notified() => {},
        _ = tokio::time::sleep(interval) => {},
        _ = stop.changed() => {},
    }
    *stop.borrow()
}
