//! cos CLI 入口（P6）：
//! `cos --config <cordis.yml> [--dump-config] [--session <id>] [--prompt <text>] [--no-save]`。
//! Ctrl-C → 取消活动 turn → 优雅退出（全插件逆序卸载）。
//! M2：`--llm-base-url/--llm-model/--llm-api-key`（或环境变量
//! `COS_LLM_BASE_URL/COS_LLM_MODEL/COS_LLM_API_KEY`）启用真实 LLM；缺省为确定性 mock。
//! `--llm-no-stream`（或 `COS_LLM_NO_STREAM=1`）关闭流式（opencode zen/go 流式只出推理文本，建议关）。
//! LLM 统一管理：`--agent-llm <id>`（或 `COS_AGENT_LLM`）指定主 agent 的提供商/后备链 id
//! （plugin-llm 装配的 yml 条目；未指定且无 --llm-* 时用 demo mock）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cos::{LlmConfig, RunConfig, run};

struct Args {
    config_path: String,
    dump_config: bool,
    session_id: String,
    prompt: String,
    session_path: Option<String>,
    llm: Option<LlmConfig>,
    agent_llm: Option<String>,
}

const USAGE: &str = "用法: cos --config <cordis.yml> [--dump-config] [--session <id>] [--prompt <text>] [--no-save] \
[--llm-base-url <url> --llm-model <model> --llm-api-key <key>] [--llm-no-stream] [--agent-llm <id>]";

fn env_or(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        config_path: "cordis.yml".into(),
        dump_config: false,
        session_id: "demo".into(),
        prompt: "帮我记一条演示 todo".into(),
        session_path: Some("sessions/demo.jsonl".into()),
        llm: None,
        agent_llm: env_or("COS_AGENT_LLM"),
    };
    let mut llm_base_url = env_or("COS_LLM_BASE_URL");
    let mut llm_model = env_or("COS_LLM_MODEL");
    let mut llm_api_key = env_or("COS_LLM_API_KEY");
    let mut no_stream = env_or("COS_LLM_NO_STREAM").is_some();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                parsed.config_path = args.next().ok_or("--config 需要路径")?;
            }
            "--dump-config" => parsed.dump_config = true,
            "--session" => {
                parsed.session_id = args.next().ok_or("--session 需要 id")?;
            }
            "--prompt" => {
                parsed.prompt = args.next().ok_or("--prompt 需要文本")?;
            }
            "--no-save" => parsed.session_path = None,
            "--llm-base-url" => {
                llm_base_url = Some(args.next().ok_or("--llm-base-url 需要 url")?);
            }
            "--llm-model" => {
                llm_model = Some(args.next().ok_or("--llm-model 需要模型名")?);
            }
            "--llm-api-key" => {
                llm_api_key = Some(args.next().ok_or("--llm-api-key 需要 key")?);
            }
            "--llm-no-stream" => no_stream = true,
            "--agent-llm" => {
                parsed.agent_llm = Some(args.next().ok_or("--agent-llm 需要 id")?);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数: {other}")),
        }
    }
    match (llm_base_url, llm_model, llm_api_key) {
        (Some(base_url), Some(model), Some(api_key)) => {
            parsed.llm = Some(LlmConfig {
                base_url,
                api_key,
                model,
                streaming: !no_stream,
            });
        }
        (None, None, None) => {}
        _ => {
            return Err("--llm-* 三个参数必须同时提供（或同时用环境变量）".to_string());
        }
    }
    Ok(parsed)
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{USAGE}");
            std::process::exit(2);
        }
    };

    // Ctrl-C 监视：置位取消信号，run 内优雅收束并卸载
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_inner = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("\n收到中断：取消活动 turn，进入优雅退出…");
        cancel_inner.store(true, Ordering::Release);
    });

    let report = match run(RunConfig {
        config_path: args.config_path,
        dump_config: args.dump_config,
        session_id: args.session_id,
        prompt: args.prompt,
        session_path: args.session_path,
        cancel: Some(cancel),
        llm: args.llm,
        agent_llm: args.agent_llm,
    })
    .await
    {
        Ok(report) => report,
        Err(error) => {
            eprintln!("启动失败: {error}");
            std::process::exit(1);
        }
    };

    if let Some(dump) = &report.dump {
        println!("{dump}");
        return;
    }

    println!("=== dsh-rust 演示完成 ===");
    println!(
        "会话事件 {} 条，模型可见消息 {} 条",
        report.events.len(),
        report.messages.len()
    );
    println!("卸载顺序（apply 逆序）: {:?}", report.unload_order);
    if report.violations.is_empty() {
        println!("不变量: 全部通过（模型可见 ⟺ 已记录、seq 单调、边界配对）");
    } else {
        eprintln!("不变量违规:");
        for violation in &report.violations {
            eprintln!("  - {violation}");
        }
        std::process::exit(1);
    }
}
