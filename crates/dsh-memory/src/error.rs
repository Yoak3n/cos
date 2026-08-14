//! 记忆边界错误。

use thiserror::Error;

/// dsh-memory 边界错误。
#[derive(Debug, Error)]
pub enum MemoryError {
    /// SQLite 失败。
    #[error("记忆存储失败: {0}")]
    Db(#[from] rusqlite::Error),
    /// JSON 失败。
    #[error("记忆 JSON 失败: {0}")]
    Json(#[from] serde_json::Error),
    /// LLM 失败。
    #[error("记忆 LLM 失败: {0}")]
    Llm(#[from] dsh_llm::LlmError),
    /// I/O 失败（建目录等）。
    #[error("记忆 I/O 失败: {0}")]
    Io(#[from] std::io::Error),
    /// 输出不符合契约。
    #[error("记忆输出无效: {0}")]
    Invalid(String),
}

/// dsh-memory 结果别名。
pub type Result<T> = std::result::Result<T, MemoryError>;
