//! plugin-memory —— 记忆插件接线（M1/M2）：打开存储、提供 `memory` 服务、注册四工具，
//! 并挂 agent 读/写路径：
//! - 写（`agent/pre-step`，step 1）：把上一 turn 消化进记忆（apply_turn，非阻塞错误）；
//! - 读（`agent/request`）：Mode A 主动 recall + Mode B 最近聊过 + 关系卡常驻注入 system。
//!
//! 接缝纪律：只依赖 Definition crate（dsh-memory / dsh-tools / dsh-core / dsh-session /
//! dsh-agent / dsh-llm），不依赖 Provider 或 dsh-agent-loop。

#![warn(missing_docs)]

use std::sync::Arc;

use dsh_agent::{PreStepDecision, PreStepPayload, current_initiator};
use dsh_core::{Context, CoreError, CoreResult, EffectHandle, Plugin, Validate};
use dsh_llm::{LlmRequest, Message};
use dsh_memory::{
    MemoryHit, MemoryLlmProvider, MemoryStore, demote_topic, inventory_topics, now_ms,
    recall_memories, remember_fact, turn_pair_from_text,
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
}

fn default_db_path() -> String {
    "memory.db".into()
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

/// 写挂钩（M2）：`agent/pre-step`，每 turn 第一步进入前消化上一 turn（会话 → 记忆）。
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
            decision
        })
    })
}

/// 读挂钩（M2）：`agent/request`，把记忆段注入 system：
/// Mode A 命中 → 相关记忆；否则 Mode B 最近聊过；关系卡常驻。
fn register_read_hook(ctx: &Context, store: Arc<MemoryStore>) -> CoreResult<EffectHandle> {
    ctx.on_waterfall::<LlmRequest, LlmRequest>("agent/request", move |d| {
        let query = last_user_text(d.value());
        let store = store.clone();
        Box::pin(async move {
            let mut request = d.next().await;
            if let Some(section) = build_memory_section(&store, query.as_deref()).await {
                request.system = Some(match request.system {
                    Some(original) => format!("{section}\n\n{original}"),
                    None => section,
                });
            }
            request
        })
    })
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

/// 插件主体（apply 时打开存储 + 注册四工具 + 挂 agent 读/写挂钩）。
#[derive(Default)]
pub struct MemoryPlugin;

impl Plugin for MemoryPlugin {
    const ID: &'static str = "plugin-memory";

    type Config = MemoryConfig;

    fn provide(&self) -> &'static [&'static str] {
        &["memory"]
    }

    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        let llm = ctx
            .get::<MemoryLlmProvider>()
            .map_err(|_| CoreError::ServiceNotFound("memory-llm"))?;
        let store = MemoryStore::open(&config.db_path, llm.inner.clone())
            .map_err(|error| CoreError::Other(error.to_string()))?;
        ctx.provide(store)?;
        let registry = ctx
            .get::<ToolRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("tools"))?;
        registry.register(Arc::new(RecallTool))?;
        registry.register(Arc::new(RememberTool))?;
        registry.register(Arc::new(InventoryTool))?;
        registry.register(Arc::new(DemoteTool))?;
        // M2：agent 读/写挂钩（随 fiber 卸载自动失效）
        let store = ctx
            .get::<MemoryStore>()
            .map_err(|_| CoreError::ServiceNotFound("memory"))?;
        register_write_hook(ctx, store.clone())?;
        register_read_hook(ctx, store)?;
        Ok(())
    }
}

dsh_loader::plugin!("memory", MemoryPlugin);
