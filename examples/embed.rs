//! 把 cos 作为库嵌入自己的程序（**零插件装配**）。
//!
//! 运行：`cargo run --example embed`
//!
//! 演示链路：
//! 1. `config_path: None` 装配 —— 不读 cordis.yml、不装载任何插件，只提供内置服务
//!    （Context 事件总线 / 服务仓库 / 工具注册表 / LLM 注册表 / agent 注册表 / 会话日志）；
//! 2. 程序化注册一个自定义工具（实现 `Tool` trait）；
//! 3. 实现一个自研 `LlmAdapter`（回显适配器，零网络依赖）；
//! 4. 经 `AgentRegistry` 创建主 agent（loop 驱动）并跑一轮 turn；
//! 5. `finish_with` 收尾（不变量校验 + 优雅卸载；agent 为自行创建 → 指定给它）。
//!
//! 把这里的适配器换成真实 LLM（或经 `LlmRegistry` 注册 plugin-opencode / plugin-deepseek
//! 等 Provider 工厂），再让模型看到你的工具 schema，就是完整的嵌入式 agent 应用。

use std::pin::Pin;
use std::sync::Arc;

use cos::agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use cos::core::Context;
use cos::llm::{ChunkDelta, LlmAdapter, LlmRequest, LlmStream, Message, StreamChunk};
use cos::session::ToolError;
use cos::tools::{Tool, ToolOutcome, ToolRegistry};
use cos::{RunConfig, UserMessage, assemble, finish_with, run_turn};

/// 回显适配器：把请求里最后一条用户消息原样回给模型侧（演示接缝，不做真实推理）。
struct EchoAdapter;

impl LlmAdapter for EchoAdapter {
    fn id(&self) -> &str {
        "echo"
    }

    fn stream(&self, request: &LlmRequest) -> LlmStream {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::User(user) => Some(user.content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let chunk = StreamChunk {
            delta: ChunkDelta::Text {
                text: format!("echo: {last_user}"),
            },
            usage: None,
        };
        Box::pin(futures::stream::iter(vec![Ok(chunk)]))
    }
}

/// 自定义工具：报告当前 unix 时间戳（注册后进入模型可见 schema；
/// 实际调用与否取决于适配器是否发出 ToolUse）。
struct NowTool;

impl Tool for NowTool {
    fn name(&self) -> &'static str {
        "now"
    }

    fn description(&self) -> &'static str {
        "返回当前 unix 时间戳（秒）。"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn execute(
        &self,
        _ctx: &Context,
        _run: &cos::tools::ToolRun,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<ToolOutcome, ToolError>> + Send + 'static>>
    {
        Box::pin(async {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Ok(ToolOutcome::ok(format!("{secs}")))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), cos::AppError> {
    // 1) 零插件装配：不读 cordis.yml、不装载任何插件
    let config = RunConfig {
        config_path: None,
        dump_config: false,
        session_id: "embed-demo".into(),
        prompt: None,
        session_path: None,
        cancel: None,
        llm: None,
        agent_llm: None,
        agent_driver: None,
        patch_files: Vec::new(),
    };
    let assembled = assemble(&config).await?;
    println!(
        "装配完成：插件 {} 个实例，主 agent: {}",
        assembled.app.instances().len(),
        if assembled.agent.is_some() {
            "有（未配置 LLM 时无）"
        } else {
            "无（下面自行创建）"
        }
    );

    // 2) 程序化注册自定义工具（框架内置服务全部可用）
    assembled
        .root
        .get::<ToolRegistry>()
        .expect("刚装配")
        .register(Arc::new(NowTool))?;
    println!("已注册工具: {}", <NowTool as Tool>::name(&NowTool));

    // 3) 自研 LLM 适配器 → 4) 创建主 agent（loop 驱动）
    let adapter: Arc<dyn LlmAdapter> = Arc::new(EchoAdapter);
    let agent = assembled
        .root
        .get::<AgentRegistry>()
        .expect("刚装配")
        .create(CreateAgentOptions {
            session_id: "embed-demo".into(),
            options: AgentOptions::default(),
            adapter,
        })
        .await?;

    // 5) 跑一轮（turn/step 主循环 + 会话日志）
    let summary = run_turn(&agent, UserMessage::new("你好，cos！"), None).await;
    println!("回复: {}", summary.reply);
    println!("工具轨迹: {:?}", summary.tool_trace);

    // 6) 收尾（不变量校验 + 优雅卸载）。agent 为自行创建 → 用 finish_with 指定它
    let report = finish_with(&assembled, &agent, &config).await?;
    println!(
        "收尾：会话事件 {} 条，不变量违规 {} 条，卸载顺序: {:?}",
        report.events.len(),
        report.violations.len(),
        report.unload_order
    );
    Ok(())
}
