//! JSON 桥服务（P9）：B 形态（dlopen）插件按名调用宿主服务的通道。
//!
//! 边界载荷纪律（docs/b-abi.md §2）：一切结构化数据 = JSON 字符串。B 插件经
//! `get_service` 拿到服务的不透明句柄、经 `service_call` 以 `method` + `args`（JSON）
//! 调用、得到结果 JSON——本模块定义宿主侧的服务方接口与注册表。
//!
//! 约定：桥名（注册键）与 [`Service::NAME`] 一致；各桥的方法集由实现方文档化，
//! 未知方法返回 [`CoreError`]。桥由宿主或插件在 apply 内注册；dlopen 桥在装载时
//! 快照（[`BridgeRegistry::snapshot`]），保证 `get_service` 返回的指针在插件生命周期内稳定。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::{Context, CoreError, CoreResult, Service};

/// JSON 桥服务：B 形态插件按名调用的宿主服务（对象安全）。
///
/// `method` + `args`（JSON）→ 结果（JSON）。实现方自行定义并文档化方法集。
///
/// 注意：本 trait 不继承 [`Service`]（其关联常量 `NAME` 使 trait 非 dyn-compatible，
/// 见 docs/b-abi.md §9 豁免条款）；注册键（桥名）按约定与 `Service::NAME` 一致。
pub trait JsonBridge: Send + Sync + 'static {
    /// 以 JSON 方式调用一个桥方法；未知方法/参数非法 → `Err`。
    fn call(&self, method: &str, args: serde_json::Value) -> CoreResult<serde_json::Value>;
}

/// JSON 桥注册表服务（`ctx.provide` 为 `"bridges"`）：B 形态插件按服务名调用宿主服务的通道。
///
/// 与 `ToolRegistry`/`LlmRegistry` 同构：宿主装配后由各服务方注册桥实现
/// （`impl JsonBridge for T` + 注册），dlopen 桥在装载时快照。
pub struct BridgeRegistry {
    bridges: Mutex<BTreeMap<&'static str, Arc<dyn JsonBridge>>>,
}

impl Service for BridgeRegistry {
    const NAME: &'static str = "bridges";
}

impl BridgeRegistry {
    /// 空注册表。
    pub fn new(_root: &Context) -> Self {
        Self {
            bridges: Mutex::new(BTreeMap::new()),
        }
    }

    /// 注册桥（同名拒绝，fail loud）；`name` 应与 `Service::NAME` 一致。
    pub fn register(&self, name: &'static str, bridge: Arc<dyn JsonBridge>) -> CoreResult<()> {
        let mut bridges = self.bridges.lock().unwrap();
        if bridges.contains_key(name) {
            return Err(CoreError::Other(format!("JSON 桥 '{name}' 已注册")));
        }
        bridges.insert(name, bridge);
        Ok(())
    }

    /// 按名取桥。
    pub fn get(&self, name: &str) -> Option<Arc<dyn JsonBridge>> {
        self.bridges.lock().unwrap().get(name).cloned()
    }

    /// 已注册桥名（排序稳定）。
    pub fn names(&self) -> Vec<&'static str> {
        self.bridges.lock().unwrap().keys().copied().collect()
    }

    /// 快照（Arc 克隆）：dlopen 桥持有快照，保证 `get_service` 返回的
    /// 不透明指针在插件生命周期内稳定（注册方卸载不会使指针悬垂）。
    pub fn snapshot(&self) -> Vec<(&'static str, Arc<dyn JsonBridge>)> {
        self.bridges
            .lock()
            .unwrap()
            .iter()
            .map(|(name, bridge)| (*name, bridge.clone()))
            .collect()
    }
}
