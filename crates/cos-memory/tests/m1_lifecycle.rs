//! M1 验收：记忆内核生命周期（脚本化 mock LLM，A 形态 DoD）。
//!
//! 覆盖：apply_turn 提取→合并 / 保守新建 / correct 取代旧陈述 / 衰减与复活 /
//! 诚实出口 / 重开持久化 / 四工具。mock 脚本按**调用序号**编排（cos-llm-mock 语义）。

use std::sync::Arc;

use cos_memory::{
    ApplyOutcome, MemoryStore, demote_topic, inventory_topics, now_ms, recall_memories,
    remember_fact, turn_pair_from_text,
};
use cos_test_support::{MockAdapter, MockReply};

const DAY: i64 = 86_400_000;

/// 测试用临时库路径（按进程隔离，测试末删除）。
fn temp_db(tag: &str) -> String {
    std::env::temp_dir()
        .join(format!("cos-memory-{tag}-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// 打开存储并注入脚本化 mock。
fn store_at(path: &str, script: Vec<MockReply>) -> MemoryStore {
    MemoryStore::open(path, Arc::new(MockAdapter::new("memory-mock", script))).unwrap()
}

/// 提取：吉他练习（new + profile 注记）。
const EXTRACT_GUITAR: &str = r#"{"facts":[{"kind":"user","action":"new","topic_text":"吉他练习","statement":"用户在练吉他"}],"card_notes":{"profile":["用户在练吉他"],"agent_model":[],"relationship":[]}}"#;
/// 关系卡 profile 合并。
const CARD_PROFILE: &str = r#"{"text":"用户在练吉他"}"#;

/// 种入第一条吉他练习事实。
async fn seed_guitar(store: &MemoryStore, ts: i64) -> ApplyOutcome {
    store
        .apply_turn(
            &turn_pair_from_text("我最近在练吉他", "好的，吉他练习要常练"),
            ts,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn apply_turn_creates_then_merges_on_exact_alias() {
    let path = temp_db("create-merge");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
            MockReply::text(
                r#"{"facts":[{"kind":"user","action":"extend","topic_text":"吉他练习","statement":"还在练吉他"}]}"#,
            ),
            MockReply::text(r#"{"state_summary":"用户在持续练吉他"}"#),
        ],
    );

    let base = now_ms();
    let first = seed_guitar(&store, base).await;
    assert_eq!(
        first,
        ApplyOutcome {
            facts: 1,
            created: 1,
            merged: 0,
            corrected: 0
        }
    );
    let topics = store.topics().unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].canonical_name, "吉他练习");
    assert_eq!(topics[0].state_summary, "用户在练吉他");
    assert_eq!(topics[0].n_times, 1);

    let second = store
        .apply_turn(&turn_pair_from_text("还在练吉他", "坚持"), base + DAY)
        .await
        .unwrap();
    assert_eq!(
        second,
        ApplyOutcome {
            facts: 1,
            created: 0,
            merged: 1,
            corrected: 0
        }
    );
    let topics = store.topics().unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].state_summary, "用户在持续练吉他");
    assert_eq!(topics[0].n_times, 2);
    // 表述与规范名一致时不记别名（不冗余）
    assert!(topics[0].aliases.is_empty());
    assert_eq!(store.events(None).unwrap().len(), 2);

    // 别名簿记：不同表述归并时记录，重复不叠加（同时回归 update_topic_merged 持锁路径）
    let topic_id = topics[0].topic_id.clone();
    store
        .update_topic_merged(
            &topic_id,
            "用户在持续练吉他",
            Some("弹吉他"),
            base + 2 * DAY,
        )
        .unwrap();
    store
        .update_topic_merged(
            &topic_id,
            "用户在持续练吉他",
            Some("弹吉他"),
            base + 2 * DAY,
        )
        .unwrap();
    let topics = store.topics().unwrap();
    assert_eq!(topics[0].aliases, vec!["弹吉他".to_string()]);
    assert_eq!(topics[0].n_times, 4);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn uncertain_arbitration_creates_conservatively() {
    let path = temp_db("conservative");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
            MockReply::text(
                r#"{"facts":[{"kind":"user","action":"new","topic_text":"吉他进阶","statement":"想学进阶指法"}]}"#,
            ),
            // 仲裁：近邻候选存在但不确定 → none → 保守新建
            MockReply::text(r#"{"merge":"none"}"#),
        ],
    );

    let base = now_ms();
    seed_guitar(&store, base).await;
    let outcome = store
        .apply_turn(
            &turn_pair_from_text("我想学吉他进阶指法", "好的"),
            base + DAY,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome {
            facts: 1,
            created: 1,
            merged: 0,
            corrected: 0
        }
    );

    let topics = store.topics().unwrap();
    assert_eq!(topics.len(), 2);
    let advanced = topics
        .iter()
        .find(|topic| topic.canonical_name == "吉他进阶")
        .unwrap();
    assert!(advanced.uncertain, "LLM 不确定时必须标记保守新建");
    assert_ne!(topics[0].topic_id, topics[1].topic_id);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn correct_supersedes_old_statements() {
    let path = temp_db("correct");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
            MockReply::text(
                r#"{"facts":[{"kind":"user","action":"correct","topic_text":"吉他练习","statement":"其实已经不练吉他了"}]}"#,
            ),
        ],
    );

    let base = now_ms();
    seed_guitar(&store, base).await;
    let outcome = store
        .apply_turn(
            &turn_pair_from_text("其实我已经不练吉他了", "好的，知道了"),
            base + DAY,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ApplyOutcome {
            facts: 1,
            created: 0,
            merged: 0,
            corrected: 1
        }
    );

    let topics = store.topics().unwrap();
    assert_eq!(topics[0].state_summary, "其实已经不练吉他了");
    assert_eq!(topics[0].n_times, 2);

    // 旧陈述 superseded，新陈述有效；真相源不删
    let events = store.events(None).unwrap();
    assert_eq!(events.len(), 2);
    assert!(events[0].3, "旧陈述应被标记 superseded");
    assert!(!events[1].3, "新陈述应有效");

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn decay_forgets_topic_but_keeps_truth_and_recall_is_honest() {
    let path = temp_db("decay");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
        ],
    );

    let base = now_ms();
    seed_guitar(&store, base).await;
    assert_eq!(store.topics().unwrap().len(), 1);

    // 90 天不提起：episodic 衰减 0.05/天 → exp(-4.5) ≈ 0.011 < 0.02 阈值 → 遗忘
    store.apply_decay(base + 90 * DAY).unwrap();
    assert!(store.topics().unwrap().is_empty(), "低于阈值的主题应被遗忘");
    assert_eq!(store.events(None).unwrap().len(), 1, "事件真相源必须保留");

    let outcome = recall_memories(&store, "吉他练习", 5).await.unwrap();
    assert!(outcome.none, "已遗忘的话题必须诚实出口");
    assert!(outcome.hits.is_empty());

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn recall_hit_activates_and_revives_weight() {
    let path = temp_db("revive");
    let store = store_at(
        &path,
        vec![
            MockReply::text(EXTRACT_GUITAR),
            MockReply::text(CARD_PROFILE),
        ],
    );

    let base = now_ms();
    seed_guitar(&store, base).await;

    // 30 天：权重压到 exp(-1.5) ≈ 0.223
    store.apply_decay(base + 30 * DAY).unwrap();
    let decayed = store.topics().unwrap()[0].weight;
    assert!(decayed < 1.0 && decayed > 0.0);

    let outcome = recall_memories(&store, "吉他练习", 5).await.unwrap();
    assert!(!outcome.none);
    assert_eq!(outcome.hits[0].canonical_name, "吉他练习");
    assert!(outcome.hits[0].confidence > 0.5);

    let topic = store.topics().unwrap().into_iter().next().unwrap();
    assert_eq!(topic.activation_count, 1, "命中即唤醒");
    assert!(topic.weight > decayed, "召回命中应恢复权重");
    assert!(topic.weight <= 1.0);

    // 无关查询 → 诚实出口
    let none = recall_memories(&store, "完全无关的猫咪", 5).await.unwrap();
    assert!(none.none);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn reopen_keeps_memory_and_card() {
    let path = temp_db("reopen");
    {
        let store = store_at(
            &path,
            vec![
                MockReply::text(EXTRACT_GUITAR),
                MockReply::text(CARD_PROFILE),
            ],
        );
        seed_guitar(&store, now_ms()).await;
    }

    let store = store_at(&path, vec![]);
    let topics = store.topics().unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].state_summary, "用户在练吉他");
    assert_eq!(store.card().unwrap().profile, "用户在练吉他");
    assert_eq!(store.events(None).unwrap().len(), 1);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn tools_remember_recall_inventory_demote() {
    let path = temp_db("tools");
    let store = store_at(&path, vec![]);

    let topic_id = remember_fact(&store, "我喜欢喝手冲咖啡", Some("咖啡偏好"))
        .await
        .unwrap();
    assert!(!topic_id.is_empty());

    let outcome = recall_memories(&store, "咖啡偏好", 5).await.unwrap();
    assert!(!outcome.none);
    assert_eq!(outcome.hits[0].canonical_name, "咖啡偏好");

    let inventory = inventory_topics(&store, None, 10).unwrap();
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].canonical_name, "咖啡偏好");

    let demoted = demote_topic(&store, "咖啡偏好", Some("测试减弱")).unwrap();
    assert_eq!(demoted.as_deref(), Some(topic_id.as_str()));
    let topic = store.topics().unwrap().into_iter().next().unwrap();
    assert!(
        (topic.weight - 0.3).abs() < 1e-9,
        "demote 权重应压到 0.3 倍"
    );

    assert_eq!(demote_topic(&store, "不存在的话题", None).unwrap(), None);

    drop(store);
    let _ = std::fs::remove_file(&path);
}

/// 提取输出非 JSON（推理模型输出思考过程）→ 追加纠偏说明重试一次 → 成功落库。
#[tokio::test]
async fn extraction_retries_once_when_first_output_is_reasoning_text() {
    let path = temp_db("extract-retry");
    let store = store_at(
        &path,
        vec![
            // 第一次：模型输出思考过程而非 JSON（opencode 网关实测行为）
            MockReply::text(
                "让我先分析一下这段对话……用户提到了咖啡，这是关于偏好的事实。\
                 接下来我会给出提取结果。",
            ),
            // 重试：纠偏后输出合法 JSON
            MockReply::text(
                r#"{"facts":[{"kind":"user","action":"new","topic_text":"咖啡偏好","statement":"用户喜欢手冲咖啡"}],"card_notes":{"profile":["用户喜欢手冲咖啡"],"agent_model":[],"relationship":[]}}"#,
            ),
            MockReply::text(r#"{"text":"用户喜欢手冲咖啡"}"#),
        ],
    );

    store
        .apply_turn(
            &turn_pair_from_text("我喜欢手冲咖啡", "好的，记住了"),
            now_ms(),
        )
        .await
        .unwrap();

    let topics = store.topics().unwrap();
    assert_eq!(topics.len(), 1, "重试后应成功落库");
    assert_eq!(topics[0].canonical_name, "咖啡偏好");
    assert_eq!(topics[0].state_summary, "用户喜欢手冲咖啡");
    assert_eq!(store.card().unwrap().profile, "用户喜欢手冲咖啡");

    drop(store);
    let _ = std::fs::remove_file(&path);
}

/// 两次都非 JSON → 报错（含首次与重试错误，便于诊断）。
#[tokio::test]
async fn extraction_fails_loud_when_both_attempts_are_not_json() {
    let path = temp_db("extract-retry-fail");
    let store = store_at(
        &path,
        vec![
            MockReply::text("思考过程……没有 JSON"),
            MockReply::text("还是不输出 JSON"),
        ],
    );

    let error = store
        .apply_turn(
            &turn_pair_from_text("我喜欢手冲咖啡", "好的，记住了"),
            now_ms(),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("提取"), "{error}");
    assert!(error.contains("重试仍失败"), "{error}");
    assert!(store.topics().unwrap().is_empty(), "失败不应留痕");

    drop(store);
    let _ = std::fs::remove_file(&path);
}
