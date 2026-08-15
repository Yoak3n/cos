//! 取消/竞态验收（主干正确性命门）：流中取消闭合 step/turn 配对、工具间取消、
//! 调度前取消、keep_inbox 保留队列、重复取消幂等。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use cos_agent::{Agent, AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::Context;
use cos_llm::{ChunkDelta, LlmAdapter, LlmRequest, LlmStream, StreamChunk, ToolCall, UserMessage};
use cos_session::{AbortCause, SessionEventData, TurnEndReason};
use cos_test_support::{MockAdapter, MockReply};
use cos_tools::{Tool, ToolOutcome, ToolRegistry, ToolRun};
use futures::future::BoxFuture;

/// 慢流适配器：每个 chunk 之间 sleep，给取消留出确定性窗口。
struct SlowStreamAdapter {
    chunks: Vec<StreamChunk>,
    delay: Duration,
}

impl LlmAdapter for SlowStreamAdapter {
    fn id(&self) -> &str {
        "slow"
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        let chunks = self.chunks.clone();
        let delay = self.delay;
        // chunks/delay 走 unfold 状态（每次迭代交还所有权，闭包可复用且 'static）
        Box::pin(futures::stream::unfold(
            (0usize, chunks, delay),
            |(i, chunks, delay)| async move {
                if i >= chunks.len() {
                    return None;
                }
                tokio::time::sleep(delay).await;
                let chunk = chunks[i].clone();
                Some((Ok(chunk), (i + 1, chunks, delay)))
            },
        ))
    }
}

/// 慢工具（name = "slow"）：执行时 sleep，给取消留窗口。
struct SlowTool {
    calls: Arc<AtomicUsize>,
}

impl Tool for SlowTool {
    fn name(&self) -> &'static str {
        "slow"
    }

    fn description(&self) -> &'static str {
        "慢工具"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }

    fn execute(
        &self,
        _ctx: &Context,
        _run: &ToolRun,
    ) -> BoxFuture<'static, Result<ToolOutcome, cos_session::ToolError>> {
        let calls = self.calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(120)).await;
            Ok(ToolOutcome::ok("慢工具完成"))
        })
    }
}

async fn base_agent(
    session_id: &str,
    adapter: Arc<dyn LlmAdapter>,
) -> (cos_core::Context, Arc<dyn Agent>) {
    let root = Context::root();
    let registry = AgentRegistry::new(&root);
    root.provide(registry.clone()).unwrap();
    registry.set_factory(Arc::new(LoopFactory)).unwrap();
    let agent = registry
        .create(CreateAgentOptions {
            session_id: session_id.into(),
            options: AgentOptions {
                provider: Some("mock".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter,
        })
        .await
        .unwrap();
    (root, agent)
}

/// 会话事件的配对校验：StepStart↔StepEnd、TurnStart↔TurnEnd 必须平衡。
fn assert_pairing_balanced(agent: &dyn Agent) {
    let mut step_start = 0usize;
    let mut step_end = 0usize;
    let mut turn_start = 0usize;
    let mut turn_end = 0usize;
    for event in agent.session().events() {
        match &event.data {
            SessionEventData::StepStart { .. } => step_start += 1,
            SessionEventData::StepEnd { .. } => step_end += 1,
            SessionEventData::TurnStart { .. } => turn_start += 1,
            SessionEventData::TurnEnd { .. } => turn_end += 1,
            _ => {}
        }
    }
    assert_eq!(step_start, step_end, "step/start 与 step/end 必须配对");
    assert_eq!(turn_start, turn_end, "turn/start 与 turn/end 必须配对");
}

fn last_turn_reason(agent: &dyn Agent) -> Option<TurnEndReason> {
    agent
        .session()
        .events()
        .iter()
        .rev()
        .find_map(|event| match &event.data {
            SessionEventData::TurnEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
}

fn count_events(agent: &dyn Agent, data: &dyn Fn(&SessionEventData) -> bool) -> usize {
    agent
        .session()
        .events()
        .iter()
        .filter(|event| data(&event.data))
        .count()
}

/// 流中取消：chunk 逐步流出时 cancel → 配对闭合、turn 以 Aborted(User) 收束、
/// 已产出的 chunk 已入账（不是空转取消）。
#[tokio::test]
async fn cancel_during_stream_closes_pairing_with_aborted_reason() {
    let chunks: Vec<StreamChunk> = (0..20)
        .map(|i| StreamChunk::text(format!("块{i}")))
        .collect();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(SlowStreamAdapter {
        chunks,
        delay: Duration::from_millis(15),
    });
    let (_root, agent) = base_agent("sess-cancel-stream", adapter).await;

    agent.followup(UserMessage::new("慢慢说"));
    // 等至少 3 个 chunk 入账后再取消（确保取消发生在流中）
    for _ in 0..200 {
        if count_events(agent.as_ref(), &|data| {
            matches!(data, SessionEventData::AssistantChunk { .. })
        }) >= 3
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    agent.cancel(AbortCause::User, false);
    agent.when_idle().await;

    assert_pairing_balanced(agent.as_ref());
    assert_eq!(
        last_turn_reason(agent.as_ref()),
        Some(TurnEndReason::Aborted {
            cause: AbortCause::User
        })
    );
    assert!(
        count_events(agent.as_ref(), &|data| {
            matches!(data, SessionEventData::AssistantChunk { .. })
        }) >= 3,
        "取消应在流中（chunk 已入账），而非空转"
    );
}

/// 调度前取消：followup 后同步 cancel（驱动器尚未运行）→ turn 以 Aborted 收束、
/// 不产生 step（check_abort 在 turn 入口即失败）。
#[tokio::test]
async fn cancel_before_turn_scheduled_aborts_cleanly() {
    let adapter: Arc<dyn LlmAdapter> =
        Arc::new(MockAdapter::new("mock", vec![MockReply::text("不该出现")]));
    let (_root, agent) = base_agent("sess-cancel-early", adapter).await;

    // current_thread runtime：followup 只把驱动器 spawn 出去，尚未执行；
    // 同步 cancel 必然先于 turn 的任何一步
    agent.followup(UserMessage::new("别跑"));
    agent.cancel(AbortCause::Parent, false);
    agent.when_idle().await;

    assert_pairing_balanced(agent.as_ref());
    assert_eq!(
        last_turn_reason(agent.as_ref()),
        Some(TurnEndReason::Aborted {
            cause: AbortCause::Parent
        })
    );
    assert_eq!(
        count_events(agent.as_ref(), &|data| matches!(
            data,
            SessionEventData::StepStart { .. }
        )),
        0,
        "取消先于调度 → 不应有 step"
    );
    assert_eq!(
        count_events(agent.as_ref(), &|data| {
            matches!(data, SessionEventData::AssistantMessage { .. })
        }),
        0,
        "不应有模型产出"
    );
}

/// 工具间取消：两个工具调用，取消发生在第一个工具执行中 → 只执行一个、
/// 配对闭合、turn 以 Aborted 收束。
#[tokio::test]
async fn cancel_during_tool_execution_stops_before_next_tool() {
    let root = Context::root();
    root.provide(ToolRegistry::new(&root)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    root.get::<ToolRegistry>()
        .unwrap()
        .register(Arc::new(SlowTool {
            calls: calls.clone(),
        }))
        .unwrap();
    let registry = AgentRegistry::new(&root);
    root.provide(registry.clone()).unwrap();
    registry.set_factory(Arc::new(LoopFactory)).unwrap();
    let tool_use = |call_id: &str| StreamChunk {
        delta: ChunkDelta::ToolUse {
            call: ToolCall {
                call_id: call_id.into(),
                name: "slow".into(),
                arguments: "{}".into(),
            },
        },
        usage: None,
    };
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::new(vec![tool_use("c1"), tool_use("c2")])],
    ));
    let agent = registry
        .create(CreateAgentOptions {
            session_id: "sess-cancel-tool".into(),
            options: AgentOptions {
                provider: Some("mock".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter,
        })
        .await
        .unwrap();

    agent.followup(UserMessage::new("执行工具"));
    // 等第一个工具开始执行（ToolResult 尚未出现，ToolCall 已入账）→ 取消
    for _ in 0..200 {
        if calls.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "第一个工具应已开始");
    agent.cancel(AbortCause::User, false);
    agent.when_idle().await;

    assert_pairing_balanced(agent.as_ref());
    assert_eq!(
        last_turn_reason(agent.as_ref()),
        Some(TurnEndReason::Aborted {
            cause: AbortCause::User
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "第二个工具不得执行");
    assert_eq!(
        count_events(agent.as_ref(), &|data| matches!(
            data,
            SessionEventData::ToolResult { .. }
        )),
        1,
        "只有第一个工具的结果入账"
    );
}

/// keep_inbox：取消保留队列 → 后续 followup 唤醒后，保留的任务与新任务都得到处理。
#[tokio::test]
async fn cancel_keep_inbox_preserves_queued_and_next_wake_resumes() {
    // inbox 一条消息一个 turn：任务A（保留）+ 任务B（新）各占一轮
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::text("继续"), MockReply::text("继续")],
    ));
    let (_root, agent) = base_agent("sess-cancel-keep", adapter).await;

    agent.followup(UserMessage::new("任务A"));
    agent.cancel(AbortCause::User, true); // 保留队列
    agent.when_idle().await;
    assert_eq!(
        last_turn_reason(agent.as_ref()),
        Some(TurnEndReason::Aborted {
            cause: AbortCause::User
        })
    );

    // 唤醒后：保留的任务A + 新任务B 都被处理（各自一轮，全部 Completed）
    agent.followup(UserMessage::new("任务B"));
    agent.when_idle().await;
    let reasons: Vec<TurnEndReason> = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::TurnEnd { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        vec![
            TurnEndReason::Aborted {
                cause: AbortCause::User
            },
            TurnEndReason::Completed,
            TurnEndReason::Completed,
        ],
        "取消轮 Aborted，随后保留+新任务各 Completed"
    );
    let texts: Vec<String> = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::UserMessage(message) => Some(message.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["任务A", "任务B"],
        "keep_inbox 应保留队列，唤醒后新旧都处理: {texts:?}"
    );
}

/// 重复取消幂等：两次 cancel（后发原因覆盖）→ 单次收束、配对闭合、无 panic。
#[tokio::test]
async fn double_cancel_is_idempotent_and_clean() {
    let adapter: Arc<dyn LlmAdapter> =
        Arc::new(MockAdapter::new("mock", vec![MockReply::text("x")]));
    let (_root, agent) = base_agent("sess-cancel-twice", adapter).await;

    agent.followup(UserMessage::new("任务"));
    agent.cancel(AbortCause::User, false);
    agent.cancel(
        AbortCause::Hook {
            reason: "双取消".into(),
        },
        false,
    );
    agent.when_idle().await;

    assert_pairing_balanced(agent.as_ref());
    assert_eq!(
        last_turn_reason(agent.as_ref()),
        Some(TurnEndReason::Aborted {
            cause: AbortCause::Hook {
                reason: "双取消".into()
            }
        }),
        "后发的取消原因生效"
    );
}
