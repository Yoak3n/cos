//! P3 验收：mock 适配器按序号选择预设输出、流式产出 chunk、确定性、耗尽报错。

use dsh_llm::{ChunkDelta, LlmAdapter, LlmError, LlmRequest, Message, StreamChunk, UserMessage};
use dsh_llm_mock::{MockAdapter, MockReply};
use futures::StreamExt;

fn request() -> LlmRequest {
    LlmRequest {
        system: None,
        messages: vec![Message::User(UserMessage::new("hi"))],
        tools: vec![],
    }
}

#[tokio::test]
async fn mock_streams_scripted_chunks_by_sequence() {
    let adapter = MockAdapter::new(
        "mock-1",
        vec![
            MockReply::new(vec![StreamChunk::text("你"), StreamChunk::text("好")]),
            MockReply::text("回"),
        ],
    );

    let first: Vec<_> = adapter.stream(&request()).collect().await;
    assert_eq!(first.len(), 2);
    assert_eq!(
        first[0].as_ref().unwrap().delta,
        ChunkDelta::Text { text: "你".into() }
    );
    assert_eq!(
        first[1].as_ref().unwrap().delta,
        ChunkDelta::Text { text: "好".into() }
    );

    let second: Vec<_> = adapter.stream(&request()).collect().await;
    assert_eq!(second.len(), 1);
    assert_eq!(
        second[0].as_ref().unwrap().delta,
        ChunkDelta::Text { text: "回".into() }
    );

    // 脚本耗尽 → 流产出 Err（带可读信息）
    let third: Vec<_> = adapter.stream(&request()).collect().await;
    assert_eq!(third.len(), 1);
    assert!(
        matches!(&third[0], Err(LlmError::Failure(message)) if message.contains("耗尽")),
        "实际 {:?}",
        third[0]
    );
    assert!(adapter.exhausted());

    // 重置回放
    adapter.reset();
    assert!(!adapter.exhausted());
    let replay: Vec<_> = adapter.stream(&request()).collect().await;
    assert_eq!(replay.len(), 2);
}

#[tokio::test]
async fn mock_is_deterministic_across_instances() {
    let script = vec![MockReply::text("ab"), MockReply::text("cd")];
    let a = MockAdapter::new("a", script.clone());
    let b = MockAdapter::new("b", script);

    let out_a: Vec<_> = a.stream(&request()).collect().await;
    let out_b: Vec<_> = b.stream(&request()).collect().await;
    assert_eq!(out_a, out_b);

    let out_a2: Vec<_> = a.stream(&request()).collect().await;
    let out_b2: Vec<_> = b.stream(&request()).collect().await;
    assert_eq!(out_a2, out_b2);
}
