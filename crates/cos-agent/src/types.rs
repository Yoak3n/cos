//! Agent 句柄契约、inbox 目标与事件载荷（照 dsh runtime-types.ts / types.ts）。

use std::sync::Arc;

use cos_llm::{LlmAdapter, UserMessage};
use cos_session::Session;
use futures::future::BoxFuture;
use thiserror::Error;

/// 两个有序待处理消息队列之一（dsh `InboxTarget`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxTarget {
    /// 下一 turn 的队列。
    NextTurn,
    /// 当前 turn 下一 step 的队列。
    NextStep,
}

/// agent 生命周期状态（dsh `AgentStatus`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// 无活动驱动器。
    Idle,
    /// 驱动器在跑。
    Running,
}

/// agent 创建选项（dsh `AgentOptions` 的子集）。
#[derive(Debug, Clone, Default)]
pub struct AgentOptions {
    /// provider 路由名。
    pub provider: Option<String>,
    /// 模型 id。
    pub model: Option<String>,
    /// 每次请求最大输出 token。
    pub max_tokens: Option<u32>,
}

/// 取消信号（loop 驱动器传给维护任务等）。
#[derive(Debug, Clone, Default)]
pub struct AbortSignal {
    aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl AbortSignal {
    /// 新建信号。
    pub fn new() -> Self {
        Self::default()
    }

    /// 触发取消。
    pub fn abort(&self) {
        self.aborted
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// 是否已取消。
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// 维护任务（对象安全包装：取消信号 + 返回 ()）。
pub type Maintenance = Box<dyn FnOnce(AbortSignal) -> BoxFuture<'static, ()> + Send>;

/// 注册表工厂的创建请求（dsh `CreateAgentOptions` 的 A 形态子集）。
#[derive(Clone)]
pub struct CreateAgentOptions {
    /// 共享的 agent/session 身份。
    pub session_id: String,
    /// agent 选项（provider/model/…）。
    pub options: AgentOptions,
    /// 该 agent 使用的 LLM 适配器。
    pub adapter: Arc<dyn LlmAdapter>,
}

/// pre-step waterfall 的载荷（dsh `agent/pre-step`）。
#[derive(Debug, Clone)]
pub struct PreStepPayload {
    /// agent id。
    pub agent_id: String,
    /// 本 step 从 inbox 取出的消息。
    pub messages: Vec<UserMessage>,
    /// 所属 turn。
    pub turn: u32,
    /// 提议的 step。
    pub step: u32,
}

/// pre-step 决策（dsh `PreStepDecision`）：拒绝或进入（可替换消息）。
#[derive(Debug, Clone, PartialEq)]
pub enum PreStepDecision {
    /// 拒绝本 step（turn 以 blocked 收束）。
    Reject,
    /// 进入本 step（携带进入的消息）。
    Enter {
        /// 进入 step 的消息。
        messages: Vec<UserMessage>,
    },
}

/// cos-agent 边界错误。
#[derive(Debug, Error, PartialEq)]
pub enum AgentError {
    /// 未注册 agent 工厂（同 dsh NO_FACTORY）。
    #[error("no agent factory registered (load an agent-loop plugin)")]
    NoFactory,
    /// 同 id agent 已注册。
    #[error("agent \"{0}\" is already registered")]
    AlreadyRegistered(String),
    /// agent 不存在。
    #[error("agent \"{0}\" is not registered")]
    NotFound(String),
    /// 已有活动工作（维护任务重入）。
    #[error("agent \"{0}\" already has active work")]
    Busy(String),
    /// 其他失败。
    #[error("{0}")]
    Other(String),
}

/// `agent/created` 载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCreatedPayload {
    /// agent id。
    pub agent_id: String,
}

/// `agent/disposed` 载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDisposedPayload {
    /// agent id。
    pub agent_id: String,
}

/// `agent/status` 载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStatusPayload {
    /// agent id。
    pub agent_id: String,
    /// 刚进入的状态。
    pub status: AgentStatus,
}

/// `agent/inbox/inserted` 载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInboxPayload {
    /// agent id。
    pub agent_id: String,
    /// 插入的消息。
    pub message: UserMessage,
}

/// `agent/error` 载荷。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentErrorPayload {
    /// agent id。
    pub agent_id: String,
    /// 出错 turn。
    pub turn: u32,
    /// 出错 step。
    pub step: u32,
    /// 人读错误文本。
    pub message: String,
}

/// 公开的 live-agent 句柄（dsh `Agent`，对象安全）。
///
/// 方法签名经对象安全裁剪（§6）：取消信号/维护任务用类型擦除的包装。
pub trait AgentTrait: Send + Sync {
    /// 共享的 agent/session 身份。
    fn id(&self) -> &str;

    /// 该 agent 的选项（provider/model/…）。
    fn options(&self) -> &AgentOptions;

    /// 该 agent 驱动的会话（日志是唯一事实源）。
    fn session(&self) -> &Session;

    /// agent 作用域上下文（agent-local 注册、卸载即回收）。
    fn ctx(&self) -> &cos_core::Context;

    /// 当前生命周期状态。
    fn status(&self) -> AgentStatus;

    /// 路由输入到 inbox 边界并可选唤醒驱动器（dsh `send`）。
    fn send(&self, message: UserMessage, target: InboxTarget, wake: bool);

    /// 排队一个普通后续 turn 并唤醒（dsh `followup`）。
    fn followup(&self, message: UserMessage);

    /// 提交最近 step 的 steering 并唤醒（dsh `steer`）。
    fn steer(&self, message: UserMessage);

    /// 排队模型可见上下文、不唤醒（dsh `inject`）。
    fn inject(&self, message: UserMessage);

    /// 待处理消息数（next-turn + next-step 队列；RPC `get_state` 用，缺省 0）。
    fn pending_count(&self) -> usize {
        0
    }

    /// 取消：清除排队（除非 keep_inbox）并中止活动 turn。
    fn cancel(&self, cause: cos_session::AbortCause, keep_inbox: bool);

    /// 等待 agent 活动收敛到空闲。
    fn when_idle(&self) -> BoxFuture<'static, ()>;

    /// 在空闲期运行一个非 turn 维护任务。
    fn run_maintenance(&self, task: Maintenance) -> BoxFuture<'static, Result<(), AgentError>>;
}

/// agent 创建工厂（loop 实现，注册进 [`AgentRegistry`]；同 dsh `AgentFactory`）。
pub trait AgentFactory: Send + Sync {
    /// 在给定根上下文上创建一个未发布 agent。
    fn create(
        &self,
        root: &cos_core::Context,
        options: CreateAgentOptions,
    ) -> BoxFuture<'static, Result<Arc<dyn AgentTrait>, AgentError>>;
}
