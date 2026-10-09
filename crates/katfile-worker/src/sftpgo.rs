//! Minimal SFTPGo REST client (admin API key with `view_users` + `quota_scans` only)
//! and the per-user policy the gateway requires before archiving.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

/// The subset of SFTPGo's user object the worker relies on.
#[derive(Debug, Clone, Deserialize)]
pub struct SftpgoUser {
    pub id: i64,
    pub username: String,
    pub status: i64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub expiration_date: i64,
    #[serde(default)]
    pub home_dir: String,
    #[serde(default)]
    pub permissions: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub virtual_folders: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub groups: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    pub filesystem: Option<Filesystem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Filesystem {
    #[serde(default)]
    pub provider: i64,
}

/// Whether the worker may archive this account's uploads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    Ok,
    /// Account disabled or expired: block new archival, keep everything.
    Disabled(String),
    /// Account configured in a way the gateway cannot handle safely.
    Unsupported(String),
}

/// Required settings (see docs/OPERATIONS.md, "Creating users"):
/// home under the users dir, local filesystem, no virtual folders or groups, and no
/// permission that allows modifying an existing file in place (`*`, `overwrite`)
/// because snapshots are hard links that must stay immutable.
pub fn evaluate_policy(user: &SftpgoUser, users_dir: &Path, now_ms: i64) -> Policy {
    let expected_home = users_dir.join(&user.username);
    if Path::new(&user.home_dir) != expected_home {
        return Policy::Unsupported(format!("home_dir must be {}", expected_home.display()));
    }
    if user.filesystem.as_ref().is_some_and(|f| f.provider != 0) {
        return Policy::Unsupported("only the local filesystem provider is supported".into());
    }
    if user.virtual_folders.as_ref().is_some_and(|v| !v.is_empty()) {
        return Policy::Unsupported("virtual folders are not supported".into());
    }
    if user.groups.as_ref().is_some_and(|g| !g.is_empty()) {
        return Policy::Unsupported("group membership is not supported (permissions must be set on the user)".into());
    }
    for (dir, perms) in &user.permissions {
        if let Some(bad) = perms.iter().find(|p| ["*", "overwrite", "create_symlinks"].contains(&p.as_str())) {
            return Policy::Unsupported(format!(
                "permission {bad:?} on {dir:?} is not allowed (use list,download,upload,create_dirs,rename,delete)"
            ));
        }
    }
    if !user.permissions.get("/").is_some_and(|p| p.iter().any(|x| x == "upload")) {
        return Policy::Unsupported("the root permission set must include upload".into());
    }
    if user.status != 1 {
        return Policy::Disabled("account is disabled in SFTPGo".into());
    }
    if user.expiration_date > 0 && user.expiration_date <= now_ms {
        return Policy::Disabled("account has expired".into());
    }
    Policy::Ok
}

#[derive(Clone)]
pub struct SftpgoClient {
    http: reqwest::Client,
    base: Url,
    api_key: String,
}

impl SftpgoClient {
    pub fn new(base: Url, api_key: String) -> Result<SftpgoClient> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(SftpgoClient { http, base, api_key })
    }

    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("SFTPGo base URL cannot have path segments"))?
            .pop_if_empty()
            .extend(["api", "v2"])
            .extend(segments);
        Ok(url)
    }

    /// Authoritative user record; `None` when the user does not exist.
    pub async fn user(&self, username: &str) -> Result<Option<SftpgoUser>> {
        let resp = self
            .http
            .get(self.url(&["users", username])?)
            .header("X-SFTPGO-API-KEY", &self.api_key)
            .send()
            .await
            .context("SFTPGo API unreachable")?;
        match resp.status().as_u16() {
            200 => Ok(Some(resp.json().await.context("decoding SFTPGo user")?)),
            404 => Ok(None),
            s => bail!("SFTPGo GET user returned HTTP {s}"),
        }
    }

    /// All users (paged by 500).
    pub async fn users(&self) -> Result<Vec<SftpgoUser>> {
        let mut out = Vec::new();
        loop {
            let mut url = self.url(&["users"])?;
            url.query_pairs_mut().append_pair("offset", &out.len().to_string()).append_pair("limit", "500");
            let resp = self.http.get(url).header("X-SFTPGO-API-KEY", &self.api_key).send().await?;
            if !resp.status().is_success() {
                bail!("SFTPGo list users returned HTTP {}", resp.status().as_u16());
            }
            let page: Vec<SftpgoUser> = resp.json().await.context("decoding SFTPGo users")?;
            let n = page.len();
            out.extend(page);
            if n < 500 {
                return Ok(out);
            }
        }
    }

    /// Ask SFTPGo to recount a user's quota after files were removed outside SFTPGo.
    pub async fn start_quota_scan(&self, username: &str) -> Result<()> {
        let resp = self
            .http
            .post(self.url(&["quotas", "users", username, "scan"])?)
            .header("X-SFTPGO-API-KEY", &self.api_key)
            .send()
            .await?;
        match resp.status().as_u16() {
            // 409: a scan is already running, which is just as good.
            200..=299 | 409 => Ok(()),
            s => bail!("SFTPGo quota scan returned HTTP {s}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_user() -> SftpgoUser {
        let raw = include_str!("../../../tests/fixtures/sftpgo_provider_user_add.json");
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        serde_json::from_value(v["body"].clone()).unwrap()
    }

    #[test]
    fn fixture_user_from_real_sftpgo_payload_is_accepted() {
        let u = fixture_user();
        assert_eq!(u.username, "alice");
        assert!(u.created_at > 0);
        assert_eq!(evaluate_policy(&u, Path::new("/srv/sftpgo/data"), 0), Policy::Ok);
    }

    #[test]
    fn unsafe_settings_are_rejected() {
        let users_dir = Path::new("/srv/sftpgo/data");
        let mut u = fixture_user();
        u.permissions.insert("/".into(), vec!["*".into()]);
        assert!(matches!(evaluate_policy(&u, users_dir, 0), Policy::Unsupported(_)));

        let mut u = fixture_user();
        u.permissions.insert("/sub".into(), vec!["list".into(), "overwrite".into()]);
        assert!(matches!(evaluate_policy(&u, users_dir, 0), Policy::Unsupported(_)));

        let mut u = fixture_user();
        u.home_dir = "/srv/sftpgo".into();
        assert!(matches!(evaluate_policy(&u, users_dir, 0), Policy::Unsupported(_)));

        let mut u = fixture_user();
        u.groups = Some(vec![serde_json::json!({"name": "g"})]);
        assert!(matches!(evaluate_policy(&u, users_dir, 0), Policy::Unsupported(_)));
    }

    #[test]
    fn disabled_and_expired_users_are_paused() {
        let users_dir = Path::new("/srv/sftpgo/data");
        let mut u = fixture_user();
        u.status = 0;
        assert!(matches!(evaluate_policy(&u, users_dir, 0), Policy::Disabled(_)));
        let mut u = fixture_user();
        u.expiration_date = 1000;
        assert!(matches!(evaluate_policy(&u, users_dir, 2000), Policy::Disabled(_)));
    }

    #[test]
    fn api_urls_are_built_from_segments() {
        let c = SftpgoClient::new(Url::parse("http://sftpgo:8080/").unwrap(), "k".into()).unwrap();
        assert_eq!(c.url(&["users", "alice"]).unwrap().as_str(), "http://sftpgo:8080/api/v2/users/alice");
        assert_eq!(
            c.url(&["quotas", "users", "a b", "scan"]).unwrap().as_str(),
            "http://sftpgo:8080/api/v2/quotas/users/a%20b/scan"
        );
    }
}
