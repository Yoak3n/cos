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

/// 插件：`inject` / `provide` 声明服务依赖与产出，`apply` 执行注册。
pub trait Plugin: Send + Sync {
    /// 插件 id（同 cordis plugin id）。
    const ID: &'static str;

    /// 配置类型：serde 反序列化 + 校验。
    type Config: DeserializeOwned + Validate;

    /// 依赖的服务名（无则空）。
    ///
    /// 返回 `'static` 切片：loader 的静态工厂注册表需要在注册期持有该列表
    /// （典型实现返回字面量，靠常量提升满足；P0 签名的细化，见 docs/decisions.md）。
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }

    /// 提供的服务名（无则空）；同 [`Service::NAME`] 一致。
    fn provide(&self) -> &'static [&'static str] {
        &[]
    }

    /// 执行注册：provide 服务、on 监听等。返回 `Err` 视为插件实例启动失败。
    fn apply(&self, ctx: &Context, config: &Self::Config) -> CoreResult<()>;
}
