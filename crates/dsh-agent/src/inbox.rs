//! 双队列 Inbox（dsh `Inbox`：next-turn / next-step 两个有序待处理列表）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use dsh_llm::UserMessage;

use crate::types::InboxTarget;

/// 内部状态（锁保护）。
struct InboxInner {
    next_turn: VecDeque<UserMessage>,
    next_step: VecDeque<UserMessage>,
}

/// 双队列 inbox：`next-turn`（普通后续轮）与 `next-step`（当前轮的 steering/注入）。
///
/// 廉价 Clone（共享内部 `Arc`）：agent 句柄与驱动器操作同一份队列。
#[derive(Clone)]
pub struct Inbox {
    inner: Arc<Mutex<InboxInner>>,
}

impl Default for Inbox {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(InboxInner {
                next_turn: VecDeque::new(),
                next_step: VecDeque::new(),
            })),
        }
    }
}

impl Inbox {
    /// 空 inbox。
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加到指定队列尾部。
    pub fn push(&self, target: InboxTarget, message: UserMessage) {
        let mut inner = self.inner.lock().unwrap();
        match target {
            InboxTarget::NextTurn => inner.next_turn.push_back(message),
            InboxTarget::NextStep => inner.next_step.push_back(message),
        }
    }

    /// 取出一步提议的完整批次（照 dsh `Inbox.claim`）：
    /// 总是先取光 next-step；`NextTurn` 目标再额外取 next-turn 队列的**恰好一条**
    /// （每个排队 turn 消费一条，同 turn 内可再消费 steering）。
    pub fn claim(&self, target: InboxTarget) -> Vec<UserMessage> {
        let mut inner = self.inner.lock().unwrap();
        let mut claimed: Vec<UserMessage> = inner.next_step.drain(..).collect();
        if target == InboxTarget::NextTurn
            && let Some(message) = inner.next_turn.pop_front()
        {
            claimed.push(message);
        }
        claimed
    }

    /// 清空两个队列。
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.next_turn.clear();
        inner.next_step.clear();
    }

    /// 是否有任何待处理消息。
    pub fn has_pending(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        !inner.next_turn.is_empty() || !inner.next_step.is_empty()
    }

    /// next-step 队列长度（turn 收敛判定用）。
    pub fn next_step_len(&self) -> usize {
        self.inner.lock().unwrap().next_step.len()
    }

    /// next-turn 队列长度。
    pub fn next_turn_len(&self) -> usize {
        self.inner.lock().unwrap().next_turn.len()
    }
}
