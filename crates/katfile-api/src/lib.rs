//! Typed, streaming client for the KatFile (XFileSharing Pro based) API.
//!
//! Behaviour is pinned to what was verified against the live katfile.biz service;
//! see `docs/API_COMPATIBILITY.md` for the evidence behind each rule.

pub mod client;
pub mod error;
pub mod folder;
pub mod models;
pub mod names;
pub mod upload;

pub use client::{ApiKey, ClientConfig, KatFileClient};
pub use error::{ErrorClass, KatFileError};
pub use models::{
    AccountInfo, FileCode, FileEntry, FileInfo, FileListPage, FolderEntry, FolderId, FolderListing, UploadServer,
};
pub use upload::{UploadMode, UploadProgress, UploadReceipt, UploadRequest};
