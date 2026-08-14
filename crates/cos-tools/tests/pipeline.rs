//! P5：工具注册表 + 执行管线（pre-execute/守卫/execute/post-execute）。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cos_core::Context;
use cos_tools::{PreDecision, Tool, ToolGuard, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;
use serde_json::json;

struct EchoTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> &'static str {
        "回声工具"
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
    }

    fn execute(
        &self,
        _ctx: &Context,
        run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, cos_session::ToolError>> {
        let calls = self.calls.clone();
        let text = run.arguments["text"].as_str().unwrap_or("").to_string();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutcome::ok(format!("echo:{text}")))
        })
    }
}

struct NameGuard {
    deny: &'static str,
}

impl ToolGuard for NameGuard {
    fn check(&self, run: &ToolRun) -> Option<String> {
        (run.name == self.deny).then(|| format!("工具 {} 被守卫拒绝", self.deny))
    }
}

fn setup() -> (Context, Arc<ToolRegistry>, Arc<AtomicUsize>) {
    let root = Context::root();
    root.provide(ToolRegistry::new(&root)).unwrap();
    let registry = root.get::<ToolRegistry>().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    registry
        .register(Arc::new(EchoTool {
            calls: calls.clone(),
        }))
        .unwrap();
    (root, registry, calls)
}

fn echo_run() -> ToolRun {
    ToolRun {
        call_id: "c1".into(),
        name: "echo".into(),
        arguments: json!({"text": "hi"}),
        turn: 1,
        step: 1,
    }
}

#[tokio::test]
async fn execute_runs_tool_body() {
    let (root, registry, calls) = setup();
    let outcome = registry.execute(&root, &echo_run()).await;
    assert!(!outcome.is_error);
    assert_eq!(outcome.content, "echo:hi");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn registry_schema_shape_and_duplicate_rejection() {
    let (root, registry, _calls) = setup();
    let schemas = registry.schemas();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0]["type"], "function");
    assert_eq!(schemas[0]["function"]["name"], "echo");
    assert!(registry.get("echo").is_some());
    assert!(registry.get("nope").is_none());

    let duplicate = Arc::new(EchoTool {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    assert!(registry.register(duplicate).is_err());
    let _ = root;
}

#[tokio::test]
async fn pre_execute_deny_vetoes_tool_body() {
    let (root, registry, calls) = setup();
    root.on_waterfall::<ToolRun, PreDecision>("tools/pre-execute", |_d| {
        Box::pin(async move { PreDecision::Deny("策略不允许".into()) })
    })
    .unwrap();

    let outcome = registry.execute(&root, &echo_run()).await;
    assert!(outcome.is_error);
    assert_eq!(outcome.content, "策略不允许");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "工具体被跳过");
}

#[tokio::test]
async fn guard_denies_tool() {
    let (root, registry, calls) = setup();
    registry.register_guard(Arc::new(NameGuard { deny: "echo" }));

    let outcome = registry.execute(&root, &echo_run()).await;
    assert!(outcome.is_error);
    assert_eq!(outcome.content, "工具 echo 被守卫拒绝");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn execute_waterfall_can_replace_body() {
    let (root, registry, calls) = setup();
    root.on_waterfall::<ToolRun, ToolOutcome>("tools/execute", |_d| {
        Box::pin(async move { ToolOutcome::ok("被包装的结果") })
    })
    .unwrap();

    let outcome = registry.execute(&root, &echo_run()).await;
    assert_eq!(outcome.content, "被包装的结果");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "工具体被 execute 瀑布替换");
}

#[tokio::test]
async fn post_execute_can_replace_result() {
    let (root, registry, calls) = setup();
    root.on_waterfall::<ToolOutcome, ToolOutcome>("tools/post-execute", |d| {
        let content = d.value().content.clone();
        Box::pin(async move { ToolOutcome::ok(format!("[{content}]")) })
    })
    .unwrap();

    let outcome = registry.execute(&root, &echo_run()).await;
    assert_eq!(outcome.content, "[echo:hi]");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "工具体已执行");
}

#[tokio::test]
async fn unknown_tool_is_error_outcome() {
    let (root, registry, _calls) = setup();
    let run = ToolRun {
        call_id: "c2".into(),
        name: "nope".into(),
        arguments: json!({}),
        turn: 1,
        step: 1,
    };
    let outcome = registry.execute(&root, &run).await;
    assert!(outcome.is_error);
    assert!(outcome.content.contains("未注册"));
    assert_eq!(outcome.error.as_ref().unwrap().code, "NOT_FOUND");
}
