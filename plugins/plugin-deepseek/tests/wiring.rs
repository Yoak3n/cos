//! plugin-deepseek 验收：官方端点解析、内置模型目录（无分组）+ config.models 覆盖、
//! 插件级 api_key（真实请求验证）、apply 注册带目录工厂。

use cos_core::{Context, Plugin};
use cos_llm::{LlmRegistry, LlmRequest, ModelDefaults};
use cos_test_support::{ChatReply, ScriptedChatServer};
use futures::StreamExt;
use plugin_deepseek::{
    BUILTIN_MODELS, DEEPSEEK_BASE_URL, DEEPSEEK_KIND, DeepseekPlugin, DeepseekPluginConfig,
    catalog_with, resolve_base_url,
};
use serde_json::json;

#[test]
fn base_url_resolves_to_official_default() {
    // 缺省 → 官方 API 端点
    assert_eq!(resolve_base_url(None), DEEPSEEK_BASE_URL);
    // 显式 base_url 覆盖
    assert_eq!(
        resolve_base_url(Some("https://my-gateway/v1")),
        "https://my-gateway/v1"
    );
}

#[test]
fn builtin_catalog_covers_official_models() {
    // 官方两模型：deepseek-v4-flash / deepseek-v4-pro，共用官方端点，无分组
    let flash = BUILTIN_MODELS
        .iter()
        .find(|entry| entry.model == "deepseek-v4-flash")
        .expect("内置 v4-flash 模型");
    assert_eq!(flash.base_url, DEEPSEEK_BASE_URL);
    assert_eq!(flash.api_style, "openai");
    let pro = BUILTIN_MODELS
        .iter()
        .find(|entry| entry.model == "deepseek-v4-pro")
        .expect("内置 pro 模型");
    assert_eq!(pro.base_url, DEEPSEEK_BASE_URL);
    // 旗舰模型预算给足（思考吃预算，以免正文被裁空）
    assert!(pro.max_tokens >= flash.max_tokens);
    // 转 ModelDefaults 后字段齐备且不分组
    let defaults = flash.to_model_defaults();
    assert_eq!(defaults.group, None);
    assert_eq!(defaults.defaults["streaming"], true);
    assert_eq!(defaults.defaults["max_tokens"], flash.max_tokens);
    // 可用模型查询 = 目录全量
    assert_eq!(
        plugin_deepseek::available_models(&[]),
        vec![
            "deepseek-v4-flash".to_string(),
            "deepseek-v4-pro".to_string()
        ]
    );
}

#[test]
fn config_models_override_builtin_catalog() {
    let extra = vec![ModelDefaults {
        model: "deepseek-v4-flash".into(),
        group: None,
        defaults: json!({ "base_url": "https://my-gateway/v1", "api_style": "openai" }),
    }];
    let merged = catalog_with(BUILTIN_MODELS, extra);
    // 拼接语义：内置 + 扩展（同名模型去重发生在注册表收集时——BTreeMap 后者生效）
    assert_eq!(merged.len(), BUILTIN_MODELS.len() + 1);
    // 其余内置条目保留
    assert!(merged.iter().any(|entry| entry.model == "deepseek-v4-pro"));
}

#[test]
fn apply_registers_kind_with_catalog() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let config = DeepseekPluginConfig {
        base_url: None,
        api_key: None,
        models: vec![],
    };
    DeepseekPlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    assert!(registry.factory_kinds().contains(&DEEPSEEK_KIND));
    // 目录命中：条目只写 model + api_key 即可构建（构造不联网）
    assert!(
        registry
            .build(
                DEEPSEEK_KIND,
                &json!({ "model": "deepseek-v4-flash", "api_key": "k" })
            )
            .is_ok()
    );
    // 未知模型也能构建（回落到插件级官方端点默认）
    assert!(
        registry
            .build(DEEPSEEK_KIND, &json!({ "model": "ghost", "api_key": "k" }))
            .is_ok()
    );
}

/// 插件级 api_key（${ENV_VAR} 展开）：条目省略 api_key → 真实请求携带插件配置的 key。
#[tokio::test]
async fn plugin_api_key_is_used_when_entry_omits_it() {
    unsafe { std::env::set_var("COS_TEST_DS_KEY", "sk-plugin") };
    // 服务器校验 Authorization 头——证明 api_key 来自插件配置（条目未写）
    let server = ScriptedChatServer::spawn_with_key(
        vec![ChatReply::Text("你好".into())],
        Some("sk-plugin".into()),
    )
    .await;
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let config = DeepseekPluginConfig {
        base_url: Some(format!("http://127.0.0.1:{}/v1", server.port)),
        api_key: Some("${COS_TEST_DS_KEY}".into()),
        models: vec![],
    };
    DeepseekPlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    // 未命中目录 → 插件级默认（端点覆盖 + api_key 展开）；条目零密钥
    let adapter = registry
        .build(DEEPSEEK_KIND, &json!({ "model": "ghost" }))
        .unwrap();
    let mut stream = adapter.stream(&LlmRequest::default());
    while let Some(item) = stream.next().await {
        let _ = item.unwrap();
    }
    server.join().await; // 服务器断言 Authorization 通过即证明
}
