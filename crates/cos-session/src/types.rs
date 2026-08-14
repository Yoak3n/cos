//! 会话事件类型：封闭枚举 + Custom 逃生舱（决策 D4），信封带 `seq` / `time`。

use cos_llm::{AssistantMessage, StreamChunk, TokenUsage, ToolResultMessage, UserMessage};
use serde::{Deserialize, Serialize};

/// 磁盘会话格式版本（同 dsh `SESSION_FORMAT_VERSION`：未发布期钉 0，
/// 不兼容日志直接拒绝、不迁移）。
pub const SESSION_FORMAT_VERSION: u32 = 0;

/// 不可变存储元数据（日志之外，见 dsh `SessionHeader`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// 磁盘格式版本。
    pub version: u32,
    /// 会话 id。
    pub id: String,
    /// 创建时间（Unix epoch 毫秒）。
    #[serde(rename = "createdAt")]
    pub created_at_ms: u64,
    /// 创建时的工作目录（如有）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// 取消原因（dsh `AgentCancelCause`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AbortCause {
    /// 用户取消。
    User,
    /// 父 agent 取消。
    Parent,
    /// 钩子取消。
    Hook {
        /// 钩子给出的原因。
        reason: String,
    },
    /// 实例卸载。
    Disposed,
}

/// turn 结束原因（dsh `TurnEndReasonMap`，合并可扩展和的封闭子集）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    /// 正常完成。
    Completed,
    /// 取消打断。
    Aborted {
        /// 取消原因。
        cause: AbortCause,
    },
    /// 被阻塞（如等待外部输入）。
    Blocked,
    /// turn 失败。
    Error {
        /// 人读错误文本。
        message: String,
    },
    /// 输出 token 触顶。
    MaxTokens,
    /// 崩溃孤儿 turn 在重载时被持久化层关闭。
    Interrupted,
}

/// 工具调用失败身份。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolError {
    /// 错误名。
    pub name: String,
    /// 错误码。
    pub code: String,
}

/// todo 条目状态（dsh `TodoItem.status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// 未开始。
    Pending,
    /// 进行中。
    InProgress,
    /// 已完成。
    Completed,
}

/// todo 清单条目（dsh `TodoItem`：content + 三态 status）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    /// 任务描述（短祈使行）。
    pub content: String,
    /// 生命周期状态。
    pub status: TodoStatus,
}

/// 请求头快照（log-only，重建请求上下文用；dsh `EpochHeader` 的简化）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestHeader {
    /// 调用配置（P4 定型前为 JSON 占位）。
    pub config: serde_json::Value,
    /// 系统提示文本（无则省略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// 工具 schema（P5 定型前为 JSON 占位）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
}

/// 路由元数据（log-only；dsh `RequestContext` 的简化）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestContext {
    /// 注册的 provider 路由名。
    pub provider: String,
    /// provider 侧模型 id。
    pub model: String,
    /// 上下文窗口 token 数（如声明）。
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "contextWindow"
    )]
    pub context_window: Option<u32>,
}

/// 事件数据：封闭枚举（PLAN.md §3）+ Custom 逃生舱。
///
/// wire 形状：`{"type": "<事件名>", "data": {...}}`（事件名同 dsh，含 `/` 分隔符）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionEventData {
    /// 开启 turn（dsh `turn/start`）。
    #[serde(rename = "turn/start")]
    TurnStart {
        /// turn 序号。
        turn: u32,
    },
    /// 关闭 turn（dsh `turn/end`）。
    #[serde(rename = "turn/end")]
    TurnEnd {
        /// turn 序号。
        turn: u32,
        /// 结束原因。
        reason: TurnEndReason,
    },
    /// 开启 step（一次模型调用 + 其工具执行）。
    #[serde(rename = "step/start")]
    StepStart {
        /// turn 序号。
        turn: u32,
        /// step 序号。
        step: u32,
    },
    /// 关闭 step。
    #[serde(rename = "step/end")]
    StepEnd {
        /// turn 序号。
        turn: u32,
        /// step 序号。
        step: u32,
    },
    /// 用户消息（surface）。
    #[serde(rename = "user/message")]
    UserMessage(UserMessage),
    /// 原始流式 chunk（token 级回放保真；log-only）。
    #[serde(rename = "assistant/chunk")]
    AssistantChunk {
        /// turn 序号。
        turn: u32,
        /// step 序号。
        step: u32,
        /// 增量块。
        chunk: StreamChunk,
    },
    /// 装配完成的 assistant 消息（surface；推导历史用这条）。
    #[serde(rename = "assistant/message")]
    AssistantMessage {
        /// turn 序号。
        turn: u32,
        /// step 序号。
        step: u32,
        /// 消息本体。
        message: AssistantMessage,
        /// 适配器报告的 token 用量（无则省略）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
    },
    /// 模型请求的一次工具调用。
    #[serde(rename = "tool/call")]
    ToolCall {
        /// turn 序号。
        turn: u32,
        /// step 序号。
        step: u32,
        /// 调用 id（与 `tool/result` 配对）。
        #[serde(rename = "callId")]
        call_id: String,
        /// 工具名。
        name: String,
        /// 模型产出的原始 JSON 参数字符串（未解析）。
        arguments: String,
    },
    /// 工具调用结果（surface）。
    #[serde(rename = "tool/result")]
    ToolResult {
        /// turn 序号。
        turn: u32,
        /// step 序号。
        step: u32,
        /// 调用 id（与 `tool/call` 配对）。
        #[serde(rename = "callId")]
        call_id: String,
        /// 模型可见的结果消息。
        message: ToolResultMessage,
        /// 内部失败身份（如有）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ToolError>,
    },
    /// todo 清单整表快照（log-only，整表替换、最后写入胜出；dsh `todo/write`）。
    #[serde(rename = "todo/write")]
    TodoWrite {
        /// 整表快照。
        todos: Vec<TodoItem>,
    },
    /// 请求头快照（log-only）。
    #[serde(rename = "request/header")]
    RequestHeader {
        /// 头内容。
        header: RequestHeader,
    },
    /// 路由元数据（log-only）。
    #[serde(rename = "request/context")]
    RequestContext {
        /// 上下文内容。
        context: RequestContext,
    },
    /// 第三方事件的逃生舱（决策 D4；derive_messages 原样透传）。
    #[serde(rename = "custom")]
    Custom {
        /// 事件名。
        name: String,
        /// 事件数据（JSON）。
        data: serde_json::Value,
    },
}

/// 一条追加日志：单调 `seq` + epoch 毫秒 `time` + 事件数据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// 会话内单调序列号（从 1 起，连续含 chunk）。
    pub seq: u64,
    /// Unix epoch 毫秒时间戳。
    pub time: u64,
    /// 事件数据（flatten：`type` / `data` 在信封顶层）。
    #[serde(flatten)]
    pub data: SessionEventData,
}

impl SessionEventData {
    /// wire 事件名（同 dsh，含 `/` 分隔符）。
    pub fn type_name(&self) -> &'static str {
        match self {
            SessionEventData::TurnStart { .. } => "turn/start",
            SessionEventData::TurnEnd { .. } => "turn/end",
            SessionEventData::StepStart { .. } => "step/start",
            SessionEventData::StepEnd { .. } => "step/end",
            SessionEventData::UserMessage(_) => "user/message",
            SessionEventData::AssistantChunk { .. } => "assistant/chunk",
            SessionEventData::AssistantMessage { .. } => "assistant/message",
            SessionEventData::ToolCall { .. } => "tool/call",
            SessionEventData::ToolResult { .. } => "tool/result",
            SessionEventData::TodoWrite { .. } => "todo/write",
            SessionEventData::RequestHeader { .. } => "request/header",
            SessionEventData::RequestContext { .. } => "request/context",
            SessionEventData::Custom { .. } => "custom",
        }
    }
}
