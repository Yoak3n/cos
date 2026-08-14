//! stdio RPC 服务（`cos --rpc`）——协议向 pi 的 RPC 模式对齐（pi `docs/rpc.md`）。
//!
//! 帧格式：严格 JSONL（LF 分隔；输入接受尾部 `\r`）。
//! 命令：`{"id"?, "type": "<command>", ...}` → 响应 `{"id"?, "type": "response",
//! "command": "<命令>", "success": bool, "data"?, "error"?}`；`id` 可选、原样回显。
//! 事件：处理期间实时流式输出 pi 事件（agent_start / turn_start / message_start /
//! message_update / message_end / tool_execution_start|end / turn_end / agent_end /
//! agent_settled），驱动自会话日志（先写日志再行动，事件即日志的投影）。
//!
//! 已实现命令：`prompt`（含 streamingBehavior: steer|followUp）、`steer`、`follow_up`、
//! `abort`、`cancel_message`（cos 扩展：取消队列中指定 id 的待处理消息）、`get_state`、
//! `get_messages`、`get_last_assistant_text`、`get_session_stats`、`get_commands`、
//! `exit`（cos 扩展）。未实现命令返回 `success: false`（协议兼容失败响应）。
//!
//! 排队消息 id：`prompt`/`steer`/`follow_up` 的命令 `id` 即排队消息 id（响应
//! `data.messageId` 原样回显；缺省自动生成 `m-<n>`），可用 `cancel_message` 取消。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cos_agent::AgentStatus;
use cos_llm::{ChunkDelta, ContentBlock, Message, ToolCall, UserMessage};
use cos_session::{SessionEventData, TurnEndReason};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{AppError, Assembled, wait_for_cancel};

/// 服务循环：读命令行 → 分发 → 写响应行；事件转发器并发流式输出。
/// EOF 或 `exit` 命令返回；Ctrl-C（cancel 信号）直接返回。
pub async fn serve_rpc<R, W>(
    reader: R,
    writer: W,
    assembled: &Assembled,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(), AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let writer = Arc::new(Mutex::new(writer));

    // 事件转发器：会话日志增量 → pi 事件流（只发新事件；客户端关闭即停）
    let forwarder_writer = writer.clone();
    let session = assembled.agent.session().clone();
    let mut seen = session.last_seq();
    tokio::spawn(async move {
        let mut forwarder = EventForwarder::default();
        loop {
            let events = session.events_after(seen);
            for event in &events {
                seen = event.seq;
                for line in forwarder.on_event(&event.data) {
                    let mut writer = forwarder_writer.lock().await;
                    if writer.write_all(line.as_bytes()).await.is_err()
                        || writer.write_all(b"\n").await.is_err()
                    {
                        return; // 客户端已关闭
                    }
                    if writer.flush().await.is_err() {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
    });

    let mut lines = tokio::io::BufReader::new(reader).lines();
    let next_id = AtomicU64::new(0);
    loop {
        let line = match &cancel {
            Some(flag) => tokio::select! {
                line = lines.next_line() => line?,
                _ = wait_for_cancel(flag.clone()) => return Ok(()),
            },
            None => lines.next_line().await?,
        };
        let Some(line) = line else { return Ok(()) }; // EOF
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                write_line(
                    &writer,
                    &json!({
                        "type": "response",
                        "command": "parse",
                        "success": false,
                        "error": format!("Failed to parse command: {error}"),
                    }),
                )
                .await?;
                continue;
            }
        };
        let command = request
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let response = dispatch(&command, &request, assembled, &next_id);
        write_line(&writer, &response).await?;
        if command == "exit" {
            return Ok(());
        }
    }
}

/// 命令分发：pi 响应信封（`type: response` + `command` + `success` + `data?`/`error?`）。
fn dispatch(command: &str, request: &Value, assembled: &Assembled, next_id: &AtomicU64) -> Value {
    let id = request.get("id").cloned();
    let respond = |success: bool, data: Option<Value>, error: Option<String>| {
        let mut value = json!({
            "id": id,
            "type": "response",
            "command": command,
            "success": success,
        });
        if let Some(data) = data {
            value["data"] = data;
        }
        if let Some(error) = error {
            value["error"] = json!(error);
        }
        value
    };
    let agent = &assembled.agent;
    match command {
        "prompt" => {
            let message = request
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if message.is_empty() {
                return respond(false, None, Some("message 缺失".into()));
            }
            let message_id = message_id(request, next_id);
            if agent.has_queued_message(&message_id) {
                // fail loud：重复 id 会令 cancel_message 产生歧义，排队时直接拒绝
                return respond(
                    false,
                    None,
                    Some(format!("message id 已存在于队列: {message_id}")),
                );
            }
            let user = user_message(request, &message_id);
            // pi 语义：agent 正在处理时未指定 streamingBehavior → 拒绝
            if agent.status() == AgentStatus::Running {
                match request.get("streamingBehavior").and_then(Value::as_str) {
                    Some("steer") => agent.steer(user),
                    Some("followUp") => agent.followup(user),
                    _ => {
                        return respond(
                            false,
                            None,
                            Some(
                                "agent 正在处理；需指定 streamingBehavior（steer | followUp）"
                                    .into(),
                            ),
                        );
                    }
                }
            } else {
                agent.followup(user);
            }
            respond(true, Some(json!({"messageId": message_id})), None)
        }
        "steer" => {
            let message = request
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if message.is_empty() {
                return respond(false, None, Some("message 缺失".into()));
            }
            let message_id = message_id(request, next_id);
            if agent.has_queued_message(&message_id) {
                return respond(
                    false,
                    None,
                    Some(format!("message id 已存在于队列: {message_id}")),
                );
            }
            agent.steer(user_message(request, &message_id));
            respond(true, Some(json!({"messageId": message_id})), None)
        }
        "follow_up" => {
            let message = request
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if message.is_empty() {
                return respond(false, None, Some("message 缺失".into()));
            }
            let message_id = message_id(request, next_id);
            if agent.has_queued_message(&message_id) {
                return respond(
                    false,
                    None,
                    Some(format!("message id 已存在于队列: {message_id}")),
                );
            }
            agent.followup(user_message(request, &message_id));
            respond(true, Some(json!({"messageId": message_id})), None)
        }
        "cancel_message" => {
            // cos 扩展：取消队列中指定 id 的待处理消息（已开始处理的无法取消）
            let message_id = request
                .get("messageId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if message_id.is_empty() {
                return respond(false, None, Some("messageId 缺失".into()));
            }
            if agent.cancel_message(message_id) {
                respond(true, Some(json!({"cancelled": true})), None)
            } else {
                respond(
                    false,
                    None,
                    Some(format!("队列中无此消息: {message_id}（可能已开始处理）")),
                )
            }
        }
        "abort" => {
            // pi 语义：中止当前操作、保留已排队消息
            agent.cancel(cos_session::AbortCause::User, true);
            respond(true, None, None)
        }
        "get_state" => {
            let session = agent.session();
            respond(
                true,
                Some(json!({
                    "isStreaming": agent.status() == AgentStatus::Running,
                    "sessionId": session.id(),
                    "sessionName": null,
                    "messageCount": session.derive_messages().len(),
                    "pendingMessageCount": agent.pending_count(),
                })),
                None,
            )
        }
        "get_messages" => {
            let messages: Vec<Value> = agent
                .session()
                .derive_messages()
                .iter()
                .map(message_to_json)
                .collect();
            respond(true, Some(json!({"messages": messages})), None)
        }
        "get_last_assistant_text" => {
            let text = agent
                .session()
                .events()
                .iter()
                .rev()
                .find_map(|event| match &event.data {
                    SessionEventData::AssistantMessage { message, .. } => {
                        let text = message.text();
                        if text.is_empty() { None } else { Some(text) }
                    }
                    _ => None,
                });
            respond(true, Some(json!({"text": text})), None)
        }
        "get_session_stats" => {
            let session = agent.session();
            let mut user_messages = 0u64;
            let mut assistant_messages = 0u64;
            let mut tool_calls = 0u64;
            let mut tool_results = 0u64;
            let mut input_tokens = 0u64;
            let mut output_tokens = 0u64;
            for event in session.events() {
                match &event.data {
                    SessionEventData::UserMessage(_) => user_messages += 1,
                    SessionEventData::AssistantMessage { usage, .. } => {
                        assistant_messages += 1;
                        if let Some(usage) = usage {
                            input_tokens += usage.input_tokens;
                            output_tokens += usage.output_tokens;
                        }
                    }
                    SessionEventData::ToolCall { .. } => tool_calls += 1,
                    SessionEventData::ToolResult { .. } => tool_results += 1,
                    _ => {}
                }
            }
            respond(
                true,
                Some(json!({
                    "sessionId": session.id(),
                    "userMessages": user_messages,
                    "assistantMessages": assistant_messages,
                    "toolCalls": tool_calls,
                    "toolResults": tool_results,
                    "totalMessages": user_messages + assistant_messages + tool_calls + tool_results,
                    "tokens": {
                        "input": input_tokens,
                        "output": output_tokens,
                        "total": input_tokens + output_tokens,
                    },
                })),
                None,
            )
        }
        "get_commands" => respond(true, Some(json!({"commands": []})), None),
        "exit" => respond(true, None, None),
        _ => respond(false, None, Some(format!("未知命令: {command}"))),
    }
}

/// 排队消息 id：命令带 `id` 则沿用（pi 关联语义 + 取消身份合一），否则自动生成 `m-<n>`。
fn message_id(request: &Value, next_id: &AtomicU64) -> String {
    request
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("m-{}", next_id.fetch_add(1, Ordering::Relaxed)))
}

/// 构造用户消息：`message` + 可选 `images` + 排队 id。
/// pi 图片格式 `{"type":"image","data":<base64>,"mimeType":...}` → data URL；
/// 纯字符串按原样透传（兼容旧格式）。
fn user_message(request: &Value, message_id: &str) -> UserMessage {
    let mut user = UserMessage {
        content: request
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        images: Vec::new(),
        id: Some(message_id.to_string()),
    };
    if let Some(images) = request.get("images").and_then(Value::as_array) {
        for image in images {
            if let Some(url) = image.as_str() {
                user.images.push(url.to_string());
            } else if let (Some(data), mime) = (
                image.get("data").and_then(Value::as_str),
                image
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("image/png"),
            ) {
                user.images.push(format!("data:{mime};base64,{data}"));
            }
        }
    }
    user
}

/// 模型可见消息 → pi 风格 JSON（get_messages）。
fn message_to_json(message: &Message) -> Value {
    match message {
        Message::System { content } => json!({"role": "system", "content": content}),
        Message::User(user) => {
            let mut value = json!({"role": "user", "content": user.content});
            if !user.images.is_empty() {
                value["attachments"] = json!(user.images);
            }
            value
        }
        Message::Assistant(assistant) => json!({
            "role": "assistant",
            "content": assistant.content.iter().map(block_to_json).collect::<Vec<_>>(),
        }),
        Message::Tool(tool) => json!({
            "role": "toolResult",
            "toolCallId": tool.call_id,
            "content": [{"type": "text", "text": tool.content}],
        }),
        Message::Custom { name, data } => json!({"role": "custom", "name": name, "data": data}),
    }
}

/// 内容块 → pi 风格 JSON。
fn block_to_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ContentBlock::ToolUse { call } => json!({
            "type": "toolCall",
            "id": call.call_id,
            "name": call.name,
            "arguments": parse_args(&call.arguments),
        }),
    }
}

/// 工具调用 → pi 风格 toolCall JSON。
fn tool_call_to_json(call: &ToolCall) -> Value {
    json!({
        "id": call.call_id,
        "name": call.name,
        "arguments": parse_args(&call.arguments),
    })
}

/// 模型产出的原始参数字符串 → JSON 值（非法 JSON 原串兜底）。
fn parse_args(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or(Value::String(raw.to_string()))
}

/// 会话事件 → pi 事件行序列（流式投影；message 状态跨事件保持）。
#[derive(Default)]
struct EventForwarder {
    /// 当前 step 的 assistant 消息是否已开（message_start 已发）。
    msg_open: bool,
    /// 当前文本块是否打开（text_start 已发、text_end 未发）。
    text_open: bool,
    /// 当前文本块在消息中的索引。
    text_index: usize,
    /// 当前文本块累积内容（text_end 携带）。
    text_buf: String,
    /// 已结束的块数（下一个块的 contentIndex）。
    block_count: usize,
    /// call_id → 工具名（tool_execution_end 需要；ToolResult 事件不带 name）。
    tool_names: HashMap<String, String>,
}

impl EventForwarder {
    fn on_event(&mut self, data: &SessionEventData) -> Vec<String> {
        let mut out = Vec::new();
        match data {
            SessionEventData::TurnStart { turn } => {
                out.push(event(json!({"type": "agent_start"})));
                out.push(event(json!({"type": "turn_start", "turn": turn})));
            }
            SessionEventData::AssistantChunk { chunk, .. } => {
                if !self.msg_open {
                    self.msg_open = true;
                    self.block_count = 0;
                    out.push(event(json!({
                        "type": "message_start",
                        "message": {"role": "assistant", "content": []},
                    })));
                }
                match &chunk.delta {
                    ChunkDelta::Text { text } if !text.is_empty() => {
                        if !self.text_open {
                            self.text_open = true;
                            self.text_index = self.block_count;
                            out.push(event(json!({
                                "type": "message_update",
                                "assistantMessageEvent": {
                                    "type": "text_start",
                                    "contentIndex": self.text_index,
                                },
                            })));
                        }
                        self.text_buf.push_str(text);
                        out.push(event(json!({
                            "type": "message_update",
                            "assistantMessageEvent": {
                                "type": "text_delta",
                                "contentIndex": self.text_index,
                                "delta": text,
                            },
                        })));
                    }
                    ChunkDelta::ToolUse { call } => {
                        self.close_text(&mut out);
                        // opencode 适配器在流尾一次性合成完整调用 → start + end 连续发出
                        out.push(event(json!({
                            "type": "message_update",
                            "assistantMessageEvent": {
                                "type": "toolcall_start",
                                "contentIndex": self.block_count,
                            },
                        })));
                        out.push(event(json!({
                            "type": "message_update",
                            "assistantMessageEvent": {
                                "type": "toolcall_end",
                                "contentIndex": self.block_count,
                                "toolCall": tool_call_to_json(call),
                            },
                        })));
                        self.block_count += 1;
                    }
                    _ => {}
                }
            }
            SessionEventData::AssistantMessage { message, .. } => {
                self.close_text(&mut out);
                self.msg_open = false;
                out.push(event(json!({
                    "type": "message_end",
                    "message": {
                        "role": "assistant",
                        "content": message.content.iter().map(block_to_json).collect::<Vec<_>>(),
                    },
                })));
            }
            SessionEventData::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } => {
                self.tool_names.insert(call_id.clone(), name.clone());
                out.push(event(json!({
                    "type": "tool_execution_start",
                    "toolCallId": call_id,
                    "toolName": name,
                    "args": parse_args(arguments),
                })));
            }
            SessionEventData::ToolResult {
                call_id,
                message,
                error,
                ..
            } => {
                let mut value = json!({
                    "type": "tool_execution_end",
                    "toolCallId": call_id,
                    "result": {
                        "content": [{"type": "text", "text": message.content}],
                        "details": {},
                    },
                    "isError": error.is_some(),
                });
                if let Some(name) = self.tool_names.get(call_id) {
                    value["toolName"] = json!(name);
                }
                out.push(event(value));
            }
            SessionEventData::TurnEnd { turn, reason } => {
                let mut value = json!({
                    "type": "turn_end",
                    "turn": turn,
                    "reason": reason_kind(reason),
                });
                if let TurnEndReason::Error { message } = reason {
                    value["message"] = json!(message);
                }
                out.push(event(value));
                // 近似：cos 一轮 = pi 一次低层 run（无自动重试/排队续跑），直接收束
                out.push(event(
                    json!({"type": "agent_end", "messages": [], "willRetry": false}),
                ));
                out.push(event(json!({"type": "agent_settled"})));
            }
            _ => {}
        }
        out
    }

    /// 关闭当前打开的文本块（text_end + 块计数推进）。
    fn close_text(&mut self, out: &mut Vec<String>) {
        if self.text_open {
            self.text_open = false;
            let content = std::mem::take(&mut self.text_buf);
            out.push(event(json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "text_end",
                    "contentIndex": self.text_index,
                    "content": content,
                },
            })));
            self.block_count += 1;
        }
    }
}

/// turn 结束原因 → pi 风格字符串。
fn reason_kind(reason: &TurnEndReason) -> &'static str {
    match reason {
        TurnEndReason::Completed => "completed",
        TurnEndReason::Aborted { .. } => "aborted",
        TurnEndReason::Blocked => "blocked",
        TurnEndReason::Error { .. } => "error",
        TurnEndReason::MaxTokens => "maxTokens",
        TurnEndReason::Interrupted => "interrupted",
    }
}

/// 序列化单条事件行。
fn event(value: Value) -> String {
    serde_json::to_string(&value).expect("事件可序列化")
}

/// 写一行 JSON 响应（换行结尾 + flush）。
async fn write_line<W: AsyncWrite + Unpin>(
    writer: &Mutex<W>,
    value: &Value,
) -> Result<(), AppError> {
    let mut writer = writer.lock().await;
    writer
        .write_all(serde_json::to_string(value).unwrap().as_bytes())
        .await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use cos_session::SessionEventData;
    use serde_json::{Value, json};

    use super::dispatch;
    use crate::{RunConfig, assemble};

    fn base_config() -> RunConfig {
        RunConfig {
            config_path: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo.yml").into(),
            dump_config: false,
            session_id: "rpc-lib".into(),
            prompt: None,
            session_path: None,
            cancel: None,
            llm: None,
            agent_llm: None,
        }
    }

    /// 重复 id 排队 → fail loud；不同 id 正常排队并可取消。
    /// 确定性：连续同步 dispatch，驱动器未调度，消息都还在队列中。
    #[tokio::test]
    async fn rpc_rejects_duplicate_queued_message_id() {
        let assembled = assemble(&base_config()).await.unwrap();
        let next_id = AtomicU64::new(0);
        let cmd = |value: Value| {
            dispatch(
                value["type"].as_str().unwrap(),
                &value,
                &assembled,
                &next_id,
            )
        };

        // 第一条（idle → 立即处理，但驱动器未调度，仍在队列）
        let r1 = cmd(json!({"id": "dup", "type": "prompt", "message": "任务A"}));
        assert_eq!(r1["success"], true);
        assert_eq!(r1["data"]["messageId"], "dup");

        // 同一 id 再排队 → 拒绝（fail loud）
        let r2 = cmd(json!({
            "id": "dup",
            "type": "prompt",
            "message": "任务B",
            "streamingBehavior": "followUp"
        }));
        assert_eq!(r2["success"], false, "{r2}");
        assert!(
            r2["error"].as_str().unwrap().contains("已存在于队列"),
            "{r2}"
        );

        // 不同 id → 正常排队
        let r3 = cmd(json!({
            "id": "ok",
            "type": "prompt",
            "message": "任务C",
            "streamingBehavior": "followUp"
        }));
        assert_eq!(r3["success"], true);
        assert_eq!(r3["data"]["messageId"], "ok");

        // 取消排队的 ok → 最终只有任务A 被处理
        let r4 = cmd(json!({"type": "cancel_message", "messageId": "ok"}));
        assert_eq!(r4["success"], true);
        assert_eq!(r4["data"]["cancelled"], true);

        assembled.agent.when_idle().await;
        let texts: Vec<String> = assembled
            .agent
            .session()
            .events()
            .iter()
            .filter_map(|event| match &event.data {
                SessionEventData::UserMessage(message) => Some(message.content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["任务A"],
            "重复 id 被拒、ok 被取消 → 只剩任务A: {texts:?}"
        );
    }
}
