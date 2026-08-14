//! 服务契约：可注入 Context 服务仓库的类型。

/// 服务：类型化 TypeMap（TypeId 键）中的条目。
///
/// [`Service::NAME`] 是 名字 → 类型 的桥：与 `Plugin::provide()` 声明的名字一致，
/// 同一名字只允许一个实现（重名报 [`crate::CoreError::DuplicateService`]）。
///
/// 对象安全豁免（P7 审计结论，见 docs/b-abi.md）：`Service` **不要求** dyn-compatible——
/// 名字是登记键，必须在无实例的泛型路径（`get::<T>` 错误信息）可取，关联常量是唯一
/// 恰当形态；服务在 B 边界按名字字符串 + 不透明句柄传递，从不作 trait object。
/// 其余接缝 trait（Plugin/LlmAdapter/Agent/Tool/ToolGuard/Shell）已过对象安全检查。
pub trait Service: Send + Sync + 'static {
    /// 服务名（与插件 `provide()` 声明一致）。
    const NAME: &'static str;
}
