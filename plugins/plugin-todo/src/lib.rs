//! plugin-todo —— todo_write 工具（P5，同 dsh 的 session 态清单）。
//!
//! 接缝纪律（PLAN.md §2）：本插件只依赖 Definition crate（cos-tools / cos-agent /
//! cos-session / cos-core），不依赖任何 Provider 或 cos-agent-loop。
//!
//! 语义：整表替换、最后写入胜出（dsh `todo/write`）；会话态记录经
//! `current_initiator()` 因果链写入当前 agent 的会话日志（无发起者时跳过）。

#![warn(missing_docs)]

use std::sync::{Arc, Mutex};

use cos_agent::current_initiator;
use cos_core::{Context, CoreError, Plugin, Service, Validate};
use cos_session::{SessionEventData, TodoItem};
use cos_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde::Deserialize;

/// session 态 todo 清单（服务；Clone 共享内部 `Arc`）。
#[derive(Clone, Default)]
pub struct TodoStore {
    items: Arc<Mutex<Vec<TodoItem>>>,
}

impl Service for TodoStore {
    const NAME: &'static str = "todo-store";
}

impl TodoStore {
    /// 当前整表快照。
    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.items.lock().unwrap().clone()
    }

    /// 整表替换（最后写入胜出）。
    pub fn replace(&self, todos: Vec<TodoItem>) {
        *self.items.lock().unwrap() = todos;
    }
}

/// 插件配置（空）。
#[derive(Deserialize)]
pub struct TodoConfig;

impl Validate for TodoConfig {}

/// todo_write 工具。
struct TodoTool {
    store: TodoStore,
}

impl TodoTool {
    fn parse_todos(arguments: &serde_json::Value) -> Result<Vec<TodoItem>, String> {
        let todos = arguments
            .get("todos")
            .ok_or_else(|| "参数缺少 todos".to_string())?;
        serde_json::from_value(todos.clone()).map_err(|error| format!("todos 解析失败: {error}"))
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo_write"
    }

    fn description(&self) -> &'static str {
        "写入任务清单（整表替换，最后写入胜出）"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "任务描述" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed"],
                                "description": "生命周期状态"
                            }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn execute(
        &self,
        _ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, cos_session::ToolError>> {
        let store = self.store.clone();
        let arguments = run.arguments.clone();
        Box::pin(async move {
            let todos = match TodoTool::parse_todos(&arguments) {
                Ok(todos) => todos,
                Err(message) => {
                    return Ok(ToolOutcome::error(
                        message,
                        cos_session::ToolError {
                            name: "TodoWrite".into(),
                            code: "INVALID_TODOS".into(),
                        },
                    ));
                }
            };
            let count = todos.len();
            store.replace(todos.clone());
            // 会话态记录（因果链内才写，同 dsh 由 session 所有）
            if let Some(agent) = current_initiator() {
                agent
                    .session()
                    .append(SessionEventData::TodoWrite { todos });
            }
            Ok(ToolOutcome::ok(format!("已写入 {count} 条任务")))
        })
    }
}

/// 插件主体。
#[derive(Clone, Default)]
pub struct TodoPlugin {
    store: TodoStore,
}

impl Plugin for TodoPlugin {
    fn id(&self) -> &'static str {
        "plugin-todo"
    }

    type Config = TodoConfig;

    fn provide(&self) -> &'static [&'static str] {
        &["todo-store"]
    }

    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        ctx.provide(self.store.clone())?;
        let registry = ctx
            .get::<ToolRegistry>()
            .map_err(|_| CoreError::ServiceNotFound("tools"))?;
        registry.register(Arc::new(TodoTool {
            store: self.store.clone(),
        }))?;
        Ok(())
    }
}

cos_loader::plugin!("todo", TodoPlugin);
