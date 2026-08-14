//! cos —— CLI 宿主库。
//!
//! 三种形态共用同一装配（[`assemble`]）与收尾（[`finish`]）：
//! - [`run`]：一次性（`--prompt`）——装配 → 一轮 → 收尾，返回 [`RunReport`]；
//! - `repl::serve_repl`：交互式 REPL（命令行持续对话）；
//! - `rpc::serve_rpc`：stdio JSON-RPC 服务（每行一个请求/响应，供外部程序调用）。

#![warn(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cos_agent::{Agent, AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::{Context, Plugin};
use cos_invariants::{InvariantRegistry, register_defaults};
use cos_llm::{
    ChunkDelta, InputContent, LlmAdapter, LlmRegistry, Message, StreamChunk, ToolCall, UserMessage,
};
use cos_llm_mock::{MockAdapter, MockReply};
use cos_llm_opencode::{OpencodeAdapter, OpencodeConfig};
use cos_loader::{self as loader, Profile};
use cos_memory::MemoryStore;
use cos_session::{
    AbortCause, SESSION_FORMAT_VERSION, SessionEvent, SessionEventData, SessionHeader, load_jsonl,
    save_jsonl,
};
use cos_shell::provide_local_shell;
use cos_system_prompt::PromptSections;
use cos_tools::ToolRegistry;
use thiserror::Error;

pub mod repl;
pub mod rpc;

/// 运行配置。
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// cordis.yml 路径。
    pub config_path: String,
    /// 只输出装载计划（不启动）。
    pub dump_config: bool,
    /// 会话 id。
    pub session_id: String,
    /// 一次性模式的用户消息（None = 交互/RPC 模式）。
    pub prompt: Option<String>,
    /// 会话 JSONL 输出路径（None = 不落盘）。
    pub session_path: Option<String>,
    /// 外部取消信号（main 的 Ctrl-C 监视任务写入）。
    pub cancel: Option<Arc<AtomicBool>>,
    /// 真实 LLM 配置（None = 确定性 mock 演示链路）。
    pub llm: Option<LlmConfig>,
    /// 主 agent 的 LLM 提供商/后备链 id（LLM 统一管理；None = 用 `llm` 的 "default" 或 demo mock）。
    pub agent_llm: Option<String>,
}

/// 真实 LLM 配置（opencode-go 等 OpenAI 兼容端点）。
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// base URL（不带 `/chat/completions` 后缀）。
    pub base_url: String,
    /// API key。
    pub api_key: String,
    /// 模型 id。
    pub model: String,
    /// 是否流式（false = 非流式单次；opencode zen/go 流式只出推理文本，建议 false）。
    pub streaming: bool,
}

/// 运行报告（测试与 CLI 打印消费）。
pub struct RunReport {
    /// `--dump-config` 的计划 JSON（仅 dump 模式）。
    pub dump: Option<String>,
    /// 优雅卸载顺序（apply 逆序）。
    pub unload_order: Vec<String>,
    /// 完整会话事件（快照/重放用）。
    pub events: Vec<SessionEvent>,
    /// 模型可见消息（derive_messages）。
    pub messages: Vec<Message>,
    /// 不变量违规（空 = 全过）。
    pub violations: Vec<String>,
    /// 卸载后插件服务已反注册（审计）。
    pub services_after_unload: bool,
}

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

/// demo 脚本：第一步工具调用 todo_write，第二步文本回复（确定性）。
fn demo_script() -> Vec<MockReply> {
    vec![
        MockReply::new(vec![StreamChunk {
            delta: ChunkDelta::ToolUse {
                call: ToolCall {
                    call_id: "demo-call-1".into(),
                    name: "todo_write".into(),
                    arguments:
                        r#"{"todos":[{"content":"演示任务：验证 A 形态","status":"in_progress"}]}"#
                            .into(),
                },
            },
            usage: None,
        }]),
        MockReply::text("已记录演示任务。"),
    ]
}

/// 内置插件的插件 id —— 同时是对插件 crate 的显式引用锚点：
/// 保证其 inventory 静态注册表被链接进 cos 可执行文件。
pub fn builtin_plugin_ids() -> [&'static str; 4] {
    [
        plugin_todo::TodoPlugin::ID,
        plugin_bash::BashPlugin::ID,
        plugin_memory::MemoryPlugin::ID,
        plugin_llm::LlmPlugin::ID,
    ]
}

/// 装配结果（REPL / RPC / 一次性共用）。
pub struct Assembled {
    /// 根上下文（服务/事件总线）。
    pub root: Context,
    /// 已装载插件树。
    pub app: loader::LoadedApp,
    /// 主 agent（对话入口）。
    pub agent: Arc<dyn Agent>,
    /// 是否回落到确定性演示脚本（未配置真实 LLM）。
    pub demo_mode: bool,
}

/// 一轮交互的摘要（REPL/RPC 共用）。
#[derive(Debug, Clone)]
pub struct TurnSummary {
    /// 结束的 turn 号。
    pub turn: u32,
    /// 该 turn 的助手文本（多步拼接）。
    pub reply: String,
    /// 工具轨迹（如 "todo_write → 已写入 1 条任务"）。
    pub tool_trace: Vec<String>,
    /// 是否被取消信号中断。
    pub cancelled: bool,
}

/// 装配：内置服务 + LLM 注册表 + 插件树 + 主 agent（三种形态共用）。
pub async fn assemble(config: &RunConfig) -> Result<Assembled, AppError> {
    // 锚点：保证插件 crate 的 inventory 注册表被链接
    let _ = builtin_plugin_ids();
    let root = Context::root();
    // 内置服务（app 装配，先于插件树）
    root.provide(ToolRegistry::new(&root))?;
    root.provide(PromptSections::new(&root))?;
    root.provide(InvariantRegistry::new(&root))?;
    provide_local_shell(&root)?;
    root.provide(AgentRegistry::new(&root))?;
    // LLM 统一管理：宿主装配空注册表；--llm-* 注册 "default"；plugin-llm 按 yml 填充
    root.provide(LlmRegistry::new(&root))?;
    if let Some(cfg) = &config.llm {
        root.get::<LlmRegistry>().expect("刚装配").register(
            "default",
            Arc::new(OpencodeAdapter::new(OpencodeConfig {
                base_url: cfg.base_url.clone(),
                api_key: cfg.api_key.clone(),
                model: cfg.model.clone(),
                streaming: cfg.streaming,
                input_content: vec![InputContent::Text],
            })),
        )?;
    } else {
        // 无 --llm-*：注册空脚本 mock（记忆插件默认消费方，失败软降级）
        root.get::<LlmRegistry>()
            .expect("刚装配")
            .register("default", Arc::new(MockAdapter::new("memory-mock", vec![])))?;
    }
    root.get::<PromptSections>()
        .expect("刚装配")
        .append("persona", "你是 cos 演示助手，工具结果要如实汇报。");
    register_defaults(&root.get::<InvariantRegistry>().expect("刚装配"));
    root.get::<AgentRegistry>()
        .expect("刚装配")
        .set_factory(Arc::new(LoopFactory))?;

    let profile = Profile::load(&config.config_path)?;
    // 装配插件树
    let app = loader::load(&root, &profile)?;

    // 主 agent：LLM 统一管理解析，优先级：
    // --agent-llm <id> > --llm-* 的 "default" > yml 的 "main"（链或提供商）>
    // yml 恰好一个非 "default" 提供商 > 确定性演示脚本
    let llm_registry = root.get::<LlmRegistry>().expect("刚装配");
    let (adapter, provider, model, demo_mode) = if let Some(id) = &config.agent_llm {
        let adapter = llm_registry
            .resolve_id(id)
            .map_err(|error| AppError::Other(format!("agent LLM '{id}' 不可用: {error}")))?;
        (
            adapter,
            Some("llm-registry".to_string()),
            Some(id.clone()),
            false,
        )
    } else if config.llm.is_some() {
        let adapter = llm_registry
            .resolve_id("default")
            .map_err(|error| AppError::Other(format!("agent LLM 'default' 不可用: {error}")))?;
        (
            adapter,
            Some("opencode".to_string()),
            config.llm.as_ref().map(|cfg| cfg.model.clone()),
            false,
        )
    } else if let Ok(adapter) = llm_registry.resolve_id("main") {
        // yml plugin-llm 定义了 main 链/提供商 → 自动使用（无需 --agent-llm）
        (
            adapter,
            Some("llm-registry".to_string()),
            Some("main".to_string()),
            false,
        )
    } else if let Some(only) = single_provider(&llm_registry) {
        // yml 恰好定义一个非 "default" 提供商 → 自动使用（零参数启动）
        let adapter = llm_registry
            .resolve_id(&only)
            .map_err(|error| AppError::Other(format!("agent LLM '{only}' 不可用: {error}")))?;
        (adapter, Some("llm-registry".to_string()), Some(only), false)
    } else {
        (
            Arc::new(MockAdapter::new("demo", demo_script())) as Arc<dyn LlmAdapter>,
            Some("demo".to_string()),
            Some("mock".to_string()),
            true,
        )
    };
    let agent = root
        .get::<AgentRegistry>()
        .expect("刚装配")
        .create(CreateAgentOptions {
            session_id: config.session_id.clone(),
            options: AgentOptions {
                provider,
                model,
                max_tokens: None,
            },
            adapter,
        })
        .await?;

    Ok(Assembled {
        root,
        app,
        agent,
        demo_mode,
    })
}

/// 跑一轮交互：followup → 等 idle（可被取消信号中断）→ 总结该 turn。
pub async fn run_turn(
    agent: &Arc<dyn Agent>,
    message: UserMessage,
    cancel: Option<&Arc<AtomicBool>>,
) -> TurnSummary {
    let before_turn = last_turn(agent.session());
    agent.followup(message);
    let mut cancelled = false;
    match cancel {
        Some(flag) => {
            tokio::select! {
                _ = agent.when_idle() => {}
                _ = wait_for_cancel(flag.clone()) => {
                    agent.cancel(AbortCause::User, false);
                    agent.when_idle().await;
                    cancelled = true;
                }
            }
        }
        None => agent.when_idle().await,
    }
    summarize_turn(agent.session(), before_turn + 1, cancelled)
}

/// 注册表中恰好一个非 "default" 提供商 → 返回其 id（零参数自动使用）。
fn single_provider(registry: &LlmRegistry) -> Option<String> {
    let mut candidates = registry
        .list()
        .into_iter()
        .filter(|(id, _)| id != "default")
        .map(|(id, _)| id);
    let only = candidates.next()?;
    if candidates.next().is_some() {
        return None; // 多于一个 → 不自动选（避免猜错意图）
    }
    Some(only)
}

/// 会话里最后一个 turn 号（0 = 空会话）。
fn last_turn(session: &cos_session::Session) -> u32 {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::TurnStart { turn } => Some(*turn),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// 从会话日志总结某 turn：助手文本 + 工具轨迹。
fn summarize_turn(session: &cos_session::Session, turn: u32, cancelled: bool) -> TurnSummary {
    let mut reply = String::new();
    let mut calls: Vec<(String, String)> = Vec::new(); // (call_id, 显示)
    let mut results: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for event in session.events() {
        match &event.data {
            SessionEventData::ToolCall {
                turn: t,
                call_id,
                name,
                arguments,
                ..
            } if *t == turn => {
                calls.push((call_id.clone(), format!("{name} {arguments}")));
            }
            SessionEventData::ToolResult {
                turn: t,
                call_id,
                message,
                ..
            } if *t == turn => {
                results.insert(call_id.clone(), message.content.clone());
            }
            SessionEventData::AssistantMessage {
                turn: t, message, ..
            } if *t == turn => {
                let text = message.text();
                if !text.is_empty() {
                    if !reply.is_empty() {
                        reply.push('\n');
                    }
                    reply.push_str(&text);
                }
            }
            _ => {}
        }
    }
    let tool_trace: Vec<String> = calls
        .into_iter()
        .map(|(call_id, display)| match results.get(&call_id) {
            Some(result) => format!("{display} → {result}"),
            None => display,
        })
        .collect();
    TurnSummary {
        turn,
        reply,
        tool_trace,
        cancelled,
    }
}

/// 收尾（三种形态共用）：不变量校验 + 会话末 digest + JSONL 落盘（含重放校验）+ 优雅卸载。
pub async fn finish(assembled: &Assembled, config: &RunConfig) -> Result<RunReport, AppError> {
    let Assembled {
        root, app, agent, ..
    } = assembled;

    // 不变量：模型可见 ⟺ 已记录、seq 单调等
    let violations = root
        .get::<InvariantRegistry>()
        .expect("刚装配")
        .verify(agent.session());

    // 会话末 digest 收尾（M3，记忆插件装配时）：统计 + 转录 → 卡三段注记（慢路径）
    if let Ok(store) = root.get::<MemoryStore>() {
        let transcript = transcript_of(agent.session());
        if let Err(error) = store.digest(&transcript, cos_memory::now_ms()).await {
            eprintln!("[memory] 会话末 digest 失败: {error}");
        }
    }

    // 持久化 + 重放校验（逐事件一致）
    if let Some(path) = &config.session_path {
        let header = SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: config.session_id.clone(),
            created_at_ms: 0,
            cwd: None,
        };
        save_jsonl(agent.session(), &header, path)?;
        let (loaded_header, loaded_events) = load_jsonl(path)?;
        if loaded_header.id != config.session_id || loaded_events != agent.session().events() {
            return Err(AppError::Other("会话重放不一致".into()));
        }
    }

    // 优雅退出：apply 逆序卸载（审计）
    let unload_order: Vec<String> = app
        .instances()
        .iter()
        .rev()
        .map(|instance| instance.name.clone())
        .collect();
    app.dispose_async().await;

    Ok(RunReport {
        dump: None,
        unload_order,
        events: agent.session().events(),
        messages: agent.session().derive_messages(),
        violations,
        services_after_unload: root.get::<plugin_todo::TodoStore>().is_err(),
    })
}

/// 一次性演示链路（`--prompt`）：装配 → 一轮 → 收尾。
pub async fn run(config: RunConfig) -> Result<RunReport, AppError> {
    if config.dump_config {
        let profile = Profile::load(&config.config_path)?;
        return Ok(RunReport {
            dump: Some(loader::dump_plan(&profile)?),
            unload_order: Vec::new(),
            events: Vec::new(),
            messages: Vec::new(),
            violations: Vec::new(),
            services_after_unload: true,
        });
    }
    let prompt = config
        .prompt
        .clone()
        .ok_or_else(|| AppError::Other("一次性模式需要 --prompt".into()))?;
    let assembled = assemble(&config).await?;
    run_turn(
        &assembled.agent,
        UserMessage::new(prompt),
        config.cancel.as_ref(),
    )
    .await;
    finish(&assembled, &config).await
}

/// 轮询取消信号（main 的 Ctrl-C 监视任务写入）。
pub(crate) async fn wait_for_cancel(flag: Arc<AtomicBool>) {
    loop {
        if flag.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// 会话日志 → 完整转录（会话末 digest 输入；turn 边界打标）。
fn transcript_of(session: &cos_session::Session) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in session.events() {
        match &event.data {
            cos_session::SessionEventData::TurnStart { turn } => {
                lines.push(format!("— turn {turn} —"));
            }
            cos_session::SessionEventData::UserMessage(message) => {
                lines.push(format!("用户: {}", message.content));
            }
            cos_session::SessionEventData::AssistantMessage { message, .. } => {
                lines.push(format!("助手: {}", message.text()));
            }
            _ => {}
        }
    }
    lines.join("\n")
}
