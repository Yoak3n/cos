//! plugin-memory —— 记忆插件接线（M1）：打开存储、提供 `memory` 服务、注册四工具。
//!
//! M2 将接 agent 读/写路径（turn 挂钩、关系卡常驻注入、pre-step 主动 recall）。
//! 接缝纪律：只依赖 Definition crate（dsh-memory / dsh-tools / dsh-core / dsh-session）。

#![warn(missing_docs)]

use std::sync::Arc;

use dsh_core::{Context, CoreError, Plugin, Validate};
use dsh_memory::{
    MemoryHit, MemoryLlmProvider, MemoryStore, demote_topic, inventory_topics, recall_memories,
    remember_fact,
};
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

/// 插件主体（apply 时打开存储 + 注册四工具）。
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
        Ok(())
    }
}

dsh_loader::plugin!("memory", MemoryPlugin);
