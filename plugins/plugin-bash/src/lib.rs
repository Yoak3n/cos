//! plugin-bash —— bash 工具（P6，v1 前台执行、无后台 job、无 sandbox）。
//!
//! 接缝纪律：只依赖 Definition crate（cos-tools / cos-shell / cos-core）。

#![warn(missing_docs)]

use std::sync::Arc;

use cos_core::{Context, CoreError, Plugin, Validate};
use cos_shell::{ShellOutput, ShellProvider};
use cos_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde::Deserialize;

/// 插件配置（空）。
#[derive(Deserialize)]
pub struct BashConfig;

impl Validate for BashConfig {}

/// bash 工具。
struct BashTool;

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "前台执行 shell 命令（v1：无 sandbox）"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的命令" }
            },
            "required": ["command"]
        })
    }

    fn execute(
        &self,
        ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, cos_session::ToolError>> {
        let shell = match ctx.get::<ShellProvider>() {
            Ok(provider) => provider.inner.clone(),
            Err(_) => {
                return Box::pin(async {
                    Ok(ToolOutcome::error(
                        "shell 服务未装配".to_string(),
                        cos_session::ToolError {
                            name: "Shell".into(),
                            code: "NO_SHELL".into(),
                        },
                    ))
                });
            }
        };
        let command = run.arguments["command"].as_str().unwrap_or("").to_string();
        Box::pin(async move {
            match shell.run(&command, None).await {
                Ok(output) => Ok(format_output(output)),
                Err(error) => Ok(ToolOutcome::error(
                    error.to_string(),
                    cos_session::ToolError {
                        name: "Shell".into(),
                        code: "SPAWN_FAILED".into(),
                    },
                )),
            }
        })
    }
}

/// 结果格式化：退出码 + 输出；非零退出码记为错误结果。
fn format_output(output: ShellOutput) -> ToolOutcome {
    let mut text = format!("exit code: {}\n", output.exit_code);
    if !output.stdout.is_empty() {
        text.push_str(&format!("stdout:\n{}", output.stdout));
    }
    if !output.stderr.is_empty() {
        text.push_str(&format!("stderr:\n{}", output.stderr));
    }
    if output.exit_code == 0 {
        ToolOutcome::ok(text)
    } else {
        ToolOutcome::error(
            text,
            cos_session::ToolError {
                name: "Shell".into(),
                code: format!("EXIT_{}", output.exit_code),
            },
        )
    }
}

/// 插件主体。
#[derive(Default)]
pub struct BashPlugin;

impl Plugin for BashPlugin {
    const ID: &'static str = "plugin-bash";

    type Config = BashConfig;

    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<ToolRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("tools"))?;
        registry.register(Arc::new(BashTool))?;
        Ok(())
    }
}

cos_loader::plugin!("bash", BashPlugin);
