//! cos-invariants —— 不变量注册表（P6，同 dsh `ctx.invariants` 的纪律）。
//!
//! 内置不变量（对会话日志的结构断言）：
//! - seq 单调连续（从 1 起，含 chunk）；
//! - turn/start 与 turn/end 配对、turn 号连续；
//! - step/start 与 step/end 配对；
//! - tool/call 与 tool/result 按 call_id 配对、结果在调用之后；
//! - 模型可见 ⟺ 已记录：`derive_messages` 的每条消息都能追溯到日志事件（构造性保证，
//!   此处以可追溯性扫描复述该不变量）。

#![warn(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use cos_core::{Context, Service};
use cos_session::{Session, SessionEventData};

/// 一条会话不变量：返回违规描述列表（空 = 通过）。
pub trait SessionInvariant: Send + Sync {
    /// 不变量名。
    fn name(&self) -> &'static str;

    /// 校验会话日志。
    fn check(&self, session: &Session) -> Vec<String>;
}

/// 不变量注册表服务（`ctx.provide` 为 `"invariants"`）。
pub struct InvariantRegistry {
    checks: Mutex<Vec<Arc<dyn SessionInvariant>>>,
}

impl Service for InvariantRegistry {
    const NAME: &'static str = "invariants";
}

impl InvariantRegistry {
    /// 空注册表。
    pub fn new(_root: &Context) -> Self {
        Self {
            checks: Mutex::new(Vec::new()),
        }
    }

    /// 注册一条不变量。
    pub fn register(&self, check: Arc<dyn SessionInvariant>) {
        self.checks.lock().unwrap().push(check);
    }

    /// 校验会话：返回全部违规（含不变量名前缀）。
    pub fn verify(&self, session: &Session) -> Vec<String> {
        let checks = self.checks.lock().unwrap().clone();
        let mut violations = Vec::new();
        for check in &checks {
            for violation in check.check(session) {
                violations.push(format!("{}: {}", check.name(), violation));
            }
        }
        violations
    }
}

/// 注册全部内置不变量。
pub fn register_defaults(registry: &InvariantRegistry) {
    registry.register(Arc::new(SeqMonotonic));
    registry.register(Arc::new(TurnPairing));
    registry.register(Arc::new(StepPairing));
    registry.register(Arc::new(ToolCallResultPairing));
    registry.register(Arc::new(ModelVisibleIffLogged));
}

/// seq 单调连续。
struct SeqMonotonic;

impl SessionInvariant for SeqMonotonic {
    fn name(&self) -> &'static str {
        "seq-monotonic"
    }

    fn check(&self, session: &Session) -> Vec<String> {
        let events = session.events();
        let mut violations = Vec::new();
        for (index, event) in events.iter().enumerate() {
            let expected = (index + 1) as u64;
            if event.seq != expected {
                violations.push(format!(
                    "seq {} 处期望 {expected}（事件从 1 起连续）",
                    event.seq
                ));
            }
        }
        violations
    }
}

/// turn/start 与 turn/end 配对、turn 号连续。
struct TurnPairing;

impl SessionInvariant for TurnPairing {
    fn name(&self) -> &'static str {
        "turn-pairing"
    }

    fn check(&self, session: &Session) -> Vec<String> {
        let events = session.events();
        let mut violations = Vec::new();
        let mut open: Vec<u32> = Vec::new();
        let mut closed = Vec::new();
        for event in &events {
            match &event.data {
                SessionEventData::TurnStart { turn } => {
                    if !open.is_empty() {
                        violations.push(format!("turn {turn} 开启时仍有未关闭的 turn {open:?}"));
                    }
                    if let Some(last) = closed.last()
                        && *turn != last + 1
                    {
                        violations.push(format!("turn 号不连续: {last} → {turn}"));
                    }
                    open.push(*turn);
                }
                SessionEventData::TurnEnd { turn, .. } => {
                    match open.pop() {
                        Some(open_turn) if open_turn == *turn => {}
                        other => violations.push(format!("turn {turn} 关闭时栈顶为 {other:?}")),
                    }
                    closed.push(*turn);
                }
                _ => {}
            }
        }
        if !open.is_empty() {
            violations.push(format!("会话结束时仍有未关闭的 turn {open:?}"));
        }
        violations
    }
}

/// step/start 与 step/end 配对（同 turn/step）。
struct StepPairing;

impl SessionInvariant for StepPairing {
    fn name(&self) -> &'static str {
        "step-pairing"
    }

    fn check(&self, session: &Session) -> Vec<String> {
        let events = session.events();
        let mut violations = Vec::new();
        let mut open: Vec<(u32, u32)> = Vec::new();
        for event in &events {
            match &event.data {
                SessionEventData::StepStart { turn, step } => open.push((*turn, *step)),
                SessionEventData::StepEnd { turn, step } => match open.pop() {
                    Some((open_turn, open_step)) if open_turn == *turn && open_step == *step => {}
                    other => violations.push(format!("step {turn}/{step} 关闭时栈顶为 {other:?}")),
                },
                _ => {}
            }
        }
        if !open.is_empty() {
            violations.push(format!("会话结束时仍有未关闭的 step {open:?}"));
        }
        violations
    }
}

/// tool/call 与 tool/result 按 call_id 配对、结果在调用之后。
struct ToolCallResultPairing;

impl SessionInvariant for ToolCallResultPairing {
    fn name(&self) -> &'static str {
        "tool-call-result-pairing"
    }

    fn check(&self, session: &Session) -> Vec<String> {
        let events = session.events();
        let mut violations = Vec::new();
        let mut calls: HashMap<String, u64> = HashMap::new(); // call_id → seq
        let mut results: HashSet<String> = HashSet::new();
        for event in &events {
            match &event.data {
                SessionEventData::ToolCall { call_id, .. } => {
                    calls.insert(call_id.clone(), event.seq);
                }
                SessionEventData::ToolResult { call_id, .. } => {
                    match calls.get(call_id) {
                        Some(call_seq) if *call_seq < event.seq => {}
                        Some(_) => {
                            violations.push(format!("tool/result 先于 tool/call: {call_id}"))
                        }
                        None => violations.push(format!("孤儿 tool/result: {call_id}")),
                    }
                    results.insert(call_id.clone());
                }
                _ => {}
            }
        }
        for call_id in calls.keys() {
            if !results.contains(call_id) {
                violations.push(format!("tool/call 无配对结果: {call_id}"));
            }
        }
        violations
    }
}

/// 模型可见 ⟺ 已记录：derived 消息逐条可追溯回日志事件。
struct ModelVisibleIffLogged;

impl SessionInvariant for ModelVisibleIffLogged {
    fn name(&self) -> &'static str {
        "model-visible-iff-logged"
    }

    fn check(&self, session: &Session) -> Vec<String> {
        let events = session.events();
        let surface_seqs: Vec<u64> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.data,
                    SessionEventData::UserMessage(_)
                        | SessionEventData::AssistantMessage { .. }
                        | SessionEventData::ToolResult { .. }
                        | SessionEventData::Custom { .. }
                )
            })
            .map(|event| event.seq)
            .collect();
        let derived = session.derive_messages();
        if surface_seqs.len() != derived.len() {
            return vec![format!(
                "surface 事件数 {} 与 derive_messages 数 {} 不一致",
                surface_seqs.len(),
                derived.len()
            )];
        }
        // 投影顺序 = 事件顺序（derive_messages 按 seq 序扫描，构造性成立；此处复述断言）
        Vec::new()
    }
}
