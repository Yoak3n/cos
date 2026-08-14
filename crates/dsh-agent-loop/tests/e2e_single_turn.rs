//! P4 验收：端到端单轮对话 —— mock LLM 回复 assistant 消息，
//! 会话日志含 turn/start、user/message、assistant/chunk*、assistant/message、turn/end；
//! derive_messages 重放与原始一致（快照测试）。另覆盖 steering、reject、cancel、
//! 维护闩、initiator 因果链与注册表语义。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dsh_agent::{
    Agent, AgentError, AgentOptions, AgentRegistry, AgentStatus, CreateAgentOptions,
    PreStepDecision, PreStepPayload, current_initiator,
};
use dsh_agent_loop::LoopFactory;
use dsh_core::Context;
use dsh_llm::{
    AssistantMessage, ContentBlock, LlmAdapter, LlmRequest, LlmStream, Message, StreamChunk,
    UserMessage,
};
use dsh_llm_mock::{MockAdapter, MockReply};
use dsh_session::{AbortCause, SessionEventData, TurnEndReason};

fn setup() -> (Context, AgentRegistry) {
    let root = Context::root();
    let registry = AgentRegistry::new(&root);
    root.provide(registry.clone()).unwrap();
    registry.set_factory(Arc::new(LoopFactory)).unwrap();
    (root, registry)
}

fn mock_adapter() -> Arc<dyn LlmAdapter> {
    Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::new(vec![
            StreamChunk::text("你"),
            StreamChunk::text("好"),
        ])],
    ))
}

fn make_options(session_id: &str, adapter: Arc<dyn LlmAdapter>) -> CreateAgentOptions {
    CreateAgentOptions {
        session_id: session_id.to_string(),
        options: AgentOptions {
            provider: Some("mock".into()),
            model: Some("mock-1".into()),
            max_tokens: None,
        },
        adapter,
    }
}

fn type_names(agent: &dyn Agent) -> Vec<&'static str> {
    agent
        .session()
        .events()
        .iter()
        .map(|event| event.data.type_name())
        .collect()
}

/// 逐字符慢速流适配器（取消/维护测试用）。
struct SlowAdapter {
    delay_ms: u64,
    text: String,
}

impl LlmAdapter for SlowAdapter {
    fn id(&self) -> &str {
        "slow"
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        let delay = self.delay_ms;
        let text = self.text.clone();
        Box::pin(futures::stream::unfold(
            Some(text),
            move |state| async move {
                let remaining = state?;
                let mut chars = remaining.chars();
                let first = chars.next().expect("非空状态必有首字符").to_string();
                let rest: String = chars.collect();
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let chunk = Ok::<_, dsh_llm::LlmError>(StreamChunk::text(first));
                Some((chunk, if rest.is_empty() { None } else { Some(rest) }))
            },
        ))
    }
}

// —— 验收主测试：端到端单轮 + 全事件序列快照 ——

#[tokio::test]
async fn single_turn_end_to_end_snapshot() {
    let (_root, registry) = setup();
    let agent = registry
        .create(make_options("sess-e2e", mock_adapter()))
        .await
        .unwrap();

    let status_log = Arc::new(Mutex::new(Vec::new()));
    let log = status_log.clone();
    agent
        .ctx()
        .on("agent/status", move |payload| {
            let p = payload
                .downcast_ref::<dsh_agent::AgentStatusPayload>()
                .expect("状态载荷");
            log.lock().unwrap().push(p.status);
        })
        .unwrap();

    agent.followup(UserMessage::new("你好"));
    agent.when_idle().await;

    // 事件类型序列快照（先写日志的产物：chunk 在 assistant/message 之前）
    assert_eq!(
        type_names(&*agent),
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "request/header",
            "assistant/chunk",
            "assistant/chunk",
            "assistant/message",
            "step/end",
            "turn/end",
        ]
    );

    // turn/step 边界正确
    let events = agent.session().events();
    assert!(matches!(
        events[0].data,
        SessionEventData::TurnStart { turn: 1 }
    ));
    assert!(matches!(
        events[events.len() - 1].data,
        SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed
        }
    ));

    // derive_messages 重放与原始一致（快照）
    let messages = agent.session().derive_messages();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0], Message::User(UserMessage::new("你好")));
    assert_eq!(
        messages[1],
        Message::Assistant(AssistantMessage::new(vec![ContentBlock::Text {
            text: "你好".into()
        }]))
    );

    // 收敛：idle + 状态事件序列
    assert_eq!(agent.status(), AgentStatus::Idle);
    assert_eq!(
        *status_log.lock().unwrap(),
        vec![AgentStatus::Running, AgentStatus::Idle]
    );
}

// —— inject + followup 同一步批处理（dsh claim 语义：next-step 全取 + next-turn 一条）——

#[tokio::test]
async fn inject_batches_with_followup_in_one_step() {
    let (_root, registry) = setup();
    let agent = registry
        .create(make_options("sess-batch", mock_adapter()))
        .await
        .unwrap();

    agent.inject(UserMessage::new("注入上下文"));
    agent.followup(UserMessage::new("用户问题"));
    agent.when_idle().await;

    let user_messages: Vec<String> = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::UserMessage(message) => Some(message.content.clone()),
            _ => None,
        })
        .collect();
    // claim：next-step 全取（注入在前）+ next-turn 恰好一条
    assert_eq!(
        user_messages,
        vec!["注入上下文".to_string(), "用户问题".to_string()]
    );
    assert_eq!(
        type_names(&*agent)
            .iter()
            .filter(|name| **name == "step/start")
            .count(),
        1
    );
}

// —— steering：turn 内第二 step（pre-step 门控保证确定性）——

#[tokio::test]
async fn steering_adds_second_step_in_same_turn() {
    let (_root, registry) = setup();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(MockAdapter::new(
        "mock",
        vec![MockReply::text("一"), MockReply::text("二")],
    ));
    let agent = registry
        .create(make_options("sess-steer", adapter))
        .await
        .unwrap();

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(Mutex::new(Some(release_rx)));
    agent
        .ctx()
        .on_waterfall::<PreStepPayload, PreStepDecision>("agent/pre-step", move |d| {
            let step = d.value().step;
            let entered = entered_tx.lock().unwrap().take();
            let release = release_rx.lock().unwrap().take();
            Box::pin(async move {
                if step == 1 {
                    if let Some(tx) = entered {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release {
                        let _ = rx.await;
                    }
                }
                d.next().await
            })
        })
        .unwrap();

    agent.followup(UserMessage::new("开始"));
    entered_rx.await.unwrap(); // 驱动器已进入 step 1 的 pre-step
    agent.steer(UserMessage::new("继续"));
    release_tx.send(()).unwrap();
    agent.when_idle().await;

    // 一个 turn、两个 step、两条 assistant 消息
    assert_eq!(
        type_names(&*agent)
            .iter()
            .filter(|name| **name == "turn/start")
            .count(),
        1
    );
    assert_eq!(
        type_names(&*agent)
            .iter()
            .filter(|name| **name == "step/start")
            .count(),
        2
    );
    assert_eq!(
        type_names(&*agent)
            .iter()
            .filter(|name| **name == "assistant/message")
            .count(),
        2
    );
    let steps: Vec<u32> = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            SessionEventData::StepStart { turn, step } => {
                assert_eq!(*turn, 1);
                Some(*step)
            }
            _ => None,
        })
        .collect();
    assert_eq!(steps, vec![1, 2]);
}

// —— pre-step 拒绝 → turn 以 blocked 收束、无模型调用 ——

#[tokio::test]
async fn pre_step_reject_blocks_turn_without_model_call() {
    let (_root, registry) = setup();
    let agent = registry
        .create(make_options("sess-reject", mock_adapter()))
        .await
        .unwrap();

    agent
        .ctx()
        .on_waterfall::<PreStepPayload, PreStepDecision>("agent/pre-step", |_d| {
            Box::pin(async move { PreStepDecision::Reject })
        })
        .unwrap();

    agent.followup(UserMessage::new("会被拒绝"));
    agent.when_idle().await;

    assert_eq!(type_names(&*agent), vec!["turn/start", "turn/end"]);
    let events = agent.session().events();
    assert!(matches!(
        events[1].data,
        SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Blocked
        }
    ));
}

// —— 取消：流进行中 cancel → turn 以 aborted 收束 ——

#[tokio::test]
async fn cancel_aborts_turn_with_aborted_reason() {
    let (_root, registry) = setup();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(SlowAdapter {
        delay_ms: 100,
        text: "慢慢回复".into(),
    });
    let agent = registry
        .create(make_options("sess-cancel", adapter))
        .await
        .unwrap();

    agent.followup(UserMessage::new("hi"));
    tokio::time::sleep(Duration::from_millis(30)).await; // 流进行中
    agent.cancel(AbortCause::User, false);
    agent.when_idle().await;

    let events = agent.session().events();
    assert!(matches!(
        events.last().unwrap().data,
        SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Aborted {
                cause: AbortCause::User
            }
        }
    ));
    assert_eq!(agent.status(), AgentStatus::Idle);
}

// —— 维护任务：空闲期运行、唤醒闩在收敛后重放 ——

#[tokio::test]
async fn maintenance_latches_wake_until_convergence() {
    let (_root, registry) = setup();
    let agent = registry
        .create(make_options("sess-maint", mock_adapter()))
        .await
        .unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let started_inner = started.clone();
    let maintenance = tokio::spawn(agent.run_maintenance(Box::new(move |_signal| {
        let started_inner = started_inner.clone();
        Box::pin(async move {
            started_inner.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
        })
    })));

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(agent.status(), AgentStatus::Idle); // 维护期公开状态仍为 idle

    agent.followup(UserMessage::new("维护后的消息")); // 闩住
    let idle = agent.when_idle();
    idle.await; // 等到维护 + 后续 turn 全部收敛
    maintenance.await.unwrap().unwrap();

    let messages = agent.session().derive_messages();
    assert_eq!(messages[0], Message::User(UserMessage::new("维护后的消息")));
    assert_eq!(messages.len(), 2); // user + assistant
}

// —— initiator 因果链：事件监听器内可见发起 agent ——

#[tokio::test]
async fn initiator_is_visible_in_event_listeners() {
    let (root, registry) = setup();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_inner = seen.clone();
    root.on("agent/status", move |_payload| {
        seen_inner
            .lock()
            .unwrap()
            .push(current_initiator().map(|agent| agent.id().to_string()));
    })
    .unwrap();

    let agent = registry
        .create(make_options("sess-init", mock_adapter()))
        .await
        .unwrap();
    agent.followup(UserMessage::new("hi"));
    agent.when_idle().await;

    let observed = seen.lock().unwrap();
    assert!(!observed.is_empty());
    // 驱动器内发出的状态事件（Idle）在 with_initiator 边界内：发起者可见。
    // Running 由调用方任务发出（同 dsh：wake 在调用方上下文），可能无发起者。
    assert!(observed.iter().any(|id| id.as_deref() == Some("sess-init")));
}

// —— 注册表：重复创建拒绝、created/disposed 事件、注销 ——

#[tokio::test]
async fn registry_rejects_duplicate_and_emits_lifecycle() {
    let (root, registry) = setup();
    let created = Arc::new(AtomicUsize::new(0));
    let created_inner = created.clone();
    root.on("agent/created", move |_| {
        created_inner.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    let disposed = Arc::new(AtomicUsize::new(0));
    let disposed_inner = disposed.clone();
    root.on("agent/disposed", move |_| {
        disposed_inner.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();

    let options = make_options("sess-dup", mock_adapter());
    let agent = registry.create(options.clone()).await.unwrap();
    assert_eq!(created.load(Ordering::SeqCst), 1);
    assert_eq!(registry.list().len(), 1);
    assert!(registry.get("sess-dup").is_some());

    assert!(matches!(
        registry.create(options).await,
        Err(AgentError::AlreadyRegistered(id)) if id == "sess-dup"
    ));

    registry.unregister("sess-dup");
    assert_eq!(disposed.load(Ordering::SeqCst), 1);
    assert!(registry.get("sess-dup").is_none());
    assert_eq!(agent.id(), "sess-dup");
}

#[tokio::test]
async fn create_without_factory_fails() {
    let root = Context::root();
    let registry = AgentRegistry::new(&root);
    let result = registry
        .create(make_options("sess-nofactory", mock_adapter()))
        .await;
    assert!(matches!(result, Err(AgentError::NoFactory)));
}
