//! P3 验收：seq 单调、derive_messages 投影正确性、JSONL 持久化 → 重载逐字节一致。

use cos_llm::{
    AssistantMessage, ChunkDelta, ContentBlock, Message, TokenUsage, ToolResultMessage, UserMessage,
};
use cos_session::{
    SESSION_FORMAT_VERSION, Session, SessionError, SessionEventData, SessionHeader, TurnEndReason,
    load_jsonl, save_jsonl,
};
use serde_json::json;

fn sample_log() -> Session {
    let session = Session::new("sess-1");
    session.append_at(SessionEventData::TurnStart { turn: 1 }, 100);
    session.append_at(SessionEventData::UserMessage(UserMessage::new("你好")), 101);
    session.append_at(
        SessionEventData::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: cos_llm::StreamChunk::text("你"),
        },
        102,
    );
    session.append_at(
        SessionEventData::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: cos_llm::StreamChunk::text("好"),
        },
        103,
    );
    session.append_at(
        SessionEventData::AssistantMessage {
            turn: 1,
            step: 1,
            message: AssistantMessage::new(vec![ContentBlock::Text {
                text: "你好".into(),
            }]),
            usage: Some(TokenUsage {
                input_tokens: 3,
                output_tokens: 2,
            }),
        },
        104,
    );
    session.append_at(
        SessionEventData::ToolCall {
            turn: 1,
            step: 1,
            call_id: "c1".into(),
            name: "todo_write".into(),
            arguments: "{}".into(),
        },
        105,
    );
    session.append_at(
        SessionEventData::ToolResult {
            turn: 1,
            step: 1,
            call_id: "c1".into(),
            message: ToolResultMessage::new("ok"),
            error: None,
        },
        106,
    );
    session.append_at(
        SessionEventData::Custom {
            name: "plugin/note".into(),
            data: json!({"text": "hi"}),
        },
        107,
    );
    session.append_at(
        SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
        108,
    );
    session
}

#[test]
fn seq_is_monotonic_from_one_with_timestamps() {
    let session = sample_log();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=9).collect::<Vec<u64>>());
    assert_eq!(session.last_seq(), 9);
    assert_eq!(session.events()[0].time, 100);
    assert_eq!(session.events()[8].time, 108);
}

#[test]
fn derive_messages_projects_surface_only() {
    let messages = sample_log().derive_messages();
    // user/message、assistant/message、tool/result、Custom —— 4 条；
    // chunk、turn/step 边界、tool/call 不参与投影。
    assert_eq!(messages.len(), 4);

    match &messages[0] {
        Message::User(user) => assert_eq!(user.content, "你好"),
        other => panic!("期望 User，实际 {other:?}"),
    }
    match &messages[1] {
        Message::Assistant(assistant) => {
            assert_eq!(
                assistant.content,
                vec![ContentBlock::Text {
                    text: "你好".into()
                }]
            );
            assert_eq!(assistant.text(), "你好");
        }
        other => panic!("期望 Assistant，实际 {other:?}"),
    }
    match &messages[2] {
        Message::Tool(tool) => assert_eq!(tool.content, "ok"),
        other => panic!("期望 Tool，实际 {other:?}"),
    }
    match &messages[3] {
        Message::Custom { name, data } => {
            assert_eq!(name, "plugin/note");
            assert_eq!(data, &json!({"text": "hi"}));
        }
        other => panic!("期望 Custom，实际 {other:?}"),
    }
}

#[test]
fn event_wire_shape_has_type_and_data_envelope() {
    let session = sample_log();
    let value = serde_json::to_value(&session.events()[0]).unwrap();
    assert_eq!(value["seq"], 1);
    assert_eq!(value["time"], 100);
    assert_eq!(value["type"], "turn/start");
    assert_eq!(value["data"]["turn"], 1);
}

#[test]
fn jsonl_roundtrip_is_byte_exact() {
    let session = sample_log();
    let header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: session.id().to_string(),
        created_at_ms: 42,
        cwd: None,
    };
    let path = std::env::temp_dir().join(format!(
        "cos-session-roundtrip-{}.jsonl",
        std::process::id()
    ));
    save_jsonl(&session, &header, &path).unwrap();
    let original = std::fs::read(&path).unwrap();

    let (loaded_header, events) = load_jsonl(&path).unwrap();
    assert_eq!(loaded_header, header);
    assert_eq!(events, session.events().to_vec());

    // 恢复后投影一致
    let restored = Session::from_events(header.id.clone(), events.clone());
    assert_eq!(restored.derive_messages(), session.derive_messages());

    // 再存：逐字节一致
    save_jsonl(&restored, &loaded_header, &path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);

    // 恢复后续写：seq 连续
    let resumed = Session::from_events(header.id, events);
    resumed.append_at(SessionEventData::TurnStart { turn: 2 }, 200);
    assert_eq!(resumed.events().last().unwrap().seq, 10);
}

#[test]
fn version_mismatch_is_rejected() {
    let path =
        std::env::temp_dir().join(format!("cos-session-version-{}.jsonl", std::process::id()));
    std::fs::write(&path, "{\"version\":99,\"id\":\"x\",\"createdAt\":1}\n").unwrap();
    let err = load_jsonl(&path).unwrap_err();
    assert!(
        matches!(
            err,
            SessionError::VersionMismatch {
                expected: 0,
                found: 99
            }
        ),
        "实际 {err:?}"
    );
}

#[test]
fn chunk_delta_roundtrips_through_json() {
    let chunk = cos_llm::StreamChunk::text("字");
    let value = serde_json::to_value(&chunk).unwrap();
    let back: cos_llm::StreamChunk = serde_json::from_value(value).unwrap();
    assert_eq!(back.delta, ChunkDelta::Text { text: "字".into() });
}
