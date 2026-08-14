//! LLM 机制：提取（三类事实 + 关系卡注记）、编号仲裁、状态合并、卡段落合并。
//! 契约均为 JSON 输出；调用经 [`cos_llm::LlmAdapter`] 接缝（测试用脚本化 mock）。

use std::sync::Arc;

use cos_llm::{ChunkDelta, LlmAdapter, LlmRequest, Message, UserMessage};
use futures::StreamExt;
use serde::Deserialize;

use crate::error::{MemoryError, Result};
use crate::pipeline::TurnPair;
use crate::store::{FactAction, FactKind, RelationCard};

/// 一条提取出的字面事实。
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// 三类事实。
    pub kind: FactKind,
    /// 动作三态。
    pub action: FactAction,
    /// 主题表述（参与词法阻塞）。
    pub topic_text: String,
    /// 陈述原文。
    pub statement: String,
}

/// 提取器输出：事实 + 关系卡各段注记。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Extraction {
    /// 字面事实。
    pub facts: Vec<Fact>,
    /// profile 段注记。
    pub profile_notes: Vec<String>,
    /// agent_model 段注记。
    pub agent_model_notes: Vec<String>,
    /// relationship 段注记。
    pub relationship_notes: Vec<String>,
}

/// 提取契约输出（LLM JSON）。
#[derive(Deserialize)]
struct RawFact {
    kind: String,
    action: String,
    #[serde(rename = "topic_text", default)]
    topic_text: String,
    statement: String,
}

/// 提取契约输出（LLM JSON）。
#[derive(Deserialize)]
struct RawExtraction {
    #[serde(default)]
    facts: Vec<RawFact>,
    #[serde(rename = "card_notes", default)]
    card_notes: RawCardNotes,
}

/// 关系卡注记（LLM JSON）。
#[derive(Deserialize, Default)]
struct RawCardNotes {
    #[serde(default)]
    profile: Vec<String>,
    #[serde(default)]
    agent_model: Vec<String>,
    #[serde(default)]
    relationship: Vec<String>,
}

/// 仲裁契约输出（LLM JSON）。
#[derive(Deserialize)]
struct RawArbitration {
    /// 归并目标 topic_id；`"none"` = 保守新建。
    merge: String,
}

/// 合并契约输出（LLM JSON）。
#[derive(Deserialize)]
struct RawMerge {
    state_summary: String,
}

/// 卡合并契约输出（LLM JSON）。
#[derive(Deserialize)]
struct RawCardMerge {
    text: String,
}

/// 提取系统提示（窄而弱：只抄字面，不推断）。
const EXTRACT_PROMPT: &str = r#"你是记忆提取器，只做字面抄录，不做任何推断。
输入：一段对话 turn（用户说了什么 + 助手说了什么）。
输出（严格 JSON）：
{
  "facts": [
    {"kind": "user|self|relation", "action": "new|extend|correct",
     "topic_text": "主题的短表述（用于归并）", "statement": "陈述原文"}
  ],
  "card_notes": {"profile": ["关于用户的事实行"], "agent_model": ["关于助手自己的事实行"], "relationship": ["关于两人关系的事实行"]}
}
规则：
- kind: user=关于用户, self=关于助手自己, relation=两人关系
- action: new=全新事实, extend=对既有事实的延伸, correct=用户纠正旧事实（"不/其实/纠正"）
- 只抄原文明确说出的，猜测、情绪归因、模式总结一律不写
- 无事实时输出空数组；card_notes 可为空对象
- 只输出 JSON，不要解释。"#;

/// 仲裁系统提示（保守偏置：不确定输出 none）。
const ARBITRATE_PROMPT: &str = r#"你是记忆编号消解仲裁员。
输入：一句新陈述的 topic_text，以及若干候选主题（id + 规范名 + 状态摘要）。
判断这句新陈述说的是不是某一个候选主题的同一件事。
输出（严格 JSON）：{"merge": "<topic_id>"} 归并，或 {"merge": "none"} 新建。
规则：
- "语义相似"不等于"同一件事"：吉他和尤克里里必须分开；"吉他练习"和"吉他"必须归并
- 只有非常确定才归并；不确定一律输出 none（保守新建，宁可晚合并不可错合并）
- 只输出 JSON，不要解释。"#;

/// 状态合并系统提示。
const MERGE_PROMPT: &str = r#"你是记忆状态合并器。
输入：一个主题的当前状态摘要 + 一条新陈述 + 动作（extend=延伸, correct=纠正）。
输出（严格 JSON）：{"state_summary": "合并后的状态"}。
规则：
- correct：以新陈述为准替换旧状态
- extend：把新信息并入，保持单行紧凑（不超过 120 字）
- 只输出 JSON，不要解释。"#;

/// 关系卡段落合并系统提示（LLM 维护、自带裁剪）。
const CARD_MERGE_PROMPT: &str = r#"你是关系卡维护器。
输入：关系卡某段的当前文本 + 新事实行列表。
输出（严格 JSON）：{"text": "合并后的段落"}。
规则：
- 并入新事实，去掉被新事实取代的旧表述（correct 语义）
- 保持紧凑（不超过 500 字），身份类事实永远保留
- 只输出 JSON，不要解释。"#;

/// 上下文压缩系统提示（滚动摘要：旧摘要 + 新增对话 → 新摘要）。
const COMPRESS_PROMPT: &str = r#"你是对话压缩器。
输入：一段既有摘要（可为空）+ 一段新的对话记录（用户/助手多轮）。
输出（严格 JSON）：{"summary": "合并后的要点摘要"}。
规则：
- 保留：身份事实、承诺、偏好、正在进行的事、纠错结论
- 丢弃：寒暄、重复表述、已完成的琐碎事务
- 要点式紧凑列表，不超过 300 字
- 只输出 JSON，不要解释。"#;

/// digest 推断系统提示（慢路径：统计 + 转录头 → 三段注记，高门槛保守）。
const DIGEST_PROMPT: &str = r#"你是记忆慢消化器（推断层）。
输入：当前关系卡三段 + 统计（事件数/主题数/高频主题/时间跨度）+ 会话转录（截断头部）。
输出（严格 JSON）：
{"profile_notes": ["关于用户的行"], "agent_model_notes": ["关于助手自己的行"], "relationship_notes": ["关于两人关系的行"]}
规则：
- 只写有统计或转录直接支撑的高置信度结论；不确定一律不写（空数组合法）
- 模式类（"经常/总是/报喜不报忧"）需多轮证据；单次提及不算
- 认知缺口：对照模板坐标系（名字/生日/工作/家人/偏好/作息/关系史），
  把"还不知道但值得主动问"的维度写进 agent_model_notes
- 每段不超过 5 条，只输出 JSON，不要解释。"#;

/// 收集流式响应文本。
pub(crate) async fn collect_text(llm: &dyn LlmAdapter, request: LlmRequest) -> Result<String> {
    let mut text = String::new();
    let mut stream = llm.stream(&request);
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                if let ChunkDelta::Text { text: delta } = chunk.delta {
                    text.push_str(&delta);
                }
            }
            Err(error) => return Err(MemoryError::Llm(error)),
        }
    }
    Ok(text)
}

/// 剥掉 ```json 围栏再解析。
pub(crate) fn parse_json_output<T: for<'de> Deserialize<'de>>(
    text: &str,
    context: &str,
) -> Result<T> {
    let trimmed = text.trim();
    let json = trimmed
        .strip_prefix("```json")
        .and_then(|rest| rest.strip_suffix("```"))
        .or_else(|| {
            trimmed
                .strip_prefix("```")
                .and_then(|rest| rest.strip_suffix("```"))
        })
        .unwrap_or(trimmed);
    serde_json::from_str(json.trim())
        .map_err(|error| MemoryError::Invalid(format!("{context} 输出不是合法 JSON: {error}")))
}

/// 提取：关系卡 + turn pair → 字面事实 + 卡注记。
pub(crate) async fn extract(
    llm: &Arc<dyn LlmAdapter>,
    card: &RelationCard,
    turn: &TurnPair,
) -> Result<Extraction> {
    let user_payload = format!(
        "关系卡（当前）：\nprofile: {}\nagent_model: {}\nrelationship: {}\n\n本轮对话：\n用户: {}\n助手: {}",
        card.profile, card.agent_model, card.relationship, turn.user, turn.assistant
    );
    let request = LlmRequest {
        system: Some(EXTRACT_PROMPT.into()),
        messages: vec![Message::User(UserMessage::new(user_payload))],
        tools: Vec::new(),
    };
    let text = collect_text(llm.as_ref(), request).await?;
    let raw: RawExtraction = parse_json_output(&text, "提取")?;

    let mut facts = Vec::new();
    for fact in raw.facts {
        facts.push(Fact {
            kind: FactKind::parse(&fact.kind)?,
            action: FactAction::parse(&fact.action)?,
            topic_text: if fact.topic_text.is_empty() {
                fact.statement.clone()
            } else {
                fact.topic_text
            },
            statement: fact.statement,
        });
    }
    Ok(Extraction {
        facts,
        profile_notes: raw.card_notes.profile,
        agent_model_notes: raw.card_notes.agent_model,
        relationship_notes: raw.card_notes.relationship,
    })
}

/// 仲裁：候选主题里找同一件事；`None` = 保守新建。
pub(crate) async fn arbitrate(
    llm: &Arc<dyn LlmAdapter>,
    topic_text: &str,
    candidates: &[(String, String, String)], // (id, canonical, state)
) -> Result<Option<String>> {
    let mut listing = String::new();
    for (id, canonical, state) in candidates {
        listing.push_str(&format!("候选 id={id} 规范名={canonical} 状态={state}\n"));
    }
    let user_payload = format!("新陈述的 topic_text: {topic_text}\n\n{listing}");
    let request = LlmRequest {
        system: Some(ARBITRATE_PROMPT.into()),
        messages: vec![Message::User(UserMessage::new(user_payload))],
        tools: Vec::new(),
    };
    let text = collect_text(llm.as_ref(), request).await?;
    let raw: RawArbitration = parse_json_output(&text, "仲裁")?;
    Ok((raw.merge != "none" && !raw.merge.is_empty()).then_some(raw.merge))
}

/// 状态合并。
pub(crate) async fn merge_state(
    llm: &Arc<dyn LlmAdapter>,
    old_state: &str,
    statement: &str,
    action: FactAction,
) -> Result<String> {
    let action_text = match action {
        FactAction::New => "new",
        FactAction::Extend => "extend",
        FactAction::Correct => "correct",
    };
    let user_payload = format!("当前状态: {old_state}\n新陈述: {statement}\n动作: {action_text}");
    let request = LlmRequest {
        system: Some(MERGE_PROMPT.into()),
        messages: vec![Message::User(UserMessage::new(user_payload))],
        tools: Vec::new(),
    };
    let text = collect_text(llm.as_ref(), request).await?;
    let raw: RawMerge = parse_json_output(&text, "合并")?;
    Ok(raw.state_summary)
}

/// 关系卡段落合并（LLM 维护、自带裁剪）。
pub(crate) async fn merge_card_section(
    llm: &Arc<dyn LlmAdapter>,
    current: &str,
    notes: &[String],
) -> Result<String> {
    if notes.is_empty() {
        return Ok(current.to_string());
    }
    let user_payload = format!("当前段落:\n{current}\n\n新事实行:\n{}", notes.join("\n"));
    let request = LlmRequest {
        system: Some(CARD_MERGE_PROMPT.into()),
        messages: vec![Message::User(UserMessage::new(user_payload))],
        tools: Vec::new(),
    };
    let text = collect_text(llm.as_ref(), request).await?;
    let raw: RawCardMerge = parse_json_output(&text, "卡合并")?;
    Ok(raw.text)
}

/// 滚动压缩：旧摘要 + 新增对话 → 新摘要（上下文自动压缩的 LLM 步）。
pub(crate) async fn compress_dialog(
    llm: &Arc<dyn LlmAdapter>,
    old_summary: &str,
    dialog: &str,
) -> Result<String> {
    let user_payload = format!("既有摘要:\n{old_summary}\n\n新增对话:\n{dialog}");
    let request = LlmRequest {
        system: Some(COMPRESS_PROMPT.into()),
        messages: vec![Message::User(UserMessage::new(user_payload))],
        tools: Vec::new(),
    };
    let text = collect_text(llm.as_ref(), request).await?;
    #[derive(Deserialize)]
    struct RawSummary {
        summary: String,
    }
    let raw: RawSummary = parse_json_output(&text, "压缩")?;
    Ok(raw.summary)
}

/// digest 输出（三段注记，复用卡合并落库）。
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct DigestNotes {
    /// profile 段注记。
    pub profile: Vec<String>,
    /// agent_model 段注记。
    pub agent_model: Vec<String>,
    /// relationship 段注记。
    pub relationship: Vec<String>,
}

/// digest 推断：统计 + 转录头 → 三段注记（高门槛保守，不确定不写）。
pub(crate) async fn digest_notes(
    llm: &Arc<dyn LlmAdapter>,
    card: &RelationCard,
    stats: &str,
    transcript: &str,
) -> Result<DigestNotes> {
    let user_payload = format!(
        "关系卡（当前）：\nprofile: {}\nagent_model: {}\nrelationship: {}\n\n统计:\n{stats}\n\n会话转录（头）:\n{transcript}",
        card.profile, card.agent_model, card.relationship
    );
    let request = LlmRequest {
        system: Some(DIGEST_PROMPT.into()),
        messages: vec![Message::User(UserMessage::new(user_payload))],
        tools: Vec::new(),
    };
    let text = collect_text(llm.as_ref(), request).await?;
    #[derive(Deserialize)]
    struct RawDigest {
        #[serde(default)]
        profile_notes: Vec<String>,
        #[serde(default)]
        agent_model_notes: Vec<String>,
        #[serde(default)]
        relationship_notes: Vec<String>,
    }
    let raw: RawDigest = parse_json_output(&text, "digest")?;
    Ok(DigestNotes {
        profile: raw.profile_notes,
        agent_model: raw.agent_model_notes,
        relationship: raw.relationship_notes,
    })
}
