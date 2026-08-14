//! plugin-rpc 接线：apply 后宿主可查到默认 RPC 提供者；重复注册拒绝。

use std::sync::Arc;

use cos_agent::Agent;
use cos_core::{Context, Plugin};
use cos_rpc::{RpcError, RpcProvider, RpcProviderRegistry};
use futures::future::BoxFuture;
use plugin_rpc::RpcPlugin;

/// 测试用占位提供者（第二个 RPC 插件的模拟）。
struct TestProvider;

impl RpcProvider for TestProvider {
    fn serve(
        &self,
        _agent: &Arc<dyn Agent>,
        _cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> BoxFuture<'static, Result<(), RpcError>> {
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn apply_registers_default_rpc_provider() {
    let root = Context::root();
    root.provide(RpcProviderRegistry::new()).unwrap();
    RpcPlugin.apply(&root, &plugin_rpc::RpcConfig).unwrap();

    let registry = root.get::<RpcProviderRegistry>().unwrap();
    assert!(registry.get().is_some(), "apply 后应注册默认 RPC 提供者");
}

#[test]
fn missing_registry_fails_loud() {
    let root = Context::root();
    // 宿主未装配 rpc-providers 服务 → apply 必须报错（fail loud）
    let error = RpcPlugin.apply(&root, &plugin_rpc::RpcConfig).unwrap_err();
    assert!(error.to_string().contains("rpc-providers"), "{error}");
}

#[test]
fn duplicate_register_is_rejected() {
    let root = Context::root();
    root.provide(RpcProviderRegistry::new()).unwrap();
    RpcPlugin.apply(&root, &plugin_rpc::RpcConfig).unwrap();

    let registry = root.get::<RpcProviderRegistry>().unwrap();
    // 直接再注册一个（模拟第二个 RPC 插件）→ 拒绝
    let result = registry.register(Arc::new(TestProvider));
    assert!(result.is_err(), "重复注册应 fail loud: {result:?}");
}
