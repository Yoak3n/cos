//! 双队列 Inbox（dsh `Inbox`：next-turn / next-step 两个有序待处理列表）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cos_llm::UserMessage;

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

    /// 按 id 移除队列中的消息（next-step 优先、再 next-turn，各取首个匹配）。
    /// 返回是否移除。已 claim 出队（开始处理）的消息不在队列中，返回 false。
    pub fn remove_by_id(&self, id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if let Some(position) = inner
            .next_step
            .iter()
            .position(|m| m.id.as_deref() == Some(id))
        {
            inner.next_step.remove(position);
            return true;
        }
        if let Some(position) = inner
            .next_turn
            .iter()
            .position(|m| m.id.as_deref() == Some(id))
        {
            inner.next_turn.remove(position);
            return true;
        }
        false
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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, content: &str) -> UserMessage {
        UserMessage {
            content: content.into(),
            images: Vec::new(),
            id: Some(id.into()),
        }
    }

    #[test]
    fn remove_by_id_drops_queued_message() {
        let inbox = Inbox::new();
        inbox.push(InboxTarget::NextTurn, msg("m-1", "任务A"));
        inbox.push(InboxTarget::NextTurn, msg("m-2", "任务B"));
        inbox.push(InboxTarget::NextTurn, msg("m-3", "任务C"));
        inbox.push(InboxTarget::NextStep, msg("s-1", "steering"));

        // next-step 优先
        assert!(inbox.remove_by_id("s-1"));
        assert!(!inbox.remove_by_id("s-1"), "重复移除应为 false");

        // next-turn 命中
        assert!(inbox.remove_by_id("m-2"));
        assert_eq!(inbox.next_turn_len(), 2);

        // 已消费/不存在的 id → false
        assert!(!inbox.remove_by_id("m-2"));
        assert!(!inbox.remove_by_id("不存在"));

        // 剩余消息顺序不变（claim 每次取一条）
        let first = inbox.claim(InboxTarget::NextTurn);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].content, "任务A");
        let second = inbox.claim(InboxTarget::NextTurn);
        assert_eq!(second[0].content, "任务C");
    }

    #[test]
    fn remove_by_id_ignores_messages_without_id() {
        let inbox = Inbox::new();
        let mut anonymous = UserMessage::new("无 id 消息");
        anonymous.images.clear();
        inbox.push(InboxTarget::NextTurn, anonymous);
        assert!(!inbox.remove_by_id("无 id 消息"), "id 匹配而非内容匹配");
        assert_eq!(inbox.next_turn_len(), 1);
    }
}
