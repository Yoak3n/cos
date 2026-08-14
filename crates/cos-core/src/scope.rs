//! scope：子 agent / 子任务的上下文隔离（决策 D6 —— A 形态就做）。
//!
//! 语义权威参考：`packages/core/scope/src/index.ts`。
//!
//! 路由规则（P1 落地）：
//! - 监听器在注册处记录其上下文的 scope tag（[`crate::Context::scope_of`]）；
//! - [`ScopeTarget::Key`]`(k)` 分发：无 tag 监听器全收；tag 属于 `k` 祖先链（含 `k`）的监听器收
//!   —— 事件只向上流：祖先监听子孙的广播，子孙不监听祖先；
//! - [`ScopeTarget::None`]：仅无 tag 监听器（对应 dsh 无 key 的 unkeyed carrier）；
//! - [`ScopeTarget::All`]：不过滤。

/// scope 的唯一标识（同 dsh 的 scope key，如 `#session:<id>`；
/// A 形态以字符串等价代替 JS 的对象同一性）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeKey(pub String);

impl ScopeKey {
    /// 由字符串构造 scope key。
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

impl std::fmt::Display for ScopeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 事件的分发目标。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ScopeTarget {
    /// 全实例广播（默认，不过滤）。
    #[default]
    All,
    /// 仅无 tag 监听器（对应 dsh 无 key 的 unkeyed carrier：排除一切带 tag 的监听器）。
    None,
    /// 路由到该 scope：无 tag 监听器 + tag 位于其祖先链上的监听器（事件只向上流）。
    Key(ScopeKey),
}
