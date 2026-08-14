//! M1 验收：plugin-memory 接线（服务提供 + 四工具注册 + 经注册表执行诚实出口）。

use std::sync::Arc;

use dsh_core::{Context, Plugin};
use dsh_llm_mock::MockAdapter;
use dsh_memory::{MemoryLlmProvider, MemoryStore};
use dsh_tools::{ToolRegistry, ToolRun};
use plugin_memory::{MemoryConfig, MemoryPlugin};
use serde_json::json;

/// 测试用临时库路径。
fn temp_db() -> String {
    std::env::temp_dir()
        .join(format!("plugin-memory-wiring-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn apply_wires_memory_service_and_four_tools() {
    let path = temp_db();
    let ctx = Context::root();
    ctx.provide(MemoryLlmProvider {
        inner: Arc::new(MockAdapter::new("memory-mock", vec![])),
    })
    .unwrap();
    ctx.provide(ToolRegistry::new(&ctx)).unwrap();

    MemoryPlugin
        .apply(
            &ctx,
            &MemoryConfig {
                db_path: path.clone(),
            },
        )
        .unwrap();

    // 服务就绪
    assert!(ctx.get::<MemoryStore>().is_ok(), "memory 服务应被提供");

    // 四工具注册
    let registry = ctx.get::<ToolRegistry>().unwrap();
    let names: Vec<String> = registry
        .list()
        .into_iter()
        .map(|tool| tool.name().to_string())
        .collect();
    for expected in ["recall", "remember", "inventory", "demote"] {
        assert!(
            names.iter().any(|name| name == expected),
            "缺少工具 {expected}（现有: {names:?}）"
        );
    }

    // 经注册表执行 recall：空库 → 诚实出口
    let run = ToolRun {
        call_id: "c1".into(),
        name: "recall".into(),
        arguments: json!({ "query": "吉他" }),
        turn: 1,
        step: 1,
    };
    let outcome = registry.execute(&ctx, &run).await;
    assert!(!outcome.is_error);
    assert!(
        outcome.content.contains("无相关记忆"),
        "空库 recall 应诚实出口，实际: {}",
        outcome.content
    );

    drop(ctx);
    let _ = std::fs::remove_file(&path);
}
