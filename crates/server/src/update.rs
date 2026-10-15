//! 固定 GitHub 仓库的更新检查与安装任务。桌面宿主显式注册 launcher 后才允许安装。

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use reqwest::{Client, Url};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[path = "update_mirrors.rs"]
mod mirrors;

use mirrors::Mirror;
pub use mirrors::DownloadSource;

const REPOSITORY_URL: &str = "https://github.com/VerSonetto/Denia";
const RELEASES_API: &str = "https://api.github.com/repos/VerSonetto/Denia/releases";
const CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const MAX_INSTALLER_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 1024 * 1024;
/// 换源前给每个源的首包等待上限：公益源经常不回响应，不能让它吃掉整个下载预算。
const SOURCE_START_TIMEOUT: Duration = Duration::from_secs(15);

pub type InstallerWaiter = Box<dyn FnOnce() + Send>;
type InstallerLauncher = Arc<dyn Fn(&Path) -> Result<InstallerWaiter, String> + Send + Sync>;

static UPDATER: OnceLock<Result<Arc<Updater>, String>> = OnceLock::new();

pub(crate) fn manager() -> Result<Arc<Updater>, String> {
    UPDATER
        .get_or_init(|| {
            let mirrors = mirrors::default_mirrors();
            let trusted = mirrors.clone();
            let client = Client::builder()
                .user_agent(concat!("Denia/", env!("CARGO_PKG_VERSION")))
                .https_only(true)
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    if attempt.previous().len() >= 8 {
                        return attempt.error("更新下载重定向次数过多");
                    }
                    if github_transport_url(attempt.url())
                        || mirrors::trusted_redirect(attempt.url(), &trusted)
                    {
                        attempt.follow()
                    } else {
                        attempt.error("更新下载重定向到了不受信任的地址")
                    }
                }))
                .build()
                .map_err(|error| format!("无法初始化更新连接: {error}"))?;
            Ok(Arc::new(Updater::new(client, Source::Github, mirrors)))
        })
        .clone()
}

/// launcher 必须在启动本机 HTTP listener 前注册。返回的 waiter 在阻塞线程等待安装器退出，
/// 以便保持已核验的临时安装文件存活，不会终止当前 Denia 会话。
pub fn register_installer_launcher(
    launcher: impl Fn(&Path) -> Result<InstallerWaiter, String> + Send + Sync + 'static,
) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("当前平台不支持桌面更新安装".to_string());
    }
    manager()?
        .launcher
        .set(Arc::new(launcher))
        .map_err(|_| "更新安装器已注册".to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallerInfo {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub version: String,
    pub tag: String,
    pub name: String,
    pub body: String,
    pub html_url: String,
    pub published_at: Option<String>,
    pub prerelease: bool,
    pub installer: Option<InstallerInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResponse {
    pub current_version: &'static str,
    pub latest: Option<ReleaseInfo>,
    pub update_available: bool,
    pub can_install: bool,
    pub checked_at: u64,
    pub repository_url: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Selecting,
    Downloading,
    Verifying,
    Launching,
    Launched,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallStatus {
    pub phase: Phase,
    pub version: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
    /// 本次实际使用的下载源；选源完成前为 None。
    pub source: Option<DownloadSource>,
}

impl Default for InstallStatus {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            version: None,
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
            source: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    name: Option<String>,
    body: Option<String>,
    html_url: String,
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone)]
struct SelectedRelease {
    info: ReleaseInfo,
    version: Version,
    installer: Option<GithubAsset>,
    checksums: Option<GithubAsset>,
}

#[derive(Clone)]
enum Source {
    Github,
    #[cfg(test)]
    Mock(Url),
}

impl Source {
    fn releases_url(&self, page: usize) -> String {
        match self {
            Self::Github => format!("{RELEASES_API}?per_page=100&page={page}"),
            #[cfg(test)]
            Self::Mock(base) => format!("{base}releases?per_page=100&page={page}"),
        }
    }

    fn asset_url(&self, tag: &str, name: &str) -> Url {
        let mut url = match self {
            Self::Github => Url::parse(&format!("{REPOSITORY_URL}/releases/download/")).unwrap(),
            #[cfg(test)]
            Self::Mock(base) => base.join("download/").unwrap(),
        };
        url.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push(tag)
            .push(name);
        url
    }

    fn valid_asset(&self, tag: &str, asset: &GithubAsset) -> bool {
        Url::parse(&asset.browser_download_url)
            .is_ok_and(|url| url == self.asset_url(tag, &asset.name))
    }
}

struct Checked {
    at: Instant,
    timestamp: u64,
    result: Result<Option<SelectedRelease>, String>,
}

pub(crate) struct Updater {
    client: Client,
    source: Source,
    mirrors: Arc<Vec<Mirror>>,
    /// 安装包 URL 的官方前缀，镜像候选地址由它拼接而来。
    origin: String,
    checked: tokio::sync::Mutex<Option<Checked>>,
    status: Mutex<InstallStatus>,
    launcher: OnceLock<InstallerLauncher>,
}

impl Updater {
    fn new(client: Client, source: Source, mirrors: Arc<Vec<Mirror>>) -> Self {
        Self {
            client,
            source,
            mirrors,
            origin: mirrors::GITHUB_ORIGIN.to_string(),
            checked: tokio::sync::Mutex::new(None),
            status: Mutex::new(InstallStatus::default()),
            launcher: OnceLock::new(),
        }
    }

    /// 测试用：镜像指向本机 mock 服务时，官方前缀也跟着换成本机地址。
    #[cfg(test)]
    fn with_origin(mut self, origin: String) -> Self {
        self.origin = origin;
        self
    }

    pub(crate) fn can_install(&self) -> bool {
        cfg!(windows) && self.launcher.get().is_some()
    }

    pub(crate) fn status(&self) -> InstallStatus {
        self.status
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn edit_status(&self, edit: impl FnOnce(&mut InstallStatus)) {
        edit(&mut self.status.lock().unwrap_or_else(|p| p.into_inner()));
    }

    pub(crate) async fn check(&self, force: bool, local: bool) -> Result<CheckResponse, String> {
        let requested_at = Instant::now();
        // 锁覆盖整个检查；同时进入的 force 请求复用刚完成的同一次检查，包括失败。
        let mut checked = self.checked.lock().await;
        let reusable = checked.as_ref().is_some_and(|cached| {
            cached.at >= requested_at
                || (!force
                    && cached.at.elapsed()
                        < if cached.result.is_ok() {
                            CACHE_TTL
                        } else {
                            Duration::from_secs(15)
                        })
        });
        if !reusable {
            let result = tokio::time::timeout(CHECK_TIMEOUT, self.fetch_latest())
                .await
                .unwrap_or_else(|_| Err("检查更新超时，请稍后重试".to_string()));
            *checked = Some(Checked {
                at: Instant::now(),
                timestamp: now_millis(),
                result,
            });
        }
        let checked = checked.as_ref().unwrap();
        let release = checked.result.clone()?;
        let update_available = release
            .as_ref()
            .is_some_and(|release| newer_than_current(&release.version));
        Ok(CheckResponse {
            current_version: env!("CARGO_PKG_VERSION"),
            latest: release.map(|release| release.info),
            update_available,
            can_install: local && self.can_install(),
            checked_at: checked.timestamp,
            repository_url: REPOSITORY_URL,
        })
    }

    async fn fetch_latest(&self) -> Result<Option<SelectedRelease>, String> {
        let mut releases = Vec::new();
        for page in 1..=10 {
            let response = self
                .client
                .get(self.source.releases_url(page))
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .timeout(CHECK_TIMEOUT)
                .send()
                .await
                .map_err(|error| network_error("检查更新", error))?;
            let bytes = bounded_body(response, MAX_METADATA_BYTES, "读取发布列表").await?;
            let batch: Vec<GithubRelease> = serde_json::from_slice(&bytes)
                .map_err(|_| "GitHub 返回了无效的发布列表".to_string())?;
            let done = batch.len() < 100;
            releases.extend(batch);
            if done {
                return Ok(select_latest(releases, &self.source));
            }
        }
        Err("GitHub 发布列表过长，无法安全确定最新版本".to_string())
    }

    pub(crate) async fn install(self: &Arc<Self>, version: &str) -> Result<InstallStatus, String> {
        let launcher = self
            .launcher
            .get()
            .cloned()
            .ok_or("仅 Windows 桌面应用支持安装更新")?;
        if !cfg!(windows) {
            return Err("仅 Windows 桌面应用支持安装更新".to_string());
        }
        let checked = self.checked.lock().await;
        let cached = checked
            .as_ref()
            .filter(|cached| cached.at.elapsed() < CACHE_TTL)
            .ok_or("请先重新检查更新，再安装已检查到的版本")?;
        let release = cached
            .result
            .as_ref()
            .map_err(Clone::clone)?
            .as_ref()
            .filter(|release| {
                release.info.version == version && newer_than_current(&release.version)
            })
            .ok_or("只能安装已检查到、且高于当前版本的更新")?
            .clone();
        let installer = release
            .installer
            .clone()
            .ok_or("该版本没有可安全安装的 Windows 安装包或 SHA256 校验值")?;
        let initial = {
            let mut status = self.status.lock().unwrap_or_else(|p| p.into_inner());
            if matches!(
                status.phase,
                Phase::Selecting
                    | Phase::Downloading
                    | Phase::Verifying
                    | Phase::Launching
                    | Phase::Launched
            ) {
                return if status.version.as_deref() == Some(version) {
                    Ok(status.clone())
                } else {
                    Err("已有另一个更新安装任务正在运行".to_string())
                };
            }
            *status = InstallStatus {
                phase: Phase::Selecting,
                version: Some(version.to_string()),
                downloaded_bytes: 0,
                total_bytes: Some(installer.size),
                error: None,
                source: None,
            };
            status.clone()
        };
        drop(checked);
        let updater = self.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                DOWNLOAD_TIMEOUT,
                updater.download_and_verify(&release, &installer),
            )
            .await
            .unwrap_or_else(|_| Err("下载安装包超时，请重试".to_string()));
            let (directory, path) = match result {
                Ok(download) => download,
                Err(error) => {
                    updater.fail(error);
                    return;
                }
            };
            updater.edit_status(|status| status.phase = Phase::Launching);
            // UAC 提示可能停留很久，不把启动回调放进会删除安装文件的下载超时里。
            let launched = tokio::task::spawn_blocking(move || launcher(&path)).await;
            match launched {
                Ok(Ok(waiter)) => {
                    updater.edit_status(|status| status.phase = Phase::Launched);
                    tokio::task::spawn_blocking(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(waiter));
                        drop(directory);
                        if result.is_err() {
                            updater.fail("等待安装器退出时发生错误，请重试".to_string());
                        } else {
                            // 安装器退出不代表安装成功；用户可能取消了安装。
                            updater.edit_status(|status| *status = InstallStatus::default());
                        }
                    });
                }
                Ok(Err(error)) => updater.fail(error),
                Err(error) => updater.fail(format!("启动安装器失败: {error}")),
            }
        });
        Ok(initial)
    }

    fn fail(&self, error: String) {
        tracing::warn!(%error, "update installation failed");
        self.edit_status(|status| {
            status.phase = Phase::Failed;
            status.error = Some(error);
        });
    }

    async fn download_and_verify(
        &self,
        release: &SelectedRelease,
        installer: &GithubAsset,
    ) -> Result<(tempfile::TempDir, PathBuf), String> {
        // 校验值只认 GitHub 记录：镜像能同时提供安装包与校验文件，配出匹配的摘要就能绕过校验。
        let expected_digest = match installer.digest.as_deref().and_then(parse_digest) {
            Some(digest) => digest,
            None => self.fetch_checksum(release, installer).await?,
        };
        let mut candidates = mirrors::rank_candidates(
            &self.client,
            &installer.browser_download_url,
            &self.origin,
            &self.mirrors,
        )
        .await;
        if candidates.is_empty() {
            candidates.push(mirrors::official_candidate(&installer.browser_download_url));
        }
        let plan = mirrors::attempt_plan(&candidates);
        let attempts = plan.len();
        let directory = tempfile::Builder::new()
            .prefix("denia-update-")
            .tempdir()
            .map_err(|error| format!("无法创建私有更新目录: {error}"))?;
        let path = directory.path().join("Denia-setup.exe");
        let mut failures = Vec::new();
        for candidate in plan {
            // 换源必须从零开始：上一源写坏的文件不能与新源的字节拼在一起算摘要。
            self.edit_status(|status| {
                status.source = Some(candidate.source());
                status.downloaded_bytes = 0;
            });
            match self.fetch_asset(candidate, installer, &path).await {
                Ok(actual) => {
                    self.edit_status(|status| status.phase = Phase::Verifying);
                    if actual == expected_digest {
                        return Ok((directory, path));
                    }
                    failures.push(format!("{}：SHA256 校验失败", candidate.label));
                }
                Err(error) => failures.push(format!("{}：{error}", candidate.label)),
            }
            self.edit_status(|status| status.phase = Phase::Downloading);
            let _ = tokio::fs::remove_file(&path).await;
        }
        if failures.len() == attempts && failures.iter().all(|failure| failure.contains("SHA256")) {
            return Err("所有下载源返回的安装包都未通过 GitHub 记录的 SHA256 校验，已拒绝执行，请稍后重试".to_string());
        }
        Err(format!(
            "已尝试 {attempts} 个下载源均失败（{}）",
            failures.join("；")
        ))
    }

    /// 安装包只从 GitHub 取，不经过任何镜像。
    async fn fetch_checksum(
        &self,
        release: &SelectedRelease,
        installer: &GithubAsset,
    ) -> Result<[u8; 32], String> {
        let checksum = release
            .checksums
            .as_ref()
            .ok_or("该版本缺少 SHA256 校验值")?;
        let response = self
            .client
            .get(&checksum.browser_download_url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| network_error("下载 SHA256 校验文件", error))?;
        let bytes = bounded_body(response, MAX_CHECKSUM_BYTES, "下载 SHA256 校验文件").await?;
        let text = std::str::from_utf8(&bytes).map_err(|_| "SHA256 校验文件不是有效文本")?;
        checksum_for(text, &installer.name)
    }

    async fn fetch_asset(
        &self,
        candidate: &mirrors::Candidate,
        installer: &GithubAsset,
        path: &Path,
    ) -> Result<[u8; 32], String> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
            .map_err(|error| format!("无法创建临时安装文件: {error}"))?;
        let response = tokio::time::timeout(
            SOURCE_START_TIMEOUT,
            self.client
                .get(&candidate.url)
                .header(reqwest::header::ACCEPT_ENCODING, "identity")
                .send(),
        )
        .await
        .map_err(|_| "下载源迟迟没有响应".to_string())?
        .map_err(|error| network_error("下载安装包", error))?;
        let response = successful(response, "下载安装包")?;
        if let Some(size) = response.content_length() {
            if size > MAX_INSTALLER_BYTES || size != installer.size {
                return Err("安装包响应大小与发布记录不一致".to_string());
            }
        }
        let mut stream = response.bytes_stream();
        let mut digest = Sha256::new();
        let mut downloaded = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| network_error("下载安装包", error))?;
            downloaded = downloaded
                .checked_add(chunk.len() as u64)
                .ok_or("安装包大小溢出")?;
            if downloaded > MAX_INSTALLER_BYTES || downloaded > installer.size {
                return Err("安装包超过允许的大小".to_string());
            }
            file.write_all(&chunk)
                .await
                .map_err(|error| format!("写入安装包失败: {error}"))?;
            digest.update(&chunk);
            self.edit_status(|status| status.downloaded_bytes = downloaded);
        }
        if downloaded != installer.size {
            return Err("安装包下载不完整，大小与发布记录不一致".to_string());
        }
        file.flush()
            .await
            .map_err(|error| format!("保存安装包失败: {error}"))?;
        file.sync_all()
            .await
            .map_err(|error| format!("保存安装包失败: {error}"))?;
        Ok(digest.finalize().into())
    }
}

fn select_latest(releases: Vec<GithubRelease>, source: &Source) -> Option<SelectedRelease> {
    releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            normalize_version(&release.tag_name).map(|version| (version, release))
        })
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(version, release)| {
            let names = [
                format!("Denia-{}-windows-x64-setup.exe", release.tag_name),
                format!("Denia-v{version}-windows-x64-setup.exe"),
            ];
            let checksums = release
                .assets
                .iter()
                .find(|asset| {
                    asset.name == "SHA256SUMS.txt"
                        && asset.size <= MAX_CHECKSUM_BYTES as u64
                        && source.valid_asset(&release.tag_name, asset)
                })
                .cloned();
            let installer = release
                .assets
                .iter()
                .find(|asset| {
                    names.contains(&asset.name)
                        && asset.size > 0
                        && asset.size <= MAX_INSTALLER_BYTES
                        && source.valid_asset(&release.tag_name, asset)
                        && (asset.digest.as_deref().and_then(parse_digest).is_some()
                            || checksums.is_some())
                })
                .cloned();
            SelectedRelease {
                info: ReleaseInfo {
                    version: version.to_string(),
                    tag: release.tag_name.clone(),
                    name: release.name.unwrap_or_else(|| release.tag_name.clone()),
                    body: release.body.unwrap_or_default(),
                    html_url: release.html_url,
                    published_at: release.published_at,
                    prerelease: release.prerelease || !version.pre.is_empty(),
                    installer: installer.as_ref().map(|asset| InstallerInfo {
                        name: asset.name.clone(),
                        size: asset.size,
                    }),
                },
                version,
                installer,
                checksums,
            }
        })
}

fn normalize_version(tag: &str) -> Option<Version> {
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    let base = tag.split(['-', '+']).next()?;
    let normalized = if base.split('.').count() == 2 {
        format!("{base}.0{}", &tag[base.len()..])
    } else {
        tag.to_string()
    };
    Version::parse(&normalized).ok()
}

fn newer_than_current(version: &Version) -> bool {
    normalize_version(env!("CARGO_PKG_VERSION"))
        .is_some_and(|current| version.cmp_precedence(&current) == Ordering::Greater)
}

fn parse_digest(value: &str) -> Option<[u8; 32]> {
    parse_hex(value.strip_prefix("sha256:")?)
}

fn parse_hex(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

fn checksum_for(text: &str, name: &str) -> Result<[u8; 32], String> {
    let mut found = None;
    for line in text.lines() {
        let Some((digest, file)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let file = file.trim().strip_prefix('*').unwrap_or(file.trim());
        if file == name {
            let digest = parse_hex(digest).ok_or("SHA256 校验文件中的摘要无效")?;
            if found.replace(digest).is_some() {
                return Err("SHA256 校验文件包含重复的安装包记录".to_string());
            }
        }
    }
    found.ok_or_else(|| "SHA256 校验文件不包含该安装包".to_string())
}

fn github_transport_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "api.github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
                    | "github-releases.githubusercontent.com"
            )
        )
}

fn successful(response: reqwest::Response, operation: &str) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        Ok(response)
    } else if response.status().as_u16() == 403 || response.status().as_u16() == 429 {
        Err(format!(
            "{operation}失败: 下载源限流或拒绝访问（HTTP {}），请稍后重试",
            response.status().as_u16()
        ))
    } else {
        Err(format!(
            "{operation}失败: HTTP {}",
            response.status().as_u16()
        ))
    }
}

async fn bounded_body(
    response: reqwest::Response,
    limit: usize,
    operation: &str,
) -> Result<Vec<u8>, String> {
    let response = successful(response, operation)?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("{operation}失败: 响应超过大小限制"));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| network_error(operation, error))?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(format!("{operation}失败: 响应超过大小限制"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn network_error(operation: &str, error: reqwest::Error) -> String {
    // reqwest 的 URL 可能含 GitHub 临时签名，不把它写入错误响应或日志。
    if error.is_timeout() {
        format!("{operation}超时，请检查网络后重试")
    } else if error.is_connect() {
        format!("{operation}失败，无法连接更新源，请检查网络")
    } else {
        format!("{operation}失败，网络连接中断或响应不被安全策略允许")
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;

#[cfg(windows)]
pub fn launch_windows_installer(path: &Path) -> Result<InstallerWaiter, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, GetLastError};
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.nShow = SW_SHOWNORMAL;
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = unsafe { GetLastError() };
        return Err(if error == ERROR_CANCELLED {
            "用户取消了安装授权，可重新点击安装更新".to_string()
        } else {
            format!("无法启动更新安装器（Windows 错误 {error}）")
        });
    }
    if info.hProcess.is_null() {
        return Err("Windows 未返回安装进程，无法安全跟踪安装器".to_string());
    }
    // HANDLE 是进程句柄；跨阻塞线程转移所有权后只等待并关闭一次。
    let process = info.hProcess as usize;
    Ok(Box::new(move || unsafe {
        WaitForSingleObject(process as _, INFINITE);
        CloseHandle(process as _);
    }))
}
