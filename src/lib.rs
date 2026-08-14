//! cos —— CLI 宿主库（A 形态收口，P6）。
//!
//! `run` 是完整演示链路：cordis.yml 装配插件树 → demo agent（mock LLM：
//! 工具调用 → 回复）→ 不变量校验 → JSONL 持久化 + 重放校验 → 优雅退出
//! （apply 逆序卸载，可审计）。`main.rs` 只做参数解析与结果打印。

#![warn(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cos_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::{Context, Plugin};
use cos_invariants::{InvariantRegistry, register_defaults};
use cos_llm::{ChunkDelta, LlmAdapter, LlmRegistry, Message, StreamChunk, ToolCall, UserMessage};
use cos_llm_mock::{MockAdapter, MockReply};
use cos_llm_opencode::{OpencodeAdapter, OpencodeConfig};
use cos_loader::{self as loader, Profile};
use cos_memory::MemoryStore;
use cos_session::{
    AbortCause, SESSION_FORMAT_VERSION, SessionEvent, SessionHeader, load_jsonl, save_jsonl,
};
use cos_shell::provide_local_shell;
use cos_system_prompt::PromptSections;
use cos_tools::ToolRegistry;
use thiserror::Error;

/// 运行配置。
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// cordis.yml 路径。
    pub config_path: String,
    /// 只输出装载计划（不启动）。
    pub dump_config: bool,
    /// demo 会话 id。
    pub session_id: String,
    /// 演示用户消息。
    pub prompt: String,
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

/// 运行完整演示链路。
pub async fn run(config: RunConfig) -> Result<RunReport, AppError> {
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
                input_content: vec![cos_llm::InputContent::Text],
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

    if config.dump_config {
        return Ok(RunReport {
            dump: Some(loader::dump_plan(&profile)?),
            unload_order: Vec::new(),
            events: Vec::new(),
            messages: Vec::new(),
            violations: Vec::new(),
            services_after_unload: true,
        });
    }

    // 装配插件树
    let app = loader::load(&root, &profile)?;

    // demo agent：LLM 统一管理解析（--agent-llm / --llm-* 的 "default"）或确定性 mock 脚本
    let llm_registry = root.get::<LlmRegistry>().expect("刚装配");
    let (adapter, provider, model) = if let Some(id) = &config.agent_llm {
        let adapter = llm_registry
            .resolve_id(id)
            .map_err(|error| AppError::Other(format!("agent LLM '{id}' 不可用: {error}")))?;
        (adapter, Some("llm-registry".to_string()), Some(id.clone()))
    } else if config.llm.is_some() {
        let adapter = llm_registry
            .resolve_id("default")
            .map_err(|error| AppError::Other(format!("agent LLM 'default' 不可用: {error}")))?;
        (
            adapter,
            Some("opencode".to_string()),
            config.llm.as_ref().map(|cfg| cfg.model.clone()),
        )
    } else {
        (
            Arc::new(MockAdapter::new("demo", demo_script())) as Arc<dyn LlmAdapter>,
            Some("demo".to_string()),
            Some("mock".to_string()),
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

    agent.followup(UserMessage::new(config.prompt.clone()));
    match &config.cancel {
        Some(flag) => {
            tokio::select! {
                _ = agent.when_idle() => {}
                _ = wait_for_cancel(flag.clone()) => {
                    agent.cancel(AbortCause::User, false);
                    agent.when_idle().await;
                }
            }
        }
        None => agent.when_idle().await,
    }

    // 不变量：模型可见 ⟺ 已记录、seq 单调等
    let violations = root
        .get::<InvariantRegistry>()
        .expect("刚装配")
        .verify(agent.session());

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

    // 会话末 digest 收尾（M3，记忆插件装配时）：统计 + 转录 → 卡三段注记（慢路径）
    if let Ok(store) = root.get::<MemoryStore>() {
        let transcript = transcript_of(agent.session());
        if let Err(error) = store.digest(&transcript, cos_memory::now_ms()).await {
            eprintln!("[memory] 会话末 digest 失败: {error}");
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

/// 轮询取消信号（main 的 Ctrl-C 监视任务写入）。
async fn wait_for_cancel(flag: Arc<AtomicBool>) {
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
