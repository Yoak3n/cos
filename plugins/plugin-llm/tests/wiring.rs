//! plugin-llm 验收：配置驱动装配（providers + chains）→ LlmRegistry；fail loud。

use std::sync::Arc;

use cos_core::{Context, Plugin};
use cos_llm::{LlmAdapter, LlmRegistry};
use cos_llm_mock::MockAdapter;
use plugin_llm::{ChainEntry, LlmConfig, LlmPlugin, ProviderEntry};
use serde_json::json;

#[test]
fn apply_registers_providers_and_chains() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();

    LlmPlugin
        .apply(
            &ctx,
            &LlmConfig {
                providers: vec![
                    ProviderEntry {
                        id: "m1".into(),
                        kind: "mock".into(),
                        config: json!({}),
                    },
                    ProviderEntry {
                        id: "m2".into(),
                        kind: "mock".into(),
                        config: json!({}),
                    },
                ],
                chains: vec![ChainEntry {
                    id: "c1".into(),
                    providers: vec!["m1".into(), "m2".into()],
                }],
            },
        )
        .unwrap();

    let registry = ctx.get::<LlmRegistry>().unwrap();
    assert_eq!(
        registry.list(),
        vec![
            ("m1".to_string(), "mock".to_string()),
            ("m2".to_string(), "mock".to_string()),
        ]
    );
    // 链解析 → FallbackAdapter（id "fallback"）
    let chain = registry.resolve_id("c1").unwrap();
    assert_eq!(chain.id(), "fallback");
    // 单提供商直取
    assert_eq!(registry.resolve_id("m1").unwrap().id(), "mock");
}

#[test]
fn apply_fails_loud_on_unknown_kind() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let result = LlmPlugin.apply(
        &ctx,
        &LlmConfig {
            providers: vec![ProviderEntry {
                id: "x".into(),
                kind: "nope".into(),
                config: json!({}),
            }],
            chains: vec![],
        },
    );
    assert!(result.is_err(), "未知 kind 必须 fail loud");
}

#[test]
fn apply_fails_loud_on_duplicate_and_unknown_chain_ref() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let base = LlmConfig {
        providers: vec![ProviderEntry {
            id: "m1".into(),
            kind: "mock".into(),
            config: json!({}),
        }],
        chains: vec![],
    };
    LlmPlugin.apply(&ctx, &base).unwrap();
    // 重复 id
    assert!(LlmPlugin.apply(&ctx, &base).is_err());
    // 链引用未注册
    let bad = LlmConfig {
        providers: vec![],
        chains: vec![ChainEntry {
            id: "c".into(),
            providers: vec!["ghost".into()],
        }],
    };
    assert!(LlmPlugin.apply(&ctx, &bad).is_err());
}

#[test]
fn mock_provider_fails_when_consumed() {
    // 空脚本 mock 是后备链测试的关键语义：任何调用即失败
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new("mock", vec![]));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use futures::StreamExt;
        let mut stream = adapter.stream(&cos_llm::LlmRequest {
            system: None,
            messages: vec![],
            tools: vec![],
        });
        let item = stream.next().await.unwrap();
        assert!(item.is_err());
    });
}
