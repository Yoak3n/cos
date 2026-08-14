//! cos-llm-opencode —— OpenAI 兼容 `chat/completions` 适配器（Provider，M2）。
//!
//! 接缝纪律：只依赖 Definition crate（cos-llm）。`stream()` 是同步方法（[`LlmAdapter`]
//! 对象安全接缝），故内部 `tokio::spawn` 转发：请求在后台任务执行，chunk 经
//! unbounded channel 流入返回的流；调用方必须在 tokio runtime 内。
//!
//! 协议：`POST {base_url}/chat/completions`，优先 `stream: true`（SSE 逐行
//! `data: {…}`，`choices[0].delta.content` 累积为文本块，usage 块映射 [`TokenUsage`]，
//! `data: [DONE]` 收束）；若流式在**未产出任何 chunk 前**失败于服务端（HTTP 5xx /
//! `{"type":"error",…}` 块），自动退化为非流式单次请求（`choices[0].message.content` +
//! usage 合成一个 chunk）——兼容流式暂不可用的网关（实测 opencode zen/go 流式不稳定）。
//! 4xx（鉴权/余额）不重试，原样报错。推理模型只流 `reasoning_content` 时兜底输出思考文本。
//!
//! LLM 统一管理：本 crate 经 `llm_factory!("opencode", build_opencode)` 注册提供商工厂。

#![warn(missing_docs)]

use std::sync::Arc;

use cos_llm::{
    ChunkDelta, LlmAdapter, LlmError, LlmRequest, LlmStream, Message, StreamChunk, TokenUsage,
};
use futures::StreamExt;
use serde::Deserialize;

/// 提供商工厂构建函数（`llm_factory!` 注册）：配置 `{base_url, api_key, model, streaming?}`。
pub fn build_opencode(config: &serde_json::Value) -> Result<Arc<dyn LlmAdapter>, LlmError> {
    /// 提供商配置（插件 yml 里的 `config` 段）。
    #[derive(Deserialize)]
    struct ProviderConfig {
        base_url: String,
        api_key: String,
        model: String,
        #[serde(default = "default_streaming")]
        streaming: bool,
    }
    fn default_streaming() -> bool {
        true
    }
    let config: ProviderConfig = serde_json::from_value(config.clone())
        .map_err(|error| LlmError::Failure(format!("opencode 提供商配置无效: {error}")))?;
    Ok(Arc::new(OpencodeAdapter::new(OpencodeConfig {
        base_url: config.base_url,
        api_key: config.api_key,
        model: config.model,
        streaming: config.streaming,
    })))
}

cos_llm::llm_factory!("opencode", build_opencode);

/// 适配器配置。
#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    /// base URL（不带 `/chat/completions` 后缀，如 `https://opencode.ai/zen/go/v1`）。
    pub base_url: String,
    /// API key。
    pub api_key: String,
    /// 模型 id。
    pub model: String,
    /// 是否用流式（`stream:true`）；false = 直接非流式单次请求。
    /// 某些网关（opencode zen/go）流式只出 `reasoning_content` 且时有 500，
    /// 非流式反而给出完整 `content` —— 此时关掉流式更稳。
    pub streaming: bool,
}

/// OpenAI 兼容适配器（流式优先、非流式自动兜底）。
#[derive(Clone)]
pub struct OpencodeAdapter {
    config: OpencodeConfig,
    client: reqwest::Client,
}

/// SSE `data:` 行结构（OpenAI chat.completion.chunk）。
#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

/// SSE choices 元素。
#[derive(Deserialize)]
struct SseChoice {
    #[serde(default)]
    delta: SseDelta,
}

/// SSE delta。
#[derive(Deserialize, Default)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    /// 推理模型（deepseek 等）把思考文本放这里；content 缺失时兜底。
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// 非流式响应（OpenAI chat.completion）。
#[derive(Deserialize)]
struct Completion {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

/// 非流式 choices 元素。
#[derive(Deserialize)]
struct CompletionChoice {
    message: CompletionMessage,
}

/// 非流式 message（content 为 null 时兜底 reasoning_content）。
#[derive(Deserialize)]
struct CompletionMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

/// usage 块（流式与非流式共用）。
#[derive(Deserialize)]
struct SseUsage {
    /// 输入 token。
    #[serde(rename = "prompt_tokens")]
    prompt_tokens: u64,
    /// 输出 token。
    #[serde(rename = "completion_tokens")]
    completion_tokens: u64,
}

/// 一步请求的失败分类：决定是否走非流式兜底。
enum StepError {
    /// 服务端侧失败（5xx / error 块）且尚无产出 → 可兜底。
    Retryable(String),
    /// 其余失败（4xx / 网络 / 解析）→ 原样报错。
    Fatal(String),
}

impl OpencodeAdapter {
    /// 由配置构造（默认请求头：Bearer + JSON）。
    pub fn new(config: OpencodeConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// 当前模型 id。
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// 组装请求体（OpenAI messages 投影 + stream 开关）。
    fn body(&self, request: &LlmRequest, streaming: bool) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|message| match message {
                Message::System { content } => {
                    serde_json::json!({ "role": "system", "content": content })
                }
                Message::User(user) => {
                    serde_json::json!({ "role": "user", "content": user.content })
                }
                Message::Assistant(assistant) => {
                    serde_json::json!({ "role": "assistant", "content": assistant.text() })
                }
                Message::Tool(tool) => {
                    serde_json::json!({ "role": "tool", "content": tool.content })
                }
                Message::Custom { name, data } => serde_json::json!({
                    "role": "user",
                    "content": format!("[{name}]\n{data}")
                }),
            })
            .collect();
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "stream": streaming,
        });
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(request.tools);
        }
        body
    }

    /// 构造一次 POST 请求（URL 非法在此失败）。
    fn build_request(
        &self,
        request: &LlmRequest,
        streaming: bool,
    ) -> Result<reqwest::Request, LlmError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        self.client
            .post(&url)
            .bearer_auth(&self.config.api_key)
            .json(&self.body(request, streaming))
            .build()
            .map_err(|error| LlmError::Failure(format!("请求构造失败: {error}")))
    }
}

impl LlmAdapter for OpencodeAdapter {
    fn id(&self) -> &str {
        "opencode"
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
                if let Err(message) = single_shot(&client, single_req, &tx).await {
                    let _ = tx.send(Err(LlmError::Failure(message)));
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
            if let Err(error) = run_request(client, streaming_req, single_req, tx.clone()).await {
                let _ = tx.send(Err(error));
            }
        });
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// 后台任务：流式优先；服务端失败且尚无产出 → 非流式兜底。
async fn run_request(
    client: reqwest::Client,
    streaming_req: reqwest::Request,
    single_req: reqwest::Request,
    tx: tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
) -> Result<(), LlmError> {
    let mut sent = 0usize;
    match stream_once(&client, streaming_req, &tx, &mut sent).await {
        Ok(()) => Ok(()),
        Err(error) => {
            if sent == 0 && matches!(error, StepError::Retryable(_)) {
                single_shot(&client, single_req, &tx)
                    .await
                    .map_err(|message| {
                        LlmError::Failure(format!("{message}（非流式兜底同样失败）"))
                    })
            } else {
                Err(match error {
                    StepError::Retryable(message) | StepError::Fatal(message) => {
                        LlmError::Failure(message)
                    }
                })
            }
        }
    }
}

/// 流式执行：SSE 逐行解析并转发；失败分类为 [`StepError`]。
async fn stream_once(
    client: &reqwest::Client,
    req: reqwest::Request,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
    sent: &mut usize,
) -> Result<(), StepError> {
    let response = client
        .execute(req)
        .await
        .map_err(|error| StepError::Fatal(format!("请求失败: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let message = format!("HTTP {status}: {}", truncate(&text, 300));
        return Err(if status.is_server_error() {
            StepError::Retryable(message)
        } else {
            StepError::Fatal(message)
        });
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut finished = false;
    let mut eof = false;
    // 推理文本缓冲：content 一出现即丢弃；仅当全程无 content 才在流尾回退输出
    let mut reasoning_buf = String::new();
    let mut saw_content = false;
    while !finished {
        if let Some(pos) = buffer.find('\n') {
            // 逐行消费 SSE；error 体可能不带 `data:` 前缀（裸 JSON）
            let line: String = buffer[..pos].trim_end_matches('\r').to_string();
            buffer.drain(..=pos);
            let data = match line.strip_prefix("data:") {
                Some(data) => data.trim(),
                None => line.trim(),
            };
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                finished = true;
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data)
                && value.get("type").and_then(|t| t.as_str()) == Some("error")
            {
                return Err(StepError::Retryable(format!(
                    "服务端错误: {}",
                    truncate(data, 300)
                )));
            }
            let chunk: SseChunk = match serde_json::from_str(data) {
                Ok(chunk) => chunk,
                Err(_) => continue, // keepalive/注释行忽略
            };
            let mut text = String::new();
            let mut reasoning = String::new();
            for choice in &chunk.choices {
                if let Some(content) = &choice.delta.content {
                    text.push_str(content);
                }
                if let Some(thought) = &choice.delta.reasoning_content {
                    reasoning.push_str(thought);
                }
            }
            let usage = chunk.usage.map(|usage| TokenUsage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            });
            *sent += 1;
            if text.is_empty() {
                // 纯推理增量：缓冲；usage 独立成块（消费方跳过空文本）
                reasoning_buf.push_str(&reasoning);
                if usage.is_some()
                    && tx
                        .send(Ok(StreamChunk {
                            delta: ChunkDelta::Text {
                                text: String::new(),
                            },
                            usage,
                        }))
                        .is_err()
                {
                    return Ok(()); // 接收方已丢弃（提前取消）
                }
            } else {
                // content 出现：丢弃已缓冲的推理（回答优先），按 content 流出
                saw_content = true;
                reasoning_buf.clear();
                if tx
                    .send(Ok(StreamChunk {
                        delta: ChunkDelta::Text { text },
                        usage,
                    }))
                    .is_err()
                {
                    return Ok(()); // 接收方已丢弃（提前取消）
                }
            }
        } else if eof {
            // EOF：残留无换行尾行（裸 JSON error 体等）补一个换行再处理一次
            if !buffer.is_empty() {
                buffer.push('\n');
                continue;
            }
            break;
        } else {
            match stream.next().await {
                Some(Ok(bytes)) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                Some(Err(error)) => {
                    return Err(StepError::Fatal(format!("流读取失败: {error}")));
                }
                None => eof = true,
            }
        }
    }
    // 流尾：全程无 content → 回退输出缓冲的推理（宁可说出思考，不可空回复）
    if !saw_content && !reasoning_buf.is_empty() {
        *sent += 1;
        let _ = tx.send(Ok(StreamChunk {
            delta: ChunkDelta::Text {
                text: std::mem::take(&mut reasoning_buf),
            },
            usage: None,
        }));
    }
    Ok(())
}

/// 非流式兜底：单次请求，整段内容合成一个 chunk。
async fn single_shot(
    client: &reqwest::Client,
    req: reqwest::Request,
    tx: &tokio::sync::mpsc::UnboundedSender<Result<StreamChunk, LlmError>>,
) -> Result<(), String> {
    let response = client
        .execute(req)
        .await
        .map_err(|error| format!("请求失败: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {}", truncate(&text, 300)));
    }
    let raw = response.text().await.map_err(|error| error.to_string())?;
    let completion: Completion =
        serde_json::from_str(&raw).map_err(|error| format!("非流式响应不是合法 JSON: {error}"))?;
    let mut text = String::new();
    let mut usage = None;
    for choice in &completion.choices {
        // content 优先（空串视同缺失）；推理模型可能把文本放在 reasoning_content
        let content = choice
            .message
            .content
            .as_deref()
            .filter(|c| !c.trim().is_empty());
        let reasoning = choice
            .message
            .reasoning_content
            .as_deref()
            .filter(|c| !c.trim().is_empty());
        match (content, reasoning) {
            (Some(content), _) => text.push_str(content),
            (None, Some(reasoning)) => text.push_str(reasoning),
            (None, None) => {}
        }
    }
    if let Some(u) = completion.usage {
        usage = Some(TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });
    }
    let _ = tx.send(Ok(StreamChunk {
        delta: ChunkDelta::Text { text },
        usage,
    }));
    Ok(())
}

/// 截断过长错误文本（避免整页回显）。
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}
