//! dsh-llm —— LLM 接缝：Message / ContentBlock、LlmAdapter trait、stream（P3）。
//!
//! 接缝纪律（PLAN.md §2 / §6）：这里只有 Definition——具体 Provider（mock / 真实适配器）
//! 与消费方（dsh-agent-loop、plugins/*）都只依赖本 crate。
//! 所有公开 trait 对象安全、方法参数窄（可 JSON 序列化的数据，§6 前置防返工）。

#![warn(missing_docs)]

use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 系统消息。
    System,
    /// 用户消息。
    User,
    /// 助手消息。
    Assistant,
    /// 工具结果消息。
    Tool,
}

/// 用户消息（模型可见输入；`source` 等 dsh 字段 P4 随 agent 补齐）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    /// 消息文本。
    pub content: String,
}

impl UserMessage {
    /// 由文本构造用户消息。
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// 模型请求的工具调用（参数为模型产出的原始 JSON 字符串，未解析）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 调用 id（与 `tool/result` 配对）。
    #[serde(rename = "callId")]
    pub call_id: String,
    /// 工具名。
    pub name: String,
    /// 原始 JSON 参数字符串。
    pub arguments: String,
}

/// assistant 消息的内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentBlock {
    /// 文本块。
    Text {
        /// 文本。
        text: String,
    },
    /// 工具调用块。
    ToolUse {
        /// 调用内容。
        call: ToolCall,
    },
}

/// assistant 消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// 内容块序列。
    pub content: Vec<ContentBlock>,
}

impl AssistantMessage {
    /// 由内容块构造 assistant 消息。
    pub fn new(content: Vec<ContentBlock>) -> Self {
        Self { content }
    }

    /// 拼接全部文本块（工具调用块跳过）。
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::ToolUse { .. } => None,
            })
            .collect()
    }
}

/// 工具结果消息（模型可见的工具返回值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultMessage {
    /// 结果文本。
    pub content: String,
}

impl ToolResultMessage {
    /// 由文本构造工具结果消息。
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

/// 模型可见消息：`derive_messages` 的投影结果与请求消息的统一载体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// 系统消息（请求装配时注入，不出现在推导历史里）。
    System {
        /// 系统提示文本。
        content: String,
    },
    /// 用户消息。
    User(UserMessage),
    /// 助手消息。
    Assistant(AssistantMessage),
    /// 工具结果消息。
    Tool(ToolResultMessage),
    /// 第三方插件的事件投影（决策 D4：Custom 原样透传）。
    Custom {
        /// 事件名。
        name: String,
        /// 事件数据（JSON）。
        data: serde_json::Value,
    },
}

/// 流式增量块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ChunkDelta {
    /// 文本增量。
    Text {
        /// 增量文本。
        text: String,
    },
    /// 工具调用增量。
    ToolUse {
        /// 调用内容。
        call: ToolCall,
    },
}

/// 一个流式 chunk（token 级回放保真的最小单位）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamChunk {
    /// 增量内容。
    pub delta: ChunkDelta,
    /// 末块可携带 token 用量（适配器报告则记，随 assistant/message 一起入库）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

impl StreamChunk {
    /// 由文本增量构造 chunk。
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            delta: ChunkDelta::Text { text: text.into() },
            usage: None,
        }
    }
}

/// Token 用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// 输入 token 数。
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    /// 输出 token 数。
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
}

/// 一次模型请求（§6：参数窄、可 JSON 序列化）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmRequest {
    /// 系统提示（无则省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// 模型可见消息。
    pub messages: Vec<Message>,
    /// 工具 schema（P5 定型前为 JSON 占位）。
    #[serde(default)]
    pub tools: Vec<serde_json::Value>,
}

/// LLM 适配器边界错误。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum LlmError {
    /// 适配器失败（message 为人读文本）。
    #[error("{0}")]
    Failure(String),
}

/// 流式响应：chunk 序列（`Err` 终止流）。
pub type LlmStream = Pin<Box<dyn futures::Stream<Item = Result<StreamChunk, LlmError>> + Send>>;

/// LLM 适配器接缝（对象安全：同步方法 + boxed stream 返回）。
pub trait LlmAdapter: Send + Sync {
    /// 适配器 id（同 provider 路由名）。
    fn id(&self) -> &str;

    /// 以流式方式执行一次请求。
    fn stream(&self, request: &LlmRequest) -> LlmStream;
}
