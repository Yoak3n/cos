//! 推理增量（ChunkDelta::Thinking）→ assistant 消息独立成块（ContentBlock::Thinking），
//! text() 只含正文（REPL/RPC 摘要不受思考污染）。

use std::sync::Arc;

use cos_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::Context;
use cos_llm::{ChunkDelta, ContentBlock, LlmAdapter, StreamChunk, UserMessage};
use cos_session::SessionEventData;
use cos_test_support::{MockAdapter, MockReply};

#[tokio::test]
async fn thinking_chunks_are_assembled_into_separate_blocks() {
    let root = Context::root();
    let registry = AgentRegistry::new(&root);
    root.provide(registry.clone()).unwrap();
    registry.set_factory(Arc::new(LoopFactory)).unwrap();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::new(vec![
            StreamChunk {
                delta: ChunkDelta::Thinking {
                    text: "思考".into(),
                },
                usage: None,
            },
            StreamChunk::text("正文"),
        ])],
    ));
    let agent = registry
        .create(CreateAgentOptions {
            session_id: "thinking".into(),
            options: AgentOptions {
                provider: Some("mock".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter,
        })
        .await
        .unwrap();

    agent.followup(UserMessage::new("你好"));
    agent.when_idle().await;

    let events = agent.session().events();
    let assistant = events
        .iter()
        .find_map(|event| match &event.data {
            SessionEventData::AssistantMessage { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("应有 assistant 消息");
    assert_eq!(assistant.content.len(), 2, "思考与正文分开成块");
    assert!(
        matches!(&assistant.content[0], ContentBlock::Thinking { text } if text == "思考"),
        "第一块应为思考块: {:?}",
        assistant.content
    );
    assert!(
        matches!(&assistant.content[1], ContentBlock::Text { text } if text == "正文"),
        "第二块应为文本块: {:?}",
        assistant.content
    );
    assert_eq!(assistant.text(), "正文", "text() 只含正文（不受思考污染）");
}
