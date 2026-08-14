//! cos-agent —— Agent trait、注册表、Inbox 接缝（P4）。
//!
//! 语义权威参考：`packages/core/agent/src/{index,runtime-types,inbox,types}.ts`。
//!
//! 接缝纪律（PLAN.md §2/§6）：本 crate 只有 Definition——Agent 句柄契约、注册表、
//! 双队列 Inbox、task-local 因果链（with_initiator）。具体驱动器在 cos-agent-loop。
//! 所有公开 trait 对象安全。

#![warn(missing_docs)]

mod inbox;
mod registry;
mod types;

pub use inbox::Inbox;
pub use registry::{AgentRegistry, current_initiator, with_initiator, without_initiator};
pub use types::{
    AbortSignal, AgentCreatedPayload, AgentDisposedPayload, AgentError, AgentErrorPayload,
    AgentFactory, AgentInboxPayload, AgentOptions, AgentStatus, AgentStatusPayload,
    CreateAgentOptions, InboxTarget, Maintenance, PreStepDecision, PreStepPayload,
};

/// [`types::AgentTrait`] 的惯用别名（同 dsh 的 `Agent` 接口名）。
pub use types::AgentTrait as Agent;
