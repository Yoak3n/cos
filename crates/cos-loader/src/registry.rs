//! 静态工厂注册表：`plugin!` 宏 + inventory 收集，`resolve_factory` 按名解析。
//!
//! §6 防返工项：`resolve_factory` 从第一天就是接口——A 形态查静态注册表，
//! B 形态（P8）只加一个 dlopen source，调用方不变。
//!
//! 注册表条目全部是 const 可构造的数据（`&'static str` / 切片 / 函数指针），
//! 因此 `inventory::submit!` 可以直接收集，无需运行时初始化。

use cos_core::{Context, Plugin, Validate};
use serde_json::Value;

use crate::error::LoadError;

/// 静态注册的插件工厂条目：`id` / `inject` / `provide` 供建图，
/// `apply` 完成 配置解析 → 校验 → 注册。
///
/// 全部字段为函数指针（const 可构造）：函数体内的 `P::default()` 在装载期才构造实例。
pub struct PluginRegistrar {
    /// 工厂名（cordis.yml 里的 `name`）。
    pub name: &'static str,
    /// 插件 id（同 `Plugin::ID`）。
    pub id: fn() -> &'static str,
    /// 依赖的服务名。
    pub inject: fn() -> &'static [&'static str],
    /// 提供的服务名。
    pub provide: fn() -> &'static [&'static str],
    /// 解析并校验配置后在给定 ctx 上 apply（ctx 由 loader fork 好）；
    /// 失败错误必须带条目上下文（`entry_id` / 工厂名）。
    pub apply: fn(&Context, &str, Value) -> Result<(), LoadError>,
}

inventory::collect!(PluginRegistrar);

/// 解析工厂名 → 工厂；未注册返回 `None`。
pub fn resolve_factory(name: &str) -> Option<&'static PluginRegistrar> {
    inventory::iter::<PluginRegistrar>
        .into_iter()
        .find(|registrar| registrar.name == name)
}

/// 已注册的工厂名（排序稳定，供报错与 `--dump-config` 使用）。
pub fn available_plugins() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = inventory::iter::<PluginRegistrar>
        .into_iter()
        .map(|r| r.name)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// `plugin!` 宏的实现细节（`#[doc(hidden)]`，勿直接使用）。
#[doc(hidden)]
pub mod private {
    use super::*;

    /// `P::ID`。
    pub fn id_of<P: Plugin + Default>() -> &'static str {
        P::ID
    }

    /// `P::default().inject()`。
    pub fn inject_of<P: Plugin + Default>() -> &'static [&'static str] {
        P::default().inject()
    }

    /// `P::default().provide()`。
    pub fn provide_of<P: Plugin + Default>() -> &'static [&'static str] {
        P::default().provide()
    }

    /// 配置解析 → 校验 → `apply`（错误统一带条目上下文）。
    pub fn apply_of<P: Plugin + Default>(
        ctx: &Context,
        entry_id: &str,
        config: Value,
    ) -> Result<(), LoadError> {
        let config: P::Config =
            serde_json::from_value(config).map_err(|source| LoadError::ConfigParse {
                id: entry_id.to_string(),
                name: P::ID.to_string(),
                source,
            })?;
        config
            .validate()
            .map_err(|source| LoadError::ConfigInvalid {
                id: entry_id.to_string(),
                name: P::ID.to_string(),
                source,
            })?;
        P::default()
            .apply(ctx, &config)
            .map_err(|source| LoadError::Apply {
                id: entry_id.to_string(),
                name: P::ID.to_string(),
                source,
            })
    }
}

/// 注册一个插件工厂：`plugin!("name", MyPlugin)`。
///
/// `MyPlugin` 需实现 [`Plugin`] + [`Default`]（实例按 `Default` 构造）。
/// 工厂名 = cordis.yml 里的 `name` 字段。
#[macro_export]
macro_rules! plugin {
    ($name:literal, $plugin:ty) => {
        ::inventory::submit! {
            $crate::PluginRegistrar {
                name: $name,
                id: $crate::private::id_of::<$plugin>,
                inject: $crate::private::inject_of::<$plugin>,
                provide: $crate::private::provide_of::<$plugin>,
                apply: $crate::private::apply_of::<$plugin>,
            }
        }
    };
}
