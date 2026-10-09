//! Environment-based configuration. Secrets are only ever read from files.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use katfile_api::{ApiKey, ClientConfig, KatFileClient, UploadMode};
use url::Url;

/// KatFile access settings shared by `serve` and `probe`.
#[derive(Debug, Clone)]
pub struct KatFileSettings {
    pub base_url: Url,
    pub api_key_file: PathBuf,
    pub upload_host_regex: String,
    pub upload_type: String,
    pub upload_mode: UploadMode,
    /// Parent folder under which per-user root folders are provisioned (0 = account root).
    pub users_parent_folder_id: u64,
    /// Test only: allow http/private endpoints (mock KatFile).
    pub insecure_test_mode: bool,
    pub upload_stall_timeout: Duration,
    /// First retry delay for idempotent API calls.
    pub api_retry_base_delay: Duration,
}

impl KatFileSettings {
    pub fn from_env() -> Result<Self> {
        let mode = match env_or("KATFILE_UPLOAD_MODE", "fixed").as_str() {
            "fixed" => UploadMode::FixedLength,
            "chunked" => UploadMode::Chunked,
            other => bail!("KATFILE_UPLOAD_MODE must be fixed or chunked, got {other:?}"),
        };
        Ok(KatFileSettings {
            base_url: parse_env("KATFILE_API_BASE_URL", "https://katfile.biz/")?,
            api_key_file: env_or("KATFILE_API_KEY_FILE", "/run/secrets/katfile_api_key").into(),
            upload_host_regex: env_or("KATFILE_UPLOAD_HOST_REGEX", r"^s[0-9]{1,6}\.katfile\.biz$"),
            upload_type: env_or("KATFILE_UPLOAD_TYPE", "prem"),
            upload_mode: mode,
            users_parent_folder_id: parse_env("KATFILE_USERS_PARENT_FOLDER_ID", "0")?,
            insecure_test_mode: parse_bool("KATFILE_ALLOW_INSECURE_TEST_ENDPOINTS", false)?,
            upload_stall_timeout: Duration::from_secs(parse_env("KATFILE_UPLOAD_STALL_SECS", "120")?),
            api_retry_base_delay: Duration::from_millis(parse_env("KATFILE_API_RETRY_BASE_MS", "750")?),
        })
    }

    /// Build the API client, reading the key from its secret file.
    pub fn client(&self) -> Result<KatFileClient> {
        let key = ApiKey::from_file(&self.api_key_file).context("loading KatFile API key")?;
        let mut cfg = ClientConfig::new(self.base_url.clone());
        cfg.upload_host_pattern = self.upload_host_regex.clone();
        cfg.upload_type = self.upload_type.clone();
        cfg.upload_stall_timeout = self.upload_stall_timeout;
        cfg.retry_base_delay = self.api_retry_base_delay;
        cfg.insecure_test_mode = self.insecure_test_mode;
        if self.insecure_test_mode {
            tracing::warn!(
                "KATFILE_ALLOW_INSECURE_TEST_ENDPOINTS is enabled: TLS and SSRF protections are OFF (tests only)"
            );
        }
        Ok(KatFileClient::new(cfg, key)?)
    }
}

/// Full worker configuration for `serve`.
#[derive(Debug, Clone)]
pub struct Config {
    pub katfile: KatFileSettings,
    pub bind: SocketAddr,
    pub db_path: PathBuf,
    pub users_dir: PathBuf,
    pub spool_dir: PathBuf,
    /// SFTPGo `temp_path` (atomic-upload temp files).
    pub temp_dir: PathBuf,
    /// Temp files untouched for this long are leftovers of a killed SFTPGo.
    pub stale_temp_after: Duration,
    pub retry_max: u32,
    pub retention: Duration,
    pub min_free_bytes: u64,
    pub unknown_upload_reservation_bytes: u64,
    pub max_active_uploads: usize,
    pub webhook_secret_file: PathBuf,
    pub admin_token_file: PathBuf,
    /// Enables `/hooks/caddy/admission` when set.
    pub admission_secret_file: Option<PathBuf>,
    pub sftpgo_api_url: Url,
    pub sftpgo_api_key_file: PathBuf,
    pub reconcile_interval: Duration,
    pub cleanup_interval: Duration,
    /// Files younger than this are left to the event path during staging scans.
    pub reconcile_min_age: Duration,
    /// When false, events are recorded but nothing is uploaded (transport testing).
    pub processing_enabled: bool,
    /// SFTPGo event protocols accepted for archival.
    pub accepted_protocols: Vec<String>,
    /// Trigger an SFTPGo quota scan after cleaning a user's files.
    pub quota_scan_after_cleanup: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let staging_root: PathBuf = env_or("WORKER_STAGING_ROOT", "/srv/sftpgo").into();
        let users_dir =
            std::env::var("WORKER_USERS_DIR").map(PathBuf::from).unwrap_or_else(|_| staging_root.join("data"));
        let spool_dir =
            std::env::var("WORKER_SPOOL_DIR").map(PathBuf::from).unwrap_or_else(|_| staging_root.join("spool"));
        let temp_dir =
            std::env::var("WORKER_TEMP_DIR").map(PathBuf::from).unwrap_or_else(|_| staging_root.join("uploads-tmp"));
        let cfg = Config {
            katfile: KatFileSettings::from_env()?,
            bind: parse_env("WORKER_BIND", "0.0.0.0:8090")?,
            db_path: env_or("WORKER_DB_PATH", "/var/lib/katfile-worker/worker.db").into(),
            users_dir,
            spool_dir,
            temp_dir,
            stale_temp_after: Duration::from_secs(parse_env("WORKER_STALE_TEMP_SECS", "3600")?),
            retry_max: parse_env("WORKER_RETRY_MAX", "8")?,
            retention: Duration::from_secs(parse_env::<u64>("WORKER_RETENTION_HOURS", "24")? * 3600),
            min_free_bytes: parse_env("WORKER_MIN_FREE_BYTES", "4294967296")?,
            unknown_upload_reservation_bytes: parse_env("WORKER_UNKNOWN_UPLOAD_RESERVATION_BYTES", "1073741824")?,
            max_active_uploads: parse_env("WORKER_MAX_ACTIVE_KATFILE_UPLOADS", "1")?,
            webhook_secret_file: env_or("WORKER_WEBHOOK_SECRET_FILE", "/run/secrets/worker_webhook_secret").into(),
            admin_token_file: env_or("WORKER_ADMIN_TOKEN_FILE", "/run/secrets/worker_admin_token").into(),
            admission_secret_file: std::env::var("WORKER_ADMISSION_SECRET_FILE")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            sftpgo_api_url: parse_env("SFTPGO_API_URL", "http://sftpgo:8080/")?,
            sftpgo_api_key_file: env_or("SFTPGO_API_KEY_FILE", "/run/secrets/sftpgo_worker_api_key").into(),
            reconcile_interval: Duration::from_secs(parse_env("WORKER_RECONCILE_INTERVAL_SECS", "600")?),
            cleanup_interval: Duration::from_secs(parse_env("WORKER_CLEANUP_INTERVAL_SECS", "300")?),
            reconcile_min_age: Duration::from_secs(parse_env("WORKER_RECONCILE_MIN_AGE_SECS", "120")?),
            processing_enabled: parse_bool("WORKER_PROCESSING_ENABLED", true)?,
            accepted_protocols: env_or("WORKER_ACCEPTED_PROTOCOLS", "SFTP,SCP,DAV,HTTP")
                .split(',')
                .map(|p| p.trim().to_owned())
                .filter(|p| !p.is_empty())
                .collect(),
            quota_scan_after_cleanup: parse_bool("WORKER_QUOTA_SCAN_AFTER_CLEANUP", true)?,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.max_active_uploads == 0 || self.max_active_uploads > 8 {
            bail!("WORKER_MAX_ACTIVE_KATFILE_UPLOADS must be between 1 and 8");
        }
        if self.retry_max == 0 {
            bail!("WORKER_RETRY_MAX must be at least 1");
        }
        for (name, p) in [("WORKER_USERS_DIR", &self.users_dir), ("WORKER_SPOOL_DIR", &self.spool_dir)] {
            if !p.is_absolute() {
                bail!("{name} must be an absolute path");
            }
        }
        if self.users_dir.starts_with(&self.spool_dir) || self.spool_dir.starts_with(&self.users_dir) {
            bail!("spool and users directories must not be nested inside each other");
        }
        Ok(())
    }
}

/// Read a secret file (trimmed); refuses empty values.
pub fn read_secret(path: &Path, what: &str) -> Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {what} from {}", path.display()))?;
    let value = raw.trim().to_owned();
    if value.len() < 16 {
        bail!("{what} in {} is shorter than 16 characters", path.display());
    }
    Ok(value)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_owned())
}

fn parse_env<T: FromStr>(name: &str, default: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    let raw = env_or(name, default);
    raw.trim().parse::<T>().map_err(|e| anyhow::anyhow!("invalid {name}={raw:?}: {e}"))
}

fn parse_bool(name: &str, default: bool) -> Result<bool> {
    match std::env::var(name).ok().map(|v| v.trim().to_ascii_lowercase()) {
        None => Ok(default),
        Some(v) if v.is_empty() => Ok(default),
        Some(v) if ["1", "true", "yes", "on"].contains(&v.as_str()) => Ok(true),
        Some(v) if ["0", "false", "no", "off"].contains(&v.as_str()) => Ok(false),
        Some(v) => bail!("invalid boolean {name}={v:?}"),
    }
}
