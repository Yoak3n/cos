//! dsh-loader —— cordis.yml 解析 → 工厂解析 → 拓扑排序 → 挂载（P2）。
//!
//! 语义权威参考：`vendor/loader/src/config/{entry,tree,group}.ts`、
//! `packages/boot/app-boot/src/profile.ts`。
//!
//! v1 范围（PLAN.md P2）：无 bundle/patch 层叠——cordis.yml 就是单一清单
//! （保留 `disabled` / `config` / `inject` 字段）；[`resolve_factory`] 是接口
//! （§6 防返工项），B 形态只加一个 dlopen source。
//!
//! 装载语义：依赖就绪才激活——由 `inject` / `provide` 建图 + 拓扑排序替代
//! dsh 的响应式重载；环 / 缺依赖 / 未知插件 / 重复 provide 一律启动即报错（fail loud at load）。

#![warn(missing_docs)]

mod compose;
mod error;
mod profile;
mod registry;

pub use compose::{LoadedApp, LoadedPlugin, PlannedEntry, dump_plan, load, plan};
pub use error::LoadError;
pub use profile::{Entry, Profile};
pub use registry::{PluginRegistrar, available_plugins, resolve_factory};

/// `plugin!` 宏的内部实现（`#[doc(hidden)]`，勿直接使用）。
#[doc(hidden)]
pub mod private {
    pub use crate::registry::private::{apply_of, id_of, inject_of, provide_of};
}
