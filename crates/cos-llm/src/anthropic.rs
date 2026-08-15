//! Anthropic Messages API 风格适配器（api style: `"anthropic"`）——POST `{base}/messages`。
//!
//! 协议要点：`system` 走顶层字段（无 system role）；`messages` 里工具结果在 **user**
//! 角色（`tool_result` 块）；`max_tokens` 必填（缺省 4096）；流式 SSE 事件
//! （`content_block_start` / `content_block_delta` / `content_block_stop` /
//! `message_delta` / `message_stop`），推理与正文不分流（Anthropic 标准 API 无
//! `reasoning_content`）；非流式返回 `content` 块数组（text / tool_use）。错误：HTTP
//! 状态按 [`crate::classify_status`] 分类；SSE `error` 事件按 `error.type` 分类
//! （authentication_error → Auth、rate_limit_error → RateLimit、
//! invalid_request_error → InvalidRequest、其余 → Server）。工具 schema：接受 OpenAI
//! 形状（`function.parameters`）自动转 `input_schema`，已是 anthropic 形状原样透传。
//! 输入内容能力暂为 text（图片块为扩展点）。流式在未产出前遇**可重试失败**自动
//! 非流式兜底（与 openai 风格一致）。
//!
//! 路径后缀：`{base_url}/messages`（base_url 含 `/v1` 前缀，如
//! `https://opencode.ai/zen/go/v1`）。

use std::sync::Arc;

use crate::{
    ChunkDelta, ContentBlock, FinishReason, InputContent, LlmAdapter, LlmError, LlmErrorCode,
    LlmRequest, LlmStream, Message, StreamChunk, TokenUsage, ToolCall, classify_status, truncate,
};
use futures::StreamExt;
use serde::Deserialize;

/// Anthropic 风格适配器工厂构建函数（经 [`crate::build_with_style`] 按
/// `api_style: "anthropic"` 分发）：配置 `{base_url, api_key, model, streaming?,
/// max_tokens?, input_content?}`（三级默认合并已完成）。
pub fn build_anthropic(config: &serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError> {
    /// 提供商配置（插件 yml 里的 `config` 段）。
    #[derive(Deserialize)]
    struct ProviderConfig {
        base_url: String,
        api_key: String,
        model: String,
        #[serde(default = "default_streaming")]
        streaming: bool,
        /// 输出预算（Anthropic 必填；缺省 4096）。
        #[serde(default = "default_max_tokens")]
        max_tokens: Option<u32>,
        /// 可输入内容标注（缺省 text）。
        #[serde(default)]
        input_content: Vec<InputContent>,
    }
    fn default_streaming() -> bool {
        // 网关流式不稳定时非流式最稳（同 openai 风格缺省）
        false
    }
    fn default_max_tokens() -> Option<u32> {
        Some(4096)
    }
    let config: ProviderConfig = serde_json::from_value(config.clone()).map_err(|error| {
        LlmError::new(
            LlmErrorCode::InvalidRequest,
            format!("anthropic 提供商配置无效: {error}"),
        )
    })?;
    let input_content = if config.input_content.is_empty() {
        vec![InputContent::Text]
    } else {
        config.input_content
    };
    Ok(Arc::new(AnthropicAdapter::new(AnthropicConfig {
        base_url: config.base_url,
        api_key: config.api_key,
        model: config.model,
        streaming: config.streaming,
        max_tokens: config.max_tokens,
        input_content,
    })))
}

/// Anthropic 风格适配器配置。
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    /// base URL（含 `/v1` 前缀；适配器追加 `/messages`）。
    pub base_url: String,
    /// API key（`x-api-key` 头）。
    pub api_key: String,
    /// 模型 id。
    pub model: String,
    /// 是否流式（SSE 事件）；false = 单次请求。
    pub streaming: bool,
    /// 输出预算（Anthropic 必填；None → 4096）。
    pub max_tokens: Option<u32>,
    /// 可输入内容标注（`input_content()` 依据）。
    pub input_content: Vec<InputContent>,
}

/// Anthropic 风格适配器（流式优先、可重试失败自动非流式兜底）。
#[derive(Clone)]
pub struct AnthropicAdapter {
    config: AnthropicConfig,
    client: reqwest::Client,
}

/// SSE 事件负载（共用形状：`type` 分类 + 可选 index/delta/content_block/usage）。
#[derive(Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    delta: Option<serde_json::Value>,
    #[serde(default)]
    content_block: Option<serde_json::Value>,
    #[serde(default)]
    usage: Option<SseUsage>,
    #[serde(default)]
    error: Option<SseError>,
}

/// usage 块（message_delta / 非流式共用）。
#[derive(Deserialize)]
struct SseUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// error 事件负载（error.type 分类用）。
#[derive(Deserialize)]
struct SseError {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: String,
}

/// 非流式响应（content 块数组 + usage + stop_reason）。
#[derive(Deserialize)]
struct MessagesCompletion {
    #[serde(default)]
    content: Vec<CompletionBlock>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

/// 非流式 content 块。
#[derive(Deserialize)]
struct CompletionBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// 把工具 schema 转 anthropic 形状：已是 `input_schema` 形状原样；OpenAI 形状
/// （`function.parameters`）转 `{name, description, input_schema}`；其余原样透传。
fn to_anthropic_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            if tool.get("input_schema").is_some() {
                return tool.clone();
            }
            if let Some(function) = tool.get("function") {
                return serde_json::json!({
                    "name": function.get("name").cloned().unwrap_or(serde_json::Value::Null),
                    "description": function
                        .get("description")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "input_schema": function
                        .get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                });
            }
            tool.clone()
        })
        .collect()
}

/// 把工具调用的 JSON 参数字符串转对象（anthropic 要求 `input` 为对象；解析失败 → null）。
fn tool_input(arguments: &str) -> serde_json::Value {
    serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null)
}

impl AnthropicAdapter {
    /// 由配置构造。
    pub fn new(config: AnthropicConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// 当前模型 id。
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// 组装请求体（Anthropic messages 投影 + stream 开关）。
    fn body(&self, request: &LlmRequest, streaming: bool) -> serde_json::Value {
        let mut system: Vec<String> = Vec::new();
        if let Some(s) = &request.system {
            system.push(s.clone());
        }
        let mut messages: Vec<serde_json::Value> = Vec::new();
        for message in &request.messages {
            match message {
                // anthropic 无 system role：并入顶层 system 字段
                Message::System { content } => system.push(content.clone()),
                Message::User(user) => {
                    // 图片块为扩展点（input_content 仍标注 text 时降级为纯文本）
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": user.content,
                    }));
                }
                Message::Assistant(assistant) => {
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    let text = assistant.text();
                    if !text.is_empty() {
                        blocks.push(serde_json::json!({ "type": "text", "text": text }));
                    }
                    for block in &assistant.content {
                        if let ContentBlock::ToolUse { call } = block {
                            blocks.push(serde_json::json!({
                                "type": "tool_use",
                                "id": call.call_id,
                                "name": call.name,
                                "input": tool_input(&call.arguments),
                            }));
                        }
                    }
                    // anthropic 要求 assistant content 非空
                    if blocks.is_empty() {
                        blocks.push(serde_json::json!({ "type": "text", "text": "" }));
                    }
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
                Message::Tool(tool) => {
                    // 工具结果在 user 角色（tool_result 块）
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": tool.call_id.clone().unwrap_or_default(),
                            "content": tool.content,
                        }],
                    }));
                }
                Message::Custom { name, data } => messages.push(serde_json::json!({
                    "role": "user",
                    "content": format!("[{name}]\n{data}"),
                })),
            }
        }
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": self.config.max_tokens.unwrap_or(4096),
            "stream": streaming,
        });
        if !system.is_empty() {
            body["system"] = serde_json::json!(system.join("\n"));
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(to_anthropic_tools(&request.tools));
        }
        body
    }

    /// 构造一次 POST 请求（URL 非法在此失败）。
    fn build_request(
        &self,
        request: &LlmRequest,
        streaming: bool,
    ) -> Result<reqwest::Request, LlmError> {
        let url = format!("{}/messages", self.config.base_url.trim_end_matches('/'));
        self.client
            .post(&url)
            .header("x-api-key", &self.config.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.body(request, streaming))
            .build()
            .map_err(|error| {
                LlmError::new(
                    LlmErrorCode::InvalidRequest,
                    format!("请求构造失败: {error}"),
                )
            })
    }
}

impl LlmAdapter for AnthropicAdapter {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn input_content(&self) -> &[InputContent] {
        &self.config.input_content
    }

    fn stream(&self, request: &LlmRequest) -> LlmStream {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let client = self.client.clone();
        if !self.config.streaming {
            // 非流式模式：直接单次请求
            let single_req = match self.build_request(request, false) {
                Ok(req) => req,
                Err(error) => {
                    return Box::pin(futures::stream::once(async move { Err(error) }));
                }
            };
            tokio::spawn(async move {
                if let Err(error) = anthropic_single(&client, single_req, &tx).await {
                    let _ = tx.send(Err(error));
                }
            });
            return Box::pin(futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }));
        }
        let streaming_req = match self.build_request(request, true) {
            Ok(req) => req,
            Err(error) => {
                return Box::pin(futures::stream::once(async move { Err(error) }));
            }
        };
        let single_req = match self.build_request(request, false) {
            Ok(req) => req,
            Err(error) => {
                return Box::pin(futures::stream::once(async move { Err(error) }));
            }
        };
        tokio::spawn(async move {
            // 错误不进 stderr：一律作为流内 Err 交付给消费方
            if let Err(error) = anthropic_run(client, streaming_req, single_req, tx.clone()).await {
                let _ = tx.send(Err(error));
            }
        });
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// 后台任务：流式优先；**可重试失败**（服务端/限流/网络/未分类）且尚无产出 → 非流式兜底。
async fn anthropic_run(
    client: reqwest::Client,
    streaming_req: reqwest::Request,
    single_req: reqwest::Request,
    tx: tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
) -> Result<(), LlmError> {
    let mut sent = 0usize;
    match anthropic_stream(&client, streaming_req, &tx, &mut sent).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if sent == 0 && error.is_retryable() {
                match anthropic_single(&client, single_req, &tx).await {
                    Ok(()) => Ok(()),
                    Err(fallback) => Err(LlmError {
                        code: fallback.code,
                        message: format!("{}（非流式兜底同样失败）", fallback.message),
                        facts: fallback.facts,
                    }),
                }
            } else {
                Err(error)
            }
        }
    }
}

/// 流式执行：SSE 事件逐条解析并转发。
async fn anthropic_stream(
    client: &reqwest::Client,
    req: reqwest::Request,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
    sent: &mut usize,
) -> Result<(), LlmError> {
    let response = client
        .execute(req)
        .await
        .map_err(|error| LlmError::new(LlmErrorCode::Network, format!("请求失败: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(classify_status(status.as_u16(), &text));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut eof = false;
    // 工具调用：按 content 块 index 归组（id/name 在 start，partial_json 在 delta 累积）
    let mut pending: std::collections::BTreeMap<usize, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut has_tools = false;
    let mut usage_tail: Option<TokenUsage> = None;
    loop {
        if let Some(pos) = buffer.find('\n') {
            let line: String = buffer[..pos].trim_end_matches('\r').to_string();
            buffer.drain(..=pos);
            // event: 头行忽略（分类以 data 负载的 type 字段为准）
            if line.starts_with("event:") {
                continue;
            }
            let Some(data) = line.strip_prefix("data:") else {
                continue; // 空行/注释行忽略
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            let payload: SseEvent = match serde_json::from_str(data) {
                Ok(payload) => payload,
                Err(_) => continue, // ping 等非事件行忽略
            };
            match payload.kind.as_str() {
                "content_block_start" => {
                    if let Some(block) = &payload.content_block
                        && block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                        && let Some(index) = payload.index
                    {
                        let id = block
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        pending.insert(index, (id, name, String::new()));
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = &payload.delta {
                        let delta_type = delta.get("type").and_then(|t| t.as_str());
                        if delta_type == Some("text_delta") {
                            let text = delta
                                .get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            *sent += 1;
                            if !text.is_empty()
                                && tx
                                    .send(Ok(StreamChunk {
                                        delta: ChunkDelta::Text { text },
                                        usage: None,
                                    }))
                                    .is_err()
                            {
                                return Ok(()); // 接收方已丢弃（提前取消）
                            }
                        } else if delta_type == Some("input_json_delta")
                            && let Some(index) = payload.index
                            && let Some(slot) = pending.get_mut(&index)
                            && let Some(partial) =
                                delta.get("partial_json").and_then(|v| v.as_str())
                        {
                            slot.2.push_str(partial);
                        }
                    }
                }
                "content_block_stop" => {
                    if let Some(index) = payload.index
                        && let Some((call_id, name, arguments)) = pending.remove(&index)
                    {
                        has_tools = true;
                        *sent += 1;
                        let _ = tx.send(Ok(StreamChunk {
                            delta: ChunkDelta::ToolUse {
                                call: ToolCall {
                                    call_id,
                                    name,
                                    arguments,
                                },
                            },
                            usage: None,
                        }));
                    }
                }
                "message_delta" => {
                    if let Some(usage) = payload.usage {
                        usage_tail = Some(TokenUsage {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                        });
                    }
                }
                "message_stop" => {
                    // 终结分片：显式结束原因
                    let _ = tx.send(Ok(StreamChunk {
                        delta: ChunkDelta::Finish {
                            reason: if has_tools {
                                FinishReason::ToolCalls
                            } else {
                                FinishReason::Stop
                            },
                        },
                        usage: usage_tail,
                    }));
                    return Ok(());
                }
                "error" => {
                    return Err(classify_anthropic_error(payload.error.as_ref()));
                }
                _ => {} // message_start / ping 等忽略
            }
        } else if eof {
            if !buffer.is_empty() {
                buffer.push('\n');
                continue;
            }
            // 流结束但未见 message_stop（异常）：交付错误
            return Err(LlmError::new(
                LlmErrorCode::Protocol,
                "anthropic 流提前结束（未见 message_stop）",
            ));
        } else {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Some(Err(error)) => {
                    return Err(LlmError::new(
                        LlmErrorCode::Network,
                        format!("流读取失败: {error}"),
                    ));
                }
                None => eof = true,
            }
        }
    }
}

/// 非流式兜底：单次请求，content 块数组合成 chunk 序列。
async fn anthropic_single(
    client: &reqwest::Client,
    req: reqwest::Request,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
) -> Result<(), LlmError> {
    let response = client
        .execute(req)
        .await
        .map_err(|error| LlmError::new(LlmErrorCode::Network, format!("请求失败: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(classify_status(status.as_u16(), &text));
    }
    let raw = response
        .text()
        .await
        .map_err(|error| LlmError::new(LlmErrorCode::Network, format!("响应读取失败: {error}")))?;
    let completion: MessagesCompletion = serde_json::from_str(&raw).map_err(|error| {
        LlmError::new(
            LlmErrorCode::Protocol,
            format!("非流式响应不是合法 JSON: {error}"),
        )
    })?;
    // content 块 → 文本块 / 工具块（保持顺序）；usage 挂最后一块
    let mut chunks: Vec<Result<StreamChunk, LlmError>> = Vec::new();
    let mut has_tools = false;
    for block in &completion.content {
        match block.kind.as_str() {
            "text" => {
                if let Some(text) = &block.text {
                    chunks.push(Ok(StreamChunk {
                        delta: ChunkDelta::Text { text: text.clone() },
                        usage: None,
                    }));
                }
            }
            "tool_use" => {
                has_tools = true;
                chunks.push(Ok(StreamChunk {
                    delta: ChunkDelta::ToolUse {
                        call: ToolCall {
                            call_id: block.id.clone().unwrap_or_default(),
                            name: block.name.clone().unwrap_or_default(),
                            arguments: serde_json::to_string(&block.input)
                                .unwrap_or_else(|_| "null".to_string()),
                        },
                    },
                    usage: None,
                }));
            }
            _ => {} // thinking 等扩展块暂跳过
        }
    }
    let usage = completion.usage.map(|usage| TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    });
    if let Some(Ok(last)) = chunks.last_mut() {
        last.usage = usage;
    }
    // 终结分片
    chunks.push(Ok(StreamChunk {
        delta: ChunkDelta::Finish {
            reason: if has_tools {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            },
        },
        usage: None,
    }));
    for chunk in chunks {
        if tx.send(chunk).is_err() {
            return Ok(()); // 接收方已丢弃（提前取消）
        }
    }
    Ok(())
}

/// 按 anthropic `error.type` 分类（无 HTTP status 事实）。
fn classify_anthropic_error(error: Option<&SseError>) -> LlmError {
    let Some(error) = error else {
        return LlmError::new(
            LlmErrorCode::Server,
            "anthropic 服务端错误（无 error 细节）",
        );
    };
    let code = match error.kind.as_str() {
        "authentication_error" | "permission_error" => LlmErrorCode::Auth,
        "rate_limit_error" => LlmErrorCode::RateLimit,
        "invalid_request_error" => LlmErrorCode::InvalidRequest,
        "overloaded_error" | "api_error" | "internal_server_error" => LlmErrorCode::Server,
        _ => LlmErrorCode::Other,
    };
    let message = if error.message.is_empty() {
        format!("anthropic 服务端错误: {}", error.kind)
    } else {
        format!("anthropic 服务端错误: {}", truncate(&error.message, 300))
    };
    LlmError::new(code, message)
}
