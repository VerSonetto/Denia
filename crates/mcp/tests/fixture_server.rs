//! 测试用 MCP 服务器(fixture):stdio 上一个最小可用的 MCP 服务器。
//!
//! 行为:
//! - `initialize` → 返回协议版本与能力;
//! - `tools/list` → 两个工具:`echo`(回显)与 `big`(产出长文本,用于验证
//!   分页截断);
//! - `tools/call` → 按工具名返回 text content;`big` 支持 `chars` 参数。
//!
//! 只依赖标准库,任何平台都能直接 spawn。**逐行读 stdin**:MCP 是长连接
//! 交互式协议,读到 EOF 再统一处理会让客户端永远等不到响应。

use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let id = request.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // 通知(无 id)不回响应。
        if id.is_null() {
            continue;
        }
        let response = match method {
            "initialize" => ok(
                &id,
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "denia-mcp-fixture", "version": "1.0.0" }
                }),
            ),
            "tools/list" => ok(
                &id,
                serde_json::json!({
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
            ),
            "tools/call" => ok(&id, call_result(&request)),
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {other}") }
            }),
        };
        let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap());
        let _ = stdout.flush();
    }
}

fn ok(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
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
            serde_json::json!({
                "content": [ { "type": "text", "text": "A".repeat(chars) } ]
            })
        }
        other => serde_json::json!({
            "content": [ { "type": "text", "text": format!("unknown tool: {other}") } ],
            "isError": true
        }),
    }
}
