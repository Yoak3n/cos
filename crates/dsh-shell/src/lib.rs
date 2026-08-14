//! dsh-shell —— shell 接缝 + local 实现（P6）。
//!
//! v1 简化（PLAN.md P6）：前台执行、无后台 job、无 sandbox 后端。
//! 接缝纪律：插件消费 [`ShellProvider`] / [`Shell`] trait，不直接绑定 [`LocalShell`]。

#![warn(missing_docs)]

use std::path::Path;
use std::sync::Arc;

use dsh_core::{Context, Service};
use futures::future::BoxFuture;
use thiserror::Error;

/// shell 执行结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ShellOutput {
    /// 标准输出。
    pub stdout: String,
    /// 标准错误。
    pub stderr: String,
    /// 退出码。
    pub exit_code: i32,
}

/// shell 边界错误（仅进程启动失败；非零退出码属于正常结果，见 [`ShellOutput::exit_code`]）。
#[derive(Debug, Error)]
pub enum ShellError {
    /// 启动进程失败。
    #[error("shell 启动失败: {0}")]
    Spawn(String),
}

/// shell 接缝（对象安全）。
pub trait Shell: Send + Sync {
    /// 前台执行一条命令。
    fn run(
        &self,
        command: &str,
        cwd: Option<&Path>,
    ) -> BoxFuture<'static, Result<ShellOutput, ShellError>>;
}

/// shell 服务包装（`ctx.provide` 为 `"shell"`；provider 由 app 装配）。
pub struct ShellProvider {
    /// 具体实现。
    pub inner: Arc<dyn Shell>,
}

impl Service for ShellProvider {
    const NAME: &'static str = "shell";
}

/// 本地实现：`cmd /C`（Windows）前台执行。
pub struct LocalShell;

impl Shell for LocalShell {
    fn run(
        &self,
        command: &str,
        cwd: Option<&Path>,
    ) -> BoxFuture<'static, Result<ShellOutput, ShellError>> {
        let mut process = std::process::Command::new("cmd");
        process.args(["/C", command]);
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }
        Box::pin(async move {
            let output = process
                .output()
                .map_err(|error| ShellError::Spawn(error.to_string()))?;
            Ok(ShellOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            })
        })
    }
}

/// 在根上下文上装配本地 shell（app 使用）。
pub fn provide_local_shell(root: &Context) -> Result<(), dsh_core::CoreError> {
    root.provide(ShellProvider {
        inner: Arc::new(LocalShell),
    })?;
    Ok(())
}
