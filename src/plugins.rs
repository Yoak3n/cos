//! 内置插件注册与装载（插件相关的宿主逻辑集中于此）。
//!
//! 职责：
//! - [`builtin_plugin_ids`]：内置插件的 id 锚点——对插件 crate 的显式引用，
//!   保证其 inventory 静态注册表被链接进可执行文件（新增内置插件只改这里）；
//! - [`load`]：cordis.yml → 插件树装载（静态注册表 + dlopen 混装，见 cos-loader）；
//! - [`plan_json`]：`--dump-config` 的计划 JSON（与装载同序）。

#![warn(missing_docs)]

use cos_core::{Context, Plugin};
use cos_loader::{self as loader, Profile};

/// 插件 id 辅助（generic 路径：既避免 unit-struct 构造 lint，也兼容带字段的插件）。
fn plugin_id<P: Plugin + Default>() -> &'static str {
    P::default().id()
}

/// 内置插件的插件 id —— 同时是对插件 crate 的显式引用锚点：
/// 保证其 inventory 静态注册表被链接进 cos 可执行文件。
pub fn builtin_plugin_ids() -> [&'static str; 5] {
    [
        plugin_id::<plugin_todo::TodoPlugin>(),
        plugin_id::<plugin_bash::BashPlugin>(),
        plugin_id::<plugin_memory::MemoryPlugin>(),
        plugin_id::<plugin_llm::LlmPlugin>(),
        plugin_id::<plugin_rpc::RpcPlugin>(),
    ]
}

/// 装载插件树：锚定内置插件 → 解析 cordis.yml → 拓扑排序 → 按序 apply。
pub fn load(root: &Context, config_path: &str) -> Result<loader::LoadedApp, loader::LoadError> {
    // 锚点：保证插件 crate 的 inventory 注册表被链接
    let _ = builtin_plugin_ids();
    let profile = Profile::load(config_path)?;
    loader::load(root, &profile)
}

/// `--dump-config` 的计划 JSON（与装载共用同一路径，保证输出与装载一致）。
pub fn plan_json(config_path: &str) -> Result<String, loader::LoadError> {
    let _ = builtin_plugin_ids();
    let profile = Profile::load(config_path)?;
    loader::dump_plan(&profile)
}
