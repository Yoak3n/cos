//! cos-tools —— 工具注册表 + 执行管线（P5）。
//!
//! 管线顺序（语义权威：`docs/tool-execution-pipeline.md`）：
//! `tools/pre-execute`(waterfall) → 单调守卫 → `tools/execute`(waterfall) →
//! 工具体 → `tools/post-execute`(waterfall) → 结果返回（`tool/result` 由 loop 写日志）。
//!
//! P5 简化（记 docs/decisions.md）：顺序执行（无并发池/屏障）、无 approval 服务
//! （pre-execute 可 veto 代替）、无 additionalContexts；`tool/call` 先写日志由 loop 负责。

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cos_core::{Context, CoreError, CoreResult, Service};
use cos_session::ToolError;
use futures::future::BoxFuture;

/// 一次工具调用（参数已解析；空串 → `{}`，非法 JSON → 原串文本，同 dsh `parseArguments`）。
#[derive(Debug, Clone)]
pub struct ToolRun {
    /// 调用 id（与 `tool/result` 配对）。
    pub call_id: String,
    /// 工具名。
    pub name: String,
    /// 解析后的参数。
    pub arguments: serde_json::Value,
    /// 所属 turn。
    pub turn: u32,
    /// 所属 step。
    pub step: u32,
}

/// 一次工具调用的结果（模型可见内容 + 内部失败身份）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// 模型可见结果文本。
    pub content: String,
    /// 是否为错误结果。
    pub is_error: bool,
    /// 内部失败身份（如有）。
    pub error: Option<ToolError>,
}

impl ToolOutcome {
    /// 成功结果。
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            error: None,
        }
    }

    /// 错误结果。
    pub fn error(content: impl Into<String>, error: ToolError) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            error: Some(error),
        }
    }
}

/// 工具（对象安全接缝）。
pub trait Tool: Send + Sync {
    /// 工具名（schema 与注册键）。
    fn name(&self) -> &'static str;

    /// 工具描述（进 schema 与 prompt）。
    fn description(&self) -> &'static str;

    /// 参数 JSON Schema。
    fn parameters(&self) -> serde_json::Value;

    /// 执行工具体（P5：`ctx` 供读取服务/因果链）。
    fn execute(
        &self,
        ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, ToolError>>;
}

/// pre-execute waterfall 的决策：放行或拒绝。
#[derive(Debug, Clone, PartialEq)]
pub enum PreDecision {
    /// 放行（继续管线）。
    Allow,
    /// 拒绝（跳过工具体；原因成为模型可见结果）。
    Deny(String),
}

/// `tools/result` 实时通知载荷（loop 在写 tool/result 会话事件后发出）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResultPayload {
    /// 调用 id。
    pub call_id: String,
    /// 工具名。
    pub name: String,
    /// 模型可见结果文本。
    pub content: String,
    /// 是否为错误结果。
    pub is_error: bool,
}

/// 单调守卫（deny 或弃权；身份受保护，同 dsh）。
pub trait ToolGuard: Send + Sync {
    /// 检查一次调用；`None` = 弃权（放行）。
    fn check(&self, run: &ToolRun) -> Option<String>;
}

/// 工具注册表服务（`ctx.provide` 为 `"tools"`）。
pub struct ToolRegistry {
    tools: Mutex<BTreeMap<&'static str, Arc<dyn Tool>>>,
    guards: Mutex<Vec<Arc<dyn ToolGuard>>>,
}

impl Service for ToolRegistry {
    const NAME: &'static str = "tools";
}

impl ToolRegistry {
    /// 新建注册表（事件在 `execute` 的调用 ctx 上分发）。
    pub fn new(_root: &Context) -> Self {
        Self {
            tools: Mutex::new(BTreeMap::new()),
            guards: Mutex::new(Vec::new()),
        }
    }

    /// 注册工具；同名报错（同 dsh 服务注册纪律）。
    pub fn register(&self, tool: Arc<dyn Tool>) -> CoreResult<()> {
        let mut tools = self.tools.lock().unwrap();
        if tools.contains_key(tool.name()) {
            return Err(CoreError::Other(format!("工具 '{}' 已注册", tool.name())));
        }
        tools.insert(tool.name(), tool);
        Ok(())
    }

    /// 注册单调守卫。
    pub fn register_guard(&self, guard: Arc<dyn ToolGuard>) {
        self.guards.lock().unwrap().push(guard);
    }

    /// 按名取工具。
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.lock().unwrap().get(name).cloned()
    }

    /// 全部工具（名字序）。
    pub fn list(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.lock().unwrap().values().cloned().collect()
    }

    /// 全部工具 schema（JSON，模型可见）。
    pub fn schemas(&self) -> Vec<serde_json::Value> {
        self.list()
            .into_iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters(),
                    }
                })
            })
            .collect()
    }

    /// 执行管线（在给定 ctx 上分发事件）：
    /// pre-execute(waterfall) → 守卫 → execute(waterfall，链尾为工具体) → post-execute(waterfall)。
    pub async fn execute(&self, ctx: &Context, run: &ToolRun) -> ToolOutcome {
        // 1. tools/pre-execute：默认 Allow；监听器可 Deny（veto）
        let pre: CoreResult<PreDecision> = ctx
            .target(cos_core::ScopeTarget::All)
            .waterfall("tools/pre-execute", run.clone(), |d| {
                Box::pin(async move {
                    let _ = d.value();
                    PreDecision::Allow
                })
            })
            .await;
        let pre = pre.unwrap_or(PreDecision::Allow);
        if let PreDecision::Deny(reason) = pre {
            return ToolOutcome::error(
                reason.clone(),
                ToolError {
                    name: "ToolDenied".into(),
                    code: "DENIED".into(),
                },
            );
        }

        // 2. 单调守卫：任一 deny → 跳过工具体
        let guards = self.guards.lock().unwrap().clone();
        for guard in &guards {
            if let Some(reason) = guard.check(run) {
                return ToolOutcome::error(
                    reason,
                    ToolError {
                        name: "ToolDenied".into(),
                        code: "DENIED".into(),
                    },
                );
            }
        }

        // 3. tools/execute：around dispatch；链尾 = 工具体
        // （闭包须 'static：克隆 ctx 与工具快照后 move 进去）
        let tools_snapshot = self.list();
        let ctx_clone = ctx.clone();
        let outcome: CoreResult<ToolOutcome> = ctx
            .target(cos_core::ScopeTarget::All)
            .waterfall("tools/execute", run.clone(), move |d| {
                let tool = tools_snapshot
                    .iter()
                    .find(|tool| tool.name() == d.value().name)
                    .cloned();
                let ctx_clone = ctx_clone.clone();
                Box::pin(async move {
                    let run = d.value();
                    match tool {
                        Some(tool) => match tool.execute(&ctx_clone, run).await {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                ToolOutcome::error(format!("{}: {}", error.name, error.code), error)
                            }
                        },
                        None => ToolOutcome::error(
                            format!("工具 '{}' 未注册", run.name),
                            ToolError {
                                name: "ToolNotFound".into(),
                                code: "NOT_FOUND".into(),
                            },
                        ),
                    }
                })
            })
            .await;
        let outcome = outcome.unwrap_or_else(|error| {
            ToolOutcome::error(
                error.to_string(),
                ToolError {
                    name: "ToolPipeline".into(),
                    code: "PIPELINE_ERROR".into(),
                },
            )
        });

        // 4. tools/post-execute：接受/替换结果；链尾 = 原结果
        ctx.target(cos_core::ScopeTarget::All)
            .waterfall("tools/post-execute", outcome.clone(), |d| {
                Box::pin(async move { d.value().clone() })
            })
            .await
            .unwrap_or(outcome)
    }
}
