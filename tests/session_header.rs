//! 会话头元数据的端到端保全：**从日志恢复的创建时间要穿过收尾落盘**。
//!
//! 回归点：`finish_with` 曾把 `created_at_ms` 写死 0。收尾落盘是覆盖式重写，于是
//! 任何「读出 → 续跑 → 写回」的用法（库嵌入模式的正常姿势）都会在第一次收尾后
//! 丢掉创建时间，而且每次重写再丢一次——嵌入方只能自己重读旧 header 兜回来。
//!
//! 走的是库嵌入模式（`config_path: None` → `assemble` 不建主 agent），
//! 与 Ariadne 的用法一致。

use std::sync::Arc;

use cos::{RunConfig, assemble, finish_with};
use cos_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_llm::{StreamChunk, UserMessage};
use cos_session::{SESSION_FORMAT_VERSION, Session, SessionHeader, load_jsonl, save_jsonl};
use cos_test_support::{MockAdapter, MockReply};

const SESSION_ID: &str = "header-e2e";

fn config_with(session_path: &str) -> RunConfig {
    RunConfig {
        // 库嵌入模式：无配置文件、无 LLM → assemble 不建主 agent，agent 由调用方自建
        config_path: None,
        dump_config: false,
        session_id: SESSION_ID.into(),
        prompt: None,
        session_path: Some(session_path.to_string()),
        cancel: None,
        llm: None,
        agent_llm: None,
        agent_driver: None,
        patch_files: Vec::new(),
    }
}

#[tokio::test]
async fn restored_creation_time_survives_finish() {
    let path = std::env::temp_dir().join(format!("cos-header-{}.jsonl", std::process::id()));
    let path_str = path.to_string_lossy().into_owned();

    // 先造一份「既有日志」，创建时间是一个可辨认的值
    let seed = Session::from_events_at(SESSION_ID, Vec::new(), 42);
    save_jsonl(
        &seed,
        &SessionHeader {
            version: SESSION_FORMAT_VERSION,
            id: SESSION_ID.into(),
            created_at_ms: 42,
            cwd: None,
        },
        &path,
    )
    .unwrap();

    let config = config_with(&path_str);
    let assembled = assemble(&config).await.unwrap();

    // 库嵌入模式的正常姿势：读出日志 → 恢复会话 → 交给 agent 续跑
    let (header, events) = load_jsonl(&path).unwrap();
    let session = Session::from_events_at(header.id.clone(), events, header.created_at_ms);
    let registry = assembled.root.get::<AgentRegistry>().expect("装配时已提供");
    let agent = registry
        .create(CreateAgentOptions {
            session: Some(session),
            session_id: SESSION_ID.into(),
            options: AgentOptions {
                provider: Some("mock".into()),
                model: Some("mock".into()),
                max_tokens: None,
                role: None,
            },
            adapter: Arc::new(MockAdapter::new(
                "mock",
                vec![MockReply::new(vec![StreamChunk::text("答")])],
            )),
        })
        .await
        .unwrap();
    agent.followup(UserMessage::new("问"));
    agent.when_idle().await;

    let report = finish_with(&assembled, &agent, &config).await.unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);

    // 收尾后：创建时间还在，id 一致，且这一轮的事件确实写进去了
    let (after, events_after) = load_jsonl(&path).unwrap();
    assert_eq!(after.created_at_ms, 42, "收尾落盘不该把创建时间冲成 0");
    assert_eq!(after.id, SESSION_ID);
    assert!(
        events_after.len() > 1,
        "续跑应追加事件，实际 {} 条",
        events_after.len()
    );

    let _ = std::fs::remove_file(&path);
}
