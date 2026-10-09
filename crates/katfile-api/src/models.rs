//! Typed DTOs for KatFile responses.
//!
//! The live API mixes types freely (`fld_id` is a string in `folder/create` but a
//! number in `folder/list`; `storage_used` is a string while `storage_left` is a
//! number), so every field is parsed from `serde_json::Value` defensively.

use std::fmt;

use serde_json::Value;

use crate::error::{KatFileError, sanitize_text};
use crate::names::demangle;

/// KatFile folder identifier; `0` is the account root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FolderId(pub u64);

impl FolderId {
    pub const ROOT: FolderId = FolderId(0);
}

impl fmt::Display for FolderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Remote file identifier returned by the upload CGI (observed: 12 lowercase alphanumerics).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileCode(String);

impl FileCode {
    /// Validate a file code; rejects placeholders such as `undef`.
    pub fn parse(raw: &str) -> Result<Self, KatFileError> {
        let ok = (8..=32).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        if ok {
            Ok(FileCode(raw.to_owned()))
        } else {
            Err(KatFileError::Malformed(format!("invalid file_code {:?}", sanitize_text(raw))))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FileCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Account summary from `account/info`. The account e-mail is intentionally not retained.
#[derive(Debug, Clone, Default)]
pub struct AccountInfo {
    pub storage_used_bytes: Option<u64>,
    /// `None` when unlimited (`"inf"`) or not reported.
    pub storage_left_bytes: Option<u64>,
    pub premium_expire: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    pub id: FolderId,
    /// Demangled (original UTF-8) name.
    pub name: String,
    /// Name exactly as returned by the API.
    pub raw_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub file_code: FileCode,
    /// Demangled (original UTF-8) name.
    pub name: String,
    pub raw_name: String,
    pub folder_id: Option<FolderId>,
    pub size: Option<u64>,
    /// Provider-local timestamp string ("YYYY-MM-DD HH:MM:SS", provider time zone).
    pub uploaded: Option<String>,
}

/// Direct children of a folder (`folder/list`). The API returned every child in one
/// response and ignored paging parameters during V0 verification.
#[derive(Debug, Clone, Default)]
pub struct FolderListing {
    pub folders: Vec<FolderEntry>,
    pub files: Vec<FileEntry>,
}

/// One page of `file/list`.
#[derive(Debug, Clone, Default)]
pub struct FileListPage {
    pub files: Vec<FileEntry>,
    pub results_total: u64,
}

/// Result of `file/info`. NOTE: this endpoint is *not* account scoped; it also
/// describes anonymous/public files, so it must never be used as ownership proof.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub file_code: FileCode,
    pub found: bool,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub uploaded: Option<String>,
}

/// Upload target from `upload/server`. The session ID is a credential and is redacted from Debug.
#[derive(Clone)]
pub struct UploadServer {
    pub url: url::Url,
    pub(crate) sess_id: String,
    /// Provider clock at the time the session was issued; used to bound reconciliation searches.
    pub server_time: Option<String>,
}

impl fmt::Debug for UploadServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadServer")
            .field("url", &self.url.as_str())
            .field("sess_id", &"<redacted>")
            .field("server_time", &self.server_time)
            .finish()
    }
}

pub(crate) fn value_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn value_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn value_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn malformed(what: &str) -> KatFileError {
    KatFileError::Malformed(what.to_owned())
}

impl AccountInfo {
    pub(crate) fn from_result(result: &Value) -> Result<Self, KatFileError> {
        let obj = result.as_object().ok_or_else(|| malformed("account/info result is not an object"))?;
        Ok(AccountInfo {
            storage_used_bytes: obj.get("storage_used").and_then(value_u64),
            storage_left_bytes: obj.get("storage_left").and_then(value_u64),
            premium_expire: obj.get("premium_expire").and_then(value_string),
        })
    }
}

impl FolderEntry {
    fn from_value(v: &Value) -> Result<Self, KatFileError> {
        let id = v.get("fld_id").and_then(value_u64).ok_or_else(|| malformed("folder entry without fld_id"))?;
        let raw_name = v.get("name").and_then(value_string).unwrap_or_default();
        Ok(FolderEntry { id: FolderId(id), name: demangle(&raw_name), raw_name })
    }
}

impl FileEntry {
    fn from_value(v: &Value) -> Result<Self, KatFileError> {
        let code = v
            .get("file_code")
            .or_else(|| v.get("filecode"))
            .and_then(value_string)
            .ok_or_else(|| malformed("file entry without file_code"))?;
        let raw_name = v.get("name").and_then(value_string).unwrap_or_default();
        Ok(FileEntry {
            file_code: FileCode::parse(&code)?,
            name: demangle(&raw_name),
            raw_name,
            folder_id: v.get("fld_id").and_then(value_u64).map(FolderId),
            size: v.get("size").and_then(value_u64),
            uploaded: v.get("uploaded").and_then(value_string),
        })
    }
}

fn parse_list<T>(v: Option<&Value>, f: fn(&Value) -> Result<T, KatFileError>) -> Result<Vec<T>, KatFileError> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(f).collect(),
        Some(_) => Err(malformed("expected an array")),
    }
}

impl FolderListing {
    pub(crate) fn from_result(result: &Value) -> Result<Self, KatFileError> {
        let obj = result.as_object().ok_or_else(|| malformed("folder/list result is not an object"))?;
        Ok(FolderListing {
            folders: parse_list(obj.get("folders"), FolderEntry::from_value)?,
            files: parse_list(obj.get("files"), FileEntry::from_value)?,
        })
    }
}

impl FileListPage {
    pub(crate) fn from_result(result: &Value) -> Result<Self, KatFileError> {
        let obj = result.as_object().ok_or_else(|| malformed("file/list result is not an object"))?;
        let files = parse_list(obj.get("files"), FileEntry::from_value)?;
        let results_total = obj.get("results_total").and_then(value_u64).unwrap_or(files.len() as u64);
        Ok(FileListPage { files, results_total })
    }
}

impl FileInfo {
    pub(crate) fn from_result(result: &Value, requested: &FileCode) -> Result<Self, KatFileError> {
        let items = result.as_array().ok_or_else(|| malformed("file/info result is not an array"))?;
        let item = items
            .iter()
            .find(|i| {
                i.get("filecode").or_else(|| i.get("file_code")).and_then(value_string).as_deref()
                    == Some(requested.as_str())
            })
            .ok_or_else(|| malformed("file/info result does not mention the requested file"))?;
        let found = item.get("status").and_then(value_i64) == Some(200);
        Ok(FileInfo {
            file_code: requested.clone(),
            found,
            name: item.get("name").and_then(value_string).map(|n| demangle(&n)),
            size: item.get("size").and_then(value_u64),
            uploaded: item.get("uploaded").and_then(value_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folder_listing_accepts_mixed_id_types_and_demangles() {
        let v = json!({
            "folders": [{"fld_id": 211628, "name": "kfgw", "code": null},
                        {"fld_id": "211633", "name": "ç\u{9b}¸ç\u{89}\u{87}"}],
            "files": [{"fld_id": 0, "file_code": "79cpmm7q3pmg", "name": "a.txt", "uploaded": "2026-10-09 04:33:59"}]
        });
        let l = FolderListing::from_result(&v).unwrap();
        assert_eq!(l.folders[0].id, FolderId(211628));
        assert_eq!(l.folders[1].id, FolderId(211633));
        assert_eq!(l.folders[1].name, "相片");
        assert_eq!(l.files[0].file_code.as_str(), "79cpmm7q3pmg");
        assert_eq!(l.files[0].folder_id, Some(FolderId::ROOT));
    }

    #[test]
    fn file_info_reports_missing_items() {
        let code = FileCode::parse("zzzzzzzzzzzz").unwrap();
        let v = json!([{"filecode": "zzzzzzzzzzzz", "status": 404}]);
        let info = FileInfo::from_result(&v, &code).unwrap();
        assert!(!info.found);
    }

    #[test]
    fn file_code_rejects_placeholders() {
        assert!(FileCode::parse("undef").is_err());
        assert!(FileCode::parse("ABCDEFGHIJKL").is_err());
        assert!(FileCode::parse("79cpmm7q3pmg").is_ok());
    }

    #[test]
    fn account_info_handles_string_and_number_sizes() {
        let v = json!({"storage_used": "180427", "storage_left": 879609302040373u64, "premium_expire": "2036-08-03 20:49:57", "email": "x@y"});
        let a = AccountInfo::from_result(&v).unwrap();
        assert_eq!(a.storage_used_bytes, Some(180427));
        assert_eq!(a.storage_left_bytes, Some(879609302040373));
        let inf = AccountInfo::from_result(&json!({"storage_left": "inf"})).unwrap();
        assert_eq!(inf.storage_left_bytes, None);
    }
}
