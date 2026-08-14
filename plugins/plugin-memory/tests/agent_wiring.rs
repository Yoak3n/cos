//! M2 验收：plugin-memory 接 agent 读/写路径（双 mock LLM）。
//!
//! 链路：turn 1 用户说咖啡（主 mock 回复）→ turn 2 预步（agent/pre-step 钩）
//! 把 turn 1 消化进记忆（记忆 mock 提取 + 卡合并）→ turn 2 请求（agent/request 钩）
//! 注入【相关记忆】+【关系卡】→ 主 mock 回复。断言落库与 system 注入。

use std::sync::Arc;

use cos_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use cos_agent_loop::LoopFactory;
use cos_core::{Context, Plugin};
use cos_llm::{LlmRegistry, UserMessage};
use cos_llm_mock::{MockAdapter, MockReply};
use cos_memory::MemoryStore;
use cos_session::SessionEventData;
use cos_system_prompt::PromptSections;
use cos_tools::ToolRegistry;
use plugin_memory::{MemoryConfig, MemoryPlugin};

fn temp_db() -> String {
    std::env::temp_dir()
        .join(format!("plugin-memory-agent-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn turn_absorbed_then_recalled_and_injected() {
    let path = temp_db();
    let ctx = Context::root();
    ctx.provide(ToolRegistry::new(&ctx)).unwrap();
    ctx.provide(PromptSections::new(&ctx)).unwrap();
    ctx.get::<PromptSections>()
        .unwrap()
        .append("persona", "你是陪伴助手，请结合记忆回答。");
    ctx.provide(AgentRegistry::new(&ctx)).unwrap();
    ctx.get::<AgentRegistry>()
        .unwrap()
        .set_factory(Arc::new(LoopFactory))
        .unwrap();
    // LLM 统一管理：注册 "default" = 记忆 mock（turn 2 预步 apply_turn 消耗：提取 → 卡合并）
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    ctx.get::<LlmRegistry>()
        .unwrap()
        .register(
            "default",
            Arc::new(MockAdapter::new(
                "memory-mock",
                vec![
                    MockReply::text(
                        r#"{"facts":[{"kind":"user","action":"new","topic_text":"咖啡偏好","statement":"用户喜欢手冲咖啡"}],"card_notes":{"profile":["用户喜欢手冲咖啡"],"agent_model":[],"relationship":[]}}"#,
                    ),
                    MockReply::text(r#"{"text":"用户喜欢手冲咖啡"}"#),
                ],
            )),
        )
        .unwrap();
    MemoryPlugin
        .apply(
            &ctx,
            &MemoryConfig {
                db_path: path.clone(),
                llm: None,
                max_context_chars: 6000,
                keep_tail: 6,
                digest_every: 8,
            },
        )
        .unwrap();

    // 主循环 mock：turn 1、turn 2 各一条文本回复
    let agent = ctx
        .get::<AgentRegistry>()
        .unwrap()
        .create(CreateAgentOptions {
            session_id: "m2-agent".into(),
            options: AgentOptions {
                provider: Some("main".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter: Arc::new(MockAdapter::new(
                "main",
                vec![
                    MockReply::text("好的，我记住了"),
                    MockReply::text("你之前说过喜欢手冲咖啡"),
                ],
            )),
        })
        .await
        .unwrap();

    agent.followup(UserMessage::new("我喜欢手冲咖啡"));
    agent.when_idle().await;
    agent.followup(UserMessage::new("咖啡"));
    agent.when_idle().await;

    // 写路径：turn 1 已被消化（topic + 关系卡）
    let store = ctx.get::<MemoryStore>().unwrap();
    let topics = store.topics().unwrap();
    assert_eq!(topics.len(), 1);
    assert_eq!(topics[0].canonical_name, "咖啡偏好");
    assert_eq!(topics[0].state_summary, "用户喜欢手冲咖啡");
    assert_eq!(store.card().unwrap().profile, "用户喜欢手冲咖啡");

    // 读路径：turn 2 请求的 system 注入【相关记忆】+【关系卡】，原 persona 保留
    let systems: Vec<Option<String>> = agent
        .session()
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            SessionEventData::RequestHeader { header } => Some(header.system),
            _ => None,
        })
        .collect();
    assert_eq!(systems.len(), 2, "两个 turn 各一次请求");
    let last = systems
        .last()
        .unwrap()
        .as_ref()
        .expect("turn 2 请求应有 system");
    assert!(last.contains("【相关记忆】"), "{last}");
    assert!(last.contains("咖啡偏好"), "{last}");
    assert!(last.contains("【关系卡】"), "{last}");
    assert!(last.contains("关于你：用户喜欢手冲咖啡"), "{last}");
    assert!(
        last.contains("你是陪伴助手"),
        "原 system（persona 段文本）应保留: {last}"
    );

    drop(ctx);
    let _ = std::fs::remove_file(&path);
}
