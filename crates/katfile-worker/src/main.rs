//! katfile-worker: receives SFTPGo events, keeps a durable SQLite job queue and archives
//! completed uploads to KatFile (see README.md and docs/ARCHITECTURE.md).

mod config;
mod probe;
mod util;

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "katfile-worker", version, about = "SFTPGo to KatFile archival worker")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the webhook server, job runner, reconciler and cleanup (default).
    Serve,
    /// Opt-in KatFile API compatibility probe (read-only unless --write).
    Probe(probe::ProbeArgs),
    /// Docker HEALTHCHECK helper: exit 0 when the local /healthz endpoint answers.
    Healthcheck,
}

/// Noisy HTTP crates are capped so request URLs never reach logs at debug/trace level.
const LOG_CAPS: &str = "hyper=warn,hyper_util=warn,reqwest=warn,h2=warn,rustls=warn,sqlx=warn,tower_http=info";

fn init_tracing() {
    let base = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    let directives =
        if std::env::var_os("KFGW_UNSAFE_HTTP_TRACE").is_some() { base } else { format!("{base},{LOG_CAPS}") };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(directives).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .init();
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async move {
        match cli.command.unwrap_or(Command::Serve) {
            Command::Serve => anyhow::bail!("serve is not implemented yet"),
            Command::Probe(args) => probe::run(args).await,
            Command::Healthcheck => healthcheck().await,
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn healthcheck() -> anyhow::Result<()> {
    let bind: std::net::SocketAddr = std::env::var("WORKER_BIND").unwrap_or_else(|_| "0.0.0.0:8090".into()).parse()?;
    let url = format!("http://127.0.0.1:{}/healthz", bind.port());
    let resp = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?.get(url).send().await?;
    anyhow::ensure!(resp.status().is_success(), "healthz returned {}", resp.status());
    Ok(())
}
