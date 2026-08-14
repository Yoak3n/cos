//! 内置插件注册与装载（插件相关的宿主逻辑集中于此）。
//!
//! 职责：
//! - [`builtin_plugin_ids`]：内置插件的 id 锚点——对插件 crate 的显式引用，
//!   保证其 inventory 静态注册表被链接进可执行文件（新增内置插件只改这里）；
//! - [`builtin_agent_drivers`]：agent 驱动 id 锚点——保证驱动器 crate（cos-agent-loop）
//!   的 `agent_factory!` 静态注册表被链接；
//! - [`load`]：cordis.yml → 插件树装载（静态注册表 + dlopen 混装，见 cos-loader）；
//! - [`plan_json`]：`--dump-config` 的计划 JSON（与装载同序）。
//!
//! 注：LLM Provider 工厂（如 opencode）不再有独立 kind 锚点——Provider 本身已插件化
//! （plugin-opencode 的 `plugin!` 注册），随内置插件锚点一并链接。

#![warn(missing_docs)]

use cos_core::Context;
#[cfg(any(
    feature = "plugin-todo",
    feature = "plugin-bash",
    feature = "plugin-memory",
    feature = "plugin-llm",
    feature = "plugin-opencode",
    feature = "plugin-deepseek",
    feature = "plugin-custom-provider",
    feature = "plugin-rpc"
))]
use cos_core::Plugin;
use cos_loader::{self as loader, Profile};

/// 插件 id 辅助（generic 路径：既避免 unit-struct 构造 lint，也兼容带字段的插件）。
/// 仅随插件 feature 编译（全关时无调用方）。
#[cfg(any(
    feature = "plugin-todo",
    feature = "plugin-bash",
    feature = "plugin-memory",
    feature = "plugin-llm",
    feature = "plugin-opencode",
    feature = "plugin-deepseek",
    feature = "plugin-custom-provider",
    feature = "plugin-rpc"
))]
fn plugin_id<P: Plugin + Default>() -> &'static str {
    P::default().id()
}

/// 内置插件的插件 id —— 同时是对插件 crate 的显式引用锚点：
/// 保证其 inventory 静态注册表被链接进 cos 可执行文件。
///
/// 随 feature 门控（`default-features = false` 时为空——库嵌入零插件）；
/// 每个插件 feature 决定其是否编译/锚定。
pub fn builtin_plugin_ids() -> Vec<&'static str> {
    [
        #[cfg(feature = "plugin-todo")]
        plugin_id::<plugin_todo::TodoPlugin>(),
        #[cfg(feature = "plugin-bash")]
        plugin_id::<plugin_bash::BashPlugin>(),
        #[cfg(feature = "plugin-memory")]
        plugin_id::<plugin_memory::MemoryPlugin>(),
        #[cfg(feature = "plugin-llm")]
        plugin_id::<plugin_llm::LlmPlugin>(),
        #[cfg(feature = "plugin-opencode")]
        plugin_id::<plugin_opencode::OpencodePlugin>(),
        #[cfg(feature = "plugin-deepseek")]
        plugin_id::<plugin_deepseek::DeepseekPlugin>(),
        #[cfg(feature = "plugin-custom-provider")]
        plugin_id::<plugin_custom_provider::CustomProviderPlugin>(),
        #[cfg(feature = "plugin-rpc")]
        plugin_id::<plugin_rpc::RpcPlugin>(),
    ]
    .into_iter()
    .collect()
}

/// agent 驱动 id 锚点：保证驱动器 crate（cos-agent-loop）的 `agent_factory!`
/// inventory 静态注册表被链接进 cos 可执行文件——`--agent-driver loop` 依赖此锚点。
pub fn builtin_agent_drivers() -> [&'static str; 1] {
    [cos_agent_loop::LOOP_DRIVER_ID]
}

/// 装载插件树：锚定内置插件与驱动 → 解析 cordis.yml → 拓扑排序 → 按序 apply。
///
/// `config_path: None` = 零插件装配（库嵌入模式）：跳过 yml 解析，装载空插件树；
/// 锚点照常执行（保证插件/驱动的 inventory 注册表被链接，后续程序化挂载可用）。
pub fn load(
    root: &Context,
    config_path: Option<&str>,
) -> Result<loader::LoadedApp, loader::LoadError> {
    // 锚点：保证插件/驱动 crate 的 inventory 注册表被链接
    let _ = builtin_plugin_ids();
    let _ = builtin_agent_drivers();
    let profile = match config_path {
        Some(path) => Profile::load(path)?,
        None => Profile::default(),
    };
    loader::load(root, &profile)
}

/// `--dump-config` 的计划 JSON（与装载共用同一路径，保证输出与装载一致）。
pub fn plan_json(config_path: &str) -> Result<String, loader::LoadError> {
    let _ = builtin_plugin_ids();
    let profile = Profile::load(config_path)?;
    loader::dump_plan(&profile)
}
