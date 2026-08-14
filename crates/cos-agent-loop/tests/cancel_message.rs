//! 排队消息取消：`Agent::cancel_message(id)` 移除 inbox 中指定 id 的待处理消息。
//!
//! 确定性编排：三个 followup 连续同步发出（驱动器尚未调度，全部在队列中）→
//! 取消中间一条 → when_idle 后断言只处理了其余两条。

use std::sync::Arc;

use cos_agent::{Agent, AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::Context;
use cos_llm::{LlmAdapter, UserMessage};
use cos_llm_mock::{MockAdapter, MockReply};
use cos_session::SessionEventData;

fn msg(id: &str, content: &str) -> UserMessage {
    UserMessage {
        content: content.into(),
        images: Vec::new(),
        id: Some(id.into()),
    }
}

/// 会话里出现的用户消息文本（按序）。
fn user_texts(agent: &dyn Agent) -> Vec<String> {
    agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::UserMessage(message) => Some(message.content.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn cancel_message_removes_queued_followup() {
    let root = Context::root();
    let registry = AgentRegistry::new(&root);
    root.provide(registry.clone()).unwrap();
    registry.set_factory(Arc::new(LoopFactory)).unwrap();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::text("第一轮"), MockReply::text("第三轮")],
    ));
    let agent = registry
        .create(CreateAgentOptions {
            session_id: "cancel-queue".into(),
            options: AgentOptions {
                provider: Some("mock".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter,
        })
        .await
        .unwrap();

    // 连续同步 followup：驱动器尚未调度，三条全部在队列中
    agent.followup(msg("m-1", "任务A"));
    agent.followup(msg("m-2", "任务B"));
    agent.followup(msg("m-3", "任务C"));
    assert_eq!(agent.pending_count(), 3);

    // 取消中间一条
    assert!(agent.cancel_message("m-2"));
    assert!(!agent.cancel_message("m-2"), "重复取消应为 false");
    assert_eq!(agent.pending_count(), 2);

    agent.when_idle().await;

    let texts = user_texts(agent.as_ref());
    assert_eq!(
        texts,
        vec!["任务A", "任务C"],
        "被取消的消息不应进入会话: {texts:?}"
    );
}

#[tokio::test]
async fn cancel_message_misses_consumed_and_unknown() {
    let root = Context::root();
    let registry = AgentRegistry::new(&root);
    root.provide(registry.clone()).unwrap();
    registry.set_factory(Arc::new(LoopFactory)).unwrap();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::text("好"), MockReply::text("好")],
    ));
    let agent = registry
        .create(CreateAgentOptions {
            session_id: "cancel-miss".into(),
            options: AgentOptions {
                provider: Some("mock".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter,
        })
        .await
        .unwrap();

    // 已开始处理的消息（不在队列）与未知 id 都取消不到
    agent.followup(msg("m-1", "第一条"));
    agent.when_idle().await;
    assert!(!agent.cancel_message("m-1"), "已消费的消息不可取消");
    assert!(!agent.cancel_message("m-99"), "未知 id 不可取消");
}
