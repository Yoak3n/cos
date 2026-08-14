//! P5：todo_write 工具 —— store 整表替换 + 会话态记录（因果链内）。

use std::sync::Arc;

use dsh_agent::{Agent, AgentError, AgentOptions, AgentStatus, Maintenance};
use dsh_core::{Context, Plugin};
use dsh_session::{Session, SessionEventData, TodoItem, TodoStatus};
use dsh_tools::{ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use plugin_todo::{TodoConfig, TodoPlugin, TodoStore};
use serde_json::json;

fn setup() -> (Context, Arc<ToolRegistry>) {
    let root = Context::root();
    root.provide(ToolRegistry::new(&root)).unwrap();
    TodoPlugin::default().apply(&root, &TodoConfig).unwrap();
    let registry = root.get::<ToolRegistry>().unwrap();
    (root, registry)
}

fn run(arguments: serde_json::Value) -> ToolRun {
    ToolRun {
        call_id: "c1".into(),
        name: "todo_write".into(),
        arguments,
        turn: 1,
        step: 1,
    }
}

#[tokio::test]
async fn todo_tool_writes_store() {
    let (root, registry) = setup();
    let outcome = registry
        .execute(
            &root,
            &run(json!({"todos": [{"content": "做 P5", "status": "in_progress"}]})),
        )
        .await;
    assert!(!outcome.is_error, "{outcome:?}");
    assert!(outcome.content.contains("1"));

    let store = root.get::<TodoStore>().unwrap();
    assert_eq!(
        store.snapshot(),
        vec![TodoItem {
            content: "做 P5".into(),
            status: TodoStatus::InProgress,
        }]
    );
}

#[tokio::test]
async fn todo_tool_appends_session_event_under_initiator() {
    let (root, registry) = setup();
    let session = Session::new("sess-t");
    let agent: Arc<dyn Agent> = Arc::new(StubAgent {
        id: "sess-t".into(),
        session: session.clone(),
    });
    dsh_agent::with_initiator(agent, async {
        let outcome = registry
            .execute(
                &root,
                &run(json!({"todos": [{"content": "做 P5", "status": "pending"}]})),
            )
            .await;
        assert!(!outcome.is_error);
    })
    .await;

    let events = session.events();
    assert!(
        matches!(&events.last().unwrap().data, SessionEventData::TodoWrite { todos } if todos.len() == 1 && todos[0].content == "做 P5"),
        "会话态应写 todo/write 事件"
    );
}

#[tokio::test]
async fn invalid_todos_is_error_outcome() {
    let (root, registry) = setup();
    let outcome = registry
        .execute(&root, &run(json!({"todos": "不是数组"})))
        .await;
    assert!(outcome.is_error);
    assert!(outcome.content.contains("todos"));
}

/// 测试用最小 Agent 桩（仅 session/id 真实）。
struct StubAgent {
    id: String,
    session: Session,
}

impl Agent for StubAgent {
    fn id(&self) -> &str {
        &self.id
    }

    fn options(&self) -> &AgentOptions {
        // 桩实现：泄漏一份默认选项（测试进程生命周期内）
        Box::leak(Box::new(AgentOptions::default()))
    }

    fn session(&self) -> &Session {
        &self.session
    }

    fn ctx(&self) -> &Context {
        panic!("stub ctx 不可用")
    }

    fn status(&self) -> AgentStatus {
        AgentStatus::Idle
    }

    fn send(&self, _message: dsh_llm::UserMessage, _target: dsh_agent::InboxTarget, _wake: bool) {}

    fn followup(&self, _message: dsh_llm::UserMessage) {}

    fn steer(&self, _message: dsh_llm::UserMessage) {}

    fn inject(&self, _message: dsh_llm::UserMessage) {}

    fn cancel(&self, _cause: dsh_session::AbortCause, _keep_inbox: bool) {}

    fn when_idle(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }

    fn run_maintenance(&self, _task: Maintenance) -> BoxFuture<'static, Result<(), AgentError>> {
        Box::pin(async { Err(AgentError::Other("stub".into())) })
    }
}
