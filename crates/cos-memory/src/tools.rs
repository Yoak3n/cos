//! 工具面（内核层，插件接线调用）：remember / recall / inventory / demote。
//! 观念：管理记忆 = 加强/减弱/检索/盘点；agent 不亲手删，删除是遗忘曲线的活。

use crate::error::Result;
use crate::recall::{MemoryHit, RecallOutcome};
use crate::store::{MemoryStore, Tier, bigram_similarity, now_ms};

/// remember：记（管线漏了/用户明说"记住这个"时加强或新建）。
/// 走同一管线：resolve → merge/create → event。
pub async fn remember_fact(
    store: &MemoryStore,
    content: &str,
    topic: Option<&str>,
) -> Result<String> {
    let ts = now_ms();
    let topic_text = topic.unwrap_or(content);
    match store.resolve_topic(topic_text).await? {
        crate::pipeline::Resolution::Merge(topic_id) => {
            let current = store
                .topic(&topic_id)?
                .map(|t| t.state_summary)
                .unwrap_or_default();
            let merged = crate::extract::merge_state(
                store.llm(),
                &current,
                content,
                crate::store::FactAction::Extend,
            )
            .await?;
            store.update_topic_merged(&topic_id, &merged, Some(topic_text), ts)?;
            store.append_event(&topic_id, content, ts)?;
            Ok(topic_id)
        }
        crate::pipeline::Resolution::Create { uncertain } => {
            let topic_id =
                store.insert_topic(topic_text, content, ts, Tier::Episodic, uncertain)?;
            store.append_event(&topic_id, content, ts)?;
            Ok(topic_id)
        }
    }
}

/// recall：查（对话中取用；命中即唤醒）。
pub async fn recall_memories(
    store: &MemoryStore,
    query: &str,
    limit: usize,
) -> Result<RecallOutcome> {
    store.recall(query, limit).await
}

/// inventory：盘点（"我关于 X 知道什么 / 我整体知道什么"）。
/// 有 query → 词法过滤；无 → 全部按权重降序。
pub fn inventory_topics(
    store: &MemoryStore,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<MemoryHit>> {
    let ts = now_ms();
    let mut hits: Vec<MemoryHit> = store
        .topics()?
        .into_iter()
        .filter(|topic| match query {
            Some(query) => {
                let name = bigram_similarity(query, &topic.canonical_name).max(
                    topic
                        .aliases
                        .iter()
                        .map(|alias| bigram_similarity(query, alias))
                        .fold(0.0, f64::max),
                );
                let state = bigram_similarity(query, &topic.state_summary);
                name.max(state) > 0.15
            }
            None => true,
        })
        .map(|topic| MemoryHit {
            when: crate::recall::humanize(ts - topic.last_discussed_at),
            topic_id: topic.topic_id,
            canonical_name: topic.canonical_name,
            state_summary: topic.state_summary,
            last_discussed_at: topic.last_discussed_at,
            n_times: topic.n_times,
            tier: topic.tier,
            confidence: topic.weight,
        })
        .collect();
    hits.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    hits.truncate(limit);
    Ok(hits)
}

/// demote：淡忘（明确减弱 → 权重压低位 → 快速衰减；可逆）。
pub fn demote_topic(
    store: &MemoryStore,
    query: &str,
    reason: Option<&str>,
) -> Result<Option<String>> {
    let ts = now_ms();
    let best = store
        .topics()?
        .into_iter()
        .map(|topic| {
            let score = bigram_similarity(query, &topic.canonical_name).max(
                topic
                    .aliases
                    .iter()
                    .map(|alias| bigram_similarity(query, alias))
                    .fold(0.0, f64::max),
            );
            (topic.topic_id, score)
        })
        .filter(|(_, score)| *score > 0.15)
        .max_by(|a, b| a.1.total_cmp(&b.1));
    match best {
        Some((topic_id, _)) => {
            store.weaken(&topic_id, 0.3, reason, ts)?;
            Ok(Some(topic_id))
        }
        None => Ok(None),
    }
}
