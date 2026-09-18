//! 测试用 HTTP MCP 服务器(fixture):同时提供 Streamable HTTP 与 SSE
//! 两种端点,只用标准库(避免为测试引入 web 框架)。
//!
//! 端点:
//! - `POST /mcp` —— Streamable HTTP:收 JSON-RPC,直接回 JSON 响应;
//! - `GET  /sse` —— 建事件流,先发 `event: endpoint`(data = `/messages`),
//!   之后 `POST /messages` 收到的请求,其响应从这里以 `event: message` 回来;
//! - `POST /messages` —— SSE 传输的 POST 端点,回 202(无正文)。
//!
//! 工具:`echo`(回显)与 `big`(产出长文本)。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    // 端口写到 stdout 第一行:测试据此拼 URL。
    println!("{}", listener.local_addr().unwrap().port());
    let _ = std::io::stdout().flush();

    // SSE 流上待投递的响应:id → 报文。POST /messages 的线程往里放,
    // GET /sse 的线程取出来推给客户端。
    let queue: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let queue = queue.clone();
        std::thread::spawn(move || handle(stream, queue));
    }
}

fn handle(stream: std::net::TcpStream, queue: Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    // 读请求行
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let (method, path) = (parts[0].to_string(), parts[1].to_string());

    // 读请求头(直到空行)
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    // 读正文
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }

    let mut stream = stream;
    match (method.as_str(), path.as_str()) {
        ("GET", "/sse") => {
            // 事件流:先 endpoint,再持续把队列里的响应推出去。
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n"
            );
            let _ = stream.flush();
            let _ = write!(stream, "event: endpoint\ndata: /messages\n\n");
            let _ = stream.flush();
            loop {
                let pending: Vec<String> = {
                    let mut guard = queue.lock().unwrap();
                    guard.drain(..).collect()
                };
                for payload in pending {
                    if write!(stream, "event: message\ndata: {payload}\n\n").is_err() {
                        return;
                    }
                    if stream.flush().is_err() {
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        ("POST", "/messages") => {
            // SSE 的 POST:把响应塞进队列,由事件流投递。
            let response = respond(&body);
            queue.lock().unwrap().push(response);
            let _ = write!(
                stream,
                "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.flush();
        }
        ("POST", "/mcp") => {
            // Streamable HTTP:直接回 JSON(带会话 id,验证客户端会带回)。
            let response = respond(&body);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nmcp-session-id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            let _ = stream.flush();
        }
        _ => {
            let _ = write!(
                stream,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.flush();
        }
    }
}

/// 对一条 JSON-RPC 请求给出响应文本。
fn respond(body: &[u8]) -> String {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::from(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"parse error"}}"#,
        );
    };
    let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "denia-mcp-http-fixture", "version": "1.0.0" }
        }),
        "tools/list" => serde_json::json!({
            "tools": [
                {
                    "name": "echo",
                    "description": "回显输入文本",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } },
                        "required": ["text"]
                    }
                },
                {
                    "name": "big",
                    "description": "产出指定长度的文本,用于验证分页",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "chars": { "type": "integer" } }
                    }
                }
            ]
        }),
        "tools/call" => call_result(&request),
        other => {
            return serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {other}") }
            })
            .to_string()
        }
    };
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn call_result(request: &serde_json::Value) -> serde_json::Value {
    let params = request.get("params").cloned().unwrap_or_default();
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_default();
    match name {
        "echo" => {
            let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
            serde_json::json!({ "content": [ { "type": "text", "text": text } ] })
        }
        "big" => {
            let chars = args.get("chars").and_then(|c| c.as_u64()).unwrap_or(10) as usize;
            serde_json::json!({ "content": [ { "type": "text", "text": "A".repeat(chars) } ] })
        }
        other => serde_json::json!({
            "content": [ { "type": "text", "text": format!("unknown tool: {other}") } ],
            "isError": true
        }),
    }
}
