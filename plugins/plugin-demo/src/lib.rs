//! plugin-demo —— echo 类演示工具（供 e2e，P6）。

#![warn(missing_docs)]

use std::sync::Arc;

use cos_core::{Context, CoreError, Plugin, Validate};
use cos_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde::Deserialize;

/// 插件配置（空）。
#[derive(Deserialize)]
pub struct DemoConfig;

impl Validate for DemoConfig {}

/// echo 工具。
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "回声演示工具"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "要回显的文本" }
            },
            "required": ["text"]
        })
    }

    fn execute(
        &self,
        _ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, cos_session::ToolError>> {
        let text = run.arguments["text"].as_str().unwrap_or("").to_string();
        Box::pin(async move { Ok(ToolOutcome::ok(format!("echo: {text}"))) })
    }
}

/// 插件主体。
#[derive(Default)]
pub struct DemoPlugin;

impl Plugin for DemoPlugin {
    const ID: &'static str = "plugin-demo";

    type Config = DemoConfig;

    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<ToolRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("tools"))?;
        registry.register(Arc::new(EchoTool))?;
        Ok(())
    }
}

cos_loader::plugin!("demo", DemoPlugin);
