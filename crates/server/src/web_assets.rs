//! Console hosting: release builds embed `web/dist` into the binary so the
//! global `dsh-rs` command serves the console from any directory; debug
//! builds read the same folder from disk. `--web <dir>` overrides both with
//! an explicit filesystem directory.

use std::path::{Component, Path, PathBuf};

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// Resolves one request path against the console assets.
///
/// `dir` (the `--web` override) wins; otherwise the embedded assets answer.
/// Extension-less paths fall back to `index.html` for SPA routing.
pub fn resolve(uri: &Uri, dir: Option<&Path>) -> Option<(String, Vec<u8>)> {
    let raw = uri.path().trim_start_matches('/');
    let requested = if raw.is_empty() { "index.html" } else { raw };
    match dir {
        Some(dir) => read_from_dir(dir, requested),
        None => WebAssets::get(requested)
            .map(|file| (requested.to_string(), file.data.to_vec()))
            .or_else(|| {
                if requested.contains('.') {
                    None
                } else {
                    WebAssets::get("index.html")
                        .map(|file| ("index.html".to_string(), file.data.to_vec()))
                }
            }),
    }
}

/// Filesystem mode with traversal rejection: only paths that normalize
/// inside `dir` are served.
fn read_from_dir(dir: &Path, requested: &str) -> Option<(String, Vec<u8>)> {
    let relative = safe_relative(requested)?;
    let candidate = dir.join(&relative);
    if candidate.is_file() {
        return std::fs::read(&candidate)
            .ok()
            .map(|bytes| (relative.to_string_lossy().to_string(), bytes));
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

/// Renders one resolved asset (or a 404) with a content type and cache
/// policy: `index.html` is never cached, hashed assets are.
pub fn response_for(uri: &Uri, dir: Option<&Path>) -> Response {
    match resolve(uri, dir) {
        Some((name, bytes)) => {
            let mime = mime_guess::from_path(&name).first_or_octet_stream();
            let cache = if name == "index.html" {
                "no-store"
            } else {
                "public, max-age=31536000, immutable"
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.to_string()),
                    (header::CACHE_CONTROL, cache.to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
