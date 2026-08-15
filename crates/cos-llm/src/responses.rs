//! OpenAI Responses API 风格适配器（api style: `"responses"`）——POST `{base}/responses`。
//!
//! 协议要点：`instructions` 顶层（系统提示）；`input` 消息数组（内容块：
//! `input_text` / `output_text` / `function_call` / `function_call_output`）；
//! 工具为扁平形状 `{type: "function", name, parameters}`；流式 SSE 事件
//! （`response.output_item.added` / `response.function_call_arguments.delta` /
//! `response.output_text.delta` / `response.output_item.done` / `response.completed`）；
//! 非流式返回 `output` 数组（message / function_call）。错误：HTTP 状态按
//! [`crate::classify_status`] 分类；SSE `error` 事件 → Server。输入内容能力暂为
//! text。流式在未产出前遇**可重试失败**自动非流式兜底（与 openai 风格一致）。
//!
//! 路径后缀：`{base_url}/responses`（base_url 含 `/v1` 前缀，如
//! `https://opencode.ai/zen/go/v1`）。

use std::sync::Arc;

use crate::{
    ChunkDelta, ContentBlock, FinishReason, InputContent, LlmAdapter, LlmError, LlmErrorCode,
    LlmRequest, LlmStream, Message, StreamChunk, TokenUsage, ToolCall, classify_status,
};
use futures::StreamExt;
use serde::Deserialize;

/// Responses 风格适配器工厂构建函数（经 [`crate::build_with_style`] 按
/// `api_style: "responses"` 分发）：配置 `{base_url, api_key, model, streaming?,
/// max_tokens?, input_content?}`（三级默认合并已完成）。
pub fn build_responses(config: &serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError> {
    /// 提供商配置（插件 yml 里的 `config` 段）。
    #[derive(Deserialize)]
    struct ProviderConfig {
        base_url: String,
        api_key: String,
        model: String,
        #[serde(default = "default_streaming")]
        streaming: bool,
        /// 输出预算（Responses API 可选；缺省 4096）。
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
            format!("responses 提供商配置无效: {error}"),
        )
    })?;
    let input_content = if config.input_content.is_empty() {
        vec![InputContent::Text]
    } else {
        config.input_content
    };
    Ok(Arc::new(ResponsesAdapter::new(ResponsesConfig {
        base_url: config.base_url,
        api_key: config.api_key,
        model: config.model,
        streaming: config.streaming,
        max_tokens: config.max_tokens,
        input_content,
    })))
}

/// Responses 风格适配器配置。
#[derive(Debug, Clone)]
pub struct ResponsesConfig {
    /// base URL（含 `/v1` 前缀；适配器追加 `/responses`）。
    pub base_url: String,
    /// API key（Bearer）。
    pub api_key: String,
    /// 模型 id。
    pub model: String,
    /// 是否流式（SSE 事件）；false = 单次请求。
    pub streaming: bool,
    /// 输出预算（None = 不发送）。
    pub max_tokens: Option<u32>,
    /// 可输入内容标注（`input_content()` 依据）。
    pub input_content: Vec<InputContent>,
}

/// Responses 风格适配器（流式优先、可重试失败自动非流式兜底）。
#[derive(Clone)]
pub struct ResponsesAdapter {
    config: ResponsesConfig,
    client: reqwest::Client,
}

/// SSE 事件负载（`type` 分类 + 可选 output_index/delta/item/response）。
#[derive(Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    output_index: Option<usize>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    item: Option<serde_json::Value>,
    #[serde(default)]
    response: Option<serde_json::Value>,
}

/// 非流式响应（output 数组 + usage）。
#[derive(Deserialize)]
struct ResponsesCompletion {
    #[serde(default)]
    output: Vec<serde_json::Value>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

/// usage 块。
#[derive(Deserialize)]
struct SseUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// 把工具 schema 转 Responses 扁平形状：已是 `{type:"function", name}` 原样；
/// OpenAI chat 形状（`function.parameters`）转扁平；其余原样透传。
fn to_responses_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            if tool.get("name").is_some() {
                return tool.clone();
            }
            if let Some(function) = tool.get("function") {
                return serde_json::json!({
                    "type": "function",
                    "name": function.get("name").cloned().unwrap_or(serde_json::Value::Null),
                    "description": function
                        .get("description")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "parameters": function
                        .get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                });
            }
            tool.clone()
        })
        .collect()
}

impl ResponsesAdapter {
    /// 由配置构造。
    pub fn new(config: ResponsesConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// 当前模型 id。
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// 组装请求体（Responses input 投影 + stream 开关）。
    fn body(&self, request: &LlmRequest, streaming: bool) -> serde_json::Value {
        let mut instructions: Vec<String> = Vec::new();
        if let Some(s) = &request.system {
            instructions.push(s.clone());
        }
        let mut input: Vec<serde_json::Value> = Vec::new();
        for message in &request.messages {
            match message {
                // 系统提示并入顶层 instructions 字段
                Message::System { content } => instructions.push(content.clone()),
                Message::User(user) => {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": user.content,
                        }],
                    }));
                }
                Message::Assistant(assistant) => {
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    let text = assistant.text();
                    if !text.is_empty() {
                        blocks.push(serde_json::json!({
                            "type": "output_text",
                            "text": text,
                        }));
                    }
                    for block in &assistant.content {
                        if let ContentBlock::ToolUse { call } = block {
                            blocks.push(serde_json::json!({
                                "type": "function_call",
                                "call_id": call.call_id,
                                "name": call.name,
                                "arguments": call.arguments,
                            }));
                        }
                    }
                    input.push(serde_json::json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
                Message::Tool(tool) => {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "function_call_output",
                            "call_id": tool.call_id.clone().unwrap_or_default(),
                            "output": tool.content,
                        }],
                    }));
                }
                Message::Custom { name, data } => input.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": format!("[{name}]\n{data}"),
                    }],
                })),
            }
        }
        let mut body = serde_json::json!({
            "model": self.config.model,
            "input": input,
            "stream": streaming,
        });
        if !instructions.is_empty() {
            body["instructions"] = serde_json::json!(instructions.join("\n"));
        }
        if let Some(max_tokens) = self.config.max_tokens {
            body["max_output_tokens"] = serde_json::json!(max_tokens);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(to_responses_tools(&request.tools));
        }
        body
    }

    /// 构造一次 POST 请求（URL 非法在此失败）。
    fn build_request(
        &self,
        request: &LlmRequest,
        streaming: bool,
    ) -> Result<reqwest::Request, LlmError> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        self.client
            .post(&url)
            .bearer_auth(&self.config.api_key)
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

impl LlmAdapter for ResponsesAdapter {
    fn id(&self) -> &str {
        "responses"
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
                if let Err(error) = responses_single(&client, single_req, &tx).await {
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
            if let Err(error) = responses_run(client, streaming_req, single_req, tx.clone()).await {
                let _ = tx.send(Err(error));
            }
        });
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// 后台任务：流式优先；**可重试失败**（服务端/限流/网络/未分类）且尚无产出 → 非流式兜底。
async fn responses_run(
    client: reqwest::Client,
    streaming_req: reqwest::Request,
    single_req: reqwest::Request,
    tx: tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
) -> Result<(), LlmError> {
    let mut sent = 0usize;
    match responses_stream(&client, streaming_req, &tx, &mut sent).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if sent == 0 && error.is_retryable() {
                match responses_single(&client, single_req, &tx).await {
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
async fn responses_stream(
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
    // 工具调用：按 output_index 归组（call_id/name 在 output_item.added，arguments 增量累积）
    let mut pending: std::collections::BTreeMap<usize, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut has_tools = false;
    let mut usage_tail: Option<TokenUsage> = None;
    loop {
        if let Some(pos) = buffer.find('\n') {
            let line: String = buffer[..pos].trim_end_matches('\r').to_string();
            buffer.drain(..=pos);
            let Some(data) = line.strip_prefix("data:") else {
                continue; // 空行/注释行忽略
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let payload: SseEvent = match serde_json::from_str(data) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            match payload.kind.as_str() {
                "response.output_item.added" => {
                    if let Some(item) = &payload.item
                        && item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                        && let Some(index) = payload.output_index
                    {
                        let call_id = item
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        pending.insert(index, (call_id, name, String::new()));
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(index) = payload.output_index
                        && let Some(slot) = pending.get_mut(&index)
                        && let Some(delta) = &payload.delta
                    {
                        slot.2.push_str(delta);
                    }
                }
                "response.output_text.delta" => {
                    if let Some(delta) = &payload.delta {
                        *sent += 1;
                        if !delta.is_empty()
                            && tx
                                .send(Ok(StreamChunk {
                                    delta: ChunkDelta::Text {
                                        text: delta.clone(),
                                    },
                                    usage: None,
                                }))
                                .is_err()
                        {
                            return Ok(()); // 接收方已丢弃（提前取消）
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = &payload.item
                        && item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                        && let Some(index) = payload.output_index
                        && let Some((call_id, name, accumulated)) = pending.remove(&index)
                    {
                        // arguments 以 done 事件的完整值为准，缺省用累积增量
                        let arguments = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .unwrap_or(&accumulated)
                            .to_string();
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
                "response.completed" => {
                    if let Some(response) = &payload.response
                        && let Some(usage) = response.get("usage")
                    {
                        usage_tail = Some(TokenUsage {
                            input_tokens: usage
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            output_tokens: usage
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                        });
                    }
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
                    let message = payload
                        .response
                        .as_ref()
                        .and_then(|v| v.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("responses 服务端错误");
                    return Err(LlmError::new(LlmErrorCode::Server, message));
                }
                _ => {} // response.created / in_progress / output_text.done 等忽略
            }
        } else if eof {
            if !buffer.is_empty() {
                buffer.push('\n');
                continue;
            }
            // 流结束但未见 response.completed（异常）
            return Err(LlmError::new(
                LlmErrorCode::Protocol,
                "responses 流提前结束（未见 response.completed）",
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

/// 非流式兜底：单次请求，output 数组合成 chunk 序列。
async fn responses_single(
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
    let completion: ResponsesCompletion = serde_json::from_str(&raw).map_err(|error| {
        LlmError::new(
            LlmErrorCode::Protocol,
            format!("非流式响应不是合法 JSON: {error}"),
        )
    })?;
    // output 数组 → 文本块 / 工具块（保持顺序）；usage 挂最后一块
    let mut chunks: Vec<Result<StreamChunk, LlmError>> = Vec::new();
    let mut has_tools = false;
    for item in &completion.output {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("output_text")
                            && let Some(text) = block.get("text").and_then(|t| t.as_str())
                        {
                            chunks.push(Ok(StreamChunk {
                                delta: ChunkDelta::Text {
                                    text: text.to_string(),
                                },
                                usage: None,
                            }));
                        }
                    }
                }
            }
            Some("function_call") => {
                has_tools = true;
                chunks.push(Ok(StreamChunk {
                    delta: ChunkDelta::ToolUse {
                        call: ToolCall {
                            call_id: item
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            arguments: item
                                .get("arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        },
                    },
                    usage: None,
                }));
            }
            _ => {}
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
