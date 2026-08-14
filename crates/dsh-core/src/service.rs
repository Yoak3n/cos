//! 服务契约：可注入 Context 服务仓库的类型。

/// 服务：类型化 TypeMap（TypeId 键）中的条目。
///
/// [`Service::NAME`] 是 名字 → 类型 的桥：与 `Plugin::provide()` 声明的名字一致，
/// 同一名字只允许一个实现（重名报 [`crate::CoreError::DuplicateService`]）。
pub trait Service: Send + Sync + 'static {
    /// 服务名（与插件 `provide()` 声明一致）。
    const NAME: &'static str;
}
