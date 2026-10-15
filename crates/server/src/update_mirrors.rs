//! 公益 GitHub 加速源：下载前并发探测各源延迟，按实测速度排序，逐个回退。
//!
//! 镜像只参与安装包下载。版本检查与 SHA256 校验值始终直连 GitHub，
//! 镜像返回的内容必须通过 GitHub 记录的摘要校验才允许执行。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, stream};
use reqwest::{Client, Url, header};
use serde::Serialize;

use super::github_transport_url;

/// 安装包在官方 GitHub 上的前缀，镜像候选地址由它与镜像前缀拼接得到。
pub(super) const GITHUB_ORIGIN: &str = "https://github.com";
/// 探测只取首字节；超过这个时间继续等也没有参考价值。
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);
/// 一次安装最多尝试几个源：探测已淘汰绝大多数慢源，剩下的失败也应当快速收敛。
pub(super) const MAX_DOWNLOAD_SOURCES: usize = 4;
const PROBE_RANGE: &str = "bytes=0-0";

/// 加速源清单：界面标签、URL 前缀、该源已知会跳转到的其他主机。
///
/// 取自 XIU2 的 Github Enhancement 油猴脚本里给 Release/Code(ZIP) 用的那组前缀；
/// 2026-10-15 实测过一轮：DNS 失效、404 与路径拼接错误的源已剔除。
const MIRROR_TABLE: &[(&str, &str, &[&str])] = &[
    ("gh.h233.eu.org", "https://gh.h233.eu.org/https://github.com", &[]),
    ("gh.ddlc.top", "https://gh.ddlc.top/https://github.com", &[]),
    ("gh-proxy", "https://gh-proxy.org/https://github.com", &[]),
    ("gh-proxy Fastly", "https://cdn.gh-proxy.org/https://github.com", &[]),
    ("gh-proxy EdgeOne", "https://edgeone.gh-proxy.org/https://github.com", &[]),
    ("isteed CORS", "https://cors.isteed.cc/github.com", &[]),
    ("ghproxy.it", "https://ghproxy.it/https://github.com", &["ghfast.top"]),
    ("boki.moe", "https://github.boki.moe/https://github.com", &[]),
    ("gh.monlor.com", "https://gh.monlor.com/https://github.com", &[]),
    ("ednovas", "https://github.ednovas.xyz/https://github.com", &[]),
    ("geekertao", "https://ghfile.geekertao.top/https://github.com", &["gh.dpik.top"]),
    ("keleyaa", "https://ghp.keleyaa.com/https://github.com", &[]),
    ("gh.chjina.com", "https://gh.chjina.com/https://github.com", &[]),
    ("hwinziej", "https://ghpxy.hwinzniej.top/https://github.com", &[]),
    ("crashmc", "https://cdn.crashmc.com/https://github.com", &["cdn.akaere.online"]),
    ("yylx", "https://git.yylx.win/https://github.com", &[]),
    ("mrhjx", "https://gitproxy.mrhjx.cn/https://github.com", &[]),
    ("cxkpro", "https://ghproxy.cxkpro.top/https://github.com", &[]),
    ("gh.idayer.com", "https://gh.idayer.com/https://github.com", &[]),
    ("ghproxy.net", "https://ghproxy.net/https://github.com", &[]),
    ("ghfast.top", "https://ghfast.top/https://github.com", &[]),
    ("wget.la", "https://wget.la/https://github.com", &[]),
];

/// 一个公益加速源。地址由内置表决定，调用方无法传入。
pub(super) struct Mirror {
    pub(super) label: String,
    pub(super) prefix: String,
    /// 该源已知会跳转到的其他主机，一并纳入受信重定向名单。
    pub(super) redirect_hosts: Vec<String>,
}

pub(super) fn default_mirrors() -> Arc<Vec<Mirror>> {
    Arc::new(
        MIRROR_TABLE
            .iter()
            .map(|(label, prefix, redirect_hosts)| Mirror {
                label: (*label).to_string(),
                prefix: (*prefix).to_string(),
                redirect_hosts: redirect_hosts
                    .iter()
                    .map(|host| (*host).to_string())
                    .collect(),
            })
            .collect(),
    )
}

/// 一个可尝试的下载来源。
#[derive(Clone)]
pub(super) struct Candidate {
    pub(super) label: String,
    pub(super) host: String,
    pub(super) url: String,
    pub(super) direct: bool,
    pub(super) latency: Option<Duration>,
}

/// 界面展示的当前下载源。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSource {
    pub label: String,
    pub host: String,
    /// true 表示 GitHub 官方直连，而非公益加速源。
    pub direct: bool,
    /// 探测得到的首包延迟；探测未完成时为 None。
    pub latency_ms: Option<u64>,
}

impl Candidate {
    pub(super) fn source(&self) -> DownloadSource {
        DownloadSource {
            label: self.label.clone(),
            host: self.host.clone(),
            direct: self.direct,
            latency_ms: self
                .latency
                .map(|latency| u64::try_from(latency.as_millis()).unwrap_or(u64::MAX)),
        }
    }
}

pub(super) fn official_candidate(canonical: &str) -> Candidate {
    Candidate {
        label: "GitHub".to_string(),
        host: host_of(canonical),
        url: canonical.to_string(),
        direct: true,
        latency: None,
    }
}

/// 重定向只允许落在 GitHub 官方域名或已知镜像域名上。
pub(super) fn trusted_redirect(url: &Url, mirrors: &[Mirror]) -> bool {
    if github_transport_url(url) {
        return true;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    // 与官方域名同一套传输约束：明文、非 443、夹带凭据的跳转都不认。
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    mirrors.iter().any(|mirror| {
        mirror
            .prefix_host()
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(host))
            || mirror
                .redirect_hosts
                .iter()
                .any(|redirect| redirect.eq_ignore_ascii_case(host))
    })
}

/// 并发探测各源：连得上的按延迟升序，连不上的排在后面，
/// GitHub 官方地址始终留作最后兜底。
pub(super) async fn rank_candidates(
    client: &Client,
    canonical: &str,
    origin: &str,
    mirrors: &[Mirror],
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = mirrors
        .iter()
        .filter_map(|mirror| {
            candidate_url(&mirror.prefix, canonical, origin).map(|url| Candidate {
                label: mirror.label.clone(),
                host: host_of(&url),
                url,
                direct: false,
                latency: None,
            })
        })
        .collect();
    candidates.push(official_candidate(canonical));
    // buffer_unordered 按完成顺序返回，延迟必须跟着下标回到各自的候选上。
    let probes = stream::iter(candidates.iter().cloned().enumerate().map(|(index, candidate)| {
        let url = candidate.url.clone();
        async move { (index, probe(client, url).await) }
    }))
    .buffer_unordered(candidates.len().max(1))
    .collect::<Vec<_>>()
    .await;
    let mut latencies: Vec<Option<Duration>> = vec![None; candidates.len()];
    for (index, latency) in probes {
        latencies[index] = latency;
    }
    let mut reachable: Vec<Candidate> = Vec::new();
    let mut unreachable: Vec<Candidate> = Vec::new();
    for (mut candidate, latency) in candidates.into_iter().zip(latencies) {
        match latency {
            Some(latency) => {
                candidate.latency = Some(latency);
                reachable.push(candidate);
            }
            None => unreachable.push(candidate),
        }
    }
    reachable.sort_by_key(|candidate| candidate.latency.unwrap_or_default());
    // 探测超时不等于一定下不动，但官方地址不该因为一次慢握手被彻底排除。
    unreachable.sort_by_key(|candidate| !candidate.direct);
    reachable.extend(unreachable);
    reachable
}

/// 下载尝试计划：最多 `MAX_DOWNLOAD_SOURCES` 个源，并保证 GitHub 官方地址在其中。
pub(super) fn attempt_plan(ordered: &[Candidate]) -> Vec<&Candidate> {
    let mut plan: Vec<&Candidate> = ordered.iter().take(MAX_DOWNLOAD_SOURCES).collect();
    let Some(direct) = ordered.iter().find(|candidate| candidate.direct) else {
        return plan;
    };
    if !plan.iter().any(|candidate| candidate.direct) {
        plan.pop();
        plan.push(direct);
    }
    plan
}

impl Mirror {
    fn prefix_host(&self) -> Option<String> {
        Url::parse(&self.prefix)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
    }
}

fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// 镜像前缀承载官方源，原地址的剩余路径直接接在后面。
/// 前缀要么以官方源结尾（`…/https://github.com`、`…/github.com`），
/// 要么本身就是官方源开头的地址（测试里指向本机 mock）。
fn candidate_url(prefix: &str, canonical: &str, origin: &str) -> Option<String> {
    let authority = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    if !prefix.ends_with(authority) && !prefix.starts_with(origin) {
        return None;
    }
    let suffix = canonical.strip_prefix(origin)?;
    let base = Url::parse(prefix).ok()?;
    if base.cannot_be_a_base() || base.host_str().is_none() {
        return None;
    }
    Some(format!("{prefix}{suffix}"))
}

/// 只读到响应头就丢弃连接：公益源不该因为一次探测被拉走整个安装包。
async fn probe(client: &Client, url: String) -> Option<Duration> {
    let started = Instant::now();
    let response = tokio::time::timeout(
        PROBE_TIMEOUT,
        client
            .get(&url)
            .header(header::RANGE, PROBE_RANGE)
            .send(),
    )
    .await
    .ok()?
    .ok()?;
    if !response.status().is_success() {
        return None;
    }
    Some(started.elapsed())
}

#[cfg(test)]
#[path = "update_mirrors_tests.rs"]
mod tests;