//! 边界错误类型（决策 D5）：cos-core 的公开 API 一律返回 [`CoreError`]；
//! 插件内部实现可自由使用 anyhow。

use thiserror::Error;

use crate::scope::ScopeKey;

/// cos-core 的边界错误。
#[derive(Debug, Error, PartialEq)]
pub enum CoreError {
    /// 服务名冲突：同一名字只允许一个实现（同 dsh 重复 provide 报错）。
    #[error("服务 '{0}' 已被提供（同一名字只允许一个实现）")]
    DuplicateService(&'static str),
    /// 服务未注册（依赖未就绪或已卸载）。
    #[error("服务 '{0}' 未注册或尚未就绪")]
    ServiceNotFound(&'static str),
    /// 服务已注册，但请求的类型与注册类型不符。
    #[error("服务 '{0}' 的类型不匹配")]
    ServiceTypeMismatch(&'static str),
    /// 事件载荷 downcast 失败（事件名 + 载荷类型应在注册处配对，决策 D1 风险应对）。
    #[error("事件 '{0}' 的载荷类型不匹配")]
    PayloadTypeMismatch(&'static str),
    /// `parallel` 分发时一个或多个监听器失败（同 JS 侧 `AggregateError`）。
    #[error("事件 '{0}' 的监听器失败: {1:?}")]
    ListenerAggregate(&'static str, Vec<CoreError>),
    /// fiber 已卸载后仍注册效果（同 dsh `INACTIVE_EFFECT`）。
    #[error("fiber 已卸载，拒绝注册新效果（{0}）")]
    InactiveFiber(&'static str),
    /// scope key 已绑定父级（每个 key 只允许绑定一次，同 dsh `bindScopeParent`）。
    #[error("scope '{0}' 已绑定父级（只允许绑定一次）")]
    ScopeParentAlreadyBound(ScopeKey),
    /// scope 父链绑定会形成环。
    #[error("scope 父链绑定 '{0}' ← '{1}' 会形成环")]
    ScopeParentCycle(ScopeKey, ScopeKey),
    /// 插件自定义失败（逃生舱，边界类型化之外的通用错误）。
    #[error("{0}")]
    Other(String),
}

/// cos-core 通用结果别名。
pub type CoreResult<T> = Result<T, CoreError>;
