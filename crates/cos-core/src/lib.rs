//! cos-core —— cos 的插件化内核：Context 服务仓库、事件总线、Plugin trait、Fiber/effect、scope。
//!
//! 语义权威参考（JS 仓库 `E:\GitVault\deepseek-harness`）：
//! `vendor/cordis/src/{context,registry,fiber,events,reflect}.ts`、`packages/core/scope/src/index.ts`。
//!
//! 契约要点（详见 PLAN.md §3、docs/decisions.md）：
//! - 一切皆插件：`Plugin::apply` 内注册服务/监听/工具，句柄进入 fiber，卸载逆序回滚；
//! - 服务仓库：类型化 TypeMap + 同名唯一（[`Service::NAME`]）；
//! - 事件：运行时开放（决策 D1），五种分发（emit/parallel/serial/bail/waterfall）；
//! - 错误：边界类型化（[`CoreError`]，决策 D5）。

#![warn(missing_docs)]

mod bridge;
mod context;
mod effect;
mod error;
mod events;
mod fiber;
mod plugin;
mod registry;
mod scope;
mod service;

pub use bridge::{BridgeRegistry, JsonBridge};
pub use context::{Context, Target};
pub use effect::EffectHandle;
pub use error::{CoreError, CoreResult};
pub use events::{
    Decision, DispatchKind, EventName, EventPayload, WaterfallDefault, WaterfallListener,
};
pub use fiber::Fiber;
pub use plugin::{Plugin, PluginTier, Validate};
pub use scope::{ScopeKey, ScopeTarget};
pub use service::Service;
