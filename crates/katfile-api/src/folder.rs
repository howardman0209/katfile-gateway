//! Folder and file-management endpoints.
//!
//! Verified semantics (docs/API_COMPATIBILITY.md):
//! * `folder/create` is NOT idempotent: the same name can be created many times.
//! * `file/set_folder` returns OK even for unknown or foreign file codes, so it must
//!   always be followed by an account-scoped listing check.
//! * `file/list` is account scoped; `fld_id=0` means "no folder filter".

use tracing::debug;

use crate::client::KatFileClient;
use crate::error::KatFileError;
use crate::models::{FileCode, FileEntry, FileInfo, FileListPage, FolderId, FolderListing, value_u64};

/// Page size used when scanning `file/list`.
const LIST_PAGE_SIZE: u32 = 200;

impl KatFileClient {
    /// List the direct children (folders and files) of `folder`.
    pub async fn list_folder(&self, folder: FolderId) -> Result<FolderListing, KatFileError> {
        let reply = self.post_api("folder/list", &[("fld_id", folder.0.to_string())], true).await?;
        FolderListing::from_result(&reply.result)
    }

    /// Create a folder. Never retried automatically: an uncertain outcome must be
    /// resolved by re-listing the parent, because duplicates are allowed remotely.
    pub async fn create_folder(&self, parent: FolderId, name: &str) -> Result<FolderId, KatFileError> {
        if name.is_empty() || name.len() > crate::names::MAX_REMOTE_NAME_BYTES {
            return Err(KatFileError::InvalidArgument("folder name length out of range".into()));
        }
        let reply = self
            .post_api("folder/create", &[("parent_id", parent.0.to_string()), ("name", name.to_owned())], false)
            .await?;
        let id = reply
            .result
            .get("fld_id")
            .and_then(value_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| KatFileError::Malformed("folder/create returned no fld_id".into()))?;
        debug!(parent = parent.0, folder = id, "created KatFile folder");
        Ok(FolderId(id))
    }

    /// Move a file into a folder. Idempotent; success does NOT prove ownership.
    pub async fn set_file_folder(&self, code: &FileCode, folder: FolderId) -> Result<(), KatFileError> {
        self.post_api(
            "file/set_folder",
            &[("file_code", code.as_str().to_owned()), ("fld_id", folder.0.to_string())],
            true,
        )
        .await?;
        Ok(())
    }

    /// One page of the account-scoped file list. `folder: None` lists the whole account.
    pub async fn list_files_page(
        &self,
        folder: Option<FolderId>,
        name_filter: Option<&str>,
        page: u32,
        per_page: u32,
    ) -> Result<FileListPage, KatFileError> {
        let mut params = vec![("page", page.max(1).to_string()), ("per_page", per_page.clamp(1, 1000).to_string())];
        if let Some(f) = folder.filter(|f| f.0 != 0) {
            params.push(("fld_id", f.0.to_string()));
        }
        if let Some(n) = name_filter.filter(|n| !n.is_empty()) {
            params.push(("name", n.to_owned()));
        }
        let reply = self.post_api("file/list", &params, true).await?;
        FileListPage::from_result(&reply.result)
    }

    /// Account-scoped lookup of a file inside `folder`; the authoritative ownership check.
    ///
    /// Tries the server-side name filter first, then falls back to a paged scan of the
    /// folder (bounded by `max_pages`) in case the filter mishandles special characters.
    pub async fn find_file_in_folder(
        &self,
        folder: FolderId,
        code: &FileCode,
        name_hint: &str,
        max_pages: u32,
    ) -> Result<Option<FileEntry>, KatFileError> {
        if folder.0 == 0 {
            return Err(KatFileError::InvalidArgument("ownership checks require a concrete folder id".into()));
        }
        for filter in [Some(name_hint), None] {
            let mut page = 1;
            loop {
                let listing = self.list_files_page(Some(folder), filter, page, LIST_PAGE_SIZE).await?;
                if let Some(hit) = listing.files.iter().find(|f| &f.file_code == code && f.folder_id == Some(folder)) {
                    return Ok(Some(hit.clone()));
                }
                let seen = u64::from(page) * u64::from(LIST_PAGE_SIZE);
                if listing.files.is_empty() || seen >= listing.results_total || page >= max_pages {
                    break;
                }
                page += 1;
            }
        }
        Ok(None)
    }

    /// Account-wide search for files whose demangled name equals `name` exactly
    /// (used to reconcile ambiguous uploads).
    pub async fn find_account_files_named(&self, name: &str, max_pages: u32) -> Result<Vec<FileEntry>, KatFileError> {
        let mut out = Vec::new();
        let mut page = 1;
        loop {
            let listing = self.list_files_page(None, Some(name), page, LIST_PAGE_SIZE).await?;
            out.extend(listing.files.iter().filter(|f| f.name == name).cloned());
            let seen = u64::from(page) * u64::from(LIST_PAGE_SIZE);
            if listing.files.is_empty() || seen >= listing.results_total || page >= max_pages {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    /// `file/info`. Not account scoped: never use as proof that the file belongs to us.
    pub async fn file_info(&self, code: &FileCode) -> Result<FileInfo, KatFileError> {
        let reply = self.post_api("file/info", &[("file_code", code.as_str().to_owned())], true).await?;
        FileInfo::from_result(&reply.result, code)
    }
}
