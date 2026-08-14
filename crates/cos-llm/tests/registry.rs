//! LLM 统一管理：LlmRegistry（工厂收集/注册/后备链）+ FallbackAdapter 切换语义。

use std::sync::Arc;

use cos_core::Context;
use cos_llm::{
    ChunkDelta, LlmAdapter, LlmError, LlmRegistry, LlmRequest, LlmStream, StreamChunk, UserMessage,
};
use cos_llm_mock::{MockAdapter, MockReply};
use futures::StreamExt;
use serde_json::json;

fn request() -> LlmRequest {
    LlmRequest {
        system: None,
        messages: vec![cos_llm::Message::User(UserMessage::new("hi"))],
        tools: vec![],
    }
}

/// 收集流式文本（忽略错误）。
async fn collect_text(adapter: Arc<dyn LlmAdapter>) -> (String, Vec<LlmError>) {
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    let mut errors = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                if let ChunkDelta::Text { text: delta } = chunk.delta {
                    text.push_str(&delta);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    (text, errors)
}

#[tokio::test]
async fn fallback_switches_to_next_on_pre_output_error() {
    let chain = Arc::new(cos_llm::FallbackAdapter {
        adapters: vec![
            Arc::new(MockAdapter::new("fail", vec![])), // 脚本耗尽 → 立即 Err
            Arc::new(MockAdapter::new("ok", vec![MockReply::text("你好")])),
        ],
    });
    let (text, errors) = collect_text(chain).await;
    assert_eq!(text, "你好", "主失败应自动切换到后备");
    assert!(errors.is_empty());
}

#[tokio::test]
async fn fallback_switches_on_empty_stream() {
    let chain = Arc::new(cos_llm::FallbackAdapter {
        adapters: vec![
            Arc::new(EmptyAdapter),
            Arc::new(MockAdapter::new("ok", vec![MockReply::text("兜底")])),
        ],
    });
    let (text, errors) = collect_text(chain).await;
    assert_eq!(text, "兜底", "空流同样视为未产出，应切换");
    assert!(errors.is_empty());
}

#[tokio::test]
async fn fallback_propagates_error_when_all_fail() {
    let chain = Arc::new(cos_llm::FallbackAdapter {
        adapters: vec![
            Arc::new(MockAdapter::new("fail1", vec![])),
            Arc::new(MockAdapter::new("fail2", vec![])),
        ],
    });
    let (text, errors) = collect_text(chain).await;
    assert!(text.is_empty());
    assert_eq!(errors.len(), 1, "全部失败 → 交付最后错误");
    assert!(errors[0].to_string().contains("脚本耗尽"), "{errors:?}");
}

#[tokio::test]
async fn fallback_propagates_mid_stream_error_without_switching() {
    // 已产出 chunk 后失败：原样传播，不切换到后备（避免内容重复）
    let chain = Arc::new(cos_llm::FallbackAdapter {
        adapters: vec![
            Arc::new(MidStreamErrorAdapter),
            Arc::new(MockAdapter::new("backup", vec![MockReply::text("B")])),
        ],
    });
    let (text, errors) = collect_text(chain).await;
    assert_eq!(text, "A", "已产出后失败不得切换到后备（避免内容重复）");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].to_string().contains("中途失败"), "{errors:?}");
}

/// 产出 1 个 chunk 后中途失败的适配器。
struct MidStreamErrorAdapter;

impl LlmAdapter for MidStreamErrorAdapter {
    fn id(&self) -> &str {
        "mid-error"
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        Box::pin(futures::stream::iter(vec![
            Ok(StreamChunk::text("A")),
            Err(LlmError::Failure("中途失败".to_string())),
        ]))
    }
}

/// 立即结束的空流适配器。
struct EmptyAdapter;

impl LlmAdapter for EmptyAdapter {
    fn id(&self) -> &str {
        "empty"
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn registry_register_get_list_and_errors() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    assert!(
        registry.factory_kinds().contains(&"mock"),
        "inventory 应收集到 cos-llm-mock 工厂: {:?}",
        registry.factory_kinds()
    );

    registry
        .register("a", Arc::new(MockAdapter::new("mock-a", vec![])))
        .unwrap();
    assert!(registry.get("a").is_some());
    assert!(registry.get("missing").is_none());
    assert_eq!(
        registry.list(),
        vec![("a".to_string(), "mock-a".to_string())]
    );
    // 同 id 拒绝（fail loud）
    assert!(
        registry
            .register("a", Arc::new(MockAdapter::new("mock-b", vec![])))
            .is_err()
    );
    // 未知 kind
    assert!(registry.build("nope", &json!({})).is_err());
    // 已知 kind + 配置
    assert!(registry.build("mock", &json!({})).is_ok());
}

#[tokio::test]
async fn registry_chains_resolve_and_fallback() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    registry
        .register("fail", Arc::new(MockAdapter::new("mock-fail", vec![])))
        .unwrap();
    registry
        .register(
            "ok",
            Arc::new(MockAdapter::new("mock-ok", vec![MockReply::text("链兜底")])),
        )
        .unwrap();

    // 链引用未注册提供商 → fail loud
    assert!(
        registry
            .register_chain("bad", vec!["missing".to_string()])
            .is_err()
    );
    registry
        .register_chain("c1", vec!["fail".to_string(), "ok".to_string()])
        .unwrap();

    // resolve_id：先单提供商，再链
    assert!(registry.resolve_id("ok").is_ok());
    let chain = registry.resolve_id("c1").unwrap();
    let (text, errors) = collect_text(chain).await;
    assert_eq!(text, "链兜底", "链内主失败应切到后备");
    assert!(errors.is_empty());

    // 未知链
    assert!(registry.resolve("nope").is_err());
}

#[tokio::test]
async fn registry_build_uses_inventory_factory() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    let adapter = registry.build("mock", &json!({})).unwrap();
    assert_eq!(adapter.id(), "mock");
    // 程序化注册工厂补充
    registry
        .register_factory("custom", |_| {
            Ok(Arc::new(MockAdapter::new("custom", vec![])))
        })
        .unwrap();
    assert!(registry.build("custom", &json!({})).is_ok());
    assert!(
        registry
            .register_factory("mock", |_| unreachable!())
            .is_err(),
        "同名工厂拒绝"
    );
}
