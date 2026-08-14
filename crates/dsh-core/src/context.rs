//! Context：插件运行环境的入口（对应 cordis Context / scope）。
//!
//! - 根 Context 用 [`Context::root`] 创建；每个插件实例 [`Context::fork`] 一个（新 Fiber、共享服务仓库）；
//! - `provide` / `get`：类型化 TypeMap（TypeId 键）+ 服务名冲突检测；
//! - `on*`：监听器自动归入当前 fiber（fiber 卸载即失效，同 dsh `fiber.effect`）；
//!   卸载后注册一律 [`CoreError::InactiveFiber`]（同 dsh `INACTIVE_EFFECT`）；
//! - 五种分发语义见 [`crate::events`]；scope 路由经 [`Context::target`] 指定（见 [`crate::scope`]）。

use std::any::{Any, TypeId};
use std::collections::VecDeque;
use std::sync::Arc;

use futures::future::BoxFuture;

use crate::effect::EffectHandle;
use crate::error::{CoreError, CoreResult};
use crate::events::{
    Decision, DispatchKind, EventName, EventPayload, ListenerBody, WaterfallListener,
};
use crate::fiber::Fiber;
use crate::registry::{RegisteredListener, Registry};
use crate::scope::{ScopeKey, ScopeTarget};
use crate::service::Service;

/// 插件运行环境入口（廉价 Clone：共享 [`Registry`]）。
#[derive(Clone)]
pub struct Context {
    registry: Arc<Registry>,
    fiber: Arc<Fiber>,
    /// 本上下文继承的 scope tag（[`Context::fork_scoped`] 打上；[`Context::fork`] 继承）。
    scope: Option<ScopeKey>,
}

impl Context {
    /// 创建根 Context（自带根 fiber、无 scope tag）。
    pub fn root() -> Self {
        Self {
            registry: Arc::new(Registry::new()),
            fiber: Arc::new(Fiber::default()),
            scope: None,
        }
    }

    /// fork 一个插件实例 Context：新 Fiber、共享服务仓库与事件总线、继承 scope tag。
    pub fn fork(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            fiber: Arc::new(Fiber::default()),
            scope: self.scope.clone(),
        }
    }

    /// fork 并打上 scope tag（同 dsh `createScope` 的 scoped ctx；
    /// 父链绑定先经 [`Context::bind_scope_parent`]）。
    pub fn fork_scoped(&self, key: ScopeKey) -> Self {
        let mut ctx = self.fork();
        ctx.scope = Some(key);
        ctx
    }

    /// 当前插件实例的 fiber（注册的句柄自动归入，卸载逆序回滚）。
    pub fn fiber(&self) -> &Fiber {
        &self.fiber
    }

    /// 本上下文的 scope tag（无则 `None`，同 dsh `scopeOf`）。
    pub fn scope_of(&self) -> Option<&ScopeKey> {
        self.scope.as_ref()
    }

    /// 绑定 scope 父链（同 dsh `bindScopeParent`）：每个 key 只允许绑定一次，拒绝成环。
    pub fn bind_scope_parent(&self, key: ScopeKey, parent: ScopeKey) -> CoreResult<()> {
        self.registry.bind_scope_parent(key, parent)
    }

    /// scope 祖先链，最近者在前（同 dsh `scopeChainOf`）：`[key, parent, grandparent, …]`。
    pub fn scope_chain_of(&self, key: &ScopeKey) -> Vec<ScopeKey> {
        self.registry.scope_chain_of(key)
    }

    /// 以指定分发目标路由事件（对应 dsh `scopeTarget` 载体；`All` 等价于不带目标的调用）。
    pub fn target(&self, target: ScopeTarget) -> Target<'_> {
        Target { ctx: self, target }
    }

    /// 注册服务；同名或同类型重复注册报错（同 dsh 重复 provide 报错）。
    pub fn provide<T: Service>(&self, value: T) -> CoreResult<EffectHandle> {
        if self.fiber.is_disposed() {
            return Err(CoreError::InactiveFiber("ctx.provide"));
        }
        let tid = TypeId::of::<T>();
        self.registry
            .insert_service(tid, T::NAME, Arc::new(value))?;
        let registry = self.registry.clone();
        let handle = EffectHandle::new(move || registry.remove_service(tid, T::NAME));
        self.fiber.push(handle.clone());
        Ok(handle)
    }

    /// 按类型取服务；未注册/未就绪 → [`CoreError::ServiceNotFound`]。
    pub fn get<T: Service>(&self) -> CoreResult<Arc<T>> {
        self.registry.get_service::<T>()
    }

    /// 注册 emit 监听器（同步、返回值忽略）；自动归入当前 fiber。
    pub fn on(
        &self,
        name: EventName,
        listener: impl Fn(&EventPayload) + Send + Sync + 'static,
    ) -> CoreResult<EffectHandle> {
        self.register(
            name,
            DispatchKind::Emit,
            ListenerBody::Emit(Arc::new(listener)),
        )
    }

    /// 注册 parallel 监听器（全并发 await）。
    pub fn on_parallel(
        &self,
        name: EventName,
        listener: impl Fn(EventPayload) -> BoxFuture<'static, CoreResult<()>> + Send + Sync + 'static,
    ) -> CoreResult<EffectHandle> {
        self.register(
            name,
            DispatchKind::Parallel,
            ListenerBody::Parallel(Arc::new(listener)),
        )
    }

    /// 注册 serial 监听器（异步按序；`Ok(Some(v))` 即 bail 值）。
    pub fn on_serial(
        &self,
        name: EventName,
        listener: impl Fn(EventPayload) -> BoxFuture<'static, CoreResult<Option<EventPayload>>>
        + Send
        + Sync
        + 'static,
    ) -> CoreResult<EffectHandle> {
        self.register(
            name,
            DispatchKind::Serial,
            ListenerBody::Serial(Arc::new(listener)),
        )
    }

    /// 注册 bail 监听器（同步按序；首个非 `None` 停止）。
    pub fn on_bail(
        &self,
        name: EventName,
        listener: impl Fn(&EventPayload) -> Option<EventPayload> + Send + Sync + 'static,
    ) -> CoreResult<EffectHandle> {
        self.register(
            name,
            DispatchKind::Bail,
            ListenerBody::Bail(Arc::new(listener)),
        )
    }

    /// 注册 waterfall 监听器（`next()` 委托、不调即短路；载荷 `P` 与返回值 `V` 分离）。
    pub fn on_waterfall<P: Send + Sync + 'static, V: Send + Sync + 'static>(
        &self,
        name: EventName,
        listener: impl for<'a> Fn(&'a mut Decision<P, V>) -> BoxFuture<'a, V> + Send + Sync + 'static,
    ) -> CoreResult<EffectHandle> {
        // 先 coerce 成 trait 对象、再包进 Arc<dyn Any>（无自动 upcast coercion）：
        // 否则 Arc<dyn Any> 里装的是具体闭包类型，分发时 downcast 会失败。
        let listener: WaterfallListener<P, V> = Arc::new(listener);
        let listener: Arc<dyn Any + Send + Sync> = Arc::new(listener);
        self.register(
            name,
            DispatchKind::Waterfall,
            ListenerBody::Waterfall {
                ty: (TypeId::of::<P>(), TypeId::of::<V>()),
                listener,
            },
        )
    }

    fn register(
        &self,
        name: EventName,
        kind: DispatchKind,
        body: ListenerBody,
    ) -> CoreResult<EffectHandle> {
        if self.fiber.is_disposed() {
            return Err(CoreError::InactiveFiber("ctx.on*"));
        }
        let id = self.registry.next_listener_id();
        self.registry.push_listener(
            name,
            RegisteredListener {
                id,
                kind,
                body,
                scope: self.scope.clone(),
            },
        );
        let registry = self.registry.clone();
        let handle = EffectHandle::new(move || registry.remove_listener(name, id));
        self.fiber.push(handle.clone());
        Ok(handle)
    }

    /// 同步分发（全实例广播，等价于 `target(All).emit`）：逐个调用 emit 监听器，返回值忽略。
    pub fn emit(&self, name: EventName, payload: EventPayload) {
        self.target(ScopeTarget::All).emit(name, payload);
    }

    /// 并发分发（全实例广播）：全部 parallel 监听器同时 await；任一失败 → 聚合错误。
    pub async fn parallel(&self, name: EventName, payload: EventPayload) -> CoreResult<()> {
        self.target(ScopeTarget::All).parallel(name, payload).await
    }

    /// 异步按序分发（全实例广播）：首个 `Ok(Some(v))` 即返回；错误即传播（同 JS `serial`）。
    pub async fn serial(
        &self,
        name: EventName,
        payload: EventPayload,
    ) -> CoreResult<Option<EventPayload>> {
        self.target(ScopeTarget::All).serial(name, payload).await
    }

    /// 同步按序分发（全实例广播）：首个非 `None` 返回（同 JS `bail`）。
    pub fn bail(&self, name: EventName, payload: EventPayload) -> Option<EventPayload> {
        self.target(ScopeTarget::All).bail(name, payload)
    }

    /// waterfall 分发（全实例广播）：载荷 `P` 与返回值 `V` 分离；
    /// 监听器包裹剩余链，链尾为调用方提供的默认行为（最内层 `next`）。
    pub async fn waterfall<P, V, D>(&self, name: EventName, initial: P, default: D) -> CoreResult<V>
    where
        P: Send + Sync + 'static,
        V: Send + Sync + 'static,
        D: for<'a> FnOnce(&'a mut Decision<P, V>) -> BoxFuture<'a, V> + Send + 'static,
    {
        self.target(ScopeTarget::All)
            .waterfall(name, initial, default)
            .await
    }
}

/// 带 scope 分发目标的事件视图（对应 dsh `scopeTarget` 载体）。
///
/// 路由规则（语义权威：`packages/core/scope/src/index.ts`）：
/// - [`ScopeTarget::All`]：所有监听器；
/// - [`ScopeTarget::None`]：仅无 tag 监听器（unkeyed carrier）；
/// - [`ScopeTarget::Key`]`(k)`：无 tag 监听器 + tag 属于 `k` 祖先链的监听器
///   （事件只向上流：祖先监听子孙，子孙不监听祖先）。
pub struct Target<'a> {
    ctx: &'a Context,
    target: ScopeTarget,
}

impl Target<'_> {
    /// 同步分发：逐个调用 emit 监听器，返回值忽略。
    pub fn emit(&self, name: EventName, payload: EventPayload) {
        for body in self
            .ctx
            .registry
            .snapshot(name, DispatchKind::Emit, &self.target)
        {
            if let ListenerBody::Emit(listener) = body {
                listener(&payload);
            }
        }
    }

    /// 并发分发：全部 parallel 监听器同时 await；任一失败 → 聚合错误。
    pub async fn parallel(&self, name: EventName, payload: EventPayload) -> CoreResult<()> {
        let bodies = self
            .ctx
            .registry
            .snapshot(name, DispatchKind::Parallel, &self.target);
        let futures = bodies.into_iter().map(|body| {
            let value = payload.clone();
            async move {
                match body {
                    ListenerBody::Parallel(listener) => listener(value).await,
                    _ => unreachable!("snapshot 已按分发模式过滤"),
                }
            }
        });
        let results = futures::future::join_all(futures).await;
        let errors: Vec<CoreError> = results.into_iter().filter_map(Result::err).collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(CoreError::ListenerAggregate(name, errors))
        }
    }

    /// 异步按序分发：首个 `Ok(Some(v))` 即返回；错误即传播（同 JS `serial`）。
    pub async fn serial(
        &self,
        name: EventName,
        payload: EventPayload,
    ) -> CoreResult<Option<EventPayload>> {
        for body in self
            .ctx
            .registry
            .snapshot(name, DispatchKind::Serial, &self.target)
        {
            let ListenerBody::Serial(listener) = body else {
                unreachable!("snapshot 已按分发模式过滤");
            };
            if let Some(value) = listener(payload.clone()).await? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    /// 同步按序分发：首个非 `None` 返回（同 JS `bail`）。
    pub fn bail(&self, name: EventName, payload: EventPayload) -> Option<EventPayload> {
        for body in self
            .ctx
            .registry
            .snapshot(name, DispatchKind::Bail, &self.target)
        {
            let ListenerBody::Bail(listener) = body else {
                unreachable!("snapshot 已按分发模式过滤");
            };
            if let Some(value) = listener(&payload) {
                return Some(value);
            }
        }
        None
    }

    /// waterfall 分发：载荷 `P` 与返回值 `V` 分离；监听器包裹剩余链，
    /// 链尾为调用方提供的默认行为（最内层 `next`）。
    pub async fn waterfall<P, V, D>(&self, name: EventName, initial: P, default: D) -> CoreResult<V>
    where
        P: Send + Sync + 'static,
        V: Send + Sync + 'static,
        D: for<'a> FnOnce(&'a mut Decision<P, V>) -> BoxFuture<'a, V> + Send + 'static,
    {
        let listeners: VecDeque<WaterfallListener<P, V>> = self
            .ctx
            .registry
            .snapshot(name, DispatchKind::Waterfall, &self.target)
            .into_iter()
            .filter_map(|body| match body {
                ListenerBody::Waterfall { ty, listener }
                    if ty == (TypeId::of::<P>(), TypeId::of::<V>()) =>
                {
                    listener
                        .downcast::<WaterfallListener<P, V>>()
                        .ok()
                        .map(|arc| (*arc).clone())
                }
                _ => None,
            })
            .collect();
        let mut decision = Decision::new(initial, listeners, Some(Box::new(default)));
        Ok(decision.next().await)
    }
}
