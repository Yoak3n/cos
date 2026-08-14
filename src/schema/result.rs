//! 运行结果。


use thiserror::Error;

pub use cos_loader as loader;

/// cos 边界错误。
#[derive(Debug, Error)]
pub enum AppError {
    /// 装载失败。
    #[error(transparent)]
    Load(#[from] loader::LoadError),
    /// 会话失败。
    #[error(transparent)]
    Session(#[from] cos_session::SessionError),
    /// 内核失败。
    #[error(transparent)]
    Core(#[from] cos_core::CoreError),
    /// agent 失败。
    #[error(transparent)]
    Agent(#[from] cos_agent::AgentError),
    /// I/O 失败。
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// 其他失败。
    #[error("{0}")]
    Other(String),
}

impl From<cos_rpc::RpcError> for AppError {
    fn from(error: cos_rpc::RpcError) -> Self {
        match error {
            cos_rpc::RpcError::Io(error) => AppError::Io(error),
            cos_rpc::RpcError::Other(message) => AppError::Other(message),
        }
    }
}