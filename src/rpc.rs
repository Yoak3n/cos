//! RPC 模式宿主接线（协议引擎在 cos-rpc；实现由 plugin-rpc 提供）。
//!
//! `--rpc` 启动流程：查 `RpcProviderRegistry`——已装配 plugin-rpc（yml 声明）→
//! 委托插件提供者；未装配 → 回退 `cos_rpc::serve_stdio`（同一引擎，零配置可用）。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::{AppError, Assembled};

/// 运行 RPC 服务：插件提供者优先，未注册回退内置 stdio。
pub async fn serve(assembled: &Assembled, cancel: Option<Arc<AtomicBool>>) -> Result<(), AppError> {
    let registry = assembled
        .root
        .get::<cos_rpc::RpcProviderRegistry>()
        .map_err(|_| AppError::Other("rpc-providers 服务未装配".into()))?;
    match registry.get() {
        Some(provider) => provider.serve(&assembled.agent, cancel).await?,
        None => cos_rpc::serve_stdio(&assembled.agent, cancel).await?,
    }
    Ok(())
}
