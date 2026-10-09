//! `katfile-worker probe`: opt-in V0 compatibility probe against a real KatFile account.
//!
//! Read-only by default. `--write` creates a disposable `kfgw-probe-<UTC>` folder and
//! uploads a few small synthetic files into it; nothing is ever deleted remotely.
//! The report never contains the API key, upload sessions or the account e-mail.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use katfile_api::{
    ApiKey, ErrorClass, FileCode, FolderId, KatFileClient, KatFileError, UploadMode, UploadProgress, UploadRequest,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::config::KatFileSettings;
use crate::util::{compact_utc, now_ms, rfc3339};

#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Allow write operations: create a disposable folder and upload small synthetic files.
    #[arg(long)]
    pub write: bool,
    /// Parent folder for the disposable probe folder (default: account root).
    #[arg(long, default_value_t = 0)]
    pub parent_folder_id: u64,
    /// Size in bytes of each synthetic upload (keep small for real accounts).
    #[arg(long, default_value_t = 4096)]
    pub file_bytes: u64,
}

#[derive(Debug, Serialize)]
struct Step {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Debug, Default, Serialize)]
struct Created {
    folders: Vec<u64>,
    files: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Report {
    started_utc: String,
    base_url: String,
    write_mode: bool,
    passed: usize,
    failed: usize,
    steps: Vec<Step>,
    created: Created,
}

struct Probe {
    client: KatFileClient,
    steps: Vec<Step>,
    created: Created,
}

impl Probe {
    fn record(&mut self, name: &'static str, ok: bool, detail: impl Into<String>) {
        let detail = detail.into();
        info!(step = name, ok, %detail, "probe step");
        self.steps.push(Step { name, ok, detail });
    }
}

pub async fn run(args: ProbeArgs) -> Result<()> {
    let settings = KatFileSettings::from_env()?;
    let client = settings.client()?;
    let started = now_ms();
    let mut p = Probe { client, steps: Vec::new(), created: Created::default() };

    read_only_checks(&mut p).await;
    if args.write {
        write_checks(&mut p, &args, settings.upload_mode).await?;
    }

    let failed = p.steps.iter().filter(|s| !s.ok).count();
    let report = Report {
        started_utc: rfc3339(started),
        base_url: settings.base_url.to_string(),
        write_mode: args.write,
        passed: p.steps.len() - failed,
        failed,
        steps: p.steps,
        created: p.created,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    if failed > 0 {
        anyhow::bail!("{failed} probe step(s) failed");
    }
    Ok(())
}

async fn read_only_checks(p: &mut Probe) {
    match p.client.account_info().await {
        Ok(a) => p.record(
            "account_info",
            true,
            format!("storage_used={:?} premium_expire={:?}", a.storage_used_bytes, a.premium_expire),
        ),
        Err(e) => p.record("account_info", false, e.to_string()),
    }

    match p.client.list_folder(FolderId::ROOT).await {
        Ok(l) => {
            p.record("folder_list_root", true, format!("{} folders, {} files at root", l.folders.len(), l.files.len()))
        }
        Err(e) => p.record("folder_list_root", false, e.to_string()),
    }

    match p.client.list_folder(FolderId(999_999_999)).await {
        Err(KatFileError::Api { status, msg }) => {
            p.record("folder_list_unknown", true, format!("status {status}: {msg}"))
        }
        other => p.record("folder_list_unknown", false, format!("expected application error, got {other:?}")),
    }

    match p.client.list_files_page(None, None, 1, 5).await {
        Ok(page) => p.record(
            "file_list_page",
            true,
            format!("results_total={} page_len={}", page.results_total, page.files.len()),
        ),
        Err(e) => p.record("file_list_page", false, e.to_string()),
    }

    match p.client.request_upload_server().await {
        Ok(s) => p.record(
            "upload_server",
            true,
            format!("host={} (allowlisted, https) server_time={:?}", s.url.host_str().unwrap_or(""), s.server_time),
        ),
        Err(e) => p.record("upload_server", false, e.to_string()),
    }

    let code = FileCode::parse("zzzzzzzzzzzz").expect("valid literal");
    match p.client.file_info(&code).await {
        Ok(info) if !info.found => p.record("file_info_unknown", true, "unknown file reported as not found"),
        other => p.record("file_info_unknown", false, format!("unexpected: {other:?}")),
    }

    // Rejected key: build a second client with a syntactically valid but wrong key.
    let bad = ApiKey::new("kfgwprobeinvalidkey0").and_then(|k| {
        let mut cfg = p.client.config().clone();
        cfg.read_retries = 0;
        KatFileClient::new(cfg, k)
    });
    match bad {
        Ok(c) => match c.account_info().await {
            Err(e) if e.class() == ErrorClass::Auth => p.record("invalid_key_rejected", true, e.to_string()),
            other => p.record("invalid_key_rejected", false, format!("unexpected: {other:?}")),
        },
        Err(e) => p.record("invalid_key_rejected", false, e.to_string()),
    }
}

async fn write_checks(p: &mut Probe, args: &ProbeArgs, mode: UploadMode) -> Result<()> {
    let parent = FolderId(args.parent_folder_id);
    let name = format!("kfgw-probe-{}", compact_utc(now_ms()));
    let folder = match p.client.create_folder(parent, &name).await {
        Ok(id) => {
            p.created.folders.push(id.0);
            p.record("folder_create", true, format!("{name} -> fld_id {id}"));
            id
        }
        Err(e) => {
            p.record("folder_create", false, e.to_string());
            return Ok(());
        }
    };

    match p.client.list_folder(parent).await {
        Ok(l) => {
            let hit = l.folders.iter().any(|f| f.id == folder && f.name == name);
            p.record("folder_create_listed", hit, format!("listed under parent {parent}: {hit}"));
        }
        Err(e) => p.record("folder_create_listed", false, e.to_string()),
    }

    // Unicode folder names come back as mojibake; the client must demangle them.
    let unicode = "nested 相片 ü";
    match p.client.create_folder(folder, unicode).await {
        Ok(id) => {
            p.created.folders.push(id.0);
            let listed = p.client.list_folder(folder).await.map(|l| l.folders.into_iter().find(|f| f.id == id));
            match listed {
                Ok(Some(f)) => p.record(
                    "folder_unicode_roundtrip",
                    f.name == unicode,
                    format!("raw={:?} demangled={:?}", f.raw_name, f.name),
                ),
                other => p.record("folder_unicode_roundtrip", false, format!("{other:?}")),
            }
        }
        Err(e) => p.record("folder_unicode_roundtrip", false, e.to_string()),
    }

    // Duplicate names are allowed remotely: document non-idempotent creation.
    let dup_a = p.client.create_folder(folder, "dup").await;
    let dup_b = p.client.create_folder(folder, "dup").await;
    match (dup_a, dup_b) {
        (Ok(a), Ok(b)) => {
            p.created.folders.extend([a.0, b.0]);
            p.record("folder_create_not_idempotent", a != b, format!("two creates -> {a} and {b}"));
        }
        (a, b) => p.record("folder_create_not_idempotent", false, format!("{a:?} / {b:?}")),
    }

    let dir = std::env::temp_dir();
    for (step, remote, upload_mode) in [
        ("upload_configured_mode", "kfgw-probe-configured.bin", mode),
        ("upload_chunked", "kfgw-probe-chunked.bin", UploadMode::Chunked),
        ("upload_fixed_length", "kfgw-probe-fixed.bin", UploadMode::FixedLength),
        ("upload_unicode_name", "kfgw-probe 相片 ü.txt", UploadMode::FixedLength),
    ] {
        let path = dir.join(format!("kfgw-probe-{}.tmp", uuid::Uuid::new_v4()));
        let expected_sha = write_synthetic(&path, args.file_bytes, remote)?;
        let outcome = upload_and_verify(&p.client, &path, args.file_bytes, remote, upload_mode, folder).await;
        let _ = std::fs::remove_file(&path);
        match outcome {
            Ok((code, sha)) => {
                p.created.files.push(code.to_string());
                let ok = sha == expected_sha;
                p.record(step, ok, format!("{remote} -> {code} verified in folder {folder}; sha256 match={ok}"));
            }
            Err(e) => p.record(step, false, format!("{remote}: {e:#}")),
        }
    }
    Ok(())
}

/// Upload a file, move it into `folder` and confirm it through the account-scoped listing.
async fn upload_and_verify(
    client: &KatFileClient,
    path: &Path,
    size: u64,
    remote: &str,
    mode: UploadMode,
    folder: FolderId,
) -> Result<(FileCode, String)> {
    let server = client.request_upload_server().await?;
    let file = tokio::fs::File::open(path).await?;
    let receipt = client
        .upload(&server, UploadRequest { file, size, remote_name: remote.to_owned(), mode }, UploadProgress::new())
        .await?;
    client.set_file_folder(&receipt.file_code, folder).await?;
    let found = client
        .find_file_in_folder(folder, &receipt.file_code, remote, 10)
        .await?
        .context("uploaded file not found in the destination folder (ownership not verified)")?;
    anyhow::ensure!(found.size == Some(size), "size mismatch: remote {:?} vs local {size}", found.size);
    anyhow::ensure!(found.name == remote, "name mismatch: remote {:?}", found.name);
    Ok((receipt.file_code, receipt.sha256_hex))
}

/// Write deterministic pseudo-random content and return its SHA-256.
fn write_synthetic(path: &Path, size: u64, label: &str) -> Result<String> {
    let mut f = std::fs::File::create(path)?;
    let mut hasher = Sha256::new();
    let header = format!("kfgw synthetic probe file {label} {}\n", rfc3339(now_ms()));
    let mut written = 0u64;
    let mut block = Sha256::digest(header.as_bytes()).to_vec();
    let mut buf = header.into_bytes();
    while written < size {
        if buf.is_empty() {
            block = Sha256::digest(&block).to_vec();
            buf.extend_from_slice(&block);
        }
        let n = (buf.len() as u64).min(size - written) as usize;
        f.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        buf.drain(..n);
        written += n as u64;
    }
    f.sync_all()?;
    Ok(hex::encode(hasher.finalize()))
}
