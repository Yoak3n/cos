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
use cos_llm::{ChunkDelta, ContentBlock, Message, TokenUsage, ToolCall, UserMessage};
use cos_session::{SessionEvent, SessionEventData, TurnEndReason};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::{AppError, Assembled, wait_for_cancel};

mod command;
use command::Command;

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
    let mut forwarder = EventForwarder::new(
        assembled.agent.options().provider.clone(),
        assembled.agent.options().model.clone(),
    );
    tokio::spawn(async move {
        loop {
            let events = session.events_after(seen);
            for event in &events {
                seen = event.seq;
                for line in forwarder.on_event(event) {
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
                    &response(
                        "parse",
                        None,
                        false,
                        None,
                        Some(format!("Failed to parse command: {error}")),
                    ),
                )
                .await?;
                continue;
            }
        };
        let Some(command) = Command::parse(&request) else {
            // 未知命令：回显原始 type 字符串（协议兼容失败响应）
            let unknown = request
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            write_line(
                &writer,
                &response(
                    &unknown,
                    request.get("id").cloned(),
                    false,
                    None,
                    Some(format!("未知命令: {unknown}")),
                ),
            )
            .await?;
            continue;
        };
        let response = dispatch(command, &request, assembled, &next_id);
        write_line(&writer, &response).await?;
        if command == Command::Exit {
            return Ok(());
        }
    }
}

/// pi 响应信封：`type: response` + `command` + `success` + `data?`/`error?`；
/// `id` 可选、原样回显（缺省省略该字段）。`dispatch` 与 serve 循环的错误路径共用。
fn response(
    command: &str,
    id: Option<Value>,
    success: bool,
    data: Option<Value>,
    error: Option<String>,
) -> Value {
    let mut value = json!({
        "type": "response",
        "command": command,
        "success": success,
    });
    if let Some(id) = id {
        value["id"] = id;
    }
    if let Some(data) = data {
        value["data"] = data;
    }
    if let Some(error) = error {
        value["error"] = json!(error);
    }
    value
}

/// 命令分发：pi 响应信封（`type: response` + `command` + `success` + `data?`/`error?`）。
fn dispatch(
    command: Command,
    request: &Value,
    assembled: &Assembled,
    next_id: &AtomicU64,
) -> Value {
    let id = request.get("id").cloned();
    let respond = |success: bool, data: Option<Value>, error: Option<String>| {
        response(command.name(), id.clone(), success, data, error)
    };
    let agent = &assembled.agent;
    match command {
        Command::Prompt => {
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
        Command::Steer => {
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
        Command::FollowUp => {
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
        Command::CancelMessage => {
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
        Command::Abort => {
            // pi 语义：中止当前操作、保留已排队消息
            agent.cancel(cos_session::AbortCause::User, true);
            respond(true, None, None)
        }
        Command::GetState => {
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
        Command::GetMessages => {
            let messages: Vec<Value> = agent
                .session()
                .derive_messages()
                .iter()
                .map(message_to_json)
                .collect();
            respond(true, Some(json!({"messages": messages})), None)
        }
        Command::GetLastAssistantText => {
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
        Command::GetSessionStats => {
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
        Command::GetCommands => respond(true, Some(json!({"commands": []})), None),
        Command::Exit => respond(true, None, None),
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

/// 内容块 → pi 风格 JSON（get_messages）。
fn block_to_json(block: &ContentBlock) -> Value {
    match block {
        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ContentBlock::Thinking { text } => json!({
            "type": "thinking",
            "thinking": text,
            "thinkingSignature": "reasoning_content",
        }),
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
///
/// 流式装配：message_update 除增量外携带 `partial`/`message`（**内部拼接好的**
/// 累积消息快照——content 按块累积：thinking / text / toolCall 分开），客户端
/// 直接展示快照即可，无需自行拼增量。推理（thinking_*）与正文（text_*）区分流式。
struct EventForwarder {
    /// 提供商名（消息快照元数据）。
    provider: Option<String>,
    /// 模型 id（消息快照元数据）。
    model: Option<String>,
    /// 当前 step 的 assistant 消息是否已开（message_start 已发）。
    msg_open: bool,
    /// 已装配的内容块（最后一个可能是未闭合的 text/thinking 块）。
    blocks: Vec<OpenBlock>,
    /// 最后见到的 usage（chunk 或 assistant/message 事件）。
    usage: Option<TokenUsage>,
    /// message_start 的时间戳（Unix epoch 毫秒）。
    started_ms: u64,
    /// call_id → 工具名（tool_execution_end 需要；ToolResult 事件不带 name）。
    tool_names: HashMap<String, String>,
}

/// 流式装配中的内容块（最后一个 Text/Thinking 即"打开"的块，随增量累积）。
#[derive(Clone)]
enum OpenBlock {
    /// 文本块。
    Text(String),
    /// 推理块。
    Thinking(String),
    /// 工具调用块（一次到位，天然闭合）。
    ToolUse(ToolCall),
}

impl EventForwarder {
    fn new(provider: Option<String>, model: Option<String>) -> Self {
        Self {
            provider,
            model,
            msg_open: false,
            blocks: Vec::new(),
            usage: None,
            started_ms: 0,
            tool_names: HashMap::new(),
        }
    }

    fn on_event(&mut self, entry: &SessionEvent) -> Vec<String> {
        let mut out = Vec::new();
        match &entry.data {
            SessionEventData::TurnStart { turn } => {
                out.push(event(json!({"type": "agent_start"})));
                out.push(event(json!({"type": "turn_start", "turn": turn})));
            }
            SessionEventData::AssistantChunk { chunk, .. } => {
                if !self.msg_open {
                    self.msg_open = true;
                    self.blocks.clear();
                    self.usage = None;
                    self.started_ms = entry.time;
                    out.push(event(json!({
                        "type": "message_start",
                        "message": self.message_json("pending"),
                    })));
                }
                match &chunk.delta {
                    ChunkDelta::Text { text } if !text.is_empty() => {
                        let index = self.open_block(OpenBlockKind::Text, &mut out);
                        self.push_delta(&mut out, "text_delta", index, text);
                    }
                    ChunkDelta::Thinking { text } if !text.is_empty() => {
                        let index = self.open_block(OpenBlockKind::Thinking, &mut out);
                        self.push_delta(&mut out, "thinking_delta", index, text);
                    }
                    ChunkDelta::ToolUse { call } => {
                        self.close_open(&mut out);
                        let index = self.blocks.len();
                        self.blocks.push(OpenBlock::ToolUse(call.clone()));
                        // opencode 适配器在流尾一次性合成完整调用 → start + end 连续发出
                        let mut start = json!({
                            "type": "toolcall_start",
                            "contentIndex": index,
                        });
                        start["partial"] = self.message_json("pending");
                        start["message"] = self.message_json("pending");
                        out.push(event(json!({
                            "type": "message_update",
                            "assistantMessageEvent": start,
                        })));
                        let mut end = json!({
                            "type": "toolcall_end",
                            "contentIndex": index,
                            "toolCall": tool_call_to_json(call),
                        });
                        end["partial"] = self.message_json("pending");
                        end["message"] = self.message_json("pending");
                        out.push(event(json!({
                            "type": "message_update",
                            "assistantMessageEvent": end,
                        })));
                    }
                    _ => {}
                }
                if let Some(usage) = chunk.usage {
                    self.usage = Some(usage);
                }
            }
            SessionEventData::AssistantMessage { usage, .. } => {
                self.close_open(&mut out);
                self.msg_open = false;
                if let Some(usage) = usage {
                    self.usage = Some(*usage);
                }
                let stop_reason = if self
                    .blocks
                    .iter()
                    .any(|block| matches!(block, OpenBlock::ToolUse(_)))
                {
                    "toolUse"
                } else {
                    "stop"
                };
                out.push(event(json!({
                    "type": "message_end",
                    "message": self.message_json(stop_reason),
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

    /// 打开（或续接）一个 text/thinking 块：返回其 contentIndex；
    /// 类型切换时先闭合当前块（发对应的 *_end）。
    fn open_block(&mut self, kind: OpenBlockKind, out: &mut Vec<String>) -> usize {
        let already_open = matches!(
            (self.blocks.last(), kind),
            (Some(OpenBlock::Text(_)), OpenBlockKind::Text)
                | (Some(OpenBlock::Thinking(_)), OpenBlockKind::Thinking)
        );
        if already_open {
            return self.blocks.len() - 1;
        }
        self.close_open(out);
        let index = self.blocks.len();
        self.blocks.push(match kind {
            OpenBlockKind::Text => OpenBlock::Text(String::new()),
            OpenBlockKind::Thinking => OpenBlock::Thinking(String::new()),
        });
        let start_kind = match kind {
            OpenBlockKind::Text => "text_start",
            OpenBlockKind::Thinking => "thinking_start",
        };
        let mut start = json!({"type": start_kind, "contentIndex": index});
        start["partial"] = self.message_json("pending");
        start["message"] = self.message_json("pending");
        out.push(event(json!({
            "type": "message_update",
            "assistantMessageEvent": start,
        })));
        index
    }

    /// 追加增量到打开的块，并回显累积快照（partial/message）。
    fn push_delta(&mut self, out: &mut Vec<String>, kind: &str, index: usize, delta: &str) {
        match self.blocks.get_mut(index) {
            Some(OpenBlock::Text(text)) | Some(OpenBlock::Thinking(text)) => text.push_str(delta),
            _ => {}
        }
        let mut update = json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": kind,
                "contentIndex": index,
                "delta": delta,
            },
        });
        update["assistantMessageEvent"]["partial"] = self.message_json("pending");
        update["assistantMessageEvent"]["message"] = self.message_json("pending");
        out.push(event(update));
    }

    /// 闭合当前打开的 text/thinking 块（发 *_end + 回显累积内容）。
    fn close_open(&mut self, out: &mut Vec<String>) {
        let Some(last) = self.blocks.last() else {
            return;
        };
        let (end_kind, index, content) = match last {
            OpenBlock::Text(text) => ("text_end", self.blocks.len() - 1, text.clone()),
            OpenBlock::Thinking(text) => ("thinking_end", self.blocks.len() - 1, text.clone()),
            OpenBlock::ToolUse(_) => return,
        };
        let mut end = json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": end_kind,
                "contentIndex": index,
                "content": content,
            },
        });
        end["assistantMessageEvent"]["partial"] = self.message_json("pending");
        end["assistantMessageEvent"]["message"] = self.message_json("pending");
        out.push(event(end));
    }

    /// 当前累积消息快照（pi AgentMessage 形状；stopReason 由调用方给定）。
    fn message_json(&self, stop_reason: &str) -> Value {
        let content: Vec<Value> = self.blocks.iter().map(open_block_to_json).collect();
        json!({
            "role": "assistant",
            "content": content,
            "api": "openai-completions",
            "provider": self.provider,
            "model": self.model,
            "usage": usage_json(self.usage),
            "stopReason": stop_reason,
            "timestamp": self.started_ms,
        })
    }
}

/// 打开块类型。
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenBlockKind {
    Text,
    Thinking,
}

/// 流式装配块 → pi 内容块 JSON。
fn open_block_to_json(block: &OpenBlock) -> Value {
    match block {
        OpenBlock::Text(text) => json!({"type": "text", "text": text}),
        OpenBlock::Thinking(text) => json!({
            "type": "thinking",
            "thinking": text,
            "thinkingSignature": "reasoning_content",
        }),
        OpenBlock::ToolUse(call) => tool_call_to_json(call),
    }
}

/// usage → pi 形状（无用量全零；cost 未核算恒零）。
fn usage_json(usage: Option<TokenUsage>) -> Value {
    let (input, output) = usage
        .map(|u| (u.input_tokens, u.output_tokens))
        .unwrap_or((0, 0));
    json!({
        "input": input,
        "output": output,
        "cacheRead": 0,
        "cacheWrite": 0,
        "totalTokens": input + output,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
    })
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

    use cos_llm::{AssistantMessage, ChunkDelta, ContentBlock, StreamChunk, TokenUsage};
    use cos_session::{Session, SessionEventData, TurnEndReason};
    use serde_json::{Value, json};

    use super::{Command, EventForwarder, dispatch};
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
            let command = Command::parse(&value).expect("测试命令应可解析");
            dispatch(command, &value, &assembled, &next_id)
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

    /// 转发器内部装配：thinking 与 text 分开成块、增量事件携带累积快照
    /// （partial/message），message_end 带 usage 与 stopReason。
    #[test]
    fn forwarder_assembles_thinking_and_text_with_partials() {
        let mut forwarder =
            EventForwarder::new(Some("opencode-go".into()), Some("deepseek-v4-flash".into()));
        let session = Session::new("t");
        let chunk = |delta: ChunkDelta| cos_session::SessionEventData::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: StreamChunk { delta, usage: None },
        };
        let events = [
            session.append(SessionEventData::TurnStart { turn: 1 }),
            session.append(chunk(ChunkDelta::Thinking { text: "想".into() })),
            session.append(chunk(ChunkDelta::Thinking { text: "考".into() })),
            session.append(chunk(ChunkDelta::Text {
                text: "你好".into(),
            })),
            session.append(SessionEventData::AssistantMessage {
                turn: 1,
                step: 1,
                message: AssistantMessage::new(vec![
                    ContentBlock::Thinking {
                        text: "思考".into(),
                    },
                    ContentBlock::Text {
                        text: "你好".into(),
                    },
                ]),
                usage: Some(TokenUsage {
                    input_tokens: 5,
                    output_tokens: 2,
                }),
            }),
            session.append(SessionEventData::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            }),
        ];
        let lines: Vec<Value> = events
            .iter()
            .flat_map(|event| forwarder.on_event(event))
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect();

        let types: Vec<&str> = lines.iter().map(|l| l["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            vec![
                "agent_start",
                "turn_start",
                "message_start",
                "message_update", // thinking_start
                "message_update", // thinking_delta 想
                "message_update", // thinking_delta 考
                "message_update", // thinking_end
                "message_update", // text_start
                "message_update", // text_delta 你好
                "message_update", // text_end
                "message_end",
                "turn_end",
                "agent_end",
                "agent_settled",
            ]
        );

        // message_start：完整元数据 + 空 content
        assert_eq!(lines[2]["message"]["api"], "openai-completions");
        assert_eq!(lines[2]["message"]["provider"], "opencode-go");
        assert_eq!(lines[2]["message"]["model"], "deepseek-v4-flash");
        assert_eq!(lines[2]["message"]["stopReason"], "pending");
        assert_eq!(lines[2]["message"]["content"].as_array().unwrap().len(), 0);

        // 增量事件携带累积快照：第二个 thinking_delta 已拼入 "想"+"考"
        let second_thinking = &lines[5]["assistantMessageEvent"];
        assert_eq!(second_thinking["type"], "thinking_delta");
        assert_eq!(second_thinking["delta"], "考");
        let partial = &second_thinking["partial"];
        assert_eq!(partial["content"][0]["type"], "thinking");
        assert_eq!(partial["content"][0]["thinking"], "想考");
        assert_eq!(
            second_thinking["message"], *partial,
            "message 与 partial 同值"
        );

        // text_delta 时快照含 thinking + text 两块
        let text_delta = &lines[8]["assistantMessageEvent"];
        assert_eq!(text_delta["type"], "text_delta");
        assert_eq!(text_delta["contentIndex"], 1);
        let content = &text_delta["partial"]["content"];
        assert_eq!(content.as_array().unwrap().len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "你好");

        // message_end：两块 + usage + stopReason stop
        let message = &lines[10]["message"];
        assert_eq!(message["content"].as_array().unwrap().len(), 2);
        assert_eq!(message["usage"]["input"], 5);
        assert_eq!(message["usage"]["output"], 2);
        assert_eq!(message["usage"]["totalTokens"], 7);
        assert_eq!(message["stopReason"], "stop");
        assert!(message["timestamp"].is_u64());
    }

    /// 纯文本流：无 thinking 事件，只有 text_*。
    #[test]
    fn forwarder_plain_text_has_no_thinking_events() {
        let mut forwarder = EventForwarder::new(None, None);
        let session = Session::new("t");
        let events = [
            session.append(SessionEventData::TurnStart { turn: 1 }),
            session.append(SessionEventData::AssistantChunk {
                turn: 1,
                step: 1,
                chunk: StreamChunk::text("好"),
            }),
            session.append(SessionEventData::AssistantMessage {
                turn: 1,
                step: 1,
                message: AssistantMessage::new(vec![ContentBlock::Text { text: "好".into() }]),
                usage: None,
            }),
            session.append(SessionEventData::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            }),
        ];
        let lines: Vec<Value> = events
            .iter()
            .flat_map(|event| forwarder.on_event(event))
            .map(|line| serde_json::from_str(&line).unwrap())
            .collect();
        for line in &lines {
            if line["type"] == "message_update" {
                let kind = line["assistantMessageEvent"]["type"].as_str().unwrap();
                assert!(
                    kind.starts_with("text_"),
                    "纯文本流不应有 thinking 事件: {kind}"
                );
            }
        }
        assert!(lines.iter().any(|l| l["type"] == "message_end"));
    }
}
