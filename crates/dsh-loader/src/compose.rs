//! 组合装载：工厂解析 → inject/provide 建图 → 拓扑排序 → 按序 fork + apply。
//!
//! 依赖就绪才激活（PLAN.md §1）：无运行时替换，拓扑排序替代 dsh 的响应式重载；
//! 环 / 缺依赖 / 重复 provide 一律装载即报错（fail loud at load，带插件名）。

use std::collections::{HashMap, VecDeque};

use dsh_core::Context;

use crate::error::LoadError;
use crate::profile::{Entry, Profile};
use crate::registry::{self, PluginRegistrar};

/// 装载结果：根上下文 + 各插件实例（按 apply 顺序）。
pub struct LoadedApp {
    root: Context,
    instances: Vec<LoadedPlugin>,
}

/// 一个已装载的插件实例。
pub struct LoadedPlugin {
    /// 条目实例 id。
    pub entry_id: String,
    /// 工厂名。
    pub name: String,
    /// 该实例 fork 出的上下文（持有其 fiber）。
    pub context: Context,
}

impl std::fmt::Debug for LoadedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedPlugin")
            .field("entry_id", &self.entry_id)
            .field("name", &self.name)
            .field("context", &"<Context>")
            .finish()
    }
}

impl std::fmt::Debug for LoadedApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedApp")
            .field("root", &"<Context>")
            .field("instances", &self.instances)
            .finish()
    }
}

impl LoadedApp {
    /// 根上下文（插件服务都在其上可查）。
    pub fn root(&self) -> &Context {
        &self.root
    }

    /// 已装载的插件实例（apply 顺序）。
    pub fn instances(&self) -> &[LoadedPlugin] {
        &self.instances
    }

    /// 同步卸载：按 apply 逆序 dispose 各实例 fiber（同 dsh 卸载逆序回滚）。
    pub fn dispose(&self) {
        for instance in self.instances.iter().rev() {
            instance.context.fiber().dispose();
        }
    }

    /// 优雅卸载：apply 逆序，同步反注册 + 等待异步 disposer。
    pub async fn dispose_async(&self) {
        for instance in self.instances.iter().rev() {
            instance.context.fiber().dispose_async().await;
        }
    }
}

/// 计划条目：拓扑排序后的待装载插件（未 apply）。
#[derive(Clone)]
pub struct PlannedEntry {
    /// 条目实例 id。
    pub entry_id: String,
    /// 工厂名。
    pub name: String,
    /// 有效配置。
    pub config: serde_json::Value,
    pub(crate) factory: &'static PluginRegistrar,
}

impl PlannedEntry {
    /// 计划的 JSON 视图（`--dump-config` 用）。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.entry_id,
            "name": self.name,
            "config": self.config,
        })
    }
}

/// 解析 + 拓扑排序，不 apply（`--dump-config` 与装载共用同一路径，保证输出与装载一致）。
pub fn plan(profile: &Profile) -> Result<Vec<PlannedEntry>, LoadError> {
    // 1. 解析工厂 + 重复 provide 检测。
    // resolved 只含未禁用条目（其下标即图节点下标）。
    let mut resolved: Vec<(&Entry, &'static PluginRegistrar)> = Vec::new();
    // 服务名 → 首个提供者节点下标。
    let mut provider: HashMap<&'static str, usize> = HashMap::new();

    for entry in &profile.0 {
        if entry.disabled {
            continue;
        }
        let factory =
            registry::resolve_factory(&entry.name).ok_or_else(|| LoadError::UnknownPlugin {
                name: entry.name.clone(),
                available: registry::available_plugins(),
            })?;
        for service in (factory.provide)() {
            if let Some(previous) = provider.get(service) {
                return Err(LoadError::DuplicateProvide {
                    service: (*service).to_string(),
                    plugins: vec![
                        resolved[*previous].0.id().to_string(),
                        entry.id().to_string(),
                    ],
                });
            }
            provider.insert(service, resolved.len());
        }
        resolved.push((entry, factory));
    }

    // 2. 建图：deps[i] = 第 i 个节点依赖的服务提供者下标（自己提供自己不成边）。
    let node_count = resolved.len();
    let mut deps: Vec<Vec<usize>> = Vec::with_capacity(node_count);
    for (entry_index, (entry, factory)) in resolved.iter().enumerate() {
        let mut required: Vec<&str> =
            Vec::with_capacity((factory.inject)().len() + entry.inject.len());
        required.extend((factory.inject)().iter().copied());
        required.extend(entry.inject.iter().map(String::as_str));

        let mut node_deps = Vec::new();
        for service in required {
            let provider_index =
                provider
                    .get(service)
                    .copied()
                    .ok_or_else(|| LoadError::MissingDependency {
                        plugin: entry.id().to_string(),
                        service: service.to_string(),
                    })?;
            if provider_index != entry_index {
                node_deps.push(provider_index);
            }
        }
        deps.push(node_deps);
    }

    // 3. Kahn 拓扑排序（提供者先于消费者）。
    let mut indegree = vec![0usize; node_count];
    let mut consumers: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    for (consumer_index, node_deps) in deps.iter().enumerate() {
        for &provider_index in node_deps {
            consumers[provider_index].push(consumer_index);
            indegree[consumer_index] += 1;
        }
    }
    let mut ready: VecDeque<usize> = (0..node_count)
        .filter(|&index| indegree[index] == 0)
        .collect();
    let mut order = Vec::with_capacity(node_count);
    while let Some(index) = ready.pop_front() {
        order.push(index);
        for &consumer_index in &consumers[index] {
            indegree[consumer_index] -= 1;
            if indegree[consumer_index] == 0 {
                ready.push_back(consumer_index);
            }
        }
    }
    if order.len() < node_count {
        let cycle: Vec<String> = (0..node_count)
            .filter(|index| !order.contains(index))
            .map(|index| resolved[index].0.id().to_string())
            .collect();
        return Err(LoadError::DependencyCycle { cycle });
    }

    Ok(order
        .into_iter()
        .map(|index| {
            let (entry, factory) = resolved[index];
            PlannedEntry {
                entry_id: entry.id().to_string(),
                name: entry.name.clone(),
                config: entry.config.clone(),
                factory,
            }
        })
        .collect())
}

/// `--dump-config`：计划的 JSON 表示（与装载同序）。
pub fn dump_plan(profile: &Profile) -> Result<String, LoadError> {
    let plan = plan(profile)?;
    let entries: Vec<serde_json::Value> = plan.iter().map(PlannedEntry::to_json).collect();
    serde_json::to_string_pretty(&entries)
        .map_err(|error| LoadError::Other(format!("dump 序列化失败: {error}")))
}

/// 在根上下文上按清单装载插件树（v1：单一清单，无层叠）。
pub fn load(root: &Context, profile: &Profile) -> Result<LoadedApp, LoadError> {
    let plan = plan(profile)?;
    let mut instances = Vec::with_capacity(plan.len());
    for entry in plan {
        let context = root.fork();
        (entry.factory.apply)(&context, &entry.entry_id, entry.config.clone())?;
        instances.push(LoadedPlugin {
            entry_id: entry.entry_id,
            name: entry.name,
            context,
        });
    }

    Ok(LoadedApp {
        root: root.clone(),
        instances,
    })
}
