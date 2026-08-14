//! assemble：主 agent LLM 解析优先级（--agent-llm > --llm-* 的 "default" > yml "main" >
//! yml 恰好一个提供商）；无任何 LLM 配置 → agent 为 None（库嵌入模式），
//! 经 [`Assembled::agent`] 报引导错误（CLI 形态 = 启动失败）。
//! agent 驱动（--agent-driver）：agent_factory! 注册表选择；未知驱动 → 报警退出。

use std::sync::Arc;

use cos::{RunConfig, assemble};
use cos_agent::{AgentError, AgentFactory};
use cos_agent_loop::LoopFactory;

// 可替换设计验证：测试二进制内注册一个自定义驱动（inventory 静态收集，
// 同生产 crate 的 `agent_factory!` 路径；实现复用 loop 但 id 不同——证明选择机制）。
cos_agent::agent_factory!("test-driver", build_test_driver);

/// 自定义驱动构建函数：配置忽略 → [`LoopFactory`]。
fn build_test_driver(_config: &serde_json::Value) -> Result<Arc<dyn AgentFactory>, AgentError> {
    Ok(Arc::new(LoopFactory))
}

fn base_config(config_path: Option<String>) -> RunConfig {
    RunConfig {
        config_path,
        dump_config: false,
        session_id: "assemble-test".into(),
        prompt: None,
        session_path: None,
        cancel: None,
        llm: None,
        agent_llm: None,
        agent_driver: None,
    }
}

/// opencode provider 条目（plugin 引用方式：引用 opencode-provider 插件，模型选自其目录；
/// 端口仅为占位——assemble 不发请求）。
const OPENCODE_PROVIDER: &str = "{ id: m1, plugin: opencode-provider, config: { base_url: \"http://127.0.0.1:1/v1\", model: \"deepseek-v4-flash\" } }";

/// 临时 yml：opencode Provider 插件 + llm 插件（providers 可指定）。
fn write_llm_yml(name: &str, providers: &str) -> String {
    let path = std::env::temp_dir().join(format!("cos-{name}-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        format!(
            "- name: opencode-provider\n  config:\n    api_key: k\n- name: llm\n  config:\n    providers:\n{providers}\n"
        ),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

#[tokio::test]
async fn yml_main_chain_is_auto_used() {
    let path = write_llm_yml(
        "assemble-main",
        &format!(
            "      - {OPENCODE_PROVIDER}\n    chains:\n      - {{ id: main, providers: [m1] }}"
        ),
    );
    let config = base_config(Some(path.clone()));
    let assembled = assemble(&config).await.unwrap();
    assert_eq!(
        assembled.agent().unwrap().options().provider.as_deref(),
        Some("llm-registry")
    );
    assert_eq!(
        assembled.agent().unwrap().options().model.as_deref(),
        Some("main")
    );
    let _ = std::fs::remove_file(&path);
}

/// Provider 插件写在 llm **之后**也能正确装载（Provider 类型的装配优先级最高——
/// loader 注册前扫描按类型排序，yml 顺序不再重要）。
#[tokio::test]
async fn provider_after_llm_in_yml_is_auto_ordered() {
    let path = std::env::temp_dir().join(format!("cos-assemble-rev-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        format!(
            "- name: llm\n  config:\n    providers:\n      - {OPENCODE_PROVIDER}\n    chains:\n      - {{ id: main, providers: [m1] }}\n- name: opencode-provider\n  config:\n    api_key: k\n"
        ),
    )
    .unwrap();
    let config = base_config(Some(path.to_string_lossy().into_owned()));
    let assembled = assemble(&config).await.unwrap();
    assert_eq!(
        assembled.agent().unwrap().options().model.as_deref(),
        Some("main"),
        "llm 声明在 opencode-provider 之前也应正确装配（类型优先级自动排序）"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn no_llm_config_assembles_without_agent() {
    let path = std::env::temp_dir().join(format!("cos-assemble-nollm-{}.yml", std::process::id()));
    std::fs::write(&path, "- name: todo\n").unwrap();
    let config = base_config(Some(path.to_string_lossy().into_owned()));
    let assembled = assemble(&config).await.unwrap();
    assert!(
        assembled.agent.is_none(),
        "无 LLM 配置 → 不创建 agent（库嵌入可用）"
    );
    let error = match assembled.agent() {
        Err(error) => error.to_string(),
        Ok(_) => panic!("agent 为 None 时 agent() 应报错"),
    };
    assert!(error.contains("未配置 LLM"), "实际: {error}");
    let _ = std::fs::remove_file(&path);
}

/// 零插件装配（库嵌入模式）：config_path: None → 不读 yml、插件树为空、agent 为 None，
/// 内置服务（工具注册表等）仍可用。
#[tokio::test]
async fn zero_config_assembles_without_plugins() {
    let config = base_config(None);
    let assembled = assemble(&config).await.unwrap();
    assert!(assembled.agent.is_none(), "无 LLM → agent 为 None");
    assert!(
        assembled.app.instances().is_empty(),
        "零插件装配 → 插件树为空"
    );
    assembled.root.get::<cos_tools::ToolRegistry>().unwrap();
    assembled.root.get::<cos_llm::LlmRegistry>().unwrap();
    assembled.root.get::<cos_agent::AgentRegistry>().unwrap();
}

#[tokio::test]
async fn agent_llm_explicit_wins_over_main() {
    let path = write_llm_yml(
        "assemble-explicit",
        &format!(
            "      - {OPENCODE_PROVIDER}\n    chains:\n      - {{ id: main, providers: [m1] }}"
        ),
    );
    let mut config = base_config(Some(path.clone()));
    config.agent_llm = Some("m1".into());
    let assembled = assemble(&config).await.unwrap();
    assert_eq!(
        assembled.agent().unwrap().options().model.as_deref(),
        Some("m1"),
        "--agent-llm 显式指定优先于自动 main"
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn single_provider_is_auto_used_without_chain() {
    // 用户场景：yml 只定义一个提供商、无链 → 自动使用（无需 --agent-llm）
    let path = write_llm_yml("assemble-single", &format!("      - {OPENCODE_PROVIDER}"));
    let config = base_config(Some(path.clone()));
    let assembled = assemble(&config).await.unwrap();
    assert_eq!(
        assembled.agent().unwrap().options().model.as_deref(),
        Some("m1")
    );
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn multiple_providers_without_chain_fail_startup() {
    // 多提供商无链 → 不自动猜（避免猜错意图）→ 不创建 agent；agent() 报引导错误
    let second = "{ id: m2, plugin: opencode-provider, config: { base_url: \"http://127.0.0.1:1/v1\", model: \"deepseek-v4-flash\" } }";
    let path = write_llm_yml(
        "assemble-multi",
        &format!("      - {OPENCODE_PROVIDER}\n      - {second}"),
    );
    let config = base_config(Some(path.clone()));
    let assembled = assemble(&config).await.unwrap();
    assert!(assembled.agent.is_none(), "多提供商无链 → 不创建 agent");
    let error = match assembled.agent() {
        Err(error) => error.to_string(),
        Ok(_) => panic!("agent 为 None 时 agent() 应报错"),
    };
    assert!(error.contains("未配置 LLM"), "实际: {error}");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn unknown_llm_kind_without_provider_plugin_fails_loud() {
    // 未声明 opencode-provider 插件而 llm 配置经 plugin: 引用它 → 映射命中但工厂未注册
    // （插件未 apply）→ build 失败，提示可用 kinds 与声明方式
    let path = std::env::temp_dir().join(format!("cos-assemble-nokind-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        format!("- name: llm\n  config:\n    providers:\n      - {OPENCODE_PROVIDER}\n"),
    )
    .unwrap();
    let config = base_config(Some(path.to_string_lossy().into_owned()));
    let error = match assemble(&config).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("未声明 Provider 插件应启动失败"),
    };
    assert!(
        error.contains("可用 kinds"),
        "应提示可用 Provider kinds: {error}"
    );
    assert!(
        error.contains("opencode"),
        "应提示声明 opencode-provider 插件: {error}"
    );
    let _ = std::fs::remove_file(&path);
}

/// demo.yml 装配了 plugin-rpc → RPC 提供者注册（--rpc 走插件路径）。
#[tokio::test]
async fn rpc_plugin_registers_provider_when_declared() {
    // demo.yml 声明 opencode-provider 插件 → 配 --llm-*（占位端点，assemble 不发请求）即可装配
    let mut config = base_config(Some(
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo.yml").to_string(),
    ));
    config.llm = Some(cos::LlmConfig {
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key: "k".into(),
        model: "m".into(),
        streaming: false,
    });
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

/// yml 未声明 plugin-rpc → 提供者缺失（--rpc 回退内置 stdio）。
#[tokio::test]
async fn rpc_provider_absent_without_plugin() {
    let path = std::env::temp_dir().join(format!("cos-assemble-norpc-{}.yml", std::process::id()));
    std::fs::write(
        &path,
        format!(
            "- name: opencode-provider\n  config:\n    api_key: k\n- name: llm\n  config:\n    providers:\n      - {OPENCODE_PROVIDER}\n- name: todo\n"
        ),
    )
    .unwrap();
    let config = base_config(Some(path.to_string_lossy().into_owned()));
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

/// 关键组件缺失报警：未知 agent 驱动 → 启动失败，报错列出可用驱动。
#[tokio::test]
async fn unknown_agent_driver_fails_startup() {
    let path = write_llm_yml("assemble-driver", &format!("      - {OPENCODE_PROVIDER}"));
    let mut config = base_config(Some(path.clone()));
    config.agent_driver = Some("nope".into());
    let error = match assemble(&config).await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("未知 agent 驱动应启动失败"),
    };
    assert!(error.contains("agent 驱动 'nope' 不可用"), "实际: {error}");
    assert!(error.contains("可用驱动"), "应列出可用驱动: {error}");
    assert!(
        error.contains("loop"),
        "默认驱动 loop 应在可用列表: {error}"
    );
    let _ = std::fs::remove_file(&path);
}

/// 可替换设计：--agent-driver 选择自定义驱动（agent_factory! 注册）→ 装配成功、agent 可用。
#[tokio::test]
async fn custom_agent_driver_is_selectable() {
    let path = write_llm_yml("assemble-driver2", &format!("      - {OPENCODE_PROVIDER}"));
    let mut config = base_config(Some(path.clone()));
    config.agent_driver = Some("test-driver".into());
    let assembled = assemble(&config).await.unwrap();
    assert_eq!(
        assembled.agent().unwrap().options().model.as_deref(),
        Some("m1"),
        "自定义驱动下主 agent 应正常创建并拿到 LLM"
    );
    // 缺省驱动仍是 loop（未被自定义驱动顶替）
    let registry = assembled.root.get::<cos_agent::AgentRegistry>().unwrap();
    assert!(
        registry.driver_ids().contains(&"loop"),
        "默认驱动 loop 应仍在注册表: {:?}",
        registry.driver_ids()
    );
    assert!(
        registry.driver_ids().contains(&"test-driver"),
        "自定义驱动应已注册: {:?}",
        registry.driver_ids()
    );
    let _ = std::fs::remove_file(&path);
}
