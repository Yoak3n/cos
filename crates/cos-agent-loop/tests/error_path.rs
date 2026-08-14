//! 回归：LLM 请求失败路径必须闭合 step/start ↔ step/end 配对，turn 以 Error 收束。
//! （M3 实端点 429 冒烟暴露：旧实现错误分支提前 return，留下未闭合 step，
//!   违反 step-pairing 不变量。）

use std::sync::Arc;

use cos_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::Context;
use cos_llm::{LlmAdapter, LlmError, LlmRequest, LlmStream, UserMessage};
use cos_session::{SessionEventData, TurnEndReason};
use cos_tools::ToolRegistry;

/// 固定失败的适配器（模拟服务端 5xx / 429）。
struct FailingAdapter;

impl LlmAdapter for FailingAdapter {
    fn id(&self) -> &str {
        "failing"
    }

    fn stream(&self, _request: &LlmRequest) -> LlmStream {
        Box::pin(futures::stream::once(async move {
            Err(LlmError::Failure("模拟失败".to_string()))
        }))
    }
}

#[tokio::test]
async fn request_error_closes_step_and_turn() {
    let ctx = Context::root();
    ctx.provide(ToolRegistry::new(&ctx)).unwrap();
    ctx.provide(AgentRegistry::new(&ctx)).unwrap();
    ctx.get::<AgentRegistry>()
        .unwrap()
        .set_factory(Arc::new(LoopFactory))
        .unwrap();

    let agent = ctx
        .get::<AgentRegistry>()
        .unwrap()
        .create(CreateAgentOptions {
            session_id: "err-agent".into(),
            options: AgentOptions {
                provider: None,
                model: None,
                max_tokens: None,
            },
            adapter: Arc::new(FailingAdapter),
        })
        .await
        .unwrap();

    agent.followup(UserMessage::new("你好"));
    agent.when_idle().await;

    let events = agent.session().events();
    // step/start 与 step/end 严格配对（失败路径也不例外）
    let mut open: Vec<(u32, u32)> = Vec::new();
    for event in &events {
        match &event.data {
            SessionEventData::StepStart { turn, step } => open.push((*turn, *step)),
            SessionEventData::StepEnd { turn, step } => {
                assert_eq!(open.pop(), Some((*turn, *step)), "step 必须配对闭合");
            }
            _ => {}
        }
    }
    assert!(open.is_empty(), "失败路径不得留下未闭合 step");

    // turn 以 Error 收束，错误已进入日志（模型可见 ⟺ 已记录）
    assert!(events.iter().any(|event| matches!(
        &event.data,
        SessionEventData::TurnEnd {
            reason: TurnEndReason::Error { message },
            ..
        } if message.contains("模拟失败")
    )));
}
