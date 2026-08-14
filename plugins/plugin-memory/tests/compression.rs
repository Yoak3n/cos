//! M3 验收（接线）：上下文自动压缩全链路 —— 长会话超阈值时旧消息压进滚动摘要
//! （落 session_state）、请求只带尾部窗口 + 摘要进 system（request/header 有日志，
//! 模型可见 ⟺ 已记录不变量不受影响）；会话末 digest 慢路径按 seq 去重触发。

use std::sync::{Arc, Mutex};

use dsh_agent::{AgentOptions, AgentRegistry, CreateAgentOptions};
use dsh_agent_loop::LoopFactory;
use dsh_core::{Context, Plugin};
use dsh_llm::{LlmAdapter, LlmRegistry, LlmRequest, LlmStream, UserMessage};
use dsh_llm_mock::{MockAdapter, MockReply};
use dsh_memory::MemoryStore;
use dsh_session::SessionEventData;
use dsh_system_prompt::PromptSections;
use dsh_tools::ToolRegistry;
use plugin_memory::{MemoryConfig, MemoryPlugin};

fn temp_db() -> String {
    std::env::temp_dir()
        .join(format!("plugin-memory-compress-{}.db", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

/// 捕获每次进入适配器的请求（压缩生效后的最终形态），再委托给脚本化 mock。
struct Capturing {
    inner: MockAdapter,
    captured: Mutex<Vec<LlmRequest>>,
}

impl Capturing {
    fn new(script: Vec<MockReply>) -> Self {
        Self {
            inner: MockAdapter::new("capture", script),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn snapshot(&self) -> Vec<LlmRequest> {
        self.captured.lock().unwrap().clone()
    }
}

impl LlmAdapter for Capturing {
    fn id(&self) -> &str {
        "capture"
    }

    fn stream(&self, request: &LlmRequest) -> LlmStream {
        self.captured.lock().unwrap().push(request.clone());
        self.inner.stream(request)
    }
}

const EMPTY_EXTRACT: &str = r#"{"facts":[],"card_notes":{}}"#;
const COMPRESS_SUMMARY: &str = r#"{"summary":"压缩摘要要点"}"#;
const EMPTY_DIGEST: &str = r#"{"profile_notes":[],"agent_model_notes":[],"relationship_notes":[]}"#;

#[tokio::test]
async fn long_session_compresses_tail_and_runs_digest() {
    let path = temp_db();
    let ctx = Context::root();
    ctx.provide(ToolRegistry::new(&ctx)).unwrap();
    ctx.provide(PromptSections::new(&ctx)).unwrap();
    ctx.get::<PromptSections>()
        .unwrap()
        .append("persona", "你是陪伴助手。");
    ctx.provide(AgentRegistry::new(&ctx)).unwrap();
    ctx.get::<AgentRegistry>()
        .unwrap()
        .set_factory(Arc::new(LoopFactory))
        .unwrap();

    // 记忆 mock 脚本（按调用序）：
    // E×5（turn 2–6 预步 apply 的空提取）→ C（turn 6 请求压缩）→ E → C → E → C → D（digest）
    let memory_script = vec![
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(COMPRESS_SUMMARY),
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(COMPRESS_SUMMARY),
        MockReply::text(EMPTY_EXTRACT),
        MockReply::text(COMPRESS_SUMMARY),
        MockReply::text(EMPTY_DIGEST),
    ];
    ctx.provide(LlmRegistry::new(&ctx)).unwrap();
    ctx.get::<LlmRegistry>()
        .unwrap()
        .register(
            "default",
            Arc::new(MockAdapter::new("memory-mock", memory_script)),
        )
        .unwrap();
    // 阈值 40：turn 5 累计 40 不超；turn 6（48）起压缩；keep_tail = 4；8 turn 触发 digest
    MemoryPlugin
        .apply(
            &ctx,
            &MemoryConfig {
                db_path: path.clone(),
                llm: None,
                max_context_chars: 40,
                keep_tail: 4,
                digest_every: 8,
            },
        )
        .unwrap();

    let capturing = Arc::new(Capturing::new(vec![MockReply::text("好"); 8]));
    let agent = ctx
        .get::<AgentRegistry>()
        .unwrap()
        .create(CreateAgentOptions {
            session_id: "m3-agent".into(),
            options: AgentOptions {
                provider: Some("capture".into()),
                model: Some("mock".into()),
                max_tokens: None,
            },
            adapter: capturing.clone(),
        })
        .await
        .unwrap();

    for turn in 1..=8 {
        agent.followup(UserMessage::new(format!("第 {turn} 条消息")));
        agent.when_idle().await;
    }

    // 摘要已落库（滚动压缩）
    let store = ctx.get::<MemoryStore>().unwrap();
    assert_eq!(
        store.get_state("summary:m3-agent").unwrap().as_deref(),
        Some("压缩摘要要点")
    );

    // 最后一次请求：只带尾部窗口 + 摘要进 system
    let requests = capturing.snapshot();
    assert_eq!(requests.len(), 8, "每 turn 一次请求");
    assert_eq!(requests[0].messages.len(), 1, "首请求无压缩");
    let last = requests.last().unwrap();
    assert_eq!(last.messages.len(), 4, "压缩后只保留尾部窗口");
    let system = last.system.as_ref().unwrap();
    assert!(system.contains("【对话摘要】"), "{system}");
    assert!(system.contains("压缩摘要要点"), "{system}");
    assert!(system.contains("你是陪伴助手"), "原 persona 保留: {system}");

    // 摘要进 request/header 日志（模型可见 ⟺ 已记录）
    let headers: Vec<String> = agent
        .session()
        .events()
        .into_iter()
        .filter_map(|event| match event.data {
            SessionEventData::RequestHeader { header } => header.system,
            _ => None,
        })
        .collect();
    assert!(
        headers.last().unwrap().contains("压缩摘要要点"),
        "摘要必须进入 request/header 日志"
    );

    // 会话中 digest 慢路径（8 turn 阈值）：轮询等待节流触发完成
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if store.get_state("digested_turn:m3-agent").unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "digest 未在时限内完成"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    drop(ctx);
    let _ = std::fs::remove_file(&path);
}
