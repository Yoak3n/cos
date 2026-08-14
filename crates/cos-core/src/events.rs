//! 事件总线：运行时开放的 `name + Arc<dyn Any>` 载荷（决策 D1）。
//!
//! 五种分发（语义权威：`vendor/cordis/src/events.ts`）：
//! - `emit`：同步逐个调用，返回值忽略；
//! - `parallel`：全部并发 await，任一失败 → 聚合错误（同 `AggregateError`）；
//! - `serial`：异步按序 await，监听器返回 `Ok(Some(v))`（bail 值）即停止并返回 `v`；
//! - `bail`：同步按序，返回第一个非 `None`；
//! - `waterfall`：监听器包裹剩余链——不调 `next()` 即短路（veto），链尾为调用方提供的默认行为。
//!
//! waterfall 载荷与返回值**分离**（`Decision<P, V>`，P4 起按 dsh 语义定型）：
//! 载荷 `P` 经 `set_value` 变换、返回值 `V` 由链尾（默认行为）产生并向外流；
//! 监听器 veto = 不调 `next()` 直接返回一个 `V`。

use futures::future::BoxFuture;
use std::any::{Any, TypeId};
use std::collections::VecDeque;
use std::sync::Arc;

use crate::error::CoreResult;

/// 事件名：运行时开放（决策 D1——不做编译期枚举）。
pub type EventName = &'static str;

/// 事件载荷：监听器内 downcast（`downcast_ref` / `downcast`）。
pub type EventPayload = Arc<dyn Any + Send + Sync>;

/// 分发模式：监听器注册时声明自己响应哪种分发（同 cordis `ctx.on(name, l, { type })`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchKind {
    /// 同步即发（返回值忽略）。
    Emit,
    /// 全并发 await。
    Parallel,
    /// 异步按序，首个 bail 值停止。
    Serial,
    /// 同步按序，首个非 `None` 停止。
    Bail,
    /// 监听器包裹剩余链（`next()` 委托）。
    Waterfall,
}

/// waterfall 的决策链节点：载荷 `P` 可变换，链返回值类型为 `V`。
///
/// - 监听器调用 [`Decision::next`] 委托给下一个监听器（链尾执行调用方默认行为，返回 `V`）；
/// - 监听器**不调 `next()` 直接返回值** = 短路（veto，同 dsh：不调 next 即否决剩余链）。
pub struct Decision<P, V> {
    value: P,
    remaining: VecDeque<WaterfallListener<P, V>>,
    default: Option<WaterfallDefault<P, V>>,
}

impl<P, V> Decision<P, V> {
    pub(crate) fn new(
        value: P,
        listeners: VecDeque<WaterfallListener<P, V>>,
        default: Option<WaterfallDefault<P, V>>,
    ) -> Self {
        Self {
            value,
            remaining: listeners,
            default,
        }
    }

    /// 当前（已变换的）载荷。
    pub fn value(&self) -> &P {
        &self.value
    }

    /// 替换当前载荷（供监听器变换）。
    pub fn set_value(&mut self, value: P) {
        self.value = value;
    }

    /// 消费决策，取回最终载荷。
    pub fn into_value(self) -> P {
        self.value
    }
}

impl<P: Send, V: Send> Decision<P, V> {
    /// 委托给下一个监听器；链尾执行调用方提供的默认行为并返回其 `V`。
    ///
    /// 命名同 dsh waterfall 的 `next()` 委托（风险表应对），非迭代器语义。
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> BoxFuture<'_, V> {
        Box::pin(async move {
            match self.remaining.pop_front() {
                Some(listener) => listener(&mut *self).await,
                None => match self.default.take() {
                    Some(default) => default(&mut *self).await,
                    None => panic!("waterfall 链无默认行为且已耗尽（调用方必须提供默认）"),
                },
            }
        })
    }
}

/// waterfall 监听器：接收决策链节点，委托 `next()` 或直接返回 `V`（veto）。
pub type WaterfallListener<P, V> =
    Arc<dyn for<'a> Fn(&'a mut Decision<P, V>) -> BoxFuture<'a, V> + Send + Sync>;

/// waterfall 的默认行为（链尾）：由调用方提供，可读取已变换的载荷。
pub type WaterfallDefault<P, V> =
    Box<dyn for<'a> FnOnce(&'a mut Decision<P, V>) -> BoxFuture<'a, V> + Send>;

/// bail 监听器（同步）：返回首个非 `None` 载荷即停止分发。
pub(crate) type BailListener = Arc<dyn Fn(&EventPayload) -> Option<EventPayload> + Send + Sync>;

/// 已注册监听器的运行时载体（存于 [`crate::registry::Registry`]）。
#[derive(Clone)]
pub(crate) enum ListenerBody {
    /// emit 监听器。
    Emit(Arc<dyn Fn(&EventPayload) + Send + Sync>),
    /// parallel 监听器。
    Parallel(Arc<dyn Fn(EventPayload) -> BoxFuture<'static, CoreResult<()>> + Send + Sync>),
    /// serial 监听器。
    Serial(
        Arc<
            dyn Fn(EventPayload) -> BoxFuture<'static, CoreResult<Option<EventPayload>>>
                + Send
                + Sync,
        >,
    ),
    /// bail 监听器（同步）。
    Bail(BailListener),
    /// waterfall 监听器（按 `(载荷类型, 返回值类型)` 配对，见决策 D1 风险应对）。
    Waterfall {
        /// (载荷, 返回值) 类型对。
        ty: (TypeId, TypeId),
        /// 监听器（分发时 downcast 到 `WaterfallListener<P, V>`）。
        listener: Arc<dyn Any + Send + Sync>,
    },
}
