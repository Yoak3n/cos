//! 会话日志：追加即事实源；`derive_messages` 从日志投影模型可见历史。
//!
//! P4 起 `Session` 具备内部可变性（`Arc<Mutex<Inner>>`）：Agent 句柄对外共享
//! `&Session` 只读视图，loop 驱动器以 `&self` 追加——写路径仍是单写者（loop）。

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use cos_llm::{Message, ToolResultMessage};

use crate::types::{SessionEvent, SessionEventData};

/// 内部状态（锁保护）。
struct Inner {
    events: Vec<SessionEvent>,
    next_seq: u64,
}

/// 追加式会话日志（唯一事实源：模型可见 ⟺ 已记录）。
///
/// 廉价 Clone（共享内部 `Arc`）：多个持有者看到同一份日志。
#[derive(Clone)]
pub struct Session {
    id: String,
    inner: Arc<Mutex<Inner>>,
}

impl Session {
    /// 新建空会话（seq 从 1 起）。
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            inner: Arc::new(Mutex::new(Inner {
                events: Vec::new(),
                next_seq: 1,
            })),
        }
    }

    /// 从既有事件恢复（重载/回放）；`next_seq = max(seq) + 1`。
    pub fn from_events(id: impl Into<String>, events: Vec<SessionEvent>) -> Self {
        let next_seq = events.iter().map(|event| event.seq).max().unwrap_or(0) + 1;
        Self {
            id: id.into(),
            inner: Arc::new(Mutex::new(Inner { events, next_seq })),
        }
    }

    /// 会话 id。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 全部事件快照（追加顺序）。
    pub fn events(&self) -> Vec<SessionEvent> {
        self.inner.lock().unwrap().events.clone()
    }

    /// 已分配的最大 seq。
    pub fn last_seq(&self) -> u64 {
        self.inner.lock().unwrap().next_seq.saturating_sub(1)
    }

    /// 追加事件（时间戳取当前 epoch 毫秒）；返回写入的事件。
    pub fn append(&self, data: SessionEventData) -> SessionEvent {
        self.append_at(data, now_ms())
    }

    /// 追加事件（显式时间戳，测试确定性用）；返回写入的事件。
    pub fn append_at(&self, data: SessionEventData, time_ms: u64) -> SessionEvent {
        let mut inner = self.inner.lock().unwrap();
        let event = SessionEvent {
            seq: inner.next_seq,
            time: time_ms,
            data,
        };
        inner.next_seq += 1;
        inner.events.push(event.clone());
        event
    }

    /// 从日志投影模型可见历史（同 dsh `deriveMessages`）：
    /// user/message、assistant/message、tool/result 按 seq 顺序进入 surface；
    /// `Custom` 原样透传（决策 D4）；chunk / 边界 / 请求头不参与。
    pub fn derive_messages(&self) -> Vec<Message> {
        self.inner
            .lock()
            .unwrap()
            .events
            .iter()
            .filter_map(|event| match &event.data {
                SessionEventData::UserMessage(message) => Some(Message::User(message.clone())),
                SessionEventData::AssistantMessage { message, .. } => {
                    Some(Message::Assistant(message.clone()))
                }
                SessionEventData::ToolResult {
                    message, call_id, ..
                } => Some(Message::Tool(ToolResultMessage {
                    content: message.content.clone(),
                    // 配对调用 id 必须随历史回流（OpenAI 协议 tool 消息需要 tool_call_id）
                    call_id: Some(call_id.clone()),
                })),
                SessionEventData::Custom { name, data } => Some(Message::Custom {
                    name: name.clone(),
                    data: data.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// 当前 epoch 毫秒。
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时间早于 Unix epoch")
        .as_millis() as u64
}
