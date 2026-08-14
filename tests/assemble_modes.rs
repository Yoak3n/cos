//! assemble：主 agent LLM 解析优先级（--agent-llm > --llm-* 的 "default" > yml "main" > demo mock）。

use cos::{RunConfig, assemble};

fn base_config(config_path: String) -> RunConfig {
    RunConfig {
        config_path,
        dump_config: false,
        session_id: "assemble-test".into(),
        prompt: None,
        session_path: None,
        cancel: None,
        llm: None,
        agent_llm: None,
    }
}

#[tokio::test]
async fn yml_main_chain_is_auto_used() {
    let path = std::env::temp_dir().join(format!("cos-assemble-main-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        "- name: llm\n  config:\n    providers:\n      - { id: m1, kind: mock, config: {} }\n\
         \x20   chains:\n      - { id: main, providers: [m1] }\n",
    )
    .unwrap();
    let config = base_config(path.to_string_lossy().into_owned());
    let assembled = assemble(&config).await.unwrap();
    assert!(
        !assembled.demo_mode,
        "yml 定义 main 链 → 自动使用，不回落演示脚本"
    );
    assert_eq!(
        assembled.agent.options().provider.as_deref(),
        Some("llm-registry")
    );
    assert_eq!(assembled.agent.options().model.as_deref(), Some("main"));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn no_llm_config_falls_back_to_demo_mock() {
    let config = base_config(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo.yml").to_string());
    let assembled = assemble(&config).await.unwrap();
    assert!(
        assembled.demo_mode,
        "无任何 LLM 配置 → 演示脚本并标记 demo_mode"
    );
    assert_eq!(assembled.agent.options().provider.as_deref(), Some("demo"));
}

#[tokio::test]
async fn agent_llm_explicit_wins_over_main() {
    let path =
        std::env::temp_dir().join(format!("cos-assemble-explicit-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        "- name: llm\n  config:\n    providers:\n      - { id: m1, kind: mock, config: {} }\n\
         \x20   chains:\n      - { id: main, providers: [m1] }\n",
    )
    .unwrap();
    let mut config = base_config(path.to_string_lossy().into_owned());
    config.agent_llm = Some("m1".into());
    let assembled = assemble(&config).await.unwrap();
    assert!(!assembled.demo_mode);
    assert_eq!(
        assembled.agent.options().model.as_deref(),
        Some("m1"),
        "--agent-llm 显式指定优先于自动 main"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn single_provider_is_auto_used_without_chain() {
    // 用户场景：yml 只定义一个提供商、无链 → 自动使用（零参数启动）
    let path = std::env::temp_dir().join(format!("cos-assemble-single-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        "- name: llm\n  config:\n    providers:\n      - { id: opencode-go, kind: mock, config: {} }\n",
    )
    .unwrap();
    let config = base_config(path.to_string_lossy().into_owned());
    let assembled = assemble(&config).await.unwrap();
    assert!(!assembled.demo_mode, "唯一提供商应自动使用");
    assert_eq!(
        assembled.agent.options().model.as_deref(),
        Some("opencode-go")
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn multiple_providers_without_chain_fall_back_to_demo() {
    let path = std::env::temp_dir().join(format!("cos-assemble-multi-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        "- name: llm\n  config:\n    providers:\n      - { id: m1, kind: mock, config: {} }\n\
         \x20     - { id: m2, kind: mock, config: {} }\n",
    )
    .unwrap();
    let config = base_config(path.to_string_lossy().into_owned());
    let assembled = assemble(&config).await.unwrap();
    assert!(
        assembled.demo_mode,
        "多提供商无链 → 不自动猜，回落演示脚本（提示用 --agent-llm）"
    );
    let _ = std::fs::remove_file(&path);
}

/// demo.yml 装配了 plugin-rpc → RPC 提供者注册（--rpc 走插件路径）。
#[tokio::test]
async fn rpc_plugin_registers_provider_when_declared() {
    let config = base_config(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo.yml").to_string());
    let assembled = assemble(&config).await.unwrap();
    let registry = assembled
        .root
        .get::<cos_rpc::RpcProviderRegistry>()
        .unwrap();
    assert!(
        registry.get().is_some(),
        "demo.yml 声明 rpc 插件 → 提供者应已注册（--rpc 委托插件）"
    );
}

/// yml 未声明 plugin-rpc → 提供者缺失（--rpc 回退内置 stdio，零配置可用）。
#[tokio::test]
async fn rpc_provider_absent_without_plugin() {
    let path = std::env::temp_dir().join(format!("cos-assemble-norpc-{}.yml", std::process::id()));
    std::fs::write(&path, "- name: todo\n").unwrap();
    let config = base_config(path.to_string_lossy().into_owned());
    let assembled = assemble(&config).await.unwrap();
    let registry = assembled
        .root
        .get::<cos_rpc::RpcProviderRegistry>()
        .unwrap();
    assert!(
        registry.get().is_none(),
        "未声明 rpc 插件 → 无提供者（宿主回退内置）"
    );
    let _ = std::fs::remove_file(&path);
}
