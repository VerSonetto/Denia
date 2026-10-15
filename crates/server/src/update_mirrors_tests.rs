use super::*;

use std::collections::HashSet;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;

const ASSET: &str = "/VerSonetto/Denia/releases/download/v0.4.6/Denia-setup.exe";

fn mock_mirror(label: &str, prefix: String) -> Mirror {
    Mirror {
        label: label.to_string(),
        prefix,
        redirect_hosts: Vec::new(),
    }
}

fn candidate(label: &str, direct: bool, latency: Option<u64>) -> Candidate {
    Candidate {
        label: label.to_string(),
        host: format!("{label}.example"),
        url: format!("https://{label}.example{ASSET}"),
        direct,
        latency: latency.map(Duration::from_millis),
    }
}

#[test]
fn built_in_mirrors_are_https_github_proxies_with_unique_labels() {
    let mirrors = default_mirrors();
    assert!(mirrors.len() >= 8, "镜像表不应被削得过空");
    assert_eq!(
        mirrors
            .iter()
            .map(|mirror| mirror.label.as_str())
            .collect::<HashSet<_>>()
            .len(),
        mirrors.len(),
        "标签重复会让界面分不清是谁在下载"
    );
    for mirror in mirrors.iter() {
        let base = Url::parse(&mirror.prefix)
            .unwrap_or_else(|error| panic!("{}: {error}", mirror.prefix));
        assert_eq!(base.scheme(), "https", "{}", mirror.prefix);
        assert!(base.host_str().is_some_and(|host| !host.is_empty()));
        let url = candidate_url(&mirror.prefix, &format!("{GITHUB_ORIGIN}{ASSET}"), GITHUB_ORIGIN)
            .unwrap_or_else(|| panic!("{} 无法拼接官方地址", mirror.prefix));
        assert!(url.starts_with(&mirror.prefix));
        assert!(url.ends_with(ASSET));
        for redirect in mirror.redirect_hosts.iter() {
            assert!(!redirect.is_empty() && !redirect.contains('/'));
        }
    }
}

#[test]
fn candidate_urls_keep_the_official_path_and_reject_foreign_sources() {
    assert_eq!(
        candidate_url(
            "https://gh-proxy.org/https://github.com",
            &format!("{GITHUB_ORIGIN}{ASSET}"),
            GITHUB_ORIGIN
        )
        .unwrap(),
        format!("https://gh-proxy.org/https://github.com{ASSET}")
    );
    // 目标源不是官方地址时不允许拼接：否则代理前缀会被喂进任意第三方 URL。
    assert!(
        candidate_url(
            "https://gh-proxy.org/https://github.com",
            "https://evil.example/setup.exe",
            GITHUB_ORIGIN
        )
        .is_none()
    );
    assert!(
        candidate_url(
            "https://evil.example/https://github.com",
            &format!("{GITHUB_ORIGIN}{ASSET}"),
            "http://127.0.0.1:1"
        )
        .is_none()
    );
    assert!(
        candidate_url(
            "https://gh-proxy.org/https://github.com",
            &format!("{GITHUB_ORIGIN}{ASSET}"),
            "https://github.co"
        )
        .is_none(),
        "前缀只碰巧包含源地址时也算不匹配"
    );
    assert!(
        candidate_url(
            "/relative/https://github.com",
            &format!("{GITHUB_ORIGIN}{ASSET}"),
            GITHUB_ORIGIN
        )
        .is_none()
    );
}

#[test]
fn redirects_are_limited_to_github_mirrors_and_their_declared_hosts() {
    let mirrors = vec![Mirror {
        label: "crashmc".to_string(),
        prefix: "https://cdn.crashmc.com/https://github.com".to_string(),
        redirect_hosts: vec!["cdn.akaere.online".to_string()],
    }];
    for url in [
        "https://github.com/x",
        "https://release-assets.githubusercontent.com/x",
        "https://cdn.crashmc.com/https://github.com/x",
        "https://cdn.akaere.online/https:/github.com/x",
    ] {
        assert!(trusted_redirect(&url.parse().unwrap(), &mirrors), "{url}");
    }
    for url in [
        "http://cdn.crashmc.com/x",
        "https://cdn.crashmc.com:444/x",
        "https://cdn.crashmc.com.evil.example/x",
        "https://user:secret@cdn.crashmc.com/x",
        "https://evil.example/x",
        "https://unrelated.example/cdn.akaere.online",
    ] {
        assert!(!trusted_redirect(&url.parse().unwrap(), &mirrors), "{url}");
    }
}

#[test]
fn download_plan_is_capped_and_always_keeps_an_official_fallback() {
    let ordered: Vec<Candidate> = (0..8)
        .map(|index| candidate(&format!("mirror-{index}"), false, Some(index)))
        .chain(std::iter::once(candidate("github", true, Some(900))))
        .collect();
    let plan = attempt_plan(&ordered);
    assert_eq!(plan.len(), MAX_DOWNLOAD_SOURCES);
    assert!(plan.last().unwrap().direct, "官方地址必须参与兜底");
    assert_eq!(plan[0].label, "mirror-0", "计划必须保持探测得出的速度顺序");

    // 官方地址排在前面被截断时，把它换回计划末尾。
    let fast_direct = vec![
        candidate("github", true, Some(1)),
        candidate("mirror-a", false, Some(2)),
        candidate("mirror-b", false, Some(3)),
        candidate("mirror-c", false, Some(4)),
        candidate("mirror-d", false, Some(5)),
    ];
    let plan = attempt_plan(&fast_direct);
    assert_eq!(plan.len(), MAX_DOWNLOAD_SOURCES);
    assert!(
        plan[0].direct,
        "官方直连实测最快时照用不误，不该被硬塞到末尾"
    );

    let without_direct: Vec<Candidate> = (0..8)
        .map(|index| candidate(&format!("mirror-{index}"), false, Some(index)))
        .collect();
    assert_eq!(attempt_plan(&without_direct).len(), MAX_DOWNLOAD_SOURCES);
}

#[tokio::test]
async fn ranking_keeps_each_latency_with_its_own_source() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/fast/{*rest}",
            get(|| async { Response::new(Body::from("fast")) }),
        )
        .route(
            "/slow/{*rest}",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(150)).await;
                Response::new(Body::from("slow"))
            }),
        )
        .route(
            "/broken/{*rest}",
            get(|| async { StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/direct/{*rest}",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Response::new(Body::from("direct"))
            }),
        );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = Client::builder().no_proxy().build().unwrap();
    let canonical = format!("{origin}/direct{ASSET}");
    let mirrors = vec![
        mock_mirror("slow", format!("{origin}/slow")),
        mock_mirror("broken", format!("{origin}/broken")),
        mock_mirror("fast", format!("{origin}/fast")),
    ];
    let ranked = rank_candidates(&client, &canonical, &origin, &mirrors).await;
    task.abort();

    assert_eq!(
        ranked.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
        vec!["fast", "GitHub", "slow", "broken"],
        "可连通的按延迟升序，连不通的排在后面"
    );
    let latency = |label: &str| {
        ranked
            .iter()
            .find(|candidate| candidate.label == label)
            .and_then(|candidate| candidate.latency)
            .unwrap_or_default()
    };
    assert!(latency("fast") < latency("slow"), "延迟必须归属各自的源");
    assert!(latency("slow") >= Duration::from_millis(140), "{:?}", latency("slow"));
    assert_eq!(latency("broken"), Duration::ZERO, "失败源不参与排序");
    let direct = ranked.iter().find(|candidate| candidate.direct).unwrap();
    assert_eq!(direct.url, canonical);
    assert!(
        direct.latency.is_some_and(|latency| latency >= Duration::from_millis(40)),
        "官方地址同样参与测速：国内直连慢时就不该被选中"
    );
}