//! P6 验收：A 形态 DoD —— demo 端到端快照（逐事件一致，keyless snapshot 等价物）、
//! 不变量全过、JSONL 可重放、卸载顺序可审计、--dump-config 与装载一致。

use cos::{RunConfig, run};

fn demo_config() -> RunConfig {
    RunConfig {
        config_path: concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo.yml").to_string(),
        dump_config: false,
        session_id: "demo-e2e".into(),
        prompt: "帮我记一条演示 todo".into(),
        session_path: None,
        cancel: None,
        llm: None,
        agent_llm: None,
    }
}

#[tokio::test]
async fn demo_end_to_end_snapshot_invariants_replay_unload() {
    let session_path = std::env::temp_dir().join(format!("cos-demo-{}.jsonl", std::process::id()));
    let mut config = demo_config();
    config.session_path = Some(session_path.to_string_lossy().into_owned());

    let report = run(config).await.unwrap();

    // 1. 快照：逐事件类型一致（时间戳除外 —— keyless snapshot 等价物）
    let types: Vec<&str> = report
        .events
        .iter()
        .map(|event| event.data.type_name())
        .collect();
    let mut expected: Vec<&str> = vec![
        "turn/start",
        "step/start",
        "user/message",
        "request/header",
        "assistant/chunk",
        "assistant/message",
        "tool/call",
        "todo/write", // 工具执行期间的会话态（因果链内）
        "tool/result",
        "step/end",
        "step/start", // 结果回流 step
        "request/header",
    ];
    // MockReply::text 按字符切块："已记录演示任务。" = 8 个 chunk
    expected.extend(std::iter::repeat_n("assistant/chunk", 8));
    expected.extend(["assistant/message", "step/end", "turn/end"]);
    assert_eq!(types, expected);

    // 2. derive_messages 重放一致（含完整 call/result 对 + 回流回复）
    assert_eq!(report.messages.len(), 4);
    match &report.messages[0] {
        cos_llm::Message::User(user) => assert_eq!(user.content, "帮我记一条演示 todo"),
        other => panic!("期望 User，实际 {other:?}"),
    }
    match &report.messages[2] {
        cos_llm::Message::Tool(tool) => assert_eq!(tool.content, "已写入 1 条任务"),
        other => panic!("期望 Tool，实际 {other:?}"),
    }
    // 任务内容在 todo/write 会话事件里（工具经因果链写入）
    let todo_written = report.events.iter().any(|event| {
        matches!(&event.data, cos_session::SessionEventData::TodoWrite { todos }
            if todos.iter().any(|item| item.content.contains("演示任务")))
    });
    assert!(todo_written, "会话日志应含 todo/write 且内容为演示任务");
    match &report.messages[3] {
        cos_llm::Message::Assistant(assistant) => {
            assert_eq!(assistant.text(), "已记录演示任务。")
        }
        other => panic!("期望 Assistant，实际 {other:?}"),
    }

    // 3. 不变量全过（模型可见 ⟺ 已记录、seq 单调、边界配对、call/result 配对）
    assert!(report.violations.is_empty(), "{:?}", report.violations);

    // 4. 卸载顺序：apply 逆序（审计）
    assert_eq!(report.unload_order, vec!["demo", "bash", "todo"]);
    assert!(report.services_after_unload, "todo 服务应已随卸载反注册");

    // 5. JSONL 落盘（重放一致性已由 run 内部校验；行数 = header + 23 事件）
    let text = std::fs::read_to_string(&session_path).unwrap();
    assert_eq!(text.lines().count(), 24);
}

#[tokio::test]
async fn dump_config_matches_loaded_order() {
    let mut config = demo_config();
    config.dump_config = true;
    let report = run(config).await.unwrap();

    let dump: serde_json::Value =
        serde_json::from_str(report.dump.as_deref().expect("dump 输出")).unwrap();
    let entries = dump.as_array().expect("计划应为数组");
    assert_eq!(entries.len(), 3);
    let names: Vec<&str> = entries
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    // 与装载顺序一致（demo.yml 无依赖 → 清单序即拓扑序）
    assert_eq!(names, vec!["todo", "bash", "demo"]);
    assert_eq!(entries[0]["id"], "todo"); // id 缺省 = name
    assert_eq!(entries[0]["config"], serde_json::Value::Null);
}

#[tokio::test]
async fn unknown_plugin_config_fails_loud() {
    let root_path = std::env::temp_dir().join(format!("cos-bad-{}.yml", std::process::id()));
    std::fs::write(&root_path, "- name: nope\n").unwrap();
    let mut config = demo_config();
    config.config_path = root_path.to_string_lossy().into_owned();
    let result = run(config).await;
    assert!(matches!(result, Err(ref error) if error.to_string().contains("nope")));
}
