//! plugin-memory —— 记忆插件接线（M1/M2/M3）：打开存储、提供 `memory` 服务、注册四工具，
//! 并挂 agent 读/写路径：
//! - 写（`agent/pre-step`，step 1）：把上一 turn 消化进记忆（apply_turn，非阻塞错误）；
//! - 读（`agent/request`）：上下文滚动压缩（超阈值 → 摘要 + 尾部窗口，M3）+ Mode A 主动
//!   recall + Mode B 最近聊过 + 关系卡常驻注入 system；
//! - 会话末（`agent/status` → Idle）：digest 慢路径（统计 + 转录 → 卡三段注记，M3）。
//!
//! 接缝纪律：只依赖 Definition crate（dsh-memory / dsh-tools / dsh-core / dsh-session /
//! dsh-agent / dsh-llm），不依赖 Provider 或 dsh-agent-loop。

#![warn(missing_docs)]

use std::sync::Arc;

use dsh_agent::{
    AgentStatus, AgentStatusPayload, PreStepDecision, PreStepPayload, current_initiator,
};
use dsh_core::{Context, CoreError, CoreResult, EffectHandle, Plugin, Validate};
use dsh_llm::{LlmRegistry, LlmRequest, Message};
use dsh_memory::{
    MemoryHit, MemoryStore, demote_topic, inventory_topics, now_ms, recall_memories, remember_fact,
    turn_pair_from_text,
};
use dsh_session::SessionEventData;
use dsh_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde::Deserialize;

/// 插件配置。
#[derive(Deserialize)]
pub struct MemoryConfig {
    /// SQLite 数据库路径。
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// LLM 提供商/后备链 id（LLM 统一管理；缺省用宿主注册的 "default"）。
    #[serde(default)]
    pub llm: Option<String>,
    /// 上下文压缩阈值（模型可见消息总字符数，M3）。
    #[serde(default = "default_max_context")]
    pub max_context_chars: usize,
    /// 压缩时保留的尾部消息条数（不压缩的窗口，M3）。
    #[serde(default = "default_keep_tail")]
    pub keep_tail: usize,
    /// 会话中每隔多少 turn 做一次 digest 慢消化（会话末尾段由宿主收尾，M3）。
    #[serde(default = "default_digest_every")]
    pub digest_every: usize,
}

fn default_db_path() -> String {
    "memory.db".into()
}

fn default_max_context() -> usize {
    6000
}

fn default_keep_tail() -> usize {
    6
}

fn default_digest_every() -> usize {
    8
}

impl Validate for MemoryConfig {}

/// recall 工具。
struct RecallTool;

impl Tool for RecallTool {
    fn name(&self) -> &'static str {
        "recall"
    }

    fn description(&self) -> &'static str {
        "检索关于某话题的记忆（返回 topic/state/时间/次数/置信度；无命中即无相关记忆）"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "检索话题" },
                "limit": { "type": "integer", "description": "返回条数上限（默认 5）" }
            },
            "required": ["query"]
        })
    }

    fn execute(
        &self,
        ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, dsh_session::ToolError>> {
        let store = match ctx.get::<MemoryStore>() {
            Ok(store) => store,
            Err(_) => {
                return Box::pin(async {
                    Ok(ToolOutcome::error(
                        "memory 服务未装配".to_string(),
                        dsh_session::ToolError {
                            name: "Memory".into(),
                            code: "NO_MEMORY".into(),
                        },
                    ))
                });
            }
        };
        let query = run.arguments["query"].as_str().unwrap_or("").to_string();
        let limit = run.arguments["limit"].as_u64().unwrap_or(5) as usize;
        Box::pin(async move {
            match recall_memories(&store, &query, limit).await {
                Ok(outcome) if outcome.none => Ok(ToolOutcome::ok(
                    "无相关记忆（诚实出口：这是新话题吗？）".to_string(),
                )),
                Ok(outcome) => Ok(ToolOutcome::ok(
                    serde_json::to_string_pretty(&format_hits(&outcome.hits)).unwrap_or_default(),
                )),
                Err(error) => Ok(ToolOutcome::error(
                    error.to_string(),
                    dsh_session::ToolError {
                        name: "Memory".into(),
                        code: "RECALL_FAILED".into(),
                    },
                )),
            }
        })
    }
}

/// remember 工具。
struct RememberTool;

impl Tool for RememberTool {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn description(&self) -> &'static str {
        "记一件事（管线漏了/用户明确要求时加强或新建）"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "要记的内容" },
                "topic": { "type": "string", "description": "主题表述（可选，缺省用 content）" }
            },
            "required": ["content"]
        })
    }

    fn execute(
        &self,
        ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, dsh_session::ToolError>> {
        let store = match ctx.get::<MemoryStore>() {
            Ok(store) => store,
            Err(_) => {
                return Box::pin(async {
                    Ok(ToolOutcome::error(
                        "memory 服务未装配".to_string(),
                        dsh_session::ToolError {
                            name: "Memory".into(),
                            code: "NO_MEMORY".into(),
                        },
                    ))
                });
            }
        };
        let content = run.arguments["content"].as_str().unwrap_or("").to_string();
        let topic = run.arguments["topic"].as_str().map(str::to_string);
        Box::pin(async move {
            match remember_fact(&store, &content, topic.as_deref()).await {
                Ok(topic_id) => Ok(ToolOutcome::ok(format!("已记入（topic {topic_id}）"))),
                Err(error) => Ok(ToolOutcome::error(
                    error.to_string(),
                    dsh_session::ToolError {
                        name: "Memory".into(),
                        code: "REMEMBER_FAILED".into(),
                    },
                )),
            }
        })
    }
}

/// inventory 工具。
struct InventoryTool;

impl Tool for InventoryTool {
    fn name(&self) -> &'static str {
        "inventory"
    }

    fn description(&self) -> &'static str {
        "盘点记忆：我关于 X 知道什么（无 query = 全部，按权重降序）"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "可选过滤话题" },
                "limit": { "type": "integer", "description": "返回条数上限（默认 10）" }
            }
        })
    }

    fn execute(
        &self,
        ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, dsh_session::ToolError>> {
        let store = match ctx.get::<MemoryStore>() {
            Ok(store) => store,
            Err(_) => {
                return Box::pin(async {
                    Ok(ToolOutcome::error(
                        "memory 服务未装配".to_string(),
                        dsh_session::ToolError {
                            name: "Memory".into(),
                            code: "NO_MEMORY".into(),
                        },
                    ))
                });
            }
        };
        let query = run.arguments["query"].as_str().map(str::to_string);
        let limit = run.arguments["limit"].as_u64().unwrap_or(10) as usize;
        Box::pin(async move {
            match inventory_topics(&store, query.as_deref(), limit) {
                Ok(hits) => Ok(ToolOutcome::ok(
                    serde_json::to_string_pretty(&format_hits(&hits)).unwrap_or_default(),
                )),
                Err(error) => Ok(ToolOutcome::error(
                    error.to_string(),
                    dsh_session::ToolError {
                        name: "Memory".into(),
                        code: "INVENTORY_FAILED".into(),
                    },
                )),
            }
        })
    }
}

/// demote 工具。
struct DemoteTool;

impl Tool for DemoteTool {
    fn name(&self) -> &'static str {
        "demote"
    }

    fn description(&self) -> &'static str {
        "淡忘一个话题：权重压低 → 加速衰减（可逆，删除交给遗忘曲线）"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "要淡忘的话题" },
                "reason": { "type": "string", "description": "可选原因" }
            },
            "required": ["query"]
        })
    }

    fn execute(
        &self,
        ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, dsh_session::ToolError>> {
        let store = match ctx.get::<MemoryStore>() {
            Ok(store) => store,
            Err(_) => {
                return Box::pin(async {
                    Ok(ToolOutcome::error(
                        "memory 服务未装配".to_string(),
                        dsh_session::ToolError {
                            name: "Memory".into(),
                            code: "NO_MEMORY".into(),
                        },
                    ))
                });
            }
        };
        let query = run.arguments["query"].as_str().unwrap_or("").to_string();
        let reason = run.arguments["reason"].as_str().map(str::to_string);
        Box::pin(async move {
            match demote_topic(&store, &query, reason.as_deref()) {
                Ok(Some(topic_id)) => Ok(ToolOutcome::ok(format!(
                    "已减弱（topic {topic_id}）——遗忘曲线接管，中途可被重新提起复活"
                ))),
                Ok(None) => Ok(ToolOutcome::ok("未找到匹配话题".to_string())),
                Err(error) => Ok(ToolOutcome::error(
                    error.to_string(),
                    dsh_session::ToolError {
                        name: "Memory".into(),
                        code: "DEMOTE_FAILED".into(),
                    },
                )),
            }
        })
    }
}

/// 命中格式化（模型可见的 JSON 文本）。
fn format_hits(hits: &[MemoryHit]) -> serde_json::Value {
    serde_json::json!({
        "hits": hits.iter().map(|hit| serde_json::json!({
            "topic": hit.canonical_name,
            "state": hit.state_summary,
            "when": hit.when,
            "n_times": hit.n_times,
            "confidence": hit.confidence,
        })).collect::<Vec<_>>()
    })
}

/// 写挂钩（M2/M3）：`agent/pre-step`，每 turn 第一步进入前消化上一 turn（会话 → 记忆），
/// 并记录当前 turn 进度（digest 节流的依据）。
///
/// 记忆失败不阻塞对话（陪伴优先）：错误只落 stderr；记忆是尽力而为的旁路。
fn register_write_hook(ctx: &Context, store: Arc<MemoryStore>) -> CoreResult<EffectHandle> {
    ctx.on_waterfall::<PreStepPayload, PreStepDecision>("agent/pre-step", move |d| {
        let store = store.clone();
        Box::pin(async move {
            let payload = d.value().clone();
            let decision = d.next().await;
            if payload.turn > 1 && payload.step == 1 {
                let previous = payload.turn - 1;
                let Some(session) = current_initiator().map(|agent| agent.session().clone()) else {
                    return decision;
                };
                let (user, assistant) = turn_text(&session, previous);
                if (!user.is_empty() || !assistant.is_empty())
                    && let Err(error) = store
                        .apply_turn(&turn_pair_from_text(user, assistant), now_ms())
                        .await
                {
                    eprintln!("[memory] 消化 turn {previous} 失败: {error}");
                }
            }
            if payload.step == 1 {
                let _ = store.set_state(
                    &format!("turn:{}", payload.agent_id),
                    &payload.turn.to_string(),
                );
            }
            decision
        })
    })
}

/// 读挂钩（M2/M3）：`agent/request`，把记忆段注入 system：
/// 上下文滚动压缩（M3，超阈值 → 摘要 + 尾部窗口）；Mode A 命中 → 相关记忆；
/// 否则 Mode B 最近聊过；关系卡常驻。
fn register_read_hook(
    ctx: &Context,
    store: Arc<MemoryStore>,
    max_context_chars: usize,
    keep_tail: usize,
) -> CoreResult<EffectHandle> {
    ctx.on_waterfall::<LlmRequest, LlmRequest>("agent/request", move |d| {
        let query = last_user_text(d.value());
        let store = store.clone();
        Box::pin(async move {
            let mut request = d.next().await;
            let mut sections: Vec<String> = Vec::new();

            // 1. 上下文压缩（先做：决定保留哪些消息 + 产出滚动摘要）
            let agent_id = current_initiator().map(|agent| agent.id().to_string());
            if let Some(id) = agent_id.as_deref()
                && let Some((summary, tail)) =
                    compress_if_needed(&store, id, &request.messages, max_context_chars, keep_tail)
                        .await
            {
                request.messages = tail;
                sections.push(format!("【对话摘要】\n{summary}"));
            }

            // 2. 记忆段（Mode A/B + 关系卡）
            if let Some(section) = build_memory_section(&store, query.as_deref()).await {
                sections.push(section);
            }

            if !sections.is_empty() {
                request.system = Some(match request.system {
                    Some(original) => format!("{}\n\n{original}", sections.join("\n\n")),
                    None => sections.join("\n\n"),
                });
            }
            request
        })
    })
}

/// 上下文压缩：超阈值时把旧消息压进滚动摘要，保留尾部 `keep_tail` 条原文。
/// 压缩失败 → 返回 `None`（宁可长，不可丢：不截断、不降级）。
async fn compress_if_needed(
    store: &MemoryStore,
    agent_id: &str,
    messages: &[Message],
    max_chars: usize,
    keep_tail: usize,
) -> Option<(String, Vec<Message>)> {
    if messages.len() <= keep_tail {
        return None;
    }
    let total: usize = messages.iter().map(message_chars).sum();
    if total <= max_chars {
        return None;
    }
    let split = messages.len() - keep_tail;
    let dialog = format_dialog(&messages[..split]);
    let old = store
        .get_state(&format!("summary:{agent_id}"))
        .ok()
        .flatten()
        .unwrap_or_default();
    let summary = match store.compress_context(&old, &dialog).await {
        Ok(summary) => summary,
        Err(error) => {
            eprintln!("[memory] 上下文压缩失败: {error}");
            return None;
        }
    };
    let _ = store.set_state(&format!("summary:{agent_id}"), &summary);
    Some((summary, messages[split..].to_vec()))
}

/// 消息的模型可见字符数（压缩阈值计量）。
fn message_chars(message: &Message) -> usize {
    match message {
        Message::System { content } => content.chars().count(),
        Message::User(user) => user.content.chars().count(),
        Message::Assistant(assistant) => assistant.text().chars().count(),
        Message::Tool(tool) => tool.content.chars().count(),
        Message::Custom { data, .. } => data.to_string().chars().count(),
    }
}

/// 消息序列 → 对话文本（压缩输入）。
fn format_dialog(messages: &[Message]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in messages {
        match message {
            Message::System { content } => lines.push(format!("系统: {content}")),
            Message::User(user) => lines.push(format!("用户: {}", user.content)),
            Message::Assistant(assistant) => lines.push(format!("助手: {}", assistant.text())),
            Message::Tool(tool) => lines.push(format!("工具结果: {}", tool.content)),
            Message::Custom { name, data } => lines.push(format!("[{name}]: {data}")),
        }
    }
    lines.join("\n")
}

/// 会话中 digest 挂钩（M3）：`agent/status` → Idle 时按 turn 进度节流派生慢消化任务。
/// 每累计 `digest_every` turn 一次；会话末尾段由宿主（cos）收尾，避免每次空闲都打 LLM。
fn register_digest_hook(
    ctx: &Context,
    store: Arc<MemoryStore>,
    digest_every: usize,
) -> CoreResult<EffectHandle> {
    ctx.on("agent/status", move |payload| {
        let Some(payload) = payload.downcast_ref::<AgentStatusPayload>() else {
            return;
        };
        if payload.status != AgentStatus::Idle {
            return;
        }
        let Some(session) = current_initiator().map(|agent| agent.session().clone()) else {
            return;
        };
        let agent_id = payload.agent_id.clone();
        let store = store.clone();
        tokio::spawn(async move {
            let turn: u32 = store
                .get_state(&format!("turn:{agent_id}"))
                .ok()
                .flatten()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let done: u32 = store
                .get_state(&format!("digested_turn:{agent_id}"))
                .ok()
                .flatten()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            if turn.saturating_sub(done) < digest_every as u32 {
                return;
            }
            let transcript = transcript_text(&session);
            if let Err(error) = store.digest(&transcript, now_ms()).await {
                eprintln!("[memory] digest 失败: {error}");
                return;
            }
            let _ = store.set_state(&format!("digested_turn:{agent_id}"), &turn.to_string());
        });
    })
}

/// 会话日志 → 完整转录（digest 输入；turn 边界打标）。
fn transcript_text(session: &dsh_session::Session) -> String {
    let mut lines: Vec<String> = Vec::new();
    for event in session.events() {
        match &event.data {
            SessionEventData::TurnStart { turn } => lines.push(format!("— turn {turn} —")),
            SessionEventData::UserMessage(message) => {
                lines.push(format!("用户: {}", message.content));
            }
            SessionEventData::AssistantMessage { message, .. } => {
                lines.push(format!("助手: {}", message.text()));
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// 请求里最后一条用户消息文本（recall 查询）。
fn last_user_text(request: &LlmRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user.content.clone()),
            _ => None,
        })
}

/// 从会话日志重建某 turn 的用户/助手文本（记忆写路径投影）。
fn turn_text(session: &dsh_session::Session, turn: u32) -> (String, String) {
    let mut user = String::new();
    let mut assistant = String::new();
    let mut current = 0u32;
    for event in session.events() {
        match &event.data {
            SessionEventData::TurnStart { turn: t } => current = *t,
            SessionEventData::UserMessage(message) if current == turn => {
                if !user.is_empty() {
                    user.push('\n');
                }
                user.push_str(&message.content);
            }
            SessionEventData::AssistantMessage {
                turn: t, message, ..
            } if *t == turn => {
                let text = message.text();
                if !text.is_empty() {
                    if !assistant.is_empty() {
                        assistant.push('\n');
                    }
                    assistant.push_str(&text);
                }
            }
            _ => {}
        }
    }
    (user, assistant)
}

/// 组装注入的记忆段；无内容 → `None`（不打扰请求）。
async fn build_memory_section(store: &MemoryStore, query: Option<&str>) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();

    // Mode A：主动 recall（命中即唤醒）
    let mut mode_a_hit = false;
    if let Some(query) = query
        && let Ok(outcome) = recall_memories(store, query, 3).await
        && !outcome.none
    {
        mode_a_hit = true;
        lines.push("【相关记忆】".to_string());
        for hit in &outcome.hits {
            lines.push(format!(
                "- {}：{}（{}，{} 次）",
                hit.canonical_name, hit.state_summary, hit.when, hit.n_times
            ));
        }
    }

    // Mode B 燃料：最近聊过（无 Mode A 命中时兜底）
    if !mode_a_hit
        && let Ok(feed) = store.recent_feed(3, now_ms())
        && !feed.recent.is_empty()
    {
        lines.push("【最近聊过】".to_string());
        for hit in feed.recent.iter().take(3) {
            lines.push(format!(
                "- {}：{}（{}）",
                hit.canonical_name, hit.state_summary, hit.when
            ));
        }
    }

    // 关系卡：常驻注入（有内容时）
    if let Ok(card) = store.card() {
        let mut card_lines: Vec<String> = Vec::new();
        if !card.profile.is_empty() {
            card_lines.push(format!("关于你：{}", card.profile));
        }
        if !card.agent_model.is_empty() {
            card_lines.push(format!("关于我：{}", card.agent_model));
        }
        if !card.relationship.is_empty() {
            card_lines.push(format!("我们之间：{}", card.relationship));
        }
        if !card_lines.is_empty() {
            lines.push("【关系卡】".to_string());
            lines.extend(card_lines);
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// 插件主体（apply 时打开存储 + 注册四工具 + 挂 agent 读/写/digest 挂钩）。
///
/// LLM 解析（LLM 统一管理）：`config.llm`（provider/链 id）→ [`LlmRegistry`]；
/// 缺省用 `"default"`（宿主装配；cos 无 `--llm-*` 时注册空脚本 mock）。
#[derive(Default)]
pub struct MemoryPlugin;

impl Plugin for MemoryPlugin {
    const ID: &'static str = "plugin-memory";

    type Config = MemoryConfig;

    fn provide(&self) -> &'static [&'static str] {
        &["memory"]
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let registry = ctx
            .get::<LlmRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("llm"))?;
        let llm_id = config.llm.as_deref().unwrap_or("default");
        let adapter = registry
            .resolve_id(llm_id)
            .map_err(|error| CoreError::Other(format!("LLM 提供商 '{llm_id}' 不可用: {error}")))?;
        let store = MemoryStore::open(&config.db_path, adapter)
            .map_err(|error| CoreError::Other(error.to_string()))?;
        ctx.provide(store)?;
        let registry = ctx
            .get::<ToolRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("tools"))?;
        registry.register(Arc::new(RecallTool))?;
        registry.register(Arc::new(RememberTool))?;
        registry.register(Arc::new(InventoryTool))?;
        registry.register(Arc::new(DemoteTool))?;
        // M2/M3：agent 读/写/会话末挂钩（随 fiber 卸载自动失效）
        let store = ctx
            .get::<MemoryStore>()
            .map_err(|_| CoreError::ServiceNotFound("memory"))?;
        register_write_hook(ctx, store.clone())?;
        register_read_hook(
            ctx,
            store.clone(),
            config.max_context_chars,
            config.keep_tail,
        )?;
        register_digest_hook(ctx, store, config.digest_every)?;
        Ok(())
    }
}

dsh_loader::plugin!("memory", MemoryPlugin);
