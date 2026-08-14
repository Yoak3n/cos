//! 写路径（apply）：提取 → 编号消解 → 状态合并 → 落库（卡 diff 先行，事务原子）。

use crate::error::Result;
use crate::extract;
use crate::store::{FactAction, MemoryStore, RelationCard, Tier};

/// 一轮对话（用户 + 助手文本）。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnPair {
    /// 用户说了什么。
    pub user: String,
    /// 助手说了什么。
    pub assistant: String,
}

/// 编号消解结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// 归并到既有主题。
    Merge(String),
    /// 新建主题（`uncertain` = LLM 不确定时的保守新建）。
    Create {
        /// 保守新建标记。
        uncertain: bool,
    },
}

/// apply 结果（测试与审计用）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyOutcome {
    /// 处理的事实数。
    pub facts: usize,
    /// 新建主题数。
    pub created: usize,
    /// 归并主题数。
    pub merged: usize,
    /// 纠正数（旧陈述 superseded）。
    pub corrected: usize,
}

impl MemoryStore {
    /// 写路径：每轮一次（卡 diff 先行 → 逐事实消解/合并 → 事件入账 → 衰减批处理）。
    pub async fn apply_turn(&self, turn: &TurnPair, ts: i64) -> Result<ApplyOutcome> {
        // 1. 提取（LLM）
        let card = self.card()?;
        let extraction = extract::extract(self.llm(), &card, turn).await?;

        // 2. 卡 diff 先行（LLM 合并各段 → 事务落库）
        let new_card = RelationCard {
            profile: extract::merge_card_section(
                self.llm(),
                &card.profile,
                &extraction.profile_notes,
            )
            .await?,
            agent_model: extract::merge_card_section(
                self.llm(),
                &card.agent_model,
                &extraction.agent_model_notes,
            )
            .await?,
            relationship: extract::merge_card_section(
                self.llm(),
                &card.relationship,
                &extraction.relationship_notes,
            )
            .await?,
            updated_at: ts,
        };
        self.set_card(&new_card)?;

        // 3. 逐事实：消解 → 合并/新建 → 事件
        let mut outcome = ApplyOutcome {
            facts: extraction.facts.len(),
            created: 0,
            merged: 0,
            corrected: 0,
        };
        for fact in &extraction.facts {
            match self.resolve_topic(&fact.topic_text).await? {
                Resolution::Merge(topic_id) => {
                    if fact.action == FactAction::Correct {
                        // correct：替换旧状态 + 旧陈述 superseded
                        self.supersede_events(&topic_id)?;
                        self.update_topic_merged(
                            &topic_id,
                            &fact.statement,
                            Some(&fact.topic_text),
                            ts,
                        )?;
                        outcome.corrected += 1;
                    } else {
                        let current = self
                            .topic(&topic_id)?
                            .map(|t| t.state_summary)
                            .unwrap_or_default();
                        let merged = extract::merge_state(
                            self.llm(),
                            &current,
                            &fact.statement,
                            fact.action,
                        )
                        .await?;
                        self.update_topic_merged(&topic_id, &merged, Some(&fact.topic_text), ts)?;
                        outcome.merged += 1;
                    }
                    self.append_event(&topic_id, &fact.statement, ts)?;
                }
                Resolution::Create { uncertain } => {
                    let topic_id = self.insert_topic(
                        &fact.topic_text,
                        &fact.statement,
                        ts,
                        Tier::Episodic,
                        uncertain,
                    )?;
                    self.append_event(&topic_id, &fact.statement, ts)?;
                    outcome.created += 1;
                }
            }
        }

        // 4. 衰减批处理
        self.apply_decay(ts)?;
        Ok(outcome)
    }

    /// 编号消解（写/读共用，解析器对称）：
    /// Stage 1 词法阻塞（canonical/alias 精确直通；bigram 近邻收候选）；
    /// Stage 2 LLM 仲裁（不确定 → 保守新建，假阴性可恢复）。
    pub async fn resolve_topic(&self, topic_text: &str) -> Result<Resolution> {
        // Stage 1a：精确别名直通（零 LLM）
        if let Some(topic) = self.topic_by_alias(topic_text)? {
            return Ok(Resolution::Merge(topic.topic_id));
        }
        // Stage 1b：词法近邻候选
        let candidates: Vec<(String, String, String)> = self
            .lexical_candidates(topic_text, 3)
            .into_iter()
            .map(|(topic, _)| (topic.topic_id, topic.canonical_name, topic.state_summary))
            .collect();
        if candidates.is_empty() {
            return Ok(Resolution::Create { uncertain: false });
        }
        // Stage 2：LLM 仲裁
        match extract::arbitrate(self.llm(), topic_text, &candidates).await? {
            Some(topic_id) => Ok(Resolution::Merge(topic_id)),
            None => Ok(Resolution::Create { uncertain: true }),
        }
    }

    /// 会话末慢消化（M3 完整实现；M1 提供接口与空默认：只做衰减）。
    pub async fn digest(&self, _transcript: &str, ts: i64) -> Result<()> {
        self.apply_decay(ts)?;
        Ok(())
    }
}

/// 由事件重建 turn pair（插件侧从会话日志投影）。
pub fn turn_pair_from_text(user: impl Into<String>, assistant: impl Into<String>) -> TurnPair {
    TurnPair {
        user: user.into(),
        assistant: assistant.into(),
    }
}
