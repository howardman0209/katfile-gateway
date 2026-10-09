//! `record-hooks`: diagnostic stand-in for the worker webhook (V1 transport testing).
//!
//! Authenticates SFTPGo hook calls exactly like `serve`, appends each call (path,
//! query and body) as one JSON line, and answers 200. `GET /recorded` returns the log.
//! Useful to capture real SFTPGo payloads without touching KatFile.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use clap::Args;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::info;

use crate::auth::bearer_matches;
use crate::config::read_secret;
use crate::util::{now_ms, rfc3339};

#[derive(Debug, Args)]
pub struct RecordArgs {
    /// JSON-lines output file.
    #[arg(long, env = "WORKER_HOOK_RECORD_FILE", default_value = "/var/lib/katfile-worker/hooks.jsonl")]
    pub output: PathBuf,
}

struct Recorder {
    secret: String,
    output: PathBuf,
    lock: Mutex<()>,
}

pub async fn run(args: RecordArgs) -> Result<()> {
    let secret_file: PathBuf = std::env::var("WORKER_WEBHOOK_SECRET_FILE")
        .unwrap_or_else(|_| "/run/secrets/worker_webhook_secret".into())
        .into();
    let bind: std::net::SocketAddr =
        std::env::var("WORKER_BIND").unwrap_or_else(|_| "0.0.0.0:8090".into()).parse().context("WORKER_BIND")?;
    let state = Arc::new(Recorder {
        secret: read_secret(&secret_file, "webhook secret")?,
        output: args.output,
        lock: Mutex::new(()),
    });
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/hooks/{*kind}", post(record))
        .route("/recorded", get(recorded))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, "recording SFTPGo hooks (no archival in this mode)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn record(
    State(rec): State<Arc<Recorder>>,
    Path(kind): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !bearer_matches(&headers, &rec.secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let body_value: Value =
        serde_json::from_slice(&body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()));
    let line = json!({"received_at": rfc3339(now_ms()), "path": kind, "query": query, "body": body_value});
    info!(path = %kind, "hook received");
    let _guard = rec.lock.lock().await;
    let result = async {
        let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&rec.output).await?;
        f.write_all(format!("{line}\n").as_bytes()).await?;
        f.flush().await
    }
    .await;
    match result {
        Ok(()) => StatusCode::OK.into_response(),
        // 503 makes SFTPGo's retryable client try again.
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn recorded(State(rec): State<Arc<Recorder>>, headers: HeaderMap) -> Response {
    if !bearer_matches(&headers, &rec.secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    match tokio::fs::read_to_string(&rec.output).await {
        Ok(s) => s.into_response(),
        Err(_) => String::new().into_response(),
    }
}
