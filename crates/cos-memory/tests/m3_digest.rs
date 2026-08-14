//! M3 验收（内核）：digest 慢路径（统计 + 转录 → 卡三段注记 → 合并落库）、
//! 上下文滚动压缩、会话级 KV 状态。

use std::sync::Arc;

use cos_memory::{MemoryStore, now_ms, turn_pair_from_text};
use cos_test_support::{MockAdapter, MockReply};

fn temp_db(tag: &str) -> String {
    std::env::temp_dir()
        .join(format!("cos-memory-m3-{tag}-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

fn store_at(path: &str, script: Vec<MockReply>) -> MemoryStore {
    MemoryStore::open(path, Arc::new(MockAdapter::new("memory-mock", script))).unwrap()
}

const EXTRACT_GUITAR: &str = r#"{"facts":[{"kind":"user","action":"new","topic_text":"吉他练习","statement":"用户在练吉他"}],"card_notes":{"profile":["用户在练吉他"],"agent_model":[],"relationship":[]}}"#;
const CARD_PROFILE: &str = r#"{"text":"用户在练吉他"}"#;

#[tokio::test]
async fn digest_merges_stats_backed_notes_into_card() {
    let path = temp_db("digest");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
            // 第二 turn 无新事实（无 LLM 合并）
            MockReply::text(r#"{"facts":[],"card_notes":{}}"#),
            // digest 推断：三段注记（高置信度才写）
            MockReply::text(
                r#"{"profile_notes":["用户音乐与咖啡双爱好"],"agent_model_notes":["应该主动问用户生日"],"relationship_notes":["聊天节奏轻松"]}"#,
            ),
            MockReply::text(r#"{"text":"用户音乐与咖啡双爱好"}"#),
            MockReply::text(r#"{"text":"应该主动问用户生日"}"#),
            MockReply::text(r#"{"text":"聊天节奏轻松"}"#),
        ],
    );

    let base = now_ms();
    store
        .apply_turn(
            &turn_pair_from_text("我最近在练吉他", "好的，吉他练习要常练"),
            base,
        )
        .await
        .unwrap();
    store
        .apply_turn(&turn_pair_from_text("今天天气不错", "是呀"), base + 1000)
        .await
        .unwrap();

    let transcript =
        "— turn 1 —\n用户: 我最近在练吉他\n助手: 好的\n— turn 2 —\n用户: 今天天气不错\n助手: 是呀";
    store.digest(transcript, base + 2000).await.unwrap();

    let card = store.card().unwrap();
    assert_eq!(card.profile, "用户音乐与咖啡双爱好");
    assert_eq!(card.agent_model, "应该主动问用户生日");
    assert_eq!(card.relationship, "聊天节奏轻松");
    assert_eq!(card.updated_at, base + 2000);

    // 事件真相源不受 digest 影响
    assert_eq!(store.events(None).unwrap().len(), 1);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn digest_with_empty_notes_keeps_card() {
    let path = temp_db("digest-empty");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
            MockReply::text(
                r#"{"profile_notes":[],"agent_model_notes":[],"relationship_notes":[]}"#,
            ),
        ],
    );

    let base = now_ms();
    store
        .apply_turn(&turn_pair_from_text("我最近在练吉他", "好的"), base)
        .await
        .unwrap();
    store.digest("— turn 1 —", base + 1000).await.unwrap();

    let card = store.card().unwrap();
    assert_eq!(card.profile, "用户在练吉他", "空注记不应改写卡");
    assert!(card.agent_model.is_empty());
    assert!(card.relationship.is_empty());

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn compress_context_rolls_summary() {
    let path = temp_db("compress");
    let store = store_at(
        &path,
        vec![
            MockReply::text(r#"{"summary":"要点A"}"#),
            MockReply::text(r#"{"summary":"要点A+要点B"}"#),
        ],
    );

    let first = store.compress_context("", "第一段对话").await.unwrap();
    assert_eq!(first, "要点A");
    let second = store.compress_context(&first, "第二段对话").await.unwrap();
    assert_eq!(second, "要点A+要点B");

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn session_state_kv_upsert() {
    let path = temp_db("kv");
    let store = store_at(&path, vec![]);

    assert_eq!(store.get_state("k").unwrap(), None);
    store.set_state("k", "v1").unwrap();
    assert_eq!(store.get_state("k").unwrap(), Some("v1".to_string()));
    store.set_state("k", "v2").unwrap();
    assert_eq!(store.get_state("k").unwrap(), Some("v2".to_string()));

    drop(store);
    let _ = std::fs::remove_file(&path);
}
