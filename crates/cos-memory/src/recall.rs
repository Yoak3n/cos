//! 读路径（recall）：Mode A 话题检索（词法 + 激活 + 近因）与 Mode B 时间检索。

use crate::error::Result;
use crate::store::{MemoryStore, Tier, bigram_similarity};

/// 召回命中（结构化返回：topic/state/when/n_times/confidence）。
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    /// 主题 id。
    pub topic_id: String,
    /// 规范名。
    pub canonical_name: String,
    /// 当前状态。
    pub state_summary: String,
    /// 最近讨论（epoch 毫秒）。
    pub last_discussed_at: i64,
    /// 讨论次数。
    pub n_times: i64,
    /// 分层。
    pub tier: Tier,
    /// 置信度（匹配得分，0-1）。
    pub confidence: f64,
    /// 人性化"什么时候"（如"两周前"）。
    pub when: String,
}

/// Mode A 召回结果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecallOutcome {
    /// 命中（按得分降序）。
    pub hits: Vec<MemoryHit>,
    /// 是否诚实出口（无相关记忆）。
    pub none: bool,
}

/// Mode B 时间检索饲料（主动性燃料）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModeBFeed {
    /// 最近 N 天讨论过的主题。
    pub recent: Vec<MemoryHit>,
    /// 今天讨论过的主题数。
    pub today_count: usize,
    /// 未完成承诺（M2 接 promises 表后填充）。
    pub open_promises: Vec<String>,
}

impl MemoryStore {
    /// Mode A 话题检索：`score = lexical × activation × recency`，命中即唤醒（激活+1、时间刷新、权重恢复）。
    pub async fn recall(&self, query: &str, limit: usize) -> Result<RecallOutcome> {
        let ts = crate::store::now_ms();
        self.apply_decay(ts)?;

        let mut scored: Vec<(MemoryHit, f64)> = Vec::new();
        for topic in self.topics()? {
            let name_sim = bigram_similarity(query, &topic.canonical_name).max(
                topic
                    .aliases
                    .iter()
                    .map(|alias| bigram_similarity(query, alias))
                    .fold(0.0, f64::max),
            );
            let state_sim = bigram_similarity(query, &topic.state_summary);
            let semantic = 0.7 * name_sim + 0.3 * state_sim;
            if semantic < 0.05 {
                continue; // 低于阈值不参与（诚实出口）
            }
            let activation = 1.0 + 0.5 * topic.activation_count as f64;
            let days = ((ts - topic.last_discussed_at) as f64) / 86_400_000.0;
            let recency = (-0.15 * days).exp();
            let score = semantic * activation * recency;
            scored.push((
                MemoryHit {
                    topic_id: topic.topic_id.clone(),
                    canonical_name: topic.canonical_name.clone(),
                    state_summary: topic.state_summary.clone(),
                    last_discussed_at: topic.last_discussed_at,
                    n_times: topic.n_times,
                    tier: topic.tier,
                    confidence: score,
                    when: humanize(ts - topic.last_discussed_at),
                },
                score,
            ));
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(limit);

        // 唤醒：命中即激活 + 时间刷新 + 权重恢复（设计 §2.2/§5）
        for (hit, _) in &scored {
            self.activate(&hit.topic_id, ts)?;
        }

        let none = scored.is_empty();
        Ok(RecallOutcome {
            hits: scored.into_iter().map(|(hit, _)| hit).collect(),
            none,
        })
    }

    /// Mode B 时间检索：最近 N 天经历、今天事件、未完成承诺（不做文本匹配）。
    pub fn recent_feed(&self, days: i64, ts: i64) -> Result<ModeBFeed> {
        let window = ts - days * 86_400_000;
        let mut recent: Vec<MemoryHit> = self
            .topics()?
            .into_iter()
            .filter(|topic| topic.last_discussed_at >= window)
            .map(|topic| MemoryHit {
                when: humanize(ts - topic.last_discussed_at),
                topic_id: topic.topic_id,
                canonical_name: topic.canonical_name,
                state_summary: topic.state_summary,
                last_discussed_at: topic.last_discussed_at,
                n_times: topic.n_times,
                tier: topic.tier,
                confidence: 1.0,
            })
            .collect();
        recent.sort_by_key(|hit| std::cmp::Reverse(hit.last_discussed_at));

        let day_start = ts - (ts % 86_400_000);
        let today_count = self
            .topics()?
            .into_iter()
            .filter(|topic| topic.last_discussed_at >= day_start)
            .count();

        Ok(ModeBFeed {
            recent,
            today_count,
            open_promises: Vec::new(), // M2：promises 表接线后填充
        })
    }
}

/// 人性化时间："刚刚 / N 分钟前 / N 小时前 / N 天前 / N 周前 / N 个月前"。
pub(crate) fn humanize(age_ms: i64) -> String {
    let minutes = age_ms / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    let weeks = days / 7;
    if minutes < 1 {
        "刚刚".into()
    } else if minutes < 60 {
        format!("{minutes} 分钟前")
    } else if hours < 24 {
        format!("{hours} 小时前")
    } else if days < 7 {
        format!("{days} 天前")
    } else if days < 30 {
        format!("{weeks} 周前")
    } else {
        format!("{} 个月前", days / 30)
    }
}
