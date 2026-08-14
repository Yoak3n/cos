//! Agent 注册表 + task-local 因果链（照 dsh AgentRegistry / withInitiator）。
//!
//! - 注册表：id → 活 agent；重复注册报错；`agent/created` / `agent/disposed`
//!   经 scope 路由（`ScopeTarget::Key(agent:<id>)`）分发；
//! - 因果链：`with_initiator` 用 tokio task-local 把发起 agent 传给下游异步链
//!   （同进程因果归因，非授权/存活证明）。

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use cos_core::{Context, ScopeKey, ScopeTarget, Service};
use futures::future::BoxFuture;

use crate::types::{
    AgentCreatedPayload, AgentDisposedPayload, AgentError, AgentFactory, AgentTrait,
    CreateAgentOptions,
};

tokio::task_local! {
    static CURRENT_INITIATOR: Option<Arc<dyn AgentTrait>>;
}

/// 在当前任务链上建立发起 agent 边界（同 dsh `withInitiator`）。
pub fn with_initiator<F>(agent: Arc<dyn AgentTrait>, future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    CURRENT_INITIATOR.scope(Some(agent), future)
}

/// 建立无发起者的边界（同 dsh `withoutInitiator`：隐藏继承的发起者）。
pub fn without_initiator<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    CURRENT_INITIATOR.scope(None, future)
}

/// 读取当前发起 agent（无则 `None`）。
pub fn current_initiator() -> Option<Arc<dyn AgentTrait>> {
    CURRENT_INITIATOR
        .try_with(|slot| slot.clone())
        .ok()
        .flatten()
}

/// 内部状态。
struct RegistryInner {
    store: HashMap<String, Arc<dyn AgentTrait>>,
    factory: Option<Arc<dyn AgentFactory>>,
}

/// agent 注册表服务（`ctx.provide` 为 `"agents"`）。
#[derive(Clone)]
pub struct AgentRegistry {
    ctx: Context,
    inner: Arc<Mutex<RegistryInner>>,
}

impl Service for AgentRegistry {
    const NAME: &'static str = "agents";
}

impl AgentRegistry {
    /// 在根上下文上新建注册表。
    pub fn new(root: &Context) -> Self {
        Self {
            ctx: root.clone(),
            inner: Arc::new(Mutex::new(RegistryInner {
                store: HashMap::new(),
                factory: None,
            })),
        }
    }

    /// 注册 agent 创建工厂（loop 调用；重复注册报错）。
    pub fn set_factory(&self, factory: Arc<dyn AgentFactory>) -> Result<(), AgentError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.factory.is_some() {
            return Err(AgentError::Other(
                "an agent factory is already registered".into(),
            ));
        }
        inner.factory = Some(factory);
        Ok(())
    }

    /// 经工厂创建并发布 agent；成功后发 `agent/created`（scope 路由）。
    pub fn create(
        &self,
        options: CreateAgentOptions,
    ) -> BoxFuture<'static, Result<Arc<dyn AgentTrait>, AgentError>> {
        let factory = self
            .inner
            .lock()
            .unwrap()
            .factory
            .clone()
            .ok_or(AgentError::NoFactory);
        let this = self.clone_for_future();
        Box::pin(async move {
            let factory = factory?;
            let agent = factory.create(&this.ctx, options).await?;
            {
                let mut inner = this.inner.lock().unwrap();
                if inner.store.contains_key(agent.id()) {
                    return Err(AgentError::AlreadyRegistered(agent.id().to_string()));
                }
                inner.store.insert(agent.id().to_string(), agent.clone());
            }
            let target = this.agent_target(agent.id());
            this.ctx.target(target).emit(
                "agent/created",
                Arc::new(AgentCreatedPayload {
                    agent_id: agent.id().to_string(),
                }),
            );
            Ok(agent)
        })
    }

    /// 注销 agent（幂等）；若确曾注册则发 `agent/disposed`。
    pub fn unregister(&self, id: &str) -> Option<Arc<dyn AgentTrait>> {
        let removed = self.inner.lock().unwrap().store.remove(id);
        if removed.is_some() {
            let target = self.agent_target(id);
            self.ctx.target(target).emit(
                "agent/disposed",
                Arc::new(AgentDisposedPayload {
                    agent_id: id.to_string(),
                }),
            );
        }
        removed
    }

    /// 按 id 取活 agent。
    pub fn get(&self, id: &str) -> Option<Arc<dyn AgentTrait>> {
        self.inner.lock().unwrap().store.get(id).cloned()
    }

    /// 全部活 agent（注册序）。
    pub fn list(&self) -> Vec<Arc<dyn AgentTrait>> {
        self.inner.lock().unwrap().store.values().cloned().collect()
    }

    /// agent 的 scope 分发目标（`agent:<id>`）。
    pub fn agent_target(&self, id: &str) -> ScopeTarget {
        ScopeTarget::Key(ScopeKey::new(format!("agent:{id}")))
    }

    fn clone_for_future(&self) -> AgentRegistry {
        AgentRegistry {
            ctx: self.ctx.clone(),
            inner: self.inner.clone(),
        }
    }
}
