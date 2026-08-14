//! plugin-custom-provider 验收：注册 `custom` kind；纯配置接任意 OpenAI 兼容端点
//! （真实请求打到本地回环服务器）；defaults 合并 + `${ENV_VAR}` 展开。

use cos_core::{Context, Plugin};
use cos_llm::{ChunkDelta, LlmRegistry, LlmRequest, Message, UserMessage};
use cos_test_support::{ChatReply, ScriptedChatServer};
use futures::StreamExt;
use plugin_custom_provider::{CUSTOM_KIND, CustomProviderConfig, CustomProviderPlugin};
use serde_json::json;

fn request() -> LlmRequest {
    LlmRequest {
        system: None,
        messages: vec![Message::User(UserMessage::new("你好"))],
        tools: vec![],
    }
}

#[test]
fn apply_registers_custom_kind() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let config = CustomProviderConfig {
        defaults: None,
        api_key: None,
        models: vec![],
    };
    CustomProviderPlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    assert!(registry.factory_kinds().contains(&CUSTOM_KIND));
    // 全配置在条目里 → 构造成功（不联网）
    assert!(
        registry
            .build(
                CUSTOM_KIND,
                &json!({ "base_url": "http://127.0.0.1:1/v1", "api_key": "k", "model": "m" })
            )
            .is_ok()
    );
}

/// defaults 下沉 base_url（含 ${ENV_VAR} 展开）→ 条目只填差异字段 → 真实请求打到本地端点。
#[tokio::test]
async fn defaults_merge_and_env_expansion_reach_local_endpoint() {
    unsafe { std::env::set_var("COS_TEST_CUSTOM_KEY", "sk-test") };
    let server = ScriptedChatServer::spawn(vec![ChatReply::Text("你好，世界".into())]).await;
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let config = CustomProviderConfig {
        defaults: Some(json!({
            "base_url": format!("http://127.0.0.1:{}/v1", server.port),
            "api_key": "${COS_TEST_CUSTOM_KEY}",
            "model": "m",
            "streaming": false,
        })),
        api_key: None,
        models: vec![],
    };
    CustomProviderPlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    // 条目零配置 → 全部来自 defaults（含 env 展开后的 api_key）
    let adapter = registry.build(CUSTOM_KIND, &json!({})).unwrap();
    let mut stream = adapter.stream(&request());
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.unwrap();
        if let ChunkDelta::Text { text: delta } = chunk.delta {
            text.push_str(&delta);
        }
    }
    assert_eq!(text, "你好，世界");
    server.join().await;
}

/// 插件级 api_key 字段（${ENV_VAR} 展开）：条目省略 api_key → 真实请求携带插件配置的 key。
#[tokio::test]
async fn plugin_api_key_field_reaches_endpoint() {
    unsafe { std::env::set_var("COS_TEST_CUSTOM_PLUGIN_KEY", "sk-plugin-custom") };
    let server = ScriptedChatServer::spawn_with_key(
        vec![ChatReply::Text("ok".into())],
        Some("sk-plugin-custom".into()),
    )
    .await;
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let config = CustomProviderConfig {
        defaults: Some(json!({
            "base_url": format!("http://127.0.0.1:{}/v1", server.port),
            "model": "m",
            "streaming": false,
        })),
        api_key: Some("${COS_TEST_CUSTOM_PLUGIN_KEY}".into()),
        models: vec![],
    };
    CustomProviderPlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    // 条目零密钥 → 插件级 api_key（展开后）经三级合并进入适配器
    let adapter = registry.build(CUSTOM_KIND, &json!({})).unwrap();
    let mut stream = adapter.stream(&request());
    while let Some(item) = stream.next().await {
        let _ = item.unwrap();
    }
    server.join().await; // 服务器断言 Authorization 通过即证明
}
