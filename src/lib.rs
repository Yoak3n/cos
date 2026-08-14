//! cos —— dsh-rust CLI 宿主库（A 形态收口，P6）。
//!
//! `run` 是完整演示链路：cordis.yml 装配插件树 → demo agent（mock LLM：
//! 工具调用 → 回复）→ 不变量校验 → JSONL 持久化 + 重放校验 → 优雅退出
//! （apply 逆序卸载，可审计）。`main.rs` 只做参数解析与结果打印。

#![warn(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dsh_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use dsh_agent_loop::LoopFactory;
use dsh_core::{Context, Plugin};
use dsh_invariants::{InvariantRegistry, register_defaults};
use dsh_llm::{ChunkDelta, LlmAdapter, Message, StreamChunk, ToolCall, UserMessage};
use dsh_llm_mock::{MockAdapter, MockReply};
use dsh_loader::{self as loader, Profile};
use dsh_session::{
    AbortCause, SESSION_FORMAT_VERSION, SessionEvent, SessionHeader, load_jsonl, save_jsonl,
};
use dsh_shell::provide_local_shell;
use dsh_system_prompt::PromptSections;
use dsh_tools::ToolRegistry;
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
    Session(#[from] dsh_session::SessionError),
    /// 内核失败。
    #[error(transparent)]
    Core(#[from] dsh_core::CoreError),
    /// agent 失败。
    #[error(transparent)]
    Agent(#[from] dsh_agent::AgentError),
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
pub fn builtin_plugin_ids() -> [&'static str; 3] {
    [
        plugin_todo::TodoPlugin::ID,
        plugin_bash::BashPlugin::ID,
        plugin_demo::DemoPlugin::ID,
    ]
}

/// 运行完整演示链路。
pub async fn run(config: RunConfig) -> Result<RunReport, AppError> {
    // 锚点：保证三个插件 crate 的 inventory 注册表被链接
    let _ = builtin_plugin_ids();
    let root = Context::root();
    // 内置服务（app 装配，先于插件树）
    root.provide(ToolRegistry::new(&root))?;
    root.provide(PromptSections::new(&root))?;
    root.provide(InvariantRegistry::new(&root))?;
    provide_local_shell(&root)?;
    root.provide(AgentRegistry::new(&root))?;
    root.get::<PromptSections>()
        .expect("刚装配")
        .append("persona", "你是 dsh-rust 演示助手，工具结果要如实汇报。");
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

    // demo agent（确定性 mock 脚本）
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new("demo", demo_script()));
    let agent = root
        .get::<AgentRegistry>()
        .expect("刚装配")
        .create(CreateAgentOptions {
            session_id: config.session_id.clone(),
            options: AgentOptions {
                provider: Some("demo".into()),
                model: Some("mock".into()),
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
