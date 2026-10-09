//! HTTP client for the KatFile API.
//!
//! Every API call is sent as `POST application/x-www-form-urlencoded` with the key in
//! the body (verified to work for all used endpoints), so the key never appears in a
//! URL, proxy log or error message. Redirects are refused, plain HTTP is refused, and
//! DNS answers pointing at non-public addresses are dropped (SSRF guard).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use serde_json::{Map, Value};
use tracing::debug;
use url::{Host, Url};

use crate::error::{ErrorClass, KatFileError, sanitize_text};
use crate::models::{AccountInfo, value_i64, value_string};

/// Largest JSON body accepted from the API (file listings with large pages stay far below this).
const MAX_API_BODY_BYTES: usize = 16 * 1024 * 1024;

/// API key wrapper that never prints its value.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(raw: &str) -> Result<Self, KatFileError> {
        let key = raw.trim();
        let valid = !key.is_empty()
            && key.len() <= 256
            && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid {
            Ok(ApiKey(key.to_owned()))
        } else {
            Err(KatFileError::InvalidArgument("API key is empty or contains unexpected characters".into()))
        }
    }

    /// Read the key from a secret file (e.g. a Docker secret).
    pub fn from_file(path: &Path) -> Result<Self, KatFileError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| KatFileError::LocalIo(format!("cannot read API key file: {}", e.kind())))?;
        Self::new(&raw)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// Client configuration. Defaults reflect the behaviour verified against katfile.biz.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// API origin, e.g. `https://katfile.biz/`.
    pub base_url: Url,
    /// Regex that every upload CGI host returned by `upload/server` must match.
    pub upload_host_pattern: String,
    /// Value of the multipart `utype` field (`prem` for premium accounts).
    pub upload_type: String,
    pub connect_timeout: Duration,
    /// Per-request timeout for JSON API calls (not used for uploads).
    pub api_timeout: Duration,
    /// Extra attempts for idempotent API calls on transient errors.
    pub read_retries: u32,
    /// First retry delay; later retries back off by a factor of three.
    pub retry_base_delay: Duration,
    /// Abort an upload when no body bytes were consumed for this long.
    pub upload_stall_timeout: Duration,
    /// Minimum time to wait for the CGI reply after the body was fully sent.
    pub upload_response_timeout: Duration,
    /// Read size for streaming the local file.
    pub upload_chunk_bytes: usize,
    pub user_agent: String,
    /// Test only: allow `http://`, any port and private/loopback addresses (mock servers).
    pub insecure_test_mode: bool,
}

impl ClientConfig {
    pub fn new(base_url: Url) -> Self {
        ClientConfig {
            base_url,
            upload_host_pattern: r"^s[0-9]{1,6}\.katfile\.biz$".to_owned(),
            upload_type: "prem".to_owned(),
            connect_timeout: Duration::from_secs(15),
            api_timeout: Duration::from_secs(60),
            read_retries: 2,
            retry_base_delay: Duration::from_millis(750),
            upload_stall_timeout: Duration::from_secs(120),
            upload_response_timeout: Duration::from_secs(600),
            upload_chunk_bytes: 256 * 1024,
            user_agent: concat!("katfile-webdav-gateway/", env!("CARGO_PKG_VERSION")).to_owned(),
            insecure_test_mode: false,
        }
    }
}

/// Parsed API envelope; `status` and `msg` were already checked.
#[derive(Debug)]
pub(crate) struct ApiReply {
    pub result: Value,
    pub root: Map<String, Value>,
}

/// Typed KatFile client. Cheap to clone (shares the connection pool).
#[derive(Clone)]
pub struct KatFileClient {
    pub(crate) http: reqwest::Client,
    pub(crate) cfg: Arc<ClientConfig>,
    key: ApiKey,
    pub(crate) upload_host_re: Arc<Regex>,
}

impl fmt::Debug for KatFileClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KatFileClient").field("base_url", &self.cfg.base_url.as_str()).finish_non_exhaustive()
    }
}

impl KatFileClient {
    pub fn new(cfg: ClientConfig, key: ApiKey) -> Result<Self, KatFileError> {
        if !cfg.insecure_test_mode && cfg.base_url.scheme() != "https" {
            return Err(KatFileError::InvalidArgument("KatFile base URL must use https".into()));
        }
        let upload_host_re = Regex::new(&cfg.upload_host_pattern)
            .map_err(|e| KatFileError::InvalidArgument(format!("invalid upload host pattern: {e}")))?;
        let mut builder = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .connect_timeout(cfg.connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .pool_max_idle_per_host(2)
            .tcp_keepalive(Duration::from_secs(30))
            .http1_only();
        if !cfg.insecure_test_mode {
            builder = builder.https_only(true).dns_resolver(Arc::new(PublicOnlyResolver));
        }
        let http = builder.build().map_err(|e| {
            KatFileError::InvalidArgument(format!("cannot build HTTP client: {}", sanitize_text(&e.to_string())))
        })?;
        Ok(KatFileClient { http, cfg: Arc::new(cfg), key, upload_host_re: Arc::new(upload_host_re) })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    /// `account/info`: validates the key and returns storage figures.
    pub async fn account_info(&self) -> Result<AccountInfo, KatFileError> {
        let reply = self.post_api("account/info", &[], true).await?;
        AccountInfo::from_result(&reply.result)
    }

    /// POST an API call; idempotent calls are retried on transient errors.
    pub(crate) async fn post_api(
        &self,
        endpoint: &'static str,
        params: &[(&'static str, String)],
        idempotent: bool,
    ) -> Result<ApiReply, KatFileError> {
        let attempts = if idempotent { 1 + self.cfg.read_retries } else { 1 };
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.post_api_once(endpoint, params).await {
                Ok(reply) => return Ok(reply),
                Err(err) if err.class() == ErrorClass::Transient && attempt < attempts => {
                    let delay = self.cfg.retry_base_delay * 3u32.pow(attempt - 1);
                    debug!(endpoint, attempt, error = %err, ?delay, "retrying idempotent KatFile call");
                    tokio::time::sleep(delay).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn post_api_once(
        &self,
        endpoint: &'static str,
        params: &[(&'static str, String)],
    ) -> Result<ApiReply, KatFileError> {
        let url = self
            .cfg
            .base_url
            .join(&format!("api/{endpoint}"))
            .map_err(|e| KatFileError::InvalidArgument(format!("bad endpoint: {e}")))?;
        let mut form: Vec<(&str, &str)> = Vec::with_capacity(params.len() + 1);
        form.push(("key", self.key.expose()));
        form.extend(params.iter().map(|(k, v)| (*k, v.as_str())));

        debug!(endpoint, "KatFile API call");
        let resp = self
            .http
            .post(url)
            .form(&form)
            .timeout(self.cfg.api_timeout)
            .send()
            .await
            .map_err(KatFileError::from_reqwest)?;
        let status = resp.status();
        if status.is_redirection() {
            return Err(KatFileError::Redirect { status: status.as_u16() });
        }
        if !status.is_success() {
            return Err(KatFileError::HttpStatus { status: status.as_u16() });
        }
        let body = read_limited(resp, MAX_API_BODY_BYTES).await?;
        parse_api_reply(&body)
    }
}

/// Read a response body, refusing anything larger than `limit` bytes.
pub(crate) async fn read_limited(mut resp: reqwest::Response, limit: usize) -> Result<Vec<u8>, KatFileError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(KatFileError::from_reqwest)? {
        if out.len() + chunk.len() > limit {
            return Err(KatFileError::Malformed(format!("response larger than {limit} bytes")));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Check both the JSON shape and the application-level `status` (HTTP is 200 even for errors).
pub(crate) fn parse_api_reply(body: &[u8]) -> Result<ApiReply, KatFileError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| KatFileError::Malformed(format!("invalid JSON: {e}")))?;
    let Value::Object(root) = value else {
        return Err(KatFileError::Malformed("API reply is not a JSON object".into()));
    };
    let status = root
        .get("status")
        .and_then(value_i64)
        .ok_or_else(|| KatFileError::Malformed("API reply has no status".into()))?;
    if status != 200 {
        let msg = root.get("msg").and_then(value_string).unwrap_or_default();
        if msg.trim().eq_ignore_ascii_case("invalid key") {
            return Err(KatFileError::InvalidKey);
        }
        return Err(KatFileError::Api { status, msg: sanitize_text(&msg) });
    }
    let result = root.get("result").cloned().unwrap_or(Value::Null);
    Ok(ApiReply { result, root })
}

/// Validate an upload CGI URL returned by the provider before any request is made to it.
pub(crate) fn validate_upload_url(raw: &str, host_re: &Regex, insecure_test_mode: bool) -> Result<Url, KatFileError> {
    let url = Url::parse(raw).map_err(|_| KatFileError::UntrustedEndpoint("unparsable upload URL".into()))?;
    if !insecure_test_mode && url.scheme() != "https" {
        return Err(KatFileError::UntrustedEndpoint(format!("upload URL scheme {:?} is not https", url.scheme())));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(KatFileError::UntrustedEndpoint("upload URL carries credentials".into()));
    }
    let host = match url.host() {
        Some(Host::Domain(d)) => d.to_ascii_lowercase(),
        Some(Host::Ipv4(ip)) if insecure_test_mode => ip.to_string(),
        Some(Host::Ipv6(ip)) if insecure_test_mode => ip.to_string(),
        _ => return Err(KatFileError::UntrustedEndpoint("upload URL host must be a DNS name".into())),
    };
    if !host_re.is_match(&host) {
        return Err(KatFileError::UntrustedEndpoint(format!("upload host {host:?} is not allowed")));
    }
    if !insecure_test_mode && url.port_or_known_default() != Some(443) {
        return Err(KatFileError::UntrustedEndpoint("upload URL must use port 443".into()));
    }
    if !url.path().ends_with("/cgi-bin/upload.cgi") {
        return Err(KatFileError::UntrustedEndpoint("unexpected upload URL path".into()));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(KatFileError::UntrustedEndpoint("upload URL must not carry a query or fragment".into()));
    }
    Ok(url)
}

/// DNS resolver that drops loopback/private/link-local/CGNAT/etc. answers.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| is_public_ip(a.ip())).collect();
            if public.is_empty() {
                let err = std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("{host} resolved only to non-public addresses"),
                );
                return Err(Box::new(err) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// True for globally routable unicast addresses.
pub(crate) fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
                || (o[0] == 198 && (o[1] & 0xfe) == 18)) // 198.18.0.0/15 benchmarking
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
                || (s[0] == 0x2001 && s[1] == 0x0db8)) // documentation
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn re() -> Regex {
        Regex::new(r"^s[0-9]{1,6}\.katfile\.biz$").unwrap()
    }

    #[test]
    fn accepts_observed_upload_url() {
        assert!(validate_upload_url("https://s5020.katfile.biz/cgi-bin/upload.cgi", &re(), false).is_ok());
    }

    #[test]
    fn rejects_untrusted_upload_urls() {
        for bad in [
            "http://s5020.katfile.biz/cgi-bin/upload.cgi",
            "https://evil.example/cgi-bin/upload.cgi",
            "https://s5020.katfile.biz.evil.example/cgi-bin/upload.cgi",
            "https://127.0.0.1/cgi-bin/upload.cgi",
            "https://s5020.katfile.biz:8443/cgi-bin/upload.cgi",
            "https://user:pw@s5020.katfile.biz/cgi-bin/upload.cgi",
            "https://s5020.katfile.biz/other.cgi",
            "https://s5020.katfile.biz/cgi-bin/upload.cgi?x=1",
        ] {
            assert!(validate_upload_url(bad, &re(), false).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn public_ip_filter() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.2",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public_ip(private.parse().unwrap()), "{private} must be blocked");
        }
        for public in ["104.20.17.211", "213.152.176.11", "2606:4700::6810:11d3"] {
            assert!(is_public_ip(public.parse().unwrap()), "{public} must be allowed");
        }
    }

    #[test]
    fn api_reply_checks_application_status() {
        assert!(matches!(parse_api_reply(br#"{"status":400,"msg":"Invalid key"}"#), Err(KatFileError::InvalidKey)));
        assert!(matches!(
            parse_api_reply(br#"{"status":403,"msg":"Folder not exist or not yours"}"#),
            Err(KatFileError::Api { status: 403, .. })
        ));
        assert!(matches!(parse_api_reply(b"<html>"), Err(KatFileError::Malformed(_))));
        let ok = parse_api_reply(br#"{"status":"200","msg":"OK","result":{"fld_id":"5"}}"#).unwrap();
        assert_eq!(ok.result["fld_id"], "5");
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let k = ApiKey::new("abc123def").unwrap();
        assert_eq!(format!("{k:?}"), "ApiKey(<redacted>)");
        assert!(ApiKey::new("has space").is_err());
    }
}
