//! Console hosting: release builds embed `web/dist` into the binary so the
//! global `denia` command serves the console from any directory; debug
//! builds read the same folder from disk. `--web <dir>` overrides both with
//! an explicit filesystem directory.
//!
//! ## 为什么自己处理编码而不是挂响应压缩中间件
//!
//! 控制台主包是 MB 级的 JS/CSS,经公网隧道发到手机上时**字节数就是延迟**
//! (首屏要在弱网上多跑几百 KB)。构建期已把每个文本产物压成 `.br`/`.gz`
//! 旁挂文件(`scripts/precompress.mjs`),这里按 `Accept-Encoding` 直接选
//! 对应实体:压缩比用最高档(brotli q11,构建期跑一次无所谓耗时),运行时
//! 零 CPU、零缓冲。没有预压缩产物的文件(以及 `--web` 指向的未构建目录)
//! 原样发出,由响应压缩中间件兜底。

use std::path::{Component, Path, PathBuf};

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// 一份可发出的实体:逻辑名(用于猜 content-type)、字节、内容编码。
pub struct Encoded {
    pub name: String,
    pub bytes: Vec<u8>,
    /// `None` = 未压缩。
    pub encoding: Option<&'static str>,
}

/// 读一个资源路径(明文)。SPA 深链(无扩展名)回落到 `index.html`。
pub fn resolve(uri: &Uri, dir: Option<&Path>) -> Option<(String, Vec<u8>)> {
    let entity = resolve_encoded(uri, dir, "")?;
    Some((entity.name, entity.bytes))
}

/// 按 `Accept-Encoding` 挑实体:优先构建期预压缩产物,回退明文。
///
/// 请求路径本身永远是 `/assets/index-*.js`;`.br`/`.gz` 只是旁挂的实体变体,
/// 不出现在 URL 里(所以远程门的资源白名单不需要为它们开口子)。
pub fn resolve_encoded(uri: &Uri, dir: Option<&Path>, accept_encoding: &str) -> Option<Encoded> {
    let raw = uri.path().trim_start_matches('/');
    let requested = if raw.is_empty() { "index.html" } else { raw };

    // 越靠前优先级越高。brotli 对 JS 比 gzip 再省 15–20%。
    let variants: &[(&str, &str)] = if wants(accept_encoding, "br") {
        &[(".br", "br"), (".gz", "gzip")]
    } else if wants(accept_encoding, "gzip") {
        &[(".gz", "gzip")]
    } else {
        &[]
    };
    for (suffix, encoding) in variants {
        if let Some((bytes, name)) = read_variant(dir, requested, suffix) {
            return Some(Encoded { name, bytes, encoding: Some(encoding) });
        }
    }

    let (name, bytes) = plain(requested, dir)?;
    Some(Encoded { name, bytes, encoding: None })
}

/// 明文资源的解析(嵌入 or 磁盘),含 SPA 回落。
fn plain(requested: &str, dir: Option<&Path>) -> Option<(String, Vec<u8>)> {
    match dir {
        Some(dir) => read_from_dir(dir, requested),
        None => WebAssets::get(requested)
            .map(|file| (requested.to_string(), file.data.to_vec()))
            .or_else(|| {
                if requested.contains('.') {
                    None
                } else {
                    WebAssets::get("index.html").map(|file| ("index.html".to_string(), file.data.to_vec()))
                }
            }),
    }
}

/// 读 `<requested><suffix>` 的预压缩变体。
///
/// 只给已存在的变体;深链(无扩展名 → index.html)没有变体也不该有的编码
/// 头,交给明文路径。
fn read_variant(dir: Option<&Path>, requested: &str, suffix: &str) -> Option<(Vec<u8>, String)> {
    let path = format!("{requested}{suffix}");
    match dir {
        Some(dir) => {
            let relative = safe_relative(&path)?;
            let candidate = dir.join(&relative);
            if !candidate.is_file() {
                return None;
            }
            let bytes = std::fs::read(&candidate).ok()?;
            Some((bytes, virtual_path(&relative)))
        }
        None => WebAssets::get(&path).map(|file| (file.data.to_vec(), path)),
    }
}

/// `Accept-Encoding` 里是否含某个编码(按 `token` 粗匹配,忽略 q 值:
/// 显式带 `br;q=0` 的客户端极少,而为它多养一套排序不值得)。
fn wants(accept_encoding: &str, encoding: &str) -> bool {
    accept_encoding.split(',').any(|token| {
        let token = token.trim_start();
        let name = token.split(';').next().unwrap_or_default().trim();
        name.eq_ignore_ascii_case(encoding) || name == "*"
    })
}

/// Filesystem mode with traversal rejection: only paths that normalize
/// inside `dir` are served.
fn read_from_dir(dir: &Path, requested: &str) -> Option<(String, Vec<u8>)> {
    let relative = safe_relative(requested)?;
    let candidate = dir.join(&relative);
    if candidate.is_file() {
        return std::fs::read(&candidate)
            .ok()
            .map(|bytes| (virtual_path(&relative), bytes));
    }
    if !requested.contains('.') {
        let index = dir.join("index.html");
        if index.is_file() {
            return std::fs::read(&index)
                .ok()
                .map(|bytes| ("index.html".to_string(), bytes));
        }
    }
    None
}

/// 磁盘路径转"虚拟路径":分隔符统一成 `/`。
///
/// 资源名要参与 content-type 猜测与 `index.html` 判定,而这些字符串在嵌入模式
/// 下永远用 `/`。Windows 的 `--web <dir>` 模式若返回 `assets\index.js`,同一
/// 份资源在两种模式下就有了两种名字,判定逻辑随之分叉 —— 归一化后两边一致。
fn virtual_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn safe_relative(requested: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(requested).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Renders one resolved entity (or a 404) with content type, cache policy
/// and — when a precompressed variant was picked — `Content-Encoding`.
pub fn response_for(uri: &Uri, dir: Option<&Path>, accept_encoding: &str) -> Response {
    let Some(entity) = resolve_encoded(uri, dir, accept_encoding) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    response_from(entity)
}

fn response_from(entity: Encoded) -> Response {
    // content-type 要按"去掉 .br/.gz 之后"的名字猜:mime_guess 取末段扩展名,
    // 拿 index.js.br 会猜成未知类型,浏览器就不执行这段脚本了。
    let logical = entity
        .name
        .strip_suffix(".br")
        .or_else(|| entity.name.strip_suffix(".gz"))
        .unwrap_or(&entity.name);
    let mime = mime_guess::from_path(logical).first_or_octet_stream();
    let cache = if logical == "index.html" {
        "no-store"
    } else {
        "public, max-age=31536000, immutable"
    };
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        // mime 表里的值都是合法 ASCII;真出现异常按 octet-stream 兜底而不是 panic。
        header::HeaderValue::from_str(&mime.to_string())
            .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static(cache));
    // 同一 URL 会因 Accept-Encoding 返回不同实体,不声明 Vary 会让中间缓存
    // (含 Cloudflare 边缘)把 gzip 版发给支持 br 的客户端。
    headers.insert(header::VARY, header::HeaderValue::from_static("Accept-Encoding"));
    if let Some(encoding) = entity.encoding {
        headers.insert(
            header::CONTENT_ENCODING,
            header::HeaderValue::from_static(encoding),
        );
    }
    (StatusCode::OK, headers, entity.bytes).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(path: &str) -> Uri {
        // Uri 的 from_str 来自 FromStr trait,要显式导入才有。
        use std::str::FromStr;
        Uri::from_str(path).unwrap()
    }

    #[test]
    fn root_falls_back_to_index() {
        let response = response_for(&uri("/"), None, "");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn deep_links_serve_the_shell() {
        let response = response_for(&uri("/some/spa/route"), None, "");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// 用临时目录造夹具:这些用例测的是"选哪个实体"的决策逻辑,不该依赖
    /// 本次构建有没有真的产出 `.br`(预压缩是构建流程的一步,单独跑测试时
    /// dist 可能还是旧的)。
    fn fixture(files: &[(&str, &[u8])]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-web-assets-{}", uuid::Uuid::new_v4()));
        for (name, bytes) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        dir
    }

    #[test]
    fn embedded_index_is_present() {
        assert!(WebAssets::get("index.html").is_some(), "嵌入产物缺失");
    }

    #[test]
    fn brotli_preferred_when_offered() {
        let dir = fixture(&[
            ("assets/app.js", b"console.log(1)".as_slice()),
            ("assets/app.js.br", b"BROTLI".as_slice()),
            ("assets/app.js.gz", b"GZIP".as_slice()),
        ]);
        let entity = resolve_encoded(&uri("/assets/app.js"), Some(&dir), "gzip, deflate, br")
            .expect("应能读到 br 变体");
        assert_eq!(entity.encoding, Some("br"));
        assert_eq!(entity.name, "assets/app.js.br");
        assert_eq!(entity.bytes, b"BROTLI");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gzip_only_clients_never_get_brotli() {
        let dir = fixture(&[
            ("assets/app.css", b"body{}".as_slice()),
            ("assets/app.css.br", b"BROTLI".as_slice()),
            ("assets/app.css.gz", b"GZIP".as_slice()),
        ]);
        let entity =
            resolve_encoded(&uri("/assets/app.css"), Some(&dir), "gzip").expect("gzip 变体");
        assert_eq!(entity.encoding, Some("gzip"));
        assert_eq!(entity.bytes, b"GZIP");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn falls_back_to_gzip_when_no_brotli_variant() {
        let dir = fixture(&[("assets/app.js", b"x".as_slice()), ("assets/app.js.gz", b"G".as_slice())]);
        let entity =
            resolve_encoded(&uri("/assets/app.js"), Some(&dir), "br, gzip").expect("gzip 兜底");
        assert_eq!(entity.encoding, Some("gzip"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn clients_without_encoding_support_get_plaintext() {
        let dir = fixture(&[
            ("assets/app.js", b"PLAIN".as_slice()),
            ("assets/app.js.br", b"BROTLI".as_slice()),
        ]);
        let entity = resolve_encoded(&uri("/assets/app.js"), Some(&dir), "").expect("明文");
        assert_eq!(entity.encoding, None);
        assert_eq!(entity.bytes, b"PLAIN");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_variant_serves_plaintext_not_404() {
        let dir = fixture(&[("index.html", b"<html>".as_slice())]);
        let entity = resolve_encoded(&uri("/index.html"), Some(&dir), "br").expect("明文兜底");
        assert_eq!(entity.encoding, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn accept_encoding_matching_ignores_case_and_parameters() {
        assert!(wants("BR", "br"));
        assert!(wants("gzip;q=1.0, br;q=0.9", "br"));
        assert!(wants("*", "br"));
        assert!(!wants("gzip", "br"));
        // "brotli" 之类的前缀不该误判为 br
        assert!(!wants("deflate, brr", "br"));
    }

    #[test]
    fn encoded_response_declares_content_encoding_and_vary() {
        let dir = fixture(&[("assets/app.js", b"x".as_slice()), ("assets/app.js.br", b"B".as_slice())]);
        let response = response_for(&uri("/assets/app.js"), Some(&dir), "br");
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_ENCODING], "br");
        assert_eq!(headers[header::VARY], "Accept-Encoding");
        // content-type 不能被 .br 后缀带偏,否则浏览器不执行这段脚本。
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .contains("javascript"),
            "{:?}",
            headers[header::CONTENT_TYPE]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unencoded_response_omits_content_encoding() {
        let dir = fixture(&[("index.html", b"<html>".as_slice())]);
        let response = response_for(&uri("/index.html"), Some(&dir), "");
        assert!(
            !response.headers().contains_key(header::CONTENT_ENCODING),
            "明文实体不该带 Content-Encoding"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn traversal_is_rejected_in_directory_mode() {
        let dir = std::env::temp_dir();
        assert!(resolve(&uri("/../../Windows/win.ini"), Some(&dir)).is_none());
        assert!(resolve_encoded(&uri("/../../etc/passwd"), Some(&dir), "br").is_none());
        // 变体路径同样受约束
        assert!(resolve_encoded(&uri("/../secret.js"), Some(&dir), "br").is_none());
    }
}
