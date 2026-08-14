//! 存储层：SQLite 五表 schema + 行类型 + 衰减/激活/复活。

use std::sync::{Arc, Mutex};

use dsh_core::Service;
use dsh_llm::LlmAdapter;
use rusqlite::{Connection, params};

use crate::error::{MemoryError, Result};

/// 记忆分层（决定"怎么到达 agent"，不是排序维度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 共同经历（recall 通道，衰减 0.05/天）。
    Episodic,
    /// 一次性琐事（recall 通道低权重，衰减 0.15/天）。
    Trivia,
}

impl Tier {
    /// 解析存储值。
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "episodic" => Ok(Self::Episodic),
            "trivia" => Ok(Self::Trivia),
            other => Err(MemoryError::Invalid(format!("未知 tier: {other}"))),
        }
    }

    /// 存储值。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Episodic => "episodic",
            Self::Trivia => "trivia",
        }
    }

    /// 日衰减率（设计文档 §5）。
    pub fn decay_rate(&self) -> f64 {
        match self {
            Self::Episodic => 0.05,
            Self::Trivia => 0.15,
        }
    }

    /// 遗忘阈值（设计文档 §5）。
    pub fn forget_threshold(&self) -> f64 {
        match self {
            Self::Episodic => 0.02,
            Self::Trivia => 0.10,
        }
    }
}

/// 提取的三类事实（设计文档 §7.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    /// 关于用户 → profile。
    User,
    /// 关于自己 → agent_model。
    SelfFacts,
    /// 关于关系 → relationship。
    Relation,
}

impl FactKind {
    /// 解析提取器输出。
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "self" => Ok(Self::SelfFacts),
            "relation" => Ok(Self::Relation),
            other => Err(MemoryError::Invalid(format!("未知 fact kind: {other}"))),
        }
    }
}

/// 陈述动作三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactAction {
    /// 新事实。
    New,
    /// 延伸既有状态。
    Extend,
    /// 纠正：替换旧状态 + 旧陈述 superseded。
    Correct,
}

impl FactAction {
    /// 解析提取器输出。
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "new" => Ok(Self::New),
            "extend" => Ok(Self::Extend),
            "correct" => Ok(Self::Correct),
            other => Err(MemoryError::Invalid(format!("未知 fact action: {other}"))),
        }
    }
}

/// topics 行（recall 只查这层）。
#[derive(Debug, Clone, PartialEq)]
pub struct Topic {
    /// 不透明稳定 id（身份，绝不随改名变化）。
    pub topic_id: String,
    /// 当前规范名（标签，可随合并更新）。
    pub canonical_name: String,
    /// 历史表述集合（别名）。
    pub aliases: Vec<String>,
    /// 合并后的当前状态。
    pub state_summary: String,
    /// 创建时间（epoch 毫秒）。
    pub created_at: i64,
    /// 最近讨论时间（epoch 毫秒）。
    pub last_discussed_at: i64,
    /// 讨论次数。
    pub n_times: i64,
    /// 分层。
    pub tier: Tier,
    /// 当前权重（遗忘曲线）。
    pub weight: f64,
    /// 唤醒次数（recall 命中累积）。
    pub activation_count: i64,
    /// 保守新建标记（LLM 不确定）。
    pub uncertain: bool,
}

/// 关系卡（单行，常驻注入）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RelationCard {
    /// 关于用户。
    pub profile: String,
    /// 关于自己。
    pub agent_model: String,
    /// 我们之间。
    pub relationship: String,
    /// 最近更新（epoch 毫秒）。
    pub updated_at: i64,
}

/// 记忆专用 LLM 服务包装（app 装配；测试注入 mock）。
pub struct MemoryLlmProvider {
    /// 适配器实例。
    pub inner: Arc<dyn LlmAdapter>,
}

impl Service for MemoryLlmProvider {
    const NAME: &'static str = "memory-llm";
}

impl Service for MemoryStore {
    const NAME: &'static str = "memory";
}

/// 记忆存储（`ctx.provide` 为 `"memory"`）。
pub struct MemoryStore {
    conn: Mutex<Connection>,
    llm: Arc<dyn LlmAdapter>,
}

/// 打开时执行的 schema。
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
  event_id   INTEGER PRIMARY KEY AUTOINCREMENT,
  topic_id   TEXT NOT NULL,
  statement  TEXT NOT NULL,
  ts         INTEGER NOT NULL,
  superseded INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS topics (
  topic_id           TEXT PRIMARY KEY,
  canonical_name     TEXT NOT NULL,
  aliases            TEXT NOT NULL DEFAULT '[]',
  state_summary      TEXT NOT NULL,
  embedding          BLOB,
  created_at         INTEGER NOT NULL,
  last_discussed_at  INTEGER NOT NULL,
  n_times            INTEGER NOT NULL DEFAULT 1,
  tier               TEXT NOT NULL DEFAULT 'episodic',
  weight             REAL NOT NULL DEFAULT 1.0,
  activation_count   INTEGER NOT NULL DEFAULT 0,
  uncertain          INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS relation_card (
  id            INTEGER PRIMARY KEY CHECK (id = 1),
  profile       TEXT NOT NULL DEFAULT '',
  agent_model   TEXT NOT NULL DEFAULT '',
  relationship  TEXT NOT NULL DEFAULT '',
  updated_at    INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO relation_card (id, profile, agent_model, relationship, updated_at)
  VALUES (1, '', '', '', 0);
CREATE TABLE IF NOT EXISTS promises (
  promise_id  TEXT PRIMARY KEY,
  topic_id    TEXT,
  content     TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT 'open',
  created_at  INTEGER NOT NULL,
  due_at      INTEGER
);
CREATE TABLE IF NOT EXISTS self_history (
  action_id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind      TEXT NOT NULL,
  topic_id  TEXT,
  content   TEXT NOT NULL,
  ts        INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_topics_last ON topics(last_discussed_at);
CREATE INDEX IF NOT EXISTS idx_events_topic ON events(topic_id);
";

/// 当前 epoch 毫秒。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间早于 Unix epoch")
        .as_millis() as i64
}

/// 新主题 id（不透明、稳定）。
fn new_topic_id() -> String {
    format!("t-{:016x}", now_ms() as u64)
}

impl MemoryStore {
    /// 打开（或创建）存储。
    pub fn open(path: &str, llm: Arc<dyn LlmAdapter>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let store = Self {
            conn: Mutex::new(conn),
            llm,
        };
        store.apply_decay(now_ms())?;
        Ok(store)
    }

    /// 记忆专用 LLM 接缝。
    pub fn llm(&self) -> &Arc<dyn LlmAdapter> {
        &self.llm
    }

    /// 全部主题（按最近讨论倒序）。
    pub fn topics(&self) -> Result<Vec<Topic>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT topic_id, canonical_name, aliases, state_summary, created_at,
                    last_discussed_at, n_times, tier, weight, activation_count, uncertain
             FROM topics ORDER BY last_discussed_at DESC",
        )?;
        let rows = statement.query_map([], row_to_topic)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// 按 id 取主题。
    pub fn topic(&self, topic_id: &str) -> Result<Option<Topic>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT topic_id, canonical_name, aliases, state_summary, created_at,
                    last_discussed_at, n_times, tier, weight, activation_count, uncertain
             FROM topics WHERE topic_id = ?1",
        )?;
        let mut rows = statement.query_map([topic_id], row_to_topic)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// 关系卡。
    pub fn card(&self) -> Result<RelationCard> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT profile, agent_model, relationship, updated_at FROM relation_card WHERE id = 1",
            [],
            |row| {
                Ok(RelationCard {
                    profile: row.get(0)?,
                    agent_model: row.get(1)?,
                    relationship: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .map_err(Into::into)
    }

    /// 覆盖关系卡（apply_card_diff 的落库）。
    pub fn set_card(&self, card: &RelationCard) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE relation_card SET profile = ?1, agent_model = ?2, relationship = ?3, updated_at = ?4 WHERE id = 1",
            params![card.profile, card.agent_model, card.relationship, card.updated_at],
        )?;
        Ok(())
    }

    /// 事件表（append-only 真相源）；`topic_id` 过滤可选。
    /// 返回 `(event_id, statement, ts, superseded)`。
    pub fn events(&self, topic_id: Option<&str>) -> Result<Vec<(i64, String, i64, bool)>> {
        let conn = self.conn.lock().unwrap();
        let sql = match topic_id {
            Some(_) => {
                "SELECT event_id, statement, ts, superseded FROM events WHERE topic_id = ?1 ORDER BY event_id"
            }
            None => "SELECT event_id, statement, ts, superseded FROM events ORDER BY event_id",
        };
        let mut statement = conn.prepare(sql)?;
        let map =
            |row: &rusqlite::Row<'_>| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?));
        let rows = match topic_id {
            Some(id) => statement
                .query_map([id], map)?
                .collect::<std::result::Result<Vec<_>, _>>(),
            None => statement
                .query_map([], map)?
                .collect::<std::result::Result<Vec<_>, _>>(),
        };
        rows.map_err(Into::into)
    }

    /// 应用遗忘曲线：按 tier 衰减；低于阈值 → 删除主题行（events 保留，真相源不改）。
    pub fn apply_decay(&self, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let topics = {
            let mut statement =
                conn.prepare("SELECT topic_id, tier, weight, last_discussed_at FROM topics")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (topic_id, tier, weight, last) in topics {
            let tier = Tier::parse(&tier)?;
            let days = ((ts - last) as f64) / 86_400_000.0;
            let decayed = weight * (-tier.decay_rate() * days).exp();
            if decayed < tier.forget_threshold() {
                conn.execute("DELETE FROM topics WHERE topic_id = ?1", [&topic_id])?;
            } else {
                conn.execute(
                    "UPDATE topics SET weight = ?1 WHERE topic_id = ?2",
                    params![decayed, topic_id],
                )?;
            }
        }
        Ok(())
    }

    /// 词法阻塞 Stage 1：canonical 或别名精确命中（规范化比较）。
    pub fn topic_by_alias(&self, text: &str) -> Result<Option<Topic>> {
        let needle = normalize(text);
        for topic in self.topics()? {
            let mut names = vec![topic.canonical_name.clone()];
            names.extend(topic.aliases.clone());
            if names.iter().any(|name| normalize(name) == needle) {
                return Ok(Some(topic));
            }
        }
        Ok(None)
    }

    /// 词法候选：字符 bigram Jaccard 相似度 top-k（无 LLM，召回优先）。
    pub fn lexical_candidates(&self, text: &str, k: usize) -> Vec<(Topic, f64)> {
        let mut scored: Vec<(Topic, f64)> = self
            .topics()
            .unwrap_or_default()
            .into_iter()
            .map(|topic| {
                let mut best = bigram_similarity(text, &topic.canonical_name);
                for alias in &topic.aliases {
                    best = best.max(bigram_similarity(text, alias));
                }
                (topic, best)
            })
            .filter(|(_, score)| *score > 0.15)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored
    }

    /// 主题最近讨论时间（召回命中时刷新 + 激活）。
    pub fn activate(&self, topic_id: &str, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE topics SET activation_count = activation_count + 1,
                               last_discussed_at = ?1,
                               weight = MIN(1.0, weight * 1.1)
             WHERE topic_id = ?2",
            params![ts, topic_id],
        )?;
        Ok(())
    }

    /// demote：权重压低（可逆的减弱，删除交给遗忘曲线）。
    pub fn weaken(&self, topic_id: &str, factor: f64, reason: Option<&str>, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE topics SET weight = weight * ?1 WHERE topic_id = ?2",
            params![factor, topic_id],
        )?;
        conn.execute(
            "INSERT INTO self_history (kind, topic_id, content, ts) VALUES ('demote', ?1, ?2, ?3)",
            params![topic_id, reason.unwrap_or("agent 主动淡忘"), ts],
        )?;
        Ok(())
    }

    /// 落一条事件（append-only 真相源）。
    pub fn append_event(&self, topic_id: &str, statement: &str, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (topic_id, statement, ts) VALUES (?1, ?2, ?3)",
            params![topic_id, statement, ts],
        )?;
        Ok(())
    }

    /// correct：旧陈述标记 superseded。
    pub fn supersede_events(&self, topic_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE events SET superseded = 1 WHERE topic_id = ?1 AND superseded = 0",
            [topic_id],
        )?;
        Ok(())
    }

    /// 新建主题行（Create）。
    pub fn insert_topic(
        &self,
        canonical_name: &str,
        state_summary: &str,
        ts: i64,
        tier: Tier,
        uncertain: bool,
    ) -> Result<String> {
        let topic_id = new_topic_id();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO topics
               (topic_id, canonical_name, aliases, state_summary, created_at,
                last_discussed_at, n_times, tier, weight, activation_count, uncertain)
             VALUES (?1, ?2, '[]', ?3, ?4, ?4, 1, ?5, 1.0, 0, ?6)",
            params![
                topic_id,
                canonical_name,
                state_summary,
                ts,
                tier.as_str(),
                uncertain as i64
            ],
        )?;
        Ok(topic_id)
    }

    /// 合并更新主题行（Merge）。
    pub fn update_topic_merged(
        &self,
        topic_id: &str,
        state_summary: &str,
        alias: Option<&str>,
        ts: i64,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE topics SET state_summary = ?1, last_discussed_at = ?2, n_times = n_times + 1
             WHERE topic_id = ?3",
            params![state_summary, ts, topic_id],
        )?;
        if let Some(alias) = alias {
            let aliases_json: String = tx.query_row(
                "SELECT aliases FROM topics WHERE topic_id = ?1",
                [topic_id],
                |row| row.get(0),
            )?;
            let mut aliases: Vec<String> = serde_json::from_str(&aliases_json).unwrap_or_default();
            if !aliases.iter().any(|existing| existing == alias) {
                let canonical: String = tx.query_row(
                    "SELECT canonical_name FROM topics WHERE topic_id = ?1",
                    [topic_id],
                    |row| row.get(0),
                )?;
                if alias != canonical {
                    aliases.push(alias.to_string());
                    tx.execute(
                        "UPDATE topics SET aliases = ?1 WHERE topic_id = ?2",
                        params![serde_json::to_string(&aliases)?, topic_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// 行 → Topic。
fn row_to_topic(row: &rusqlite::Row<'_>) -> rusqlite::Result<Topic> {
    let aliases_json: String = row.get(2)?;
    Ok(Topic {
        topic_id: row.get(0)?,
        canonical_name: row.get(1)?,
        aliases: serde_json::from_str(&aliases_json).unwrap_or_default(),
        state_summary: row.get(3)?,
        created_at: row.get(4)?,
        last_discussed_at: row.get(5)?,
        n_times: row.get(6)?,
        tier: Tier::parse(&row.get::<_, String>(7)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        weight: row.get(8)?,
        activation_count: row.get(9)?,
        uncertain: row.get::<_, i64>(10)? != 0,
    })
}

/// 规范化：小写 + 去空白（词法阻塞用）。
pub(crate) fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 字符 bigram Jaccard 相似度（无 embedding 时的词法层）。
pub(crate) fn bigram_similarity(a: &str, b: &str) -> f64 {
    fn bigrams(text: &str) -> std::collections::HashSet<[char; 2]> {
        let chars: Vec<char> = text.chars().collect();
        chars.windows(2).map(|w| [w[0], w[1]]).collect()
    }
    let left = bigrams(&normalize(a));
    let right = bigrams(&normalize(b));
    if left.is_empty() || right.is_empty() {
        return if normalize(a) == normalize(b) {
            1.0
        } else {
            0.0
        };
    }
    let intersection = left.intersection(&right).count() as f64;
    let union = left.union(&right).count() as f64;
    intersection / union
}
