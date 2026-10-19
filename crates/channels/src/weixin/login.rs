//! 扫码登录状态机。
//!
//! 与 hermes 相同:先取二维码,再每秒轮询状态;`scaned_but_redirect` 时把
//! 轮询地址切到重定向主机;`expired` 自动刷新(最多 3 次);`confirmed`
//! 落盘凭据并结束。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::client;
use super::protocol::{ILINK_BASE_URL, QrState, QrTicket};
use super::store::{Account, save_account};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(480);
const MAX_QR_REFRESH: u32 = 3;

/// 启动登录任务。状态写入共享快照,前端轮询 `qr_svg`/`status`。
pub fn spawn_login(
    dir: PathBuf,
    base_url: String,
    snapshot: Arc<Mutex<QrState>>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_login(dir, base_url, snapshot, cancel).await;
    })
}

async fn run_login(
    dir: PathBuf,
    base_url: String,
    snapshot: Arc<Mutex<QrState>>,
    cancel: CancellationToken,
) {
    let mut current_base = if base_url.trim().is_empty() {
        ILINK_BASE_URL.to_string()
    } else {
        base_url
    };
    let mut ticket = match client::fetch_qr(&current_base).await {
        Ok(ticket) => ticket,
        Err(error) => {
            set_status(&snapshot, "failed", Some(error));
            return;
        }
    };
    update_snapshot(&snapshot, &ticket, "wait", None, None);

    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut refresh_count = 0u32;
    loop {
        if cancel.is_cancelled() {
            set_status(&snapshot, "failed", Some("登录已取消".to_string()));
            return;
        }
        if Instant::now() > deadline {
            set_status(&snapshot, "failed", Some("微信登录超时".to_string()));
            return;
        }
        let value = match client::poll_qr(&current_base, &ticket.qrcode).await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "微信扫码状态查询失败");
                sleep_or_cancel(Duration::from_secs(1), &cancel).await;
                continue;
            }
        };
        let status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("wait");
        match status {
            "wait" => update_snapshot(&snapshot, &ticket, "wait", None, None),
            "scaned" => update_snapshot(
                &snapshot,
                &ticket,
                "scaned",
                Some("已扫码，请在微信里确认".to_string()),
                None,
            ),
            "scaned_but_redirect" => {
                if let Some(host) = value.get("redirect_host").and_then(Value::as_str)
                    && !host.trim().is_empty()
                {
                    current_base = format!("https://{}", host.trim());
                }
            }
            "expired" => {
                refresh_count += 1;
                if refresh_count > MAX_QR_REFRESH {
                    set_status(&snapshot, "failed", Some("二维码多次过期，请重新登录".to_string()));
                    return;
                }
                match client::fetch_qr(&current_base).await {
                    Ok(next) => {
                        ticket = next;
                        update_snapshot(
                            &snapshot,
                            &ticket,
                            "wait",
                            Some("二维码已过期，已自动刷新".to_string()),
                            None,
                        );
                    }
                    Err(error) => {
                        set_status(&snapshot, "failed", Some(error));
                        return;
                    }
                }
            }
            "confirmed" => {
                let account = Account {
                    account_id: string_field(&value, "ilink_bot_id"),
                    token: string_field(&value, "bot_token"),
                    base_url: {
                        let base = string_field(&value, "baseurl");
                        if base.is_empty() {
                            current_base.clone()
                        } else {
                            base
                        }
                    },
                    user_id: string_field(&value, "ilink_user_id"),
                    saved_at: epoch_seconds(),
                };
                if account.account_id.is_empty() || account.token.is_empty() {
                    set_status(
                        &snapshot,
                        "failed",
                        Some("微信返回的登录凭据不完整".to_string()),
                    );
                    return;
                }
                if let Err(error) = save_account(&dir, &account) {
                    set_status(&snapshot, "failed", Some(error));
                    return;
                }
                update_snapshot(
                    &snapshot,
                    &ticket,
                    "confirmed",
                    Some("微信连接成功".to_string()),
                    Some(account),
                );
                return;
            }
            _ => {}
        }
        sleep_or_cancel(Duration::from_secs(1), &cancel).await;
    }
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn epoch_seconds() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_default()
}

fn set_status(snapshot: &Mutex<QrState>, status: &str, message: Option<String>) {
    let mut state = snapshot.lock().unwrap_or_else(|poison| poison.into_inner());
    state.status = status.to_string();
    state.message = message;
}

fn update_snapshot(
    snapshot: &Mutex<QrState>,
    ticket: &QrTicket,
    status: &str,
    message: Option<String>,
    account: Option<Account>,
) {
    let content = if ticket.content.trim().is_empty() {
        ticket.qrcode.clone()
    } else {
        ticket.content.clone()
    };
    let svg = render_qr_svg(&content);
    let mut state = snapshot.lock().unwrap_or_else(|poison| poison.into_inner());
    state.qrcode = ticket.qrcode.clone();
    state.qr_content = content;
    if !svg.is_empty() {
        state.qr_svg = svg;
    }
    state.status = status.to_string();
    state.message = message;
    if account.is_some() {
        state.account = account;
    }
}

/// SVG 用矢量渲染,前端 `<img>` 直接展示,扫码最稳。
fn render_qr_svg(content: &str) -> String {
    use qrcode::render::svg;
    match qrcode::QrCode::with_error_correction_level(content.as_bytes(), qrcode::EcLevel::M) {
        Ok(code) => code
            .render::<svg::Color>()
            .min_dimensions(240, 240)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
        Err(error) => {
            tracing::warn!(%error, "微信二维码渲染失败");
            String::new()
        }
    }
}

async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) {
    tokio::select! {
        _ = cancel.cancelled() => {}
        _ = tokio::time::sleep(duration) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_svg_for_qr_content() {
        let svg = render_qr_svg("https://example.invalid/liteapp?x=1");
        assert!(svg.contains("<svg"), "SVG 头缺失: {svg}");
        assert!(svg.contains("viewBox"));
    }
}
