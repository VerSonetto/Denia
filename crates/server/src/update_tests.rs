use super::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

use axum::body::{Body, Bytes};
use axum::extract::Path as AxumPath;
use axum::http::{Response, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

const TEST_TAG: &str = "v99.0.0-beta.2";
const TEST_VERSION: &str = "99.0.0-beta.2";
const TEST_NAME: &str = "Denia-v99.0.0-beta.2-windows-x64-setup.exe";
const PAYLOAD: &[u8] = b"verified installer bytes";

fn release(tag: &str) -> GithubRelease {
    serde_json::from_value(json!({
        "tag_name": tag,
        "html_url": format!("{REPOSITORY_URL}/releases/tag/{tag}"),
        "prerelease": tag.contains('-'),
        "assets": []
    }))
    .unwrap()
}

fn hash() -> String {
    format!("{:x}", Sha256::digest(PAYLOAD))
}

struct MockServer {
    updater: Arc<Updater>,
    releases: Arc<Mutex<Value>>,
    checks: Arc<AtomicUsize>,
    downloads: Arc<AtomicUsize>,
    mode: Arc<AtomicUsize>,
    check_error: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base: Url = format!("http://{}/", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let source = Source::Mock(base);
        let releases = Arc::new(Mutex::new(json!([{
            "tag_name": TEST_TAG,
            "html_url": format!("{REPOSITORY_URL}/releases/tag/{TEST_TAG}"),
            "prerelease": true,
            "assets": [{
                "name": TEST_NAME,
                "size": PAYLOAD.len(),
                "browser_download_url": source.asset_url(TEST_TAG, TEST_NAME).to_string(),
                "digest": format!("sha256:{}", hash())
            }, {
                "name": "SHA256SUMS.txt",
                "size": 128,
                "browser_download_url": source.asset_url(TEST_TAG, "SHA256SUMS.txt").to_string()
            }]
        }])));
        let checks = Arc::new(AtomicUsize::new(0));
        let downloads = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(AtomicUsize::new(0));
        let check_error = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/releases",
                get({
                    let releases = releases.clone();
                    let checks = checks.clone();
                    let check_error = check_error.clone();
                    move || {
                        let releases = releases.clone();
                        let checks = checks.clone();
                        let check_error = check_error.clone();
                        async move {
                            checks.fetch_add(1, AtomicOrdering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            if check_error.load(AtomicOrdering::SeqCst) != 0 {
                                (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    Json(json!({"message":"unavailable"})),
                                )
                            } else {
                                (StatusCode::OK, Json(releases.lock().unwrap().clone()))
                            }
                        }
                    }
                }),
            )
            .route(
                "/download/{tag}/{name}",
                get({
                    let downloads = downloads.clone();
                    let mode = mode.clone();
                    move |AxumPath((_tag, name)): AxumPath<(String, String)>| {
                        let downloads = downloads.clone();
                        let mode = mode.clone();
                        async move {
                            if name == "SHA256SUMS.txt" {
                                return Response::new(Body::from(format!(
                                    "{}  {TEST_NAME}\r\n",
                                    hash()
                                )));
                            }
                            downloads.fetch_add(1, AtomicOrdering::SeqCst);
                            match mode.load(AtomicOrdering::SeqCst) {
                                1 => {
                                    let mut bad = PAYLOAD.to_vec();
                                    bad[0] ^= 1;
                                    Response::new(Body::from(bad))
                                }
                                2 => Response::new(Body::from(PAYLOAD[..2].to_vec())),
                                3 => Response::builder()
                                    .status(StatusCode::SERVICE_UNAVAILABLE)
                                    .body(Body::from("unavailable"))
                                    .unwrap(),
                                4 => {
                                    let chunks = futures::stream::iter([
                                        Ok::<_, std::io::Error>(Bytes::copy_from_slice(PAYLOAD)),
                                        Ok(Bytes::from_static(b"extra")),
                                    ]);
                                    Response::new(Body::from_stream(chunks))
                                }
                                5 => {
                                    let chunks = futures::stream::iter([
                                        Ok(Bytes::copy_from_slice(&PAYLOAD[..2])),
                                        Err(std::io::Error::other("connection interrupted")),
                                    ]);
                                    Response::builder()
                                        .header(header::CONTENT_LENGTH, PAYLOAD.len())
                                        .body(Body::from_stream(chunks))
                                        .unwrap()
                                }
                                _ => Response::new(Body::from(PAYLOAD)),
                            }
                        }
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .read_timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        Self {
            updater: Arc::new(Updater::new(client, source)),
            releases,
            checks,
            downloads,
            mode,
            check_error,
            task,
        }
    }

    async fn selected(&self) -> SelectedRelease {
        self.updater.check(false, true).await.unwrap();
        self.updater
            .checked
            .lock()
            .await
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone()
    }
}

#[test]
fn versions_use_semver_precedence_and_accept_legacy_minor_tags() {
    assert_eq!(normalize_version("v0.3").unwrap().to_string(), "0.3.0");
    assert_eq!(
        normalize_version("v0.3-beta.2").unwrap().to_string(),
        "0.3.0-beta.2"
    );
    assert!(normalize_version("garbage").is_none());
    assert!(normalize_version("v01.3").is_none());
    let mut draft = release("v100.0.0");
    draft.draft = true;
    let chosen = select_latest(
        vec![
            release("v0.3"),
            release("invalid"),
            release("v1.0.0-beta.2"),
            release("v1.0.0-beta.10"),
            draft,
        ],
        &Source::Github,
    )
    .unwrap();
    assert_eq!(chosen.info.version, "1.0.0-beta.10");
    let stable = select_latest(
        vec![release("v1.0.0-beta.99"), release("v1.0.0")],
        &Source::Github,
    )
    .unwrap();
    assert_eq!(stable.info.version, "1.0.0");
    assert_eq!(
        normalize_version("1.0.0+one")
            .unwrap()
            .cmp_precedence(&normalize_version("1.0.0+two").unwrap()),
        Ordering::Equal
    );
    let current = normalize_version(env!("CARGO_PKG_VERSION")).unwrap();
    assert!(!newer_than_current(&current));
    assert!(newer_than_current(&Version::new(current.major + 1, 0, 0)));
    assert!(!newer_than_current(&normalize_version("v0.3").unwrap()));
}

#[test]
fn installer_must_belong_to_the_fixed_repository_and_have_a_checksum() {
    let mut latest = release(TEST_TAG);
    let asset = GithubAsset {
        name: TEST_NAME.to_string(),
        size: 100,
        browser_download_url: Source::Github.asset_url(TEST_TAG, TEST_NAME).to_string(),
        digest: Some(format!("sha256:{}", hash())),
    };
    latest.assets.push(asset.clone());
    assert!(
        select_latest(vec![latest.clone()], &Source::Github)
            .unwrap()
            .installer
            .is_some()
    );
    latest.assets[0].browser_download_url = "https://evil.example/setup.exe".to_string();
    assert!(
        select_latest(vec![latest.clone()], &Source::Github)
            .unwrap()
            .installer
            .is_none()
    );
    latest.assets[0] = asset.clone();
    latest.assets[0].size = MAX_INSTALLER_BYTES + 1;
    assert!(
        select_latest(vec![latest.clone()], &Source::Github)
            .unwrap()
            .installer
            .is_none()
    );
    latest.assets[0] = asset;
    latest.assets[0].digest = None;
    assert!(
        select_latest(vec![latest], &Source::Github)
            .unwrap()
            .installer
            .is_none()
    );
}

#[test]
fn checksum_lookup_is_exact_and_rejects_ambiguous_or_invalid_records() {
    let expected = parse_hex(&hash()).unwrap();
    assert_eq!(
        checksum_for(&format!("{}  {TEST_NAME}\r\n", hash()), TEST_NAME).unwrap(),
        expected
    );
    assert_eq!(
        checksum_for(&format!("{} *{TEST_NAME}\n", hash()), TEST_NAME).unwrap(),
        expected
    );
    assert!(checksum_for(&format!("{}  other.exe\n", hash()), TEST_NAME).is_err());
    assert!(checksum_for(&format!("bad  {TEST_NAME}\n"), TEST_NAME).is_err());
    assert!(
        checksum_for(
            &format!("{0}  {TEST_NAME}\n{0}  {TEST_NAME}\n", hash()),
            TEST_NAME
        )
        .is_err()
    );
    assert!(parse_digest(&format!("md5:{}", hash())).is_none());
}

#[test]
fn redirects_only_allow_https_github_asset_hosts() {
    for url in [
        "https://github.com/x",
        "https://release-assets.githubusercontent.com/x",
        "https://api.github.com/x",
    ] {
        assert!(github_transport_url(&url.parse().unwrap()));
    }
    for url in [
        "http://github.com/x",
        "https://github.com:444/x",
        "https://github.com.evil.example/x",
        "https://user:secret@github.com/x",
        "https://evil.example/x",
    ] {
        assert!(!github_transport_url(&url.parse().unwrap()));
    }
}

#[tokio::test]
async fn checks_cache_for_thirty_minutes_and_deduplicate_concurrent_force_calls() {
    let server = MockServer::start().await;
    let requests = (0..12).map(|_| server.updater.check(true, true));
    let results = futures::future::join_all(requests).await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 1);
    assert!(!results[0].as_ref().unwrap().can_install);
    assert_eq!(
        results[0]
            .as_ref()
            .unwrap()
            .latest
            .as_ref()
            .unwrap()
            .version,
        TEST_VERSION
    );
    let original = results[0].as_ref().unwrap().checked_at;
    assert_eq!(
        server.updater.check(false, false).await.unwrap().checked_at,
        original
    );
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 1);
    server.updater.check(true, false).await.unwrap();
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 2);
    {
        let mut cached = server.updater.checked.lock().await;
        cached.as_mut().unwrap().at = Instant::now() - CACHE_TTL - Duration::from_secs(1);
    }
    server.updater.check(false, false).await.unwrap();
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 3);
}

#[tokio::test]
async fn check_failures_are_readable_deduplicated_and_force_retryable() {
    let server = MockServer::start().await;
    server.check_error.store(1, AtomicOrdering::SeqCst);
    let results =
        futures::future::join_all((0..8).map(|_| server.updater.check(true, false))).await;
    assert!(
        results
            .iter()
            .all(|result| result.as_ref().unwrap_err().contains("HTTP 503"))
    );
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 1);
    assert!(server.updater.check(false, false).await.is_err());
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 1);
    server.check_error.store(0, AtomicOrdering::SeqCst);
    assert!(server.updater.check(true, false).await.is_ok());
    assert_eq!(server.checks.load(AtomicOrdering::SeqCst), 2);
}

#[tokio::test]
async fn request_timeout_is_explained_without_leaking_urls() {
    let server = MockServer::start().await;
    let client = Client::builder()
        .no_proxy()
        .read_timeout(Duration::from_millis(5))
        .build()
        .unwrap();
    let updater = Updater::new(client, server.updater.source.clone());
    let error = updater.check(true, false).await.unwrap_err();
    assert!(error.contains("超时"), "{error}");
    assert!(!error.contains("http"));
}

#[tokio::test]
async fn streamed_download_verifies_digest_and_cleans_its_unique_directory() {
    let server = MockServer::start().await;
    let selected = server.selected().await;
    let installer = selected.installer.as_ref().unwrap();
    let (directory, path) = server
        .updater
        .download_and_verify(&selected, installer)
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(&path).await.unwrap(), PAYLOAD);
    assert_eq!(
        server.updater.status().downloaded_bytes,
        PAYLOAD.len() as u64
    );
    let parent = directory.path().to_owned();
    drop(directory);
    assert!(tokio::fs::metadata(parent).await.is_err());
}

#[tokio::test]
async fn checksum_asset_is_used_when_github_has_no_digest() {
    let server = MockServer::start().await;
    server.releases.lock().unwrap()[0]["assets"][0]["digest"] = Value::Null;
    let selected = server.selected().await;
    let (directory, path) = server
        .updater
        .download_and_verify(&selected, selected.installer.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(path).await.unwrap(), PAYLOAD);
    drop(directory);
}

#[tokio::test]
async fn download_rejects_bad_hash_truncation_http_failure_oversize_and_broken_stream() {
    let server = MockServer::start().await;
    let selected = server.selected().await;
    for (mode, expected) in [
        (1, "SHA256"),
        (2, "大小"),
        (3, "HTTP 503"),
        (4, "大小"),
        (5, "失败"),
    ] {
        server.mode.store(mode, AtomicOrdering::SeqCst);
        let error = server
            .updater
            .download_and_verify(&selected, selected.installer.as_ref().unwrap())
            .await
            .unwrap_err();
        assert!(error.contains(expected), "mode {mode}: {error}");
    }
}

#[cfg(windows)]
struct WaiterGate(Arc<(Mutex<bool>, std::sync::Condvar)>);

#[cfg(windows)]
impl WaiterGate {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), std::sync::Condvar::new())))
    }

    fn release(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
}

#[cfg(windows)]
impl Drop for WaiterGate {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(windows)]
fn held_waiter(gate: Arc<(Mutex<bool>, std::sync::Condvar)>) -> InstallerWaiter {
    Box::new(move || {
        let (lock, condition) = &*gate;
        drop(
            condition
                .wait_while(lock.lock().unwrap(), |released| !*released)
                .unwrap(),
        );
    })
}

#[cfg(windows)]
async fn terminal_status(updater: &Updater) -> InstallStatus {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = updater.status();
            if matches!(status.phase, Phase::Failed | Phase::Launched) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mock installer task completes")
}

#[cfg(windows)]
#[tokio::test]
async fn install_requires_checked_new_version_is_deduplicated_and_retries_after_uac_cancel() {
    let server = MockServer::start().await;
    let launches = Arc::new(AtomicUsize::new(0));
    let paths = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let gate = WaiterGate::new();
    server
        .updater
        .launcher
        .set(Arc::new({
            let launches = launches.clone();
            let paths = paths.clone();
            let gate = gate.0.clone();
            move |path: &Path| {
                paths.lock().unwrap().push(path.to_owned());
                assert_eq!(std::fs::read(path).unwrap(), PAYLOAD);
                if launches.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                    Err("用户取消了安装授权".to_string())
                } else {
                    Ok(held_waiter(gate.clone()))
                }
            }
        }))
        .ok()
        .unwrap();
    assert!(server.updater.install(TEST_VERSION).await.is_err());
    server.updater.check(false, true).await.unwrap();
    assert!(
        !server
            .updater
            .check(false, false)
            .await
            .unwrap()
            .can_install
    );
    assert!(server.updater.install("0.1.0").await.is_err());
    assert!(
        server
            .updater
            .install("https://evil.example/installer.exe")
            .await
            .is_err()
    );
    let installs =
        futures::future::join_all((0..10).map(|_| server.updater.install(TEST_VERSION))).await;
    assert!(installs.iter().all(Result::is_ok));
    let failed = terminal_status(&server.updater).await;
    assert_eq!(failed.phase, Phase::Failed);
    assert!(failed.error.unwrap().contains("取消"));
    assert_eq!(server.downloads.load(AtomicOrdering::SeqCst), 1);
    let first_path = paths.lock().unwrap()[0].clone();
    assert!(tokio::fs::metadata(first_path).await.is_err());
    let retry = server.updater.install(TEST_VERSION).await.unwrap();
    assert_eq!(retry.phase, Phase::Downloading);
    assert!(retry.error.is_none());
    assert_eq!(
        terminal_status(&server.updater).await.phase,
        Phase::Launched
    );
    assert_eq!(server.downloads.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(launches.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(
        server.updater.install(TEST_VERSION).await.unwrap().phase,
        Phase::Launched
    );
    assert_eq!(server.downloads.load(AtomicOrdering::SeqCst), 2);
}

#[cfg(windows)]
#[tokio::test]
async fn failed_verification_never_calls_launcher_and_can_be_retried() {
    let server = MockServer::start().await;
    let launches = Arc::new(AtomicUsize::new(0));
    let gate = WaiterGate::new();
    server
        .updater
        .launcher
        .set(Arc::new({
            let launches = launches.clone();
            let gate = gate.0.clone();
            move |_path: &Path| {
                launches.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(held_waiter(gate.clone()))
            }
        }))
        .ok()
        .unwrap();
    server.selected().await;
    server.mode.store(1, AtomicOrdering::SeqCst);
    server.updater.install(TEST_VERSION).await.unwrap();
    assert_eq!(terminal_status(&server.updater).await.phase, Phase::Failed);
    assert_eq!(launches.load(AtomicOrdering::SeqCst), 0);
    server.mode.store(0, AtomicOrdering::SeqCst);
    server.updater.install(TEST_VERSION).await.unwrap();
    assert_eq!(
        terminal_status(&server.updater).await.phase,
        Phase::Launched
    );
    assert_eq!(launches.load(AtomicOrdering::SeqCst), 1);
}

#[cfg(windows)]
#[tokio::test]
async fn closing_installer_returns_to_idle_cleans_download_and_allows_reopening() {
    let server = MockServer::start().await;
    let gate = WaiterGate::new();
    let paths = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    server
        .updater
        .launcher
        .set(Arc::new({
            let paths = paths.clone();
            let gate = gate.0.clone();
            move |path: &Path| {
                paths.lock().unwrap().push(path.to_owned());
                Ok(held_waiter(gate.clone()))
            }
        }))
        .ok()
        .unwrap();
    server.selected().await;
    server.updater.install(TEST_VERSION).await.unwrap();
    assert_eq!(
        terminal_status(&server.updater).await.phase,
        Phase::Launched
    );
    let path = paths.lock().unwrap()[0].clone();
    assert!(tokio::fs::metadata(&path).await.is_ok());
    assert_eq!(
        server.updater.install(TEST_VERSION).await.unwrap().phase,
        Phase::Launched
    );
    assert_eq!(server.downloads.load(AtomicOrdering::SeqCst), 1);
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.updater.status().phase != Phase::Idle {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(tokio::fs::metadata(path).await.is_err());
    assert_eq!(
        server.updater.install(TEST_VERSION).await.unwrap().phase,
        Phase::Downloading
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while server.updater.status().phase != Phase::Idle {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.downloads.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(paths.lock().unwrap().len(), 2);
}
