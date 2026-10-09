//! In-process test double for the KatFile API and upload CGI.
//!
//! It reproduces the quirks verified against katfile.biz during V0 so that the
//! client and worker can be tested without a real API key:
//! * HTTP 200 with an application-level `status` for API errors;
//! * names stored as bytes and rendered Latin-1 style (UTF-8 mojibake), `"` stored
//!   as `&quote;`, names truncated to 128 bytes, duplicate folder names allowed;
//! * `folder/create` returns `fld_id` as a string, listings return numbers;
//! * `file/set_folder` answers OK for unknown or foreign file codes;
//! * an invalid upload session produces an anonymous (not account owned) file;
//! * empty uploads are rejected with `null filesize or wrong file path`;
//! * `file/list` with `fld_id=0` lists the whole account.
//!
//! Faults can be queued per endpoint to exercise retry and ambiguity handling.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use url::Url;

/// Maximum stored name length observed on katfile.biz.
const MAX_STORED_NAME: usize = 128;

/// Fault injected for the next call of an endpoint (`"folder/create"`, `"upload.cgi"`, ...).
#[derive(Debug, Clone)]
pub enum Fault {
    /// Reply with this HTTP status and an HTML body.
    HttpStatus(u16),
    /// HTTP 200 with `{"status": code, "msg": msg}`.
    AppError(i64, String),
    /// Sleep before handling the request normally.
    Delay(Duration),
    /// HTTP 200 with a non-JSON body.
    MalformedJson,
    /// `folder/create`: create the folder, then answer HTTP 502 (uncertain outcome).
    CreateThenFail,
    /// `upload.cgi`: store the file, then answer HTTP 500 (lost reply).
    StoreThenFail,
    /// `upload.cgi`: store the file, then wait before answering OK (client timeout).
    StoreThenHang(Duration),
    /// `upload.cgi`: store the file anonymously (as if the session had expired) and answer OK.
    StoreAnonymously,
    /// `upload.cgi`: refuse the file with this `file_status`.
    Reject(String),
}

#[derive(Debug, Clone)]
pub struct MockFolder {
    pub id: u64,
    pub parent: u64,
    /// Bytes as KatFile stores them (after `&quote;` rewrite and truncation).
    pub stored_name: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MockFile {
    pub code: String,
    pub stored_name: Vec<u8>,
    pub size: u64,
    pub sha256: String,
    /// `None` = anonymous upload (not visible in account-scoped listings).
    pub folder: Option<u64>,
    pub uploaded: String,
    pub chunked: bool,
}

#[derive(Debug, Default)]
pub struct MockState {
    pub api_key: String,
    pub next_folder_id: u64,
    pub next_file_seq: u64,
    pub folders: BTreeMap<u64, MockFolder>,
    pub files: BTreeMap<String, MockFile>,
    pub sessions: HashSet<String>,
    pub faults: HashMap<String, VecDeque<Fault>>,
    /// Endpoint names in call order (`"account/info"`, `"upload.cgi"`, ...).
    pub calls: Vec<String>,
    /// Answer 411 to chunked uploads (the real service accepts them).
    pub reject_chunked: bool,
    /// Base URL advertised for the upload CGI.
    pub upload_base: Option<Url>,
}

impl MockState {
    /// Files owned by the account inside `folder` (0 = root).
    pub fn files_in(&self, folder: u64) -> Vec<MockFile> {
        self.files.values().filter(|f| f.folder == Some(folder)).cloned().collect()
    }

    pub fn anonymous_files(&self) -> Vec<MockFile> {
        self.files.values().filter(|f| f.folder.is_none()).cloned().collect()
    }

    pub fn children(&self, parent: u64) -> Vec<MockFolder> {
        self.folders.values().filter(|f| f.parent == parent).cloned().collect()
    }

    pub fn call_count(&self, endpoint: &str) -> usize {
        self.calls.iter().filter(|c| c.as_str() == endpoint).count()
    }

    /// Render stored bytes the way the live API does (each byte as a Latin-1 char).
    pub fn render_name(stored: &[u8]) -> String {
        stored.iter().map(|b| char::from(*b)).collect()
    }

    /// Store a name the way the live API does.
    pub fn store_name(input: &str) -> Vec<u8> {
        let mut bytes = input.replace('"', "&quote;").into_bytes();
        bytes.truncate(MAX_STORED_NAME);
        bytes
    }
}

/// Handle to a running mock server.
pub struct MockKatFile {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    task: JoinHandle<()>,
}

impl MockKatFile {
    /// Start on an ephemeral loopback port.
    pub async fn start(api_key: &str) -> MockKatFile {
        Self::start_on("127.0.0.1:0".parse().expect("valid addr"), api_key, None).await
    }

    /// Start on `addr`; `public_base` overrides the advertised upload URL base.
    pub async fn start_on(addr: SocketAddr, api_key: &str, public_base: Option<Url>) -> MockKatFile {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind mock listener");
        let addr = listener.local_addr().expect("local addr");
        let upload_base = public_base.unwrap_or_else(|| Url::parse(&format!("http://{addr}/")).expect("url"));
        let state = Arc::new(Mutex::new(MockState {
            api_key: api_key.to_owned(),
            next_folder_id: 300_000,
            next_file_seq: 1,
            upload_base: Some(upload_base),
            ..MockState::default()
        }));
        let app = Router::new()
            .route("/api/{*endpoint}", any(api_handler))
            .route("/cgi-bin/upload.cgi", post(upload_handler))
            .layer(DefaultBodyLimit::disable())
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockKatFile { addr, state, task }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn base_url(&self) -> Url {
        Url::parse(&format!("http://{}/", self.addr)).expect("url")
    }

    pub fn state(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().expect("mock state poisoned")
    }

    /// Queue a fault for the next call to `endpoint`.
    pub fn inject(&self, endpoint: &str, fault: Fault) {
        self.state().faults.entry(endpoint.to_owned()).or_default().push_back(fault);
    }

    /// Plant an account-owned file (e.g. an unrelated upload with the same name).
    pub fn add_file(&self, name: &str, size: u64, folder: u64, uploaded: &str) -> String {
        let mut st = self.state();
        st.next_file_seq += 1;
        let code = file_code_for(st.next_file_seq);
        st.files.insert(
            code.clone(),
            MockFile {
                code: code.clone(),
                stored_name: MockState::store_name(name),
                size,
                sha256: String::new(),
                folder: Some(folder),
                uploaded: uploaded.to_owned(),
                chunked: false,
            },
        );
        code
    }

    /// Pre-create a folder (e.g. a name collision) and return its id.
    pub fn add_folder(&self, parent: u64, name: &str) -> u64 {
        let mut st = self.state();
        st.next_folder_id += 1;
        let id = st.next_folder_id;
        st.folders.insert(id, MockFolder { id, parent, stored_name: MockState::store_name(name) });
        id
    }
}

impl Drop for MockKatFile {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Shared = Arc<Mutex<MockState>>;

fn json_reply(v: Value) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn api_error(status: i64, msg: &str) -> Response {
    json_reply(json!({"status": status, "msg": msg, "server_time": now_string()}))
}

fn api_ok(result: Value) -> Response {
    json_reply(json!({"status": 200, "msg": "OK", "result": result, "server_time": now_string()}))
}

/// Pop the next queued fault for an endpoint and record the call.
fn take_fault(state: &Shared, endpoint: &str) -> Option<Fault> {
    let mut st = state.lock().expect("poisoned");
    st.calls.push(endpoint.to_owned());
    st.faults.get_mut(endpoint).and_then(|q| q.pop_front())
}

/// Apply generic faults; returns a response when the fault short-circuits the call.
async fn apply_generic_fault(fault: &Option<Fault>) -> Option<Response> {
    match fault {
        Some(Fault::Delay(d)) => {
            tokio::time::sleep(*d).await;
            None
        }
        Some(Fault::HttpStatus(code)) => Some(
            (StatusCode::from_u16(*code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), "<html>error</html>")
                .into_response(),
        ),
        Some(Fault::AppError(status, msg)) => Some(api_error(*status, msg)),
        Some(Fault::MalformedJson) => Some((StatusCode::OK, "<html>not json</html>").into_response()),
        _ => None,
    }
}

async fn api_handler(
    State(state): State<Shared>,
    Path(endpoint): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut params = query;
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if is_form {
        for (k, v) in url::form_urlencoded::parse(&body) {
            params.insert(k.into_owned(), v.into_owned());
        }
    }
    let fault = take_fault(&state, &endpoint);
    if let Some(resp) = apply_generic_fault(&fault).await {
        return resp;
    }
    {
        let st = state.lock().expect("poisoned");
        if params.get("key").map(String::as_str) != Some(st.api_key.as_str()) {
            return api_error(400, "Invalid key");
        }
    }
    let p = |name: &str| params.get(name).cloned().unwrap_or_default();
    let num = |name: &str| params.get(name).and_then(|v| v.parse::<u64>().ok());
    match endpoint.as_str() {
        "account/info" => api_ok(json!({
            "storage_used": "1024", "storage_left": 879609302040373u64,
            "premium_expire": "2036-08-03 20:49:57", "balance": "0.00000", "email": "mock@example.invalid"
        })),
        "folder/list" => folder_list(&state, num("fld_id").unwrap_or(0)),
        "folder/create" => folder_create(&state, num("parent_id").unwrap_or(0), &p("name"), &fault),
        "upload/server" => upload_server(&state),
        "file/set_folder" => set_folder(&state, &p("file_code"), num("fld_id").unwrap_or(0)),
        "file/list" => file_list(&state, &params),
        "file/info" => file_info(&state, &p("file_code")),
        "file/direct_link" => api_error(403, "Not enabled"),
        _ => (StatusCode::NOT_FOUND, "<html>Not Found</html>").into_response(),
    }
}

fn folder_list(state: &Shared, fld_id: u64) -> Response {
    let st = state.lock().expect("poisoned");
    if fld_id != 0 && !st.folders.contains_key(&fld_id) {
        return api_error(403, "Folder not exist or not yours");
    }
    let folders: Vec<Value> = st
        .children(fld_id)
        .iter()
        .map(|f| json!({"fld_id": f.id, "name": MockState::render_name(&f.stored_name), "code": null}))
        .collect();
    let files: Vec<Value> = st
        .files_in(fld_id)
        .iter()
        .map(|f| {
            json!({"fld_id": fld_id, "file_code": f.code, "name": MockState::render_name(&f.stored_name),
                   "link": format!("https://mock.invalid/{}.html", f.code), "uploaded": f.uploaded})
        })
        .collect();
    api_ok(json!({"folders": folders, "files": files}))
}

fn folder_create(state: &Shared, parent: u64, name: &str, fault: &Option<Fault>) -> Response {
    let mut st = state.lock().expect("poisoned");
    if parent != 0 && !st.folders.contains_key(&parent) {
        return api_error(403, "Folder not exist or not yours");
    }
    if name.is_empty() {
        return api_error(400, "No name");
    }
    st.next_folder_id += 1;
    let id = st.next_folder_id;
    st.folders.insert(id, MockFolder { id, parent, stored_name: MockState::store_name(name) });
    if matches!(fault, Some(Fault::CreateThenFail)) {
        return (StatusCode::BAD_GATEWAY, "<html>bad gateway</html>").into_response();
    }
    // The live API returns the new id as a string here.
    api_ok(json!({"fld_id": id.to_string()}))
}

fn upload_server(state: &Shared) -> Response {
    let mut st = state.lock().expect("poisoned");
    st.next_file_seq += 1;
    let sess = format!("{:0>61}", format!("sess{}", st.next_file_seq));
    st.sessions.insert(sess.clone());
    let base = st.upload_base.clone().expect("upload base");
    let url = base.join("cgi-bin/upload.cgi").expect("upload url");
    json_reply(
        json!({"status": 200, "msg": "OK", "result": url.as_str(), "sess_id": sess, "server_time": now_string()}),
    )
}

fn set_folder(state: &Shared, code: &str, fld_id: u64) -> Response {
    let mut st = state.lock().expect("poisoned");
    if fld_id != 0 && !st.folders.contains_key(&fld_id) {
        return api_error(403, "Your account don't have such folder");
    }
    // Verified quirk: unknown or anonymous files are silently ignored with status 200.
    if let Some(f) = st.files.get_mut(code)
        && f.folder.is_some()
    {
        f.folder = Some(fld_id);
    }
    json_reply(json!({"status": 200, "msg": "OK", "server_time": now_string()}))
}

fn file_list(state: &Shared, params: &HashMap<String, String>) -> Response {
    let st = state.lock().expect("poisoned");
    let fld = params.get("fld_id").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let name = params.get("name").cloned().unwrap_or_default();
    let page = params.get("page").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1);
    let per_page = params.get("per_page").and_then(|v| v.parse::<usize>().ok()).unwrap_or(20).clamp(1, 1000);
    let name_bytes = name.as_bytes();
    let matching: Vec<&MockFile> = st
        .files
        .values()
        .filter(|f| f.folder.is_some())
        .filter(|f| fld == 0 || f.folder == Some(fld))
        .filter(|f| name_bytes.is_empty() || f.stored_name.windows(name_bytes.len()).any(|w| w == name_bytes))
        .collect();
    let total = matching.len();
    let files: Vec<Value> = matching
        .iter()
        .skip((page - 1) * per_page)
        .take(per_page)
        .map(|f| {
            json!({"file_code": f.code, "name": MockState::render_name(&f.stored_name), "fld_id": f.folder.unwrap_or(0),
                   "size": f.size, "uploaded": f.uploaded, "public": 0, "downloads": 0, "thumbnail": null,
                   "link": format!("https://mock.invalid/{}.html", f.code)})
        })
        .collect();
    let results = files.len();
    api_ok(json!({"files": files, "results_total": total, "results": results}))
}

fn file_info(state: &Shared, code: &str) -> Response {
    let st = state.lock().expect("poisoned");
    let item = match st.files.get(code) {
        Some(f) => json!({"filecode": f.code, "status": 200, "name": MockState::render_name(&f.stored_name),
                          "size": f.size, "uploaded": f.uploaded, "downloads": 0}),
        None => json!({"filecode": code, "status": 404}),
    };
    api_ok(json!([item]))
}

async fn upload_handler(State(state): State<Shared>, headers: HeaderMap, mut multipart: Multipart) -> Response {
    let chunked = headers.get(header::TRANSFER_ENCODING).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"chunked"));
    let fault = take_fault(&state, "upload.cgi");
    if let Some(resp) = apply_generic_fault(&fault).await {
        return resp;
    }
    if let Some(Fault::Reject(status)) = &fault {
        return json_reply(json!([{"file_status": status, "file_code": "undef"}]));
    }
    if chunked && state.lock().expect("poisoned").reject_chunked {
        return (StatusCode::LENGTH_REQUIRED, "<html>length required</html>").into_response();
    }

    let mut sess = String::new();
    let mut upload: Option<(String, u64, String)> = None;
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => return (StatusCode::BAD_REQUEST, "bad multipart").into_response(),
        };
        match field.name().unwrap_or("") {
            "sess_id" => sess = field.text().await.unwrap_or_default(),
            "file_0" => {
                let name = field.file_name().unwrap_or("").to_owned();
                let mut field = field;
                let mut hasher = Sha256::new();
                let mut size = 0u64;
                loop {
                    match field.chunk().await {
                        Ok(Some(c)) => {
                            size += c.len() as u64;
                            hasher.update(&c);
                        }
                        Ok(None) => break,
                        Err(_) => return (StatusCode::BAD_REQUEST, "truncated upload").into_response(),
                    }
                }
                upload = Some((name, size, hex::encode(hasher.finalize())));
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let Some((name, size, sha256)) = upload.filter(|(_, size, _)| *size > 0) else {
        return json_reply(json!([{"file_status": "null filesize or wrong file path", "file_code": "undef"}]));
    };
    let code = {
        let mut st = state.lock().expect("poisoned");
        st.next_file_seq += 1;
        let code = file_code_for(st.next_file_seq);
        // Verified quirk: an unknown session still stores the file, anonymously.
        let anonymous = matches!(fault, Some(Fault::StoreAnonymously));
        let folder = (st.sessions.contains(&sess) && !anonymous).then_some(0);
        st.files.insert(
            code.clone(),
            MockFile {
                code: code.clone(),
                stored_name: MockState::store_name(&name),
                size,
                sha256,
                folder,
                uploaded: now_string(),
                chunked,
            },
        );
        code
    };
    match fault {
        Some(Fault::StoreThenFail) => (StatusCode::INTERNAL_SERVER_ERROR, "<html>error</html>").into_response(),
        Some(Fault::StoreThenHang(d)) => {
            tokio::time::sleep(d).await;
            json_reply(json!([{"file_status": "OK", "file_code": code}]))
        }
        _ => json_reply(json!([{"file_status": "OK", "file_code": code}])),
    }
}

/// Deterministic 12-character lowercase alphanumeric file code.
fn file_code_for(seq: u64) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = seq.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..12)
        .map(|_| {
            let c = ALPHABET[(n % 36) as usize] as char;
            n /= 36;
            if n == 0 {
                n = seq + 7;
            }
            c
        })
        .collect()
}

/// UTC timestamp in the provider's "YYYY-MM-DD HH:MM:SS" format.
fn now_string() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_names_like_katfile() {
        let stored = MockState::store_name("quote\"x");
        assert_eq!(stored, b"quote&quote;x");
        let rendered = MockState::render_name(&MockState::store_name("相片"));
        assert_eq!(rendered, "ç\u{9b}¸ç\u{89}\u{87}");
    }

    #[test]
    fn file_codes_are_valid_and_unique() {
        let a = file_code_for(1);
        let b = file_code_for(2);
        assert_ne!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_735), (2026, 10, 9));
    }
}
