//! Stand-alone mock KatFile server for local end-to-end experiments.
//!
//! Environment:
//! * `KFMOCK_LISTEN` (default `127.0.0.1:8099`)
//! * `KFMOCK_API_KEY` (default `mock-key`)
//! * `KFMOCK_PUBLIC_BASE` (optional, e.g. `http://kfmock:8099/`)

use std::time::Duration;

use katfile_mock::MockKatFile;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let listen = std::env::var("KFMOCK_LISTEN").unwrap_or_else(|_| "127.0.0.1:8099".into());
    let key = std::env::var("KFMOCK_API_KEY").unwrap_or_else(|_| "mock-key".into());
    let public_base = std::env::var("KFMOCK_PUBLIC_BASE").ok().and_then(|u| url::Url::parse(&u).ok());
    let mock = MockKatFile::start_on(listen.parse().expect("KFMOCK_LISTEN must be host:port"), &key, public_base).await;
    tracing::info!(addr = %mock.addr(), "mock KatFile listening");
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
