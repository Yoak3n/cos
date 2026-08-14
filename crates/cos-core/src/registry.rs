//! 服务仓库 + 事件监听器存储（根 Context 创建，fork 出的 Context 共享）。

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{CoreError, CoreResult};
use crate::events::{DispatchKind, ListenerBody};
use crate::scope::{ScopeKey, ScopeTarget};
use crate::service::Service;

/// 服务仓库与事件总线的共享存储。
pub(crate) struct Registry {
    /// `TypeId` → 服务实例（同类型只允许一个实现）。
    services: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
    /// 服务名 → `TypeId`（同名冲突检测与依赖解析）。
    service_names: Mutex<HashMap<&'static str, TypeId>>,
    /// 事件名 → 已注册监听器。
    listeners: Mutex<HashMap<&'static str, Vec<RegisteredListener>>>,
    /// scope 父链（同 dsh `scopeParents`）：每个 key 至多一个父级。
    scope_parents: Mutex<HashMap<ScopeKey, ScopeKey>>,
    /// 监听器稳定 id 计数器。
    next_listener_id: AtomicU64,
}

/// 带稳定 id 的监听器条目（卸载时按 id 移除）。
pub(crate) struct RegisteredListener {
    /// 稳定 id。
    pub id: u64,
    /// 声明的分发模式。
    pub kind: DispatchKind,
    /// 监听器本体。
    pub body: ListenerBody,
    /// 注册处上下文的 scope tag（参与路由，同 dsh hook 所属 ctx 的 scope）。
    pub scope: Option<ScopeKey>,
}

impl Registry {
    /// 新建空注册表。
    pub fn new() -> Self {
        Self {
            services: Mutex::new(HashMap::new()),
            service_names: Mutex::new(HashMap::new()),
            listeners: Mutex::new(HashMap::new()),
            scope_parents: Mutex::new(HashMap::new()),
            next_listener_id: AtomicU64::new(1),
        }
    }

    /// 注册服务；同名或同类型重复注册 → [`CoreError::DuplicateService`]。
    pub fn insert_service(
        &self,
        tid: TypeId,
        name: &'static str,
        value: Arc<dyn Any + Send + Sync>,
    ) -> CoreResult<()> {
        let mut names = self.service_names.lock().unwrap();
        if let Some(previous) = names.get(name)
            && *previous != tid
        {
            return Err(CoreError::DuplicateService(name));
        }
        let mut services = self.services.lock().unwrap();
        if services.contains_key(&tid) {
            return Err(CoreError::DuplicateService(name));
        }
        services.insert(tid, value);
        names.insert(name, tid);
        Ok(())
    }

    /// 反注册服务；名字映射仅在其仍指向该类型时移除（防旧句柄误删新服务）。
    pub fn remove_service(&self, tid: TypeId, name: &'static str) {
        let mut names = self.service_names.lock().unwrap();
        if names.get(name) == Some(&tid) {
            names.remove(name);
        }
        drop(names);
        self.services.lock().unwrap().remove(&tid);
    }

    /// 按类型取服务；未注册 → [`CoreError::ServiceNotFound`]。
    pub fn get_service<T: Service>(&self) -> CoreResult<Arc<T>> {
        let services = self.services.lock().unwrap();
        let any = services
            .get(&TypeId::of::<T>())
            .ok_or(CoreError::ServiceNotFound(T::NAME))?;
        any.clone()
            .downcast::<T>()
            .map_err(|_| CoreError::ServiceTypeMismatch(T::NAME))
    }

    /// 绑定 scope 父链（同 dsh `bindScopeParent`）：每个 key 只允许绑定一次；拒绝成环。
    pub fn bind_scope_parent(&self, key: ScopeKey, parent: ScopeKey) -> CoreResult<()> {
        let mut parents = self.scope_parents.lock().unwrap();
        if parents.contains_key(&key) {
            return Err(CoreError::ScopeParentAlreadyBound(key));
        }
        let mut cursor = Some(parent.clone());
        while let Some(current) = cursor {
            if current == key {
                return Err(CoreError::ScopeParentCycle(key, parent));
            }
            cursor = parents.get(&current).cloned();
        }
        parents.insert(key, parent);
        Ok(())
    }

    /// scope 祖先链，最近者在前（同 dsh `scopeChainOf`）：`[key, parent, grandparent, …]`。
    pub fn scope_chain_of(&self, key: &ScopeKey) -> Vec<ScopeKey> {
        let parents = self.scope_parents.lock().unwrap();
        let mut chain = vec![key.clone()];
        let mut cursor = parents.get(key).cloned();
        while let Some(current) = cursor {
            chain.push(current.clone());
            cursor = parents.get(&current).cloned();
        }
        chain
    }

    /// 分配下一个监听器 id。
    pub fn next_listener_id(&self) -> u64 {
        self.next_listener_id.fetch_add(1, Ordering::Relaxed)
    }

    /// 追加监听器。
    pub fn push_listener(&self, name: &'static str, listener: RegisteredListener) {
        self.listeners
            .lock()
            .unwrap()
            .entry(name)
            .or_default()
            .push(listener);
    }

    /// 按 id 移除监听器（幂等）。
    pub fn remove_listener(&self, name: &'static str, id: u64) {
        let mut listeners = self.listeners.lock().unwrap();
        if let Some(entries) = listeners.get_mut(name) {
            entries.retain(|entry| entry.id != id);
            if entries.is_empty() {
                listeners.remove(name);
            }
        }
    }

    /// 取某个分发模式的监听器快照（`Arc` 克隆、按 scope 目标过滤），供分发阶段无锁调用。
    pub fn snapshot(
        &self,
        name: &'static str,
        kind: DispatchKind,
        target: &ScopeTarget,
    ) -> Vec<ListenerBody> {
        let entries: Vec<(Option<ScopeKey>, ListenerBody)> = self
            .listeners
            .lock()
            .unwrap()
            .get(name)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry.kind == kind)
                    .map(|entry| (entry.scope.clone(), entry.body.clone()))
                    .collect()
            })
            .unwrap_or_default();
        match target {
            ScopeTarget::All => entries.into_iter().map(|(_, body)| body).collect(),
            ScopeTarget::None => entries
                .into_iter()
                .filter_map(|(scope, body)| scope.is_none().then_some(body))
                .collect(),
            ScopeTarget::Key(key) => {
                let chain = self.scope_chain_of(key);
                entries
                    .into_iter()
                    .filter_map(|(scope, body)| {
                        let admitted = scope.is_none()
                            || scope.as_ref().is_some_and(|tag| chain.contains(tag));
                        admitted.then_some(body)
                    })
                    .collect()
            }
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}
