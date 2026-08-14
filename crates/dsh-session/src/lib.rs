//! dsh-session —— SessionEvent 追加日志、derive_messages、JSONL 持久化（P3）。
//!
//! 会话日志为唯一事实源（PLAN.md §0）：模型可见 ⟺ 已记录。
//! 事件为封闭枚举 + `Custom` 逃生舱（决策 D4）；`seq` 单调连续（含原始 chunk），
//! 持久化可逐字节存回（JSONL 一行一事件）。
//!
//! 语义权威参考：`packages/core/session/src/types.ts`。
//! P3 范围：surface 投影（user/message、assistant/message、tool/result、Custom 透传）；
//! compaction/surface-op/seed 等高级机制留待后续阶段。

#![warn(missing_docs)]

mod error;
mod jsonl;
mod session;
mod types;

pub use error::SessionError;
pub use jsonl::{load_jsonl, save_jsonl};
pub use session::Session;
pub use types::{
    AbortCause, RequestContext, RequestHeader, SESSION_FORMAT_VERSION, SessionEvent,
    SessionEventData, SessionHeader, TodoItem, TodoStatus, ToolError, TurnEndReason,
};
