//! Bounded-memory evidence for large uploads (V0 acceptance).
//!
//! Ignored by default because it streams gigabytes; run with:
//! `KFGW_LARGE_TEST_BYTES=2147483648 cargo test -p katfile-api --release --test large_upload -- --ignored --nocapture`

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use katfile_api::{ApiKey, ClientConfig, KatFileClient, UploadMode, UploadProgress, UploadRequest};
use katfile_mock::MockKatFile;

/// Resident set size of this process in KiB, via `ps` (works on macOS and Linux).
fn rss_kib() -> u64 {
    let out = Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "streams gigabytes; run explicitly"]
async fn large_upload_streams_with_bounded_memory() {
    let size: u64 = std::env::var("KFGW_LARGE_TEST_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(2 << 30);
    let mode =
        if std::env::var("KFGW_LARGE_TEST_CHUNKED").is_ok() { UploadMode::Chunked } else { UploadMode::FixedLength };

    // Sparse file: occupies almost no disk but reads back `size` bytes.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.as_file().set_len(size).unwrap();

    let mock = MockKatFile::start("k").await;
    let mut cfg = ClientConfig::new(mock.base_url());
    cfg.insecure_test_mode = true;
    cfg.upload_host_pattern = r"^127\.0\.0\.1$".into();
    let client = KatFileClient::new(cfg, ApiKey::new("k").unwrap()).unwrap();

    let baseline = rss_kib();
    let peak = Arc::new(AtomicU64::new(baseline));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, stop) = (peak.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                peak.fetch_max(rss_kib(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };

    let started = Instant::now();
    let server = client.request_upload_server().await.unwrap();
    let file = tokio::fs::File::open(tmp.path()).await.unwrap();
    let progress = UploadProgress::new();
    let receipt = client
        .upload(&server, UploadRequest { file, size, remote_name: "large.bin".into(), mode }, progress.clone())
        .await
        .unwrap();
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();

    let elapsed = started.elapsed();
    let growth_mib = peak.load(Ordering::Relaxed).saturating_sub(baseline) / 1024;
    let stored = mock.state().files.get(receipt.file_code.as_str()).cloned().unwrap();
    println!(
        "uploaded {size} bytes ({mode:?}) in {:.1}s; baseline RSS {} MiB; peak growth {growth_mib} MiB",
        elapsed.as_secs_f64(),
        baseline / 1024
    );
    assert_eq!(stored.size, size);
    assert_eq!(stored.sha256, receipt.sha256_hex);
    assert_eq!(progress.file_bytes_sent(), size);
    // Client and mock both stream; memory must not scale with the file size.
    assert!(growth_mib < 64, "RSS grew by {growth_mib} MiB while streaming {size} bytes");
}
