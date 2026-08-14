//! dsh-memory —— 记忆内核（关系层记忆，M1–M3）。
//!
//! 设计文档：`docs/memory-plugin.md`。核心主张：
//! - **双层模型 + 关系卡**：events（append-only 真相源）与 topics（可合并的当前状态行，
//!   recall 只查这层）分离；relation_card 单行常驻注入；
//! - **编号消解**（resolve_topic）：身份是不透明 `topic_id`，表述只是别名；
//!   词法阻塞先行（精确直通），LLM 仲裁兜底，**不确定时保守新建**（假阴性可恢复，假阳性不可恢复）；
//! - **异构机制**：提取（窄而弱，只抄字面）＋统计（确定性地面真值）＋推断（digest 慢路径，
//!   高门槛保守）＋策略（规则确定性）；
//! - **遗忘曲线**：系统衰减的活，agent 只加强/减弱（remember/demote），删除可逆到最后一刻；
//! - **诚实出口**：低于阈值显式"无相关记忆"；
//! - **上下文压缩**（M3）：滚动摘要 + 尾部保留窗口，摘要经 `session_state` 持久化。
//!
//! LLM 机制（提取/仲裁/合并/压缩/digest）一律经 [`dsh_llm::LlmAdapter`] 接缝注入
//! （测试用脚本化 mock）。

#![warn(missing_docs)]

mod error;
mod extract;
mod pipeline;
mod recall;
mod store;
mod tools;

pub use error::MemoryError;
pub use pipeline::{ApplyOutcome, Resolution, TurnPair, turn_pair_from_text};
pub use recall::{MemoryHit, ModeBFeed, RecallOutcome};
pub use store::{FactAction, FactKind, MemoryStore, RelationCard, Tier, Topic, now_ms};
pub use tools::{demote_topic, inventory_topics, recall_memories, remember_fact};
