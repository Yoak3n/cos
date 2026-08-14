//! LLM 统一管理：LlmRegistry（工厂收集/注册/后备链）+ FallbackAdapter 切换语义。

use std::sync::Arc;

use cos_core::Context;
use cos_llm::{
    ChunkDelta, FallbackAdapter, InputContent, LlmAdapter, LlmError, LlmRegistry, LlmRequest,
    LlmStream, ModelDefaults, StreamChunk, UserMessage,
};
use cos_test_support::{MockAdapter, MockReply};
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
    let chain = Arc::new(FallbackAdapter::new(vec![
        Arc::new(MockAdapter::new("fail", vec![])), // 脚本耗尽 → 立即 Err
        Arc::new(MockAdapter::new("ok", vec![MockReply::text("你好")])),
    ]));
    let (text, errors) = collect_text(chain).await;
    assert_eq!(text, "你好", "主失败应自动切换到后备");
    assert!(errors.is_empty());
}

#[tokio::test]
async fn fallback_switches_on_empty_stream() {
    let chain = Arc::new(FallbackAdapter::new(vec![
        Arc::new(EmptyAdapter),
        Arc::new(MockAdapter::new("ok", vec![MockReply::text("兜底")])),
    ]));
    let (text, errors) = collect_text(chain).await;
    assert_eq!(text, "兜底", "空流同样视为未产出，应切换");
    assert!(errors.is_empty());
}

#[tokio::test]
async fn fallback_propagates_error_when_all_fail() {
    let chain = Arc::new(FallbackAdapter::new(vec![
        Arc::new(MockAdapter::new("fail1", vec![])),
        Arc::new(MockAdapter::new("fail2", vec![])),
    ]));
    let (text, errors) = collect_text(chain).await;
    assert!(text.is_empty());
    assert_eq!(errors.len(), 1, "全部失败 → 交付最后错误");
    assert!(errors[0].to_string().contains("脚本耗尽"), "{errors:?}");
}

#[tokio::test]
async fn fallback_propagates_mid_stream_error_without_switching() {
    // 已产出 chunk 后失败：原样传播，不切换到后备（避免内容重复）
    let chain = Arc::new(FallbackAdapter::new(vec![
        Arc::new(MidStreamErrorAdapter),
        Arc::new(MockAdapter::new("backup", vec![MockReply::text("B")])),
    ]));
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
    // Provider 工厂现为声明式（plugin-opencode 等插件 apply 时注册）；测试二进制无插件 → 空表
    assert!(
        registry.factory_kinds().is_empty(),
        "测试二进制不应收集到运行时 Provider 工厂: {:?}",
        registry.factory_kinds()
    );
    // 程序化注册工厂（register_factory 是 LlmRegistry 的扩展点）
    registry
        .register_factory("echo", |_| Ok(Arc::new(MockAdapter::new("echo", vec![]))))
        .unwrap();

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
    assert!(registry.build("echo", &json!({})).is_ok());
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
async fn registry_build_uses_registered_factory() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    // 程序化注册工厂（inventory 之外的补充路径；插件 apply 走同一条）
    registry
        .register_factory("p1", |_| Ok(Arc::new(MockAdapter::new("p1", vec![]))))
        .unwrap();
    let adapter = registry.build("p1", &json!({})).unwrap();
    assert_eq!(adapter.id(), "p1");
    // 同名拒绝（fail loud）
    assert!(
        registry.register_factory("p1", |_| unreachable!()).is_err(),
        "同名工厂拒绝"
    );
}

/// 视觉适配器（测试桩：声明 text + image）。
struct VisionAdapter;

impl LlmAdapter for VisionAdapter {
    fn id(&self) -> &str {
        "vision"
    }

    fn input_content(&self) -> &[InputContent] {
        &[InputContent::Text, InputContent::Image]
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn registry_capabilities_supports_and_by_capability() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    registry
        .register("text-only", Arc::new(MockAdapter::new("mock-t", vec![])))
        .unwrap();
    registry
        .register("vision", Arc::new(VisionAdapter))
        .unwrap();

    // 缺省能力 = text；视觉适配器声明 text + image
    assert!(registry.supports("text-only", InputContent::Text));
    assert!(!registry.supports("text-only", InputContent::Image));
    assert!(registry.supports("vision", InputContent::Image));

    // 按能力路由查询
    assert_eq!(registry.by_capability(InputContent::Image), vec!["vision"]);
    assert!(
        registry
            .by_capability(InputContent::Text)
            .contains(&"text-only".to_string())
    );

    // 链能力 = 成员并集
    registry
        .register_chain("c1", vec!["text-only".to_string(), "vision".to_string()])
        .unwrap();
    let caps = registry.capabilities("c1").unwrap();
    assert!(caps.contains(&InputContent::Text) && caps.contains(&InputContent::Image));

    // FallbackAdapter::new 同样计算并集
    let fallback = FallbackAdapter::new(vec![
        Arc::new(MockAdapter::new("t", vec![])),
        Arc::new(VisionAdapter),
    ]);
    assert_eq!(
        fallback.input_content(),
        &[InputContent::Text, InputContent::Image]
    );

    // 未知 id
    assert!(registry.capabilities("nope").is_none());
    assert!(!registry.supports("nope", InputContent::Text));
}

/// 默认配置（Provider 插件下沉的公共字段）：build 时浅合并，条目 config 覆盖默认。
#[tokio::test]
async fn factory_defaults_merge_under_provider_config() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    // 捕获合并后 config 的测试工厂：把 base_url|model 编进 adapter id 便于断言
    registry
        .register_factory_with_defaults(
            "with-defaults",
            |config| {
                let base = config
                    .get("base_url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let model = config
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                Ok(Arc::new(MockAdapter::new(
                    format!("{base}|{model}"),
                    vec![],
                )))
            },
            json!({ "base_url": "https://default/v1", "model": "default-model" }),
        )
        .unwrap();

    // 条目只填差异字段 → 默认补齐
    assert_eq!(
        registry
            .build("with-defaults", &json!({ "model": "mine" }))
            .unwrap()
            .id(),
        "https://default/v1|mine"
    );
    // 条目显式覆盖默认
    assert_eq!(
        registry
            .build(
                "with-defaults",
                &json!({ "base_url": "https://override/v1" })
            )
            .unwrap()
            .id(),
        "https://override/v1|default-model"
    );
    // 无默认的普通注册不受影响
    registry
        .register_factory("plain", |_| Ok(Arc::new(MockAdapter::new("plain", vec![]))))
        .unwrap();
    assert_eq!(registry.build("plain", &json!({})).unwrap().id(), "plain");
}

/// 模型目录（Provider 插件的可用模型清单）：三级合并 插件级 < 模型级 < 条目。
#[tokio::test]
async fn catalog_merges_model_level_defaults() {
    let ctx = Context::root();
    let registry = LlmRegistry::new(&ctx);
    registry
        .register_factory_with_catalog(
            "cataloged",
            |config| {
                // base_url|api_style|model 编进 id 便于断言
                let base = config
                    .get("base_url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let style = config
                    .get("api_style")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let model = config
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                Ok(Arc::new(MockAdapter::new(
                    format!("{base}|{style}|{model}"),
                    vec![],
                )))
            },
            json!({ "base_url": "https://fallback/v1", "api_style": "openai" }),
            vec![
                ModelDefaults {
                    model: "go-model".into(),
                    group: Some("go".into()),
                    defaults: json!({ "base_url": "https://go/v1", "max_tokens": 4096 }),
                },
                ModelDefaults {
                    model: "zen-model".into(),
                    group: Some("zen".into()),
                    defaults: json!({ "base_url": "https://zen/v1", "api_style": "anthropic" }),
                },
            ],
        )
        .unwrap();

    // 分组查询：组标签与按组模型列表
    assert_eq!(registry.available_groups("cataloged"), vec!["go", "zen"]);
    assert_eq!(
        registry.models_in_group("cataloged", "go"),
        vec!["go-model"]
    );
    assert_eq!(
        registry.models_in_group("cataloged", "zen"),
        vec!["zen-model"]
    );
    assert!(registry.models_in_group("cataloged", "nope").is_empty());

    // 目录命中：模型级默认覆盖插件级（api_style 继承插件级 openai）
    assert_eq!(
        registry
            .build("cataloged", &json!({ "model": "go-model" }))
            .unwrap()
            .id(),
        "https://go/v1|openai|go-model"
    );
    // 另一模型：自带 api_style
    assert_eq!(
        registry
            .build("cataloged", &json!({ "model": "zen-model" }))
            .unwrap()
            .id(),
        "https://zen/v1|anthropic|zen-model"
    );
    // 条目显式覆盖模型级
    assert_eq!(
        registry
            .build(
                "cataloged",
                &json!({ "model": "go-model", "base_url": "https://override/v1" })
            )
            .unwrap()
            .id(),
        "https://override/v1|openai|go-model"
    );
    // 目录未命中：回落到插件级默认
    assert_eq!(
        registry
            .build("cataloged", &json!({ "model": "ghost" }))
            .unwrap()
            .id(),
        "https://fallback/v1|openai|ghost"
    );
}
