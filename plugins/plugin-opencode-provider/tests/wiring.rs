//! plugin-opencode 验收：套餐端点解析（plan: go|zen）、非法套餐校验、内置模型目录
//! （go/zen 分离，**多 api style**：openai/anthropic/responses）+ config.models 覆盖、
//! models_endpoint 拉取、插件级 api_key（真实请求验证）、apply 注册带目录工厂。

use cos_core::{Context, Plugin, Validate};
use cos_llm::{LlmRegistry, LlmRequest, ModelDefaults};
use cos_test_support::{ChatReply, ScriptedChatServer};
use futures::StreamExt;
use plugin_opencode_provider::{
    BUILTIN_MODELS, GO_BASE_URL, OPENCODE_KIND, OpencodePlugin, OpencodePluginConfig, ZEN_BASE_URL,
    catalog_with, resolve_base_url,
};
use serde_json::json;

fn config(plan: Option<&str>) -> OpencodePluginConfig {
    OpencodePluginConfig {
        plan: plan.map(str::to_string),
        base_url: None,
        api_key: None,
        models: vec![],
        models_endpoint: None,
        models_api_style: None,
    }
}

#[test]
fn plan_resolves_to_plan_endpoints() {
    // 缺省 plan → go 套餐端点
    assert_eq!(resolve_base_url(None, None), GO_BASE_URL);
    assert_eq!(resolve_base_url(Some("go"), None), GO_BASE_URL);
    // zen 套餐
    assert_eq!(resolve_base_url(Some("zen"), None), ZEN_BASE_URL);
    // 显式 base_url 覆盖套餐
    assert_eq!(
        resolve_base_url(Some("zen"), Some("https://my-gateway/v1")),
        "https://my-gateway/v1"
    );
}

#[test]
fn builtin_catalog_covers_go_and_zen_models_with_own_endpoints() {
    // go 模型 → go 端点；zen 模型 → zen 端点（目录按模型区分，不与套餐混用）
    let go = BUILTIN_MODELS
        .iter()
        .find(|entry| entry.model == "deepseek-v4-flash")
        .expect("内置 go 模型");
    assert_eq!(go.base_url, GO_BASE_URL);
    assert_eq!(go.api_style, "openai");
    let zen = BUILTIN_MODELS
        .iter()
        .find(|entry| entry.model == "deepseek-v4-flash-free")
        .expect("内置 zen 模型");
    assert_eq!(zen.base_url, ZEN_BASE_URL);
    // go/zen 端点确实不一致（需求前提）
    assert_ne!(GO_BASE_URL, ZEN_BASE_URL);
    // 转 ModelDefaults 后字段齐备
    let defaults = go.to_model_defaults();
    assert_eq!(defaults.defaults["max_tokens"], 4096);
    assert_eq!(defaults.defaults["streaming"], false);
}

/// go 套餐模型按官方 API 文档标注 api style（同一 base_url，路径由风格决定）。
#[test]
fn builtin_go_catalog_marks_per_model_api_styles() {
    let style_of = |model: &str| {
        BUILTIN_MODELS
            .iter()
            .find(|entry| entry.model == model)
            .expect("内置模型")
            .api_style
    };
    // chat/completions（openai）
    assert_eq!(style_of("glm-5.3"), "openai");
    assert_eq!(style_of("kimi-k3"), "openai");
    assert_eq!(style_of("deepseek-v4-flash"), "openai");
    assert_eq!(style_of("hy3"), "openai");
    // /messages（anthropic）
    assert_eq!(style_of("minimax-m3"), "anthropic");
    assert_eq!(style_of("qwen3.8-max"), "anthropic");
    // /responses（responses）
    assert_eq!(style_of("grok-4.5"), "responses");
    assert_eq!(style_of("gpt-5.6-luna"), "responses");
    // 三种风格在同一 base_url 下并存
    assert!(
        BUILTIN_MODELS
            .iter()
            .filter(|entry| entry.group == "go")
            .all(|entry| entry.base_url == GO_BASE_URL)
    );
}

#[test]
fn config_models_override_builtin_catalog() {
    let extra = vec![ModelDefaults {
        model: "deepseek-v4-flash".into(),
        group: Some("go".into()),
        defaults: json!({ "base_url": "https://my-gateway/v1", "api_style": "openai" }),
    }];
    let merged = catalog_with(BUILTIN_MODELS, extra);
    // 拼接语义：内置 + 扩展（同名模型去重发生在注册表收集时——BTreeMap 后者生效）
    assert_eq!(merged.len(), BUILTIN_MODELS.len() + 1);
    // 其余内置条目保留
    assert!(
        merged
            .iter()
            .any(|entry| entry.model == "deepseek-v4-flash-free")
    );
}

#[test]
fn validate_rejects_unknown_plan() {
    let bad = config(Some("nope"));
    assert!(bad.validate().is_err());
    let ok = config(Some("zen"));
    assert!(ok.validate().is_ok());
    let default = config(None);
    assert!(default.validate().is_ok());
}

#[test]
fn apply_registers_kind_with_catalog() {
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let mut config = config(Some("go"));
    config.api_key = None;
    OpencodePlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    assert!(registry.factory_kinds().contains(&OPENCODE_KIND));
    // 目录命中：条目只写 model + api_key 即可构建（构造不联网）
    assert!(
        registry
            .build(
                OPENCODE_KIND,
                &json!({ "model": "deepseek-v4-flash-free", "api_key": "k" })
            )
            .is_ok()
    );
    // 未知模型也能构建（回落到插件级套餐默认）
    assert!(
        registry
            .build(OPENCODE_KIND, &json!({ "model": "ghost", "api_key": "k" }))
            .is_ok()
    );
}

/// 插件级 api_key（${ENV_VAR} 展开）：条目省略 api_key → 真实请求携带插件配置的 key。
#[tokio::test]
async fn plugin_api_key_is_used_when_entry_omits_it() {
    unsafe { std::env::set_var("COS_TEST_OP_KEY", "sk-plugin") };
    // 服务器校验 Authorization 头——证明 api_key 来自插件配置（条目未写）
    let server = ScriptedChatServer::spawn_with_key(
        vec![ChatReply::Text("你好".into())],
        Some("sk-plugin".into()),
    )
    .await;
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let mut config = config(None);
    config.base_url = Some(format!("http://127.0.0.1:{}/v1", server.port));
    config.api_key = Some("${COS_TEST_OP_KEY}".into());
    OpencodePlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    // 未命中目录 → 插件级默认（base_url 覆盖 + api_key 展开）；条目零密钥
    let adapter = registry
        .build(OPENCODE_KIND, &json!({ "model": "ghost" }))
        .unwrap();
    let mut stream = adapter.stream(&LlmRequest::default());
    while let Some(item) = stream.next().await {
        let _ = item.unwrap();
    }
    server.join().await; // 服务器断言 Authorization 通过即证明
}

/// models_endpoint 拉取：本地回环服务器返回模型清单 → 目录并入（默认 api style）。
#[test]
fn models_endpoint_fetches_and_merges_catalog() {
    // 专用 /models 服务器（返回字符串数组）
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut socket, &mut buf);
            let body = r#"["m-fetched-a","m-fetched-b"]"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = std::io::Write::write_all(&mut socket, response.as_bytes());
        }
    });
    let ctx = Context::root();
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    let mut config = config(None);
    config.models_endpoint = Some(format!("http://{addr}/v1/models"));
    config.models_api_style = Some("anthropic".into());
    OpencodePlugin.apply(&ctx, &config).unwrap();
    let registry = ctx.get::<LlmRegistry>().unwrap();
    let available = registry.available_models(OPENCODE_KIND);
    assert!(
        available.contains(&"m-fetched-a".to_string()),
        "{available:?}"
    );
    assert!(
        available.contains(&"m-fetched-b".to_string()),
        "{available:?}"
    );
    // 拉取模型默认 api_style = anthropic（目录命中时按此构建，构造不联网）
    assert!(
        registry
            .build(
                OPENCODE_KIND,
                &json!({ "model": "m-fetched-a", "api_key": "k" })
            )
            .is_ok()
    );
}
