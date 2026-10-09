//! Client behaviour against the in-process mock (no real API key needed).

use std::io::Write;
use std::time::Duration;

use katfile_api::{
    ApiKey, ClientConfig, ErrorClass, FileCode, FolderId, KatFileClient, KatFileError, UploadMode, UploadProgress,
    UploadRequest,
};
use katfile_mock::{Fault, MockKatFile};

const KEY: &str = "test-key-123";

fn client_for(mock: &MockKatFile) -> KatFileClient {
    let mut cfg = ClientConfig::new(mock.base_url());
    cfg.insecure_test_mode = true;
    cfg.upload_host_pattern = r"^127\.0\.0\.1$".into();
    cfg.retry_base_delay = Duration::from_millis(10);
    cfg.upload_chunk_bytes = 16 * 1024;
    KatFileClient::new(cfg, ApiKey::new(KEY).unwrap()).unwrap()
}

fn temp_file_with(content: &[u8]) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content).unwrap();
    f.flush().unwrap();
    f
}

async fn upload_bytes(
    client: &KatFileClient,
    content: &[u8],
    name: &str,
    mode: UploadMode,
) -> Result<katfile_api::UploadReceipt, KatFileError> {
    let tmp = temp_file_with(content);
    let file = tokio::fs::File::open(tmp.path()).await.unwrap();
    let server = client.request_upload_server().await?;
    let req = UploadRequest { file, size: content.len() as u64, remote_name: name.to_owned(), mode };
    client.upload(&server, req, UploadProgress::new()).await
}

#[tokio::test]
async fn account_info_and_invalid_key() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let info = client.account_info().await.unwrap();
    assert_eq!(info.storage_used_bytes, Some(1024));

    let mut cfg = client.config().clone();
    cfg.read_retries = 0;
    let bad = KatFileClient::new(cfg, ApiKey::new("wrong").unwrap()).unwrap();
    let err = bad.account_info().await.unwrap_err();
    assert!(matches!(err, KatFileError::InvalidKey));
    assert_eq!(err.class(), ErrorClass::Auth);
}

#[tokio::test]
async fn folders_allow_duplicates_and_names_are_demangled() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let a = client.create_folder(FolderId::ROOT, "photos").await.unwrap();
    let b = client.create_folder(FolderId::ROOT, "photos").await.unwrap();
    assert_ne!(a, b, "remote folder creation is not idempotent");
    let u = client.create_folder(a, "相片 ü").await.unwrap();

    let root = client.list_folder(FolderId::ROOT).await.unwrap();
    assert_eq!(root.folders.iter().filter(|f| f.name == "photos").count(), 2);
    let inner = client.list_folder(a).await.unwrap();
    let entry = inner.folders.iter().find(|f| f.id == u).unwrap();
    assert_eq!(entry.name, "相片 ü");
    assert_ne!(entry.raw_name, entry.name, "API renders UTF-8 as Latin-1 mojibake");

    let err = client.list_folder(FolderId(999_999_999)).await.unwrap_err();
    assert!(matches!(err, KatFileError::Api { status: 403, .. }));
}

#[tokio::test]
async fn fixed_length_upload_assign_and_verify() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let folder = client.create_folder(FolderId::ROOT, "alice").await.unwrap();
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();

    let receipt = upload_bytes(&client, &content, "holiday 相片.jpg", UploadMode::FixedLength).await.unwrap();
    let stored = mock.state().files.get(receipt.file_code.as_str()).cloned().unwrap();
    assert!(!stored.chunked, "fixed mode must send Content-Length");
    assert_eq!(stored.size, content.len() as u64);
    assert_eq!(stored.sha256, receipt.sha256_hex);
    assert_eq!(stored.folder, Some(0), "uploads land in the account root first");

    client.set_file_folder(&receipt.file_code, folder).await.unwrap();
    let found = client
        .find_file_in_folder(folder, &receipt.file_code, "holiday 相片.jpg", 5)
        .await
        .unwrap()
        .expect("file must be listed in its folder");
    assert_eq!(found.size, Some(content.len() as u64));
    assert_eq!(found.name, "holiday 相片.jpg");
}

#[tokio::test]
async fn chunked_upload_is_supported() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let receipt = upload_bytes(&client, b"chunked payload", "c.txt", UploadMode::Chunked).await.unwrap();
    assert!(mock.state().files.get(receipt.file_code.as_str()).unwrap().chunked);
}

#[tokio::test]
async fn empty_files_are_rejected_without_network_upload() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let err = upload_bytes(&client, b"", "empty.txt", UploadMode::FixedLength).await.unwrap_err();
    assert!(matches!(err, KatFileError::UploadRejected(_)));
    assert_eq!(err.class(), ErrorClass::Permanent);
    assert_eq!(mock.state().call_count("upload.cgi"), 0);
}

#[tokio::test]
async fn unsanitized_remote_names_are_refused() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let err = upload_bytes(&client, b"x", "bad\"name.txt", UploadMode::FixedLength).await.unwrap_err();
    assert!(matches!(err, KatFileError::InvalidArgument(_)));
}

#[tokio::test]
async fn idempotent_reads_retry_transient_errors() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    mock.inject("file/list", Fault::HttpStatus(503));
    mock.inject("file/list", Fault::HttpStatus(429));
    let page = client.list_files_page(None, None, 1, 10).await.unwrap();
    assert_eq!(page.results_total, 0);
    assert_eq!(mock.state().call_count("file/list"), 3);
}

#[tokio::test]
async fn retries_are_bounded() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    for _ in 0..3 {
        mock.inject("folder/list", Fault::HttpStatus(500));
    }
    let err = client.list_folder(FolderId::ROOT).await.unwrap_err();
    assert!(matches!(err, KatFileError::HttpStatus { status: 500 }));
    assert_eq!(err.class(), ErrorClass::Transient);
}

#[tokio::test]
async fn folder_create_is_never_retried() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    mock.inject("folder/create", Fault::CreateThenFail);
    let err = client.create_folder(FolderId::ROOT, "bob").await.unwrap_err();
    assert_eq!(err.class(), ErrorClass::Transient);
    assert_eq!(mock.state().call_count("folder/create"), 1, "create must not be retried blindly");
    // The folder exists although the reply was lost: callers must re-list before retrying.
    let root = client.list_folder(FolderId::ROOT).await.unwrap();
    assert_eq!(root.folders.iter().filter(|f| f.name == "bob").count(), 1);
}

#[tokio::test]
async fn malformed_and_application_errors_are_typed() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    mock.inject("account/info", Fault::MalformedJson);
    assert!(matches!(client.account_info().await.unwrap_err(), KatFileError::Malformed(_)));
    mock.inject("file/set_folder", Fault::AppError(403, "Your account don't have such folder".into()));
    let code = FileCode::parse("79cpmm7q3pmg").unwrap();
    let err = client.set_file_folder(&code, FolderId(5)).await.unwrap_err();
    assert!(matches!(err, KatFileError::Api { status: 403, .. }));
    assert_eq!(err.class(), ErrorClass::Permanent);
}

#[tokio::test]
async fn lost_upload_reply_is_ambiguous_and_reconcilable() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    mock.inject("upload.cgi", Fault::StoreThenFail);
    let err = upload_bytes(&client, b"important bytes", "lost-reply.bin", UploadMode::FixedLength).await.unwrap_err();
    assert!(matches!(err, KatFileError::UploadAmbiguous(_)), "got {err:?}");
    assert_eq!(err.class(), ErrorClass::Ambiguous);
    // The provider did keep the file; an account-wide search finds exactly one candidate.
    let hits = client.find_account_files_named("lost-reply.bin", 5).await.unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn reply_timeout_after_full_body_is_ambiguous() {
    let mock = MockKatFile::start(KEY).await;
    let mut cfg = client_for(&mock).config().clone();
    cfg.upload_response_timeout = Duration::from_millis(300);
    let client = KatFileClient::new(cfg, ApiKey::new(KEY).unwrap()).unwrap();
    mock.inject("upload.cgi", Fault::StoreThenHang(Duration::from_secs(10)));
    let err = upload_bytes(&client, b"slow reply", "slow.bin", UploadMode::FixedLength).await.unwrap_err();
    assert!(matches!(err, KatFileError::UploadAmbiguous(_)), "got {err:?}");
}

#[tokio::test]
async fn shrinking_local_file_is_detected() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let tmp = temp_file_with(b"only ten b");
    let file = tokio::fs::File::open(tmp.path()).await.unwrap();
    let server = client.request_upload_server().await.unwrap();
    // Declare more bytes than the file holds.
    let req = UploadRequest { file, size: 1000, remote_name: "short.bin".into(), mode: UploadMode::FixedLength };
    let err = client.upload(&server, req, UploadProgress::new()).await.unwrap_err();
    assert!(matches!(err, KatFileError::LocalFileChanged), "got {err:?}");
}

#[tokio::test]
async fn upload_host_policy_is_enforced() {
    let mock = MockKatFile::start(KEY).await;
    let mut cfg = client_for(&mock).config().clone();
    cfg.upload_host_pattern = r"^s[0-9]+\.katfile\.biz$".into();
    let client = KatFileClient::new(cfg, ApiKey::new(KEY).unwrap()).unwrap();
    let err = client.request_upload_server().await.unwrap_err();
    assert!(matches!(err, KatFileError::UntrustedEndpoint(_)), "got {err:?}");
}

#[tokio::test]
async fn foreign_or_anonymous_files_fail_ownership_check() {
    let mock = MockKatFile::start(KEY).await;
    let client = client_for(&mock);
    let folder = client.create_folder(FolderId::ROOT, "carol").await.unwrap();
    let server = client.request_upload_server().await.unwrap();
    // Simulate an expired session: the provider stores the file anonymously.
    mock.state().sessions.clear();
    let tmp = temp_file_with(b"private data");
    let file = tokio::fs::File::open(tmp.path()).await.unwrap();
    let req = UploadRequest { file, size: 12, remote_name: "private.txt".into(), mode: UploadMode::FixedLength };
    let receipt = client.upload(&server, req, UploadProgress::new()).await.unwrap();
    // set_folder "succeeds" for the foreign file, but the ownership check does not find it.
    client.set_file_folder(&receipt.file_code, folder).await.unwrap();
    let found = client.find_file_in_folder(folder, &receipt.file_code, "private.txt", 5).await.unwrap();
    assert!(found.is_none(), "anonymous uploads must never verify as archived");
    assert_eq!(mock.state().anonymous_files().len(), 1);
}
