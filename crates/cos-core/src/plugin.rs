//! 插件契约：inject/provide 声明 + apply 注册（同 dsh 插件语义）。
//!
//! 注册即效果：`apply` 内通过 ctx 注册的服务/监听/工具句柄自动进入 `ctx.fiber()`，
//! 插件实例卸载时逆序回滚（RAII，决策 D7）。

use serde::de::DeserializeOwned;

use crate::context::Context;
use crate::error::CoreResult;

/// 配置校验（决策 D3）：serde 反序列化后手写校验；schemars 生成的 JSON Schema 于 P2 接入。
pub trait Validate {
    /// 默认放行；插件可按需覆盖（返回 `Err` 即启动失败）。
    fn validate(&self) -> CoreResult<()> {
        Ok(())
    }
}

/// **插件类型**（装配优先级层级）：loader 注册前先扫描全部插件，按类型分配基准
/// 装载顺序——**Provider 最先，其次是 Core，最后 Other**；同类型内保持配置顺序
/// （稳定）；显式依赖边（`inject`）仍然优先于类型（硬约束）。
///
/// 为什么需要：插件之间的真实依赖（`inject`/`provide` 边）之外的"隐含先后"——
/// 如 plugin-llm 装配 providers 时要求 Provider 插件的工厂已注册、memory 要求
/// llm 已装配——不再依赖 yml 条目顺序或可选依赖标记，由类型声明直接保证。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PluginTier {
    /// Provider 插件（注册 LLM 工厂等）：**最先**装载。
    Provider = 0,
    /// 核心插件（plugin-llm 等装配枢纽）：次之。
    Core = 1,
    /// 其他插件（工具 / 记忆 / RPC 等）：最后。
    Other = 2,
}

/// 插件：`inject` / `provide` 声明服务依赖与产出，`apply` 执行注册。
///
/// 对象安全（P7 冻结）：元数据一律关联函数（`id`/`inject`/`provide`/`tier` 无 self），
/// 不含关联常量——trait 可作 `dyn` 使用（B 形态 FFI 转发前提）。
pub trait Plugin: Send + Sync {
    /// 插件 id（同 cordis plugin id；实例方法——关联常量会破坏 dyn-compatible，P7 冻结）。
    fn id(&self) -> &'static str;

    /// 配置类型：serde 反序列化 + 校验。
    type Config: DeserializeOwned + Validate;

    /// **插件类型**（装配优先级层级，缺省 `Other`）：Provider < Core < Other；
    /// 同类型保持配置顺序；`inject` 边优先于类型。
    fn tier(&self) -> PluginTier {
        PluginTier::Other
    }

    /// 依赖的服务名（无则空）。
    ///
    /// 返回 `'static` 切片：loader 的静态工厂注册表需要在注册期持有该列表
    /// （典型实现返回字面量，靠常量提升满足；P0 签名的细化，见 docs/decisions.md）。
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }

    /// 提供的服务名（无则空）；同 [`Service::name`] 一致。
    fn provide(&self) -> &'static [&'static str] {
        &[]
    }

    /// 执行注册：provide 服务、on 监听等。返回 `Err` 视为插件实例启动失败。
    fn apply(&self, ctx: &Context, config: &Self::Config) -> CoreResult<()>;
}
