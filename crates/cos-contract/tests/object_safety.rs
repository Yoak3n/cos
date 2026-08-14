//! P7 冻结审计（编译期证明，过不了就编不过）：
//! 1. 接缝 trait 全部对象安全（可作 `dyn` 使用——宿主侧 trait object / FFI 转发前提）。
//!    豁免：`Service` 保留关联常量 `NAME`（登记键必须免实例可取；服务在 B 边界
//!    按名字字符串 + 不透明句柄传递，从不作 trait object）——见 docs/b-abi.md §9；
//! 2. 跨边界类型全部 JSON 可序列化（B-ABI 载荷 = JSON 字符串，serde 即 wire 契约）。

use cos_agent::Agent;
use cos_core::{Plugin, Validate};
use cos_llm::LlmAdapter;
use cos_shell::Shell;
use cos_tools::{Tool, ToolGuard};

/// 审计用插件配置（Config 是 Plugin 的关联类型，审计时取一个具体类型）。
#[derive(serde::Deserialize)]
struct TestConfig;

impl Validate for TestConfig {}

#[test]
fn seam_traits_are_object_safe() {
    // 以下引用若任一 trait 非对象安全 → 编译失败（P7 冻结项）
    fn _plugin(_: &dyn Plugin<Config = TestConfig>) {}
    fn _llm(_: &dyn LlmAdapter) {}
    fn _agent(_: &dyn Agent) {}
    fn _tool(_: &dyn Tool) {}
    fn _tool_guard(_: &dyn ToolGuard) {}
    fn _shell(_: &dyn Shell) {}
    // 显式触发实例化（防 dead_code 裁剪）
    let _ = (
        _plugin as fn(&dyn Plugin<Config = TestConfig>),
        _llm as fn(&dyn LlmAdapter),
        _agent as fn(&dyn Agent),
        _tool as fn(&dyn Tool),
        _tool_guard as fn(&dyn ToolGuard),
        _shell as fn(&dyn Shell),
    );
}

/// 跨边界类型审计：以下类型必须可 JSON 序列化/反序列化（B-ABI 载荷即 JSON 字符串）。
#[test]
fn boundary_types_are_json_serializable() {
    fn assert_serialize<T: serde::Serialize>() {}
    fn assert_deserialize<T: serde::de::DeserializeOwned>() {}

    // 消息与流块（cos-llm）
    assert_serialize::<cos_llm::UserMessage>();
    assert_deserialize::<cos_llm::UserMessage>();
    assert_serialize::<cos_llm::StreamChunk>();
    assert_deserialize::<cos_llm::StreamChunk>();
    assert_serialize::<cos_llm::AssistantMessage>();
    assert_deserialize::<cos_llm::AssistantMessage>();
    assert_serialize::<cos_llm::ToolResultMessage>();
    assert_deserialize::<cos_llm::ToolResultMessage>();
    assert_serialize::<cos_llm::LlmRequest>();
    assert_deserialize::<cos_llm::LlmRequest>();
    assert_serialize::<cos_llm::ToolCall>();
    assert_deserialize::<cos_llm::ToolCall>();

    // 会话事件（cos-session，封闭枚举 + Custom 逃生舱）
    assert_serialize::<cos_session::SessionEventData>();
    assert_deserialize::<cos_session::SessionEventData>();
    assert_serialize::<cos_session::SessionEvent>();
    assert_deserialize::<cos_session::SessionEvent>();

    // 工具调用（cos-tools）
    assert_serialize::<cos_tools::ToolRun>();
    assert_deserialize::<cos_tools::ToolRun>();
    assert_serialize::<cos_tools::ToolOutcome>();
    assert_deserialize::<cos_tools::ToolOutcome>();

    // 契约自身（清单 JSON）
    assert_serialize::<cos_contract::PluginManifest>();
    assert_deserialize::<cos_contract::PluginManifest>();
}
