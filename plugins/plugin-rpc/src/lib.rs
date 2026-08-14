//! plugin-rpc —— RPC 模式插件（协议向 pi 对齐，引擎在 cos-rpc）。
//!
//! 接缝纪律：只依赖 Definition crate（cos-rpc / cos-agent / cos-core / cos-loader），
//! 不依赖宿主。apply 时向 [`cos_rpc::RpcProviderRegistry`] 注册默认提供者
//! （`serve_stdio`：stdin/stdout JSONL + 事件流）。宿主 `--rpc` 查注册表委托；
//! 未装配本插件时回退内置（同一引擎）。
//!
//! 配置（空）：
//! ```yaml
//! - name: rpc
//! ```

#![warn(missing_docs)]

use std::sync::Arc;

use cos_agent::Agent;
use cos_core::{Context, CoreError, Plugin, Validate};
use cos_rpc::{RpcError, RpcProvider, RpcProviderRegistry};
use futures::future::BoxFuture;
use serde::Deserialize;

/// 插件配置（空）。
#[derive(Deserialize)]
pub struct RpcConfig;

impl Validate for RpcConfig {}

/// 默认 RPC 提供者：cos-rpc 引擎的 stdio 服务。
#[derive(Clone, Default)]
struct DefaultProvider;

impl RpcProvider for DefaultProvider {
    fn serve(
        &self,
        agent: &Arc<dyn Agent>,
        cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    ) -> BoxFuture<'static, Result<(), RpcError>> {
        let agent = agent.clone();
        Box::pin(async move { cos_rpc::serve_stdio(&agent, cancel).await })
    }
}

/// 插件主体。
#[derive(Clone, Default)]
pub struct RpcPlugin;

impl Plugin for RpcPlugin {
    fn id(&self) -> &'static str {
        "plugin-rpc"
    }

    type Config = RpcConfig;

    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<RpcProviderRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("rpc-providers"))?;
        registry
            .register(Arc::new(DefaultProvider))
            .map_err(|error| CoreError::Other(error.to_string()))?;
        Ok(())
    }
}

cos_loader::plugin!("rpc", RpcPlugin);
