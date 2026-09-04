use denia_browser::cdp::CdpHandle;
use serde_json::json;
#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).expect("ws url");
    let (handle, mut events) = CdpHandle::connect(&url).await.expect("connect");
    let targets = handle.send("Target.getTargets", json!({})).await.unwrap();
    let page = targets
        .get("targetInfos")
        .and_then(serde_json::Value::as_array)
        .expect("infos")
        .iter()
        .find(|t| t.get("type").and_then(serde_json::Value::as_str) == Some("page"))
        .expect("page target")
        .clone();
    let target_id = page.get("targetId").and_then(serde_json::Value::as_str).unwrap().to_string();
    let attached = handle.send("Target.attachToTarget", json!({"targetId": target_id, "flatten": true})).await.unwrap();
    let session = attached.get("sessionId").and_then(serde_json::Value::as_str).unwrap().to_string();
    handle.send_with_session("Page.enable", json!({}), Some(&session)).await.unwrap();
    handle.send_with_session("Network.enable", json!({}), Some(&session)).await.unwrap();
    handle.send_with_session("Page.navigate", json!({"url": "https://example.com"}), Some(&session)).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut seen = 0;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await {
            Ok(Some(event)) => {
                if event.method.starts_with("Network.") {
                    seen += 1;
                    println!("NETWORK EVENT: {} session={:?}", event.method, event.session_id);
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }
    println!("total network events: {seen}");
}