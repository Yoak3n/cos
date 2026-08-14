//! plugin-llm 验收：配置驱动装配（providers + chains）→ LlmRegistry；plugin/kind/group 解析；
//! 省略模型 = 目录全量/按组展开；fail loud。

use std::sync::Arc;

use cos_core::{Context, Plugin};
use cos_llm::{LlmAdapter, LlmRegistry};
use cos_test_support::MockAdapter;
use plugin_llm::{ChainEntry, LlmConfig, LlmPlugin, ProviderEntry};
use serde_json::json;

// 插件名 → kind 映射（provider_plugin! 的等价物，测试二进制内静态收集）
inventory::submit! {
    cos_llm::ProviderPluginEntry { plugin_name: "test-provider", kind: "mock" }
}

/// 装配带 "mock" 工厂 + "test-provider" 插件映射的注册表（模拟 Provider 插件已声明——
/// plugin-opencode 走同一 `register_factory_with_catalog`/`provider_plugin!` 接口）。
/// 目录：mock-model（组 go）/ mock-model-2（组 go）/ mock-model-3（组 zen）。
fn provide_registry_with_mock_factory(ctx: &Context) {
    ctx.provide(LlmRegistry::new(ctx)).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    registry
        .register_factory_with_catalog(
            "mock",
            |_| Ok(Arc::new(MockAdapter::new("mock", vec![])) as Arc<dyn LlmAdapter>),
            serde_json::Value::Null,
            vec![
                cos_llm::ModelDefaults {
                    model: "mock-model".into(),
                    group: Some("go".into()),
                    defaults: json!({}),
                },
                cos_llm::ModelDefaults {
                    model: "mock-model-2".into(),
                    group: Some("go".into()),
                    defaults: json!({}),
                },
                cos_llm::ModelDefaults {
                    model: "mock-model-3".into(),
                    group: Some("zen".into()),
                    defaults: json!({}),
                },
            ],
        )
        .unwrap();
}

fn entry(
    id: &str,
    kind: Option<&str>,
    plugin: Option<&str>,
    group: Option<&str>,
    model: Option<&str>,
    models: Option<&[&str]>,
    config: serde_json::Value,
) -> ProviderEntry {
    ProviderEntry {
        id: id.into(),
        kind: kind.map(str::to_string),
        plugin: plugin.map(str::to_string),
        group: group.map(str::to_string),
        model: model.map(str::to_string),
        models: models.map(|ms| ms.iter().map(|m| m.to_string()).collect()),
        config,
    }
}

#[test]
fn apply_registers_providers_and_chains() {
    let ctx = Context::root();
    provide_registry_with_mock_factory(&ctx);

    LlmPlugin
        .apply(
            &ctx,
            &LlmConfig {
                providers: vec![
                    entry("m1", Some("mock"), None, None, None, None, json!({})),
                    entry("m2", Some("mock"), None, None, None, None, json!({})),
                ],
                chains: vec![ChainEntry {
                    id: "c1".into(),
                    providers: vec!["m1".into(), "m2".into()],
                }],
            },
        )
        .unwrap();

    let registry = ctx.get::<LlmRegistry>().unwrap();
    // 省略模型 → 目录全量展开（mock-model / mock-model-2 / mock-model-3）
    assert_eq!(
        registry.list(),
        vec![
            ("m1.mock-model".to_string(), "mock".to_string()),
            ("m1.mock-model-2".to_string(), "mock".to_string()),
            ("m1.mock-model-3".to_string(), "mock".to_string()),
            ("m2.mock-model".to_string(), "mock".to_string()),
            ("m2.mock-model-2".to_string(), "mock".to_string()),
            ("m2.mock-model-3".to_string(), "mock".to_string()),
        ]
    );
    // 链引用组 → 展开为组内模型按序
    let chain = registry.resolve_id("c1").unwrap();
    assert_eq!(chain.id(), "fallback");
    // 组内单模型直取
    assert_eq!(registry.resolve_id("m1.mock-model").unwrap().id(), "mock");
}

/// plugin 引用方式：插件名 → kind 静态映射解析；model 命中目录。
#[test]
fn apply_resolves_plugin_reference_and_validates_model() {
    let ctx = Context::root();
    provide_registry_with_mock_factory(&ctx);
    LlmPlugin
        .apply(
            &ctx,
            &LlmConfig {
                providers: vec![entry(
                    "main",
                    None,
                    Some("test-provider"),
                    None,
                    Some("mock-model"),
                    None,
                    json!({}),
                )],
                chains: vec![],
            },
        )
        .unwrap();
    assert!(
        ctx.get::<LlmRegistry>().unwrap().get("main").is_some(),
        "plugin 引用应解析 kind 并注册"
    );

    // 模型未命中目录 → fail loud 列出可用模型
    let bad = LlmConfig {
        providers: vec![entry(
            "bad",
            None,
            Some("test-provider"),
            None,
            Some("ghost"),
            None,
            json!({}),
        )],
        chains: vec![],
    };
    let message = LlmPlugin.apply(&ctx, &bad).unwrap_err().to_string();
    assert!(message.contains("不在 Provider 插件目录中"), "{message}");
    assert!(message.contains("mock-model"), "应列出可用模型: {message}");

    // 未知插件 → fail loud 列出可用插件
    let bad = LlmConfig {
        providers: vec![entry(
            "bad",
            None,
            Some("nope"),
            None,
            None,
            None,
            json!({}),
        )],
        chains: vec![],
    };
    let message = LlmPlugin.apply(&ctx, &bad).unwrap_err().to_string();
    assert!(message.contains("未知 Provider 插件 'nope'"), "{message}");
    assert!(
        message.contains("test-provider"),
        "应列出可用插件: {message}"
    );

    // kind 与 plugin 互斥
    let bad = LlmConfig {
        providers: vec![entry(
            "bad",
            Some("mock"),
            Some("test-provider"),
            None,
            None,
            None,
            json!({}),
        )],
        chains: vec![],
    };
    let message = LlmPlugin.apply(&ctx, &bad).unwrap_err().to_string();
    assert!(message.contains("二选一"), "{message}");

    // 都没有 → fail loud
    let bad = LlmConfig {
        providers: vec![entry("bad", None, None, None, None, None, json!({}))],
        chains: vec![],
    };
    assert!(LlmPlugin.apply(&ctx, &bad).is_err());
}

/// group 选择：省略模型 + group → 只展开该组的模型（组间不串）；未知组 fail loud。
#[test]
fn apply_group_selects_models_and_validates_group() {
    let ctx = Context::root();
    provide_registry_with_mock_factory(&ctx);
    LlmPlugin
        .apply(
            &ctx,
            &LlmConfig {
                providers: vec![
                    entry(
                        "go",
                        None,
                        Some("test-provider"),
                        Some("go"),
                        None,
                        None,
                        json!({}),
                    ),
                    entry(
                        "zen",
                        None,
                        Some("test-provider"),
                        Some("zen"),
                        None,
                        None,
                        json!({}),
                    ),
                ],
                chains: vec![],
            },
        )
        .unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    // go 组两个模型展开为 go.mock-model / go.mock-model-2；zen 组单模型注册条目 id "zen"
    assert!(registry.get("go.mock-model").is_some());
    assert!(registry.get("go.mock-model-2").is_some());
    assert!(registry.get("go.mock-model-3").is_none(), "组间不串");
    assert!(registry.get("zen").is_some());
    assert!(
        registry.get("zen.mock-model-3").is_none(),
        "单模型注册条目 id"
    );
    assert!(registry.get("zen.mock-model").is_none(), "组间不串");

    // 未知组 → fail loud 列出可用分组
    let bad = LlmConfig {
        providers: vec![entry(
            "bad",
            None,
            Some("test-provider"),
            Some("nope"),
            None,
            None,
            json!({}),
        )],
        chains: vec![],
    };
    let message = LlmPlugin.apply(&ctx, &bad).unwrap_err().to_string();
    assert!(message.contains("未知分组 'nope'"), "{message}");
    assert!(message.contains("可用分组"), "{message}");
    assert!(
        message.contains("go") && message.contains("zen"),
        "{message}"
    );
}

#[test]
fn apply_fails_loud_on_unknown_kind() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let result = LlmPlugin.apply(
        &ctx,
        &LlmConfig {
            providers: vec![entry("x", Some("nope"), None, None, None, None, json!({}))],
            chains: vec![],
        },
    );
    assert!(result.is_err(), "未知 kind 必须 fail loud");
}

#[test]
fn apply_expands_env_vars_in_provider_config() {
    // Rust 2024：set_var 为 unsafe（进程级副作用，测试内使用）
    unsafe { std::env::set_var("COS_TEST_LLM_KEY", "sk-test-env") };
    let ctx = Context::root();
    provide_registry_with_mock_factory(&ctx);
    LlmPlugin
        .apply(
            &ctx,
            &LlmConfig {
                providers: vec![entry(
                    "env-provider",
                    Some("mock"),
                    None,
                    None,
                    None,
                    None,
                    json!({ "note": "${COS_TEST_LLM_KEY}" }),
                )],
                chains: vec![],
            },
        )
        .unwrap();
    assert!(
        ctx.get::<LlmRegistry>()
            .unwrap()
            .get("env-provider.mock-model")
            .is_some(),
        "省略模型 → 目录全量展开（env 展开仍生效）"
    );

    // 缺失环境变量 → fail loud
    let bad = LlmConfig {
        providers: vec![entry(
            "bad",
            Some("mock"),
            None,
            None,
            None,
            None,
            json!({ "note": "${COS_TEST_MISSING_VAR}" }),
        )],
        chains: vec![],
    };
    let result = LlmPlugin.apply(&ctx, &bad);
    assert!(result.is_err(), "引用了未设置的环境变量必须 fail loud");
    let message = result.unwrap_err().to_string();
    assert!(message.contains("COS_TEST_MISSING_VAR"), "{message}");
}

#[test]
fn apply_fails_loud_on_duplicate_and_unknown_chain_ref() {
    let ctx = Context::root();
    provide_registry_with_mock_factory(&ctx);
    let base = LlmConfig {
        providers: vec![entry("m1", Some("mock"), None, None, None, None, json!({}))],
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

/// 无分组的 Provider（目录模型 group 全为 None）上用了 `group:` → fail loud，
/// 提示"未定义任何分组"并列出可用模型（而非空列表的"未知分组"）。
#[test]
fn apply_group_on_groupless_provider_fails_with_models_listed() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    registry
        .register_factory_with_catalog(
            "mock-nogroup",
            |_| Ok(Arc::new(MockAdapter::new("mock", vec![])) as Arc<dyn LlmAdapter>),
            serde_json::Value::Null,
            vec![
                cos_llm::ModelDefaults {
                    model: "only-a".into(),
                    group: None,
                    defaults: json!({}),
                },
                cos_llm::ModelDefaults {
                    model: "only-b".into(),
                    group: None,
                    defaults: json!({}),
                },
            ],
        )
        .unwrap();
    let bad = LlmConfig {
        providers: vec![entry(
            "bad",
            Some("mock-nogroup"),
            None,
            Some("go"),
            None,
            None,
            json!({}),
        )],
        chains: vec![],
    };
    let message = LlmPlugin.apply(&ctx, &bad).unwrap_err().to_string();
    assert!(message.contains("未定义任何分组"), "{message}");
    assert!(message.contains("only-a"), "应列出可用模型: {message}");
    assert!(message.contains("only-b"), "应列出可用模型: {message}");
    // 工厂未注册（kind 无目录）→ 提示插件声明顺序
    let missing = LlmConfig {
        providers: vec![entry(
            "bad",
            Some("never-registered"),
            None,
            Some("go"),
            None,
            None,
            json!({}),
        )],
        chains: vec![],
    };
    let message = LlmPlugin.apply(&ctx, &missing).unwrap_err().to_string();
    assert!(message.contains("没有模型目录"), "{message}");
    assert!(message.contains("在 llm 之前声明"), "{message}");
}

/// 省略模型 = 插件目录全量展开：每个模型注册 <id>.<model>，条目 id 成为组链。
#[test]
fn apply_omitted_models_expand_full_catalog() {
    let ctx = Context::root();
    provide_registry_with_mock_factory(&ctx);
    LlmPlugin
        .apply(
            &ctx,
            &LlmConfig {
                providers: vec![
                    entry(
                        "go",
                        None,
                        Some("test-provider"),
                        None,
                        None,
                        None,
                        json!({}),
                    ),
                    entry(
                        "zen",
                        None,
                        Some("test-provider"),
                        None,
                        None,
                        Some(&["mock-model-2"]),
                        json!({}),
                    ),
                ],
                chains: vec![ChainEntry {
                    id: "main".into(),
                    providers: vec!["go".into(), "zen".into()],
                }],
            },
        )
        .unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    // 全量展开：目录三个模型 → go.mock-model / go.mock-model-2 / go.mock-model-3；
    // 显式单模型 → 注册 id = zen
    assert!(registry.get("go.mock-model").is_some());
    assert!(registry.get("go.mock-model-2").is_some());
    assert!(registry.get("go.mock-model-3").is_some());
    assert!(registry.get("zen").is_some());
    assert!(
        registry.get("zen.mock-model").is_none(),
        "显式 models 不应全量展开"
    );
    // 组 id 不是 provider（由 chains 引用展开）
    assert!(registry.get("go").is_none());
    // 链引用组 → 展开为组内模型按序
    let main = registry.resolve_id("main").unwrap();
    assert_eq!(main.id(), "fallback", "组引用应解析为后备链");
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
