//! 会话边界错误（决策 D5）。

use thiserror::Error;

/// dsh-session 的边界错误。
#[derive(Debug, Error)]
pub enum SessionError {
    /// 持久化 I/O 失败。
    #[error("会话 I/O 失败: {0}")]
    Io(#[from] std::io::Error),
    /// JSON（反）序列化失败。
    #[error("会话 JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
    /// 格式版本不匹配（旧运行时拒绝读新日志，同 dsh 无迁移语义）。
    #[error("会话格式版本不匹配: 期望 {expected}，实际 {found}")]
    VersionMismatch {
        /// 本运行时支持的版本。
        expected: u32,
        /// 文件里的版本。
        found: u32,
    },
    /// 日志结构无效。
    #[error("会话日志无效: {0}")]
    Invalid(String),
}
