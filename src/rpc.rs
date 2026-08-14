//! stdio JSON-RPC 服务（`cos --rpc`）：每行一个请求、每行一个响应，供外部程序调用。
//!
//! 协议（JSON-RPC 2.0 子集）：
//! - 请求 `{"id": <任意>, "method": "...", "params": {...}}`，响应同 id；
//! - `ping` → `{"result": "pong"}`（健康检查）；
//! - `chat` `{message, images?}` → 跑一轮 turn，`{"result": {"turn", "reply", "tools", "cancelled"}}`；
//! - `session` → `{"result": {"session", "events", "messages"}}`；
//! - `exit` → `{"result": "bye"}` 后进程优雅退出；
//! - 解析失败 → `{"id": null, "error": {"code": -32700}}`；未知方法 → `-32601`。
//!
//! 用法示例（PowerShell）：`'{"id":1,"method":"chat","params":{"message":"你好"}}' | cos --rpc`

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use cos_llm::UserMessage;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::{AppError, Assembled, run_turn, wait_for_cancel};

/// 服务循环：读输入行 → 分发 → 写响应行；EOF 或 `exit` 返回。
pub async fn serve_rpc<R, W>(
    mut reader: R,
    mut writer: W,
    assembled: &Assembled,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(), AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let read = match &cancel {
            Some(flag) => tokio::select! {
                n = reader.read_line(&mut line) => n?,
                _ = wait_for_cancel(flag.clone()) => return Ok(()),
            },
            None => reader.read_line(&mut line).await?,
        };
        if read == 0 {
            return Ok(()); // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                write_line(
                    &mut writer,
                    &json!({"id": null, "error": {"code": -32700, "message": "请求不是合法 JSON"}}),
                )
                .await?;
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let response = match method.as_str() {
            "ping" => json!({"id": id, "result": "pong"}),
            "help" => json!({
                "id": id,
                "result": {"methods": ["ping", "chat", "session", "exit", "help"]}
            }),
            "session" => {
                let session = assembled.agent.session();
                json!({
                    "id": id,
                    "result": {
                        "session": session.id(),
                        "events": session.events().len(),
                        "messages": session.derive_messages().len(),
                    }
                })
            }
            "chat" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut user = UserMessage::new(message);
                if let Some(images) = params.get("images").and_then(Value::as_array) {
                    user.images
                        .extend(images.iter().filter_map(Value::as_str).map(str::to_string));
                }
                let summary = run_turn(&assembled.agent, user, cancel.as_ref()).await;
                json!({
                    "id": id,
                    "result": {
                        "turn": summary.turn,
                        "reply": summary.reply,
                        "tools": summary.tool_trace,
                        "cancelled": summary.cancelled,
                    }
                })
            }
            "exit" => {
                write_line(&mut writer, &json!({"id": id, "result": "bye"})).await?;
                return Ok(());
            }
            _ => json!({
                "id": id,
                "error": {"code": -32601, "message": format!("未知方法: {method}")}
            }),
        };
        write_line(&mut writer, &response).await?;
    }
}

/// 写一行 JSON 响应（换行结尾 + flush）。
async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<(), AppError> {
    writer
        .write_all(serde_json::to_string(value).unwrap().as_bytes())
        .await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
