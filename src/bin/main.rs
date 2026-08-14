//! cos CLI 入口：
//! - `cos --config <cordis.yml>` 无 `--prompt` → 交互式 REPL（一键启动）；
//! - `cos --config <cordis.yml> --rpc` → stdio RPC 服务（协议对齐 pi `docs/rpc.md`，供外部程序调用）；
//! - `cos --config <cordis.yml> --prompt <text>` → 一次性运行（演示/脚本）；
//! - `--dump-config` 只输出装载计划。
//!
//! Ctrl-C：一次性 = 取消活动 turn 后优雅退出；REPL = 回复中取消当前 turn、提示符处退出；
//! RPC = 取消当前 prompt（若在跑），空闲时退出。退出统一走 `finish`（不变量/digest/落盘/卸载）。
//! LLM：`--llm-base-url/--llm-model/--llm-api-key`（或 `COS_LLM_*` 环境变量）启用真实 LLM，
//! `--llm-no-stream` 关流式；`--agent-llm <id>`（或 `COS_AGENT_LLM`）指定主 agent 提供商/后备链；
//! `--agent-driver <id>`（或 `COS_AGENT_DRIVER`）指定主 agent 驱动（agent_factory! 注册表，缺省 loop）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cos::{schema::{LlmConfig, RunConfig}, assemble, finish, run};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    OneShot,
    Repl,
    Rpc,
}

struct Args {
    config_path: String,
    dump_config: bool,
    session_id: String,
    prompt: Option<String>,
    session_path: Option<String>,
    llm: Option<LlmConfig>,
    agent_llm: Option<String>,
    agent_driver: Option<String>,
    mode: Mode,
}

const USAGE: &str = "用法: cos --config <cordis.yml> [--session <id>] [--no-save] [--repl | --rpc | --prompt <text>] \
[--dump-config]\n  \
[--llm-base-url <url> --llm-model <model> --llm-api-key <key>] [--llm-no-stream] [--agent-llm <id>] [--agent-driver <id>]\n  \
缺省（无 --prompt/--rpc）为交互式 REPL；--prompt 为一次性；--rpc 为 stdio RPC 服务（pi 协议）";

fn env_or(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        config_path: "cordis.yml".into(),
        dump_config: false,
        session_id: "demo".into(),
        prompt: None,
        session_path: Some("sessions/demo.jsonl".into()),
        llm: None,
        agent_llm: env_or("COS_AGENT_LLM"),
        agent_driver: env_or("COS_AGENT_DRIVER"),
        mode: Mode::Repl,
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
                parsed.prompt = Some(args.next().ok_or("--prompt 需要文本")?);
                parsed.mode = Mode::OneShot;
            }
            "--repl" => parsed.mode = Mode::Repl,
            "--rpc" => parsed.mode = Mode::Rpc,
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
            "--agent-driver" => {
                parsed.agent_driver = Some(args.next().ok_or("--agent-driver 需要 id")?);
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
    // --dump-config 只输出装载计划（不装配、不进入 REPL）
    if parsed.dump_config {
        parsed.mode = Mode::OneShot;
    }
    Ok(parsed)
}

/// 打印收尾报告（不变量违规则非零退出）。
fn print_report(report: &cos::RunReport, mode: Mode) {
    if let Some(dump) = &report.dump {
        println!("{dump}");
        return;
    }
    if mode == Mode::OneShot {
        println!("=== cos 演示完成 ===");
        println!(
            "会话事件 {} 条，模型可见消息 {} 条",
            report.events.len(),
            report.messages.len()
        );
    }
    if !report.violations.is_empty() {
        eprintln!("不变量违规:");
        for violation in &report.violations {
            eprintln!("  - {violation}");
        }
        std::process::exit(1);
    }
    if mode != Mode::Rpc {
        println!("卸载顺序（apply 逆序）: {:?}", report.unload_order);
        if mode == Mode::OneShot {
            println!("不变量: 全部通过（模型可见 ⟺ 已记录、seq 单调、边界配对）");
        }
    }
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

    // Ctrl-C 监视：置位取消信号，各模式内优雅收束
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_inner = cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel_inner.store(true, Ordering::Release);
    });

    let config = RunConfig {
        config_path: Some(args.config_path),
        dump_config: args.dump_config,
        session_id: args.session_id,
        prompt: args.prompt,
        session_path: args.session_path,
        cancel: Some(cancel.clone()),
        llm: args.llm,
        agent_llm: args.agent_llm,
        agent_driver: args.agent_driver,
    };

    let result = match args.mode {
        Mode::OneShot => run(config).await.map(|report| (report, args.mode)),
        Mode::Repl => {
            let assembled = require_assembled(assemble(&config).await);
            if let Err(error) = cos::repl::serve_repl(&assembled, Some(cancel)).await {
                eprintln!("REPL 失败: {error}");
                std::process::exit(1);
            }
            finish(&assembled, &config)
                .await
                .map(|report| (report, args.mode))
        }
        Mode::Rpc => {
            let assembled = require_assembled(assemble(&config).await);
            if let Err(error) = cos::rpc::serve(&assembled, Some(cancel)).await {
                eprintln!("RPC 失败: {error}");
                std::process::exit(1);
            }
            finish(&assembled, &config)
                .await
                .map(|report| (report, args.mode))
        }
    };

    match result {
        Ok((report, mode)) => print_report(&report, mode),
        Err(error) => {
            eprintln!("启动失败: {error}");
            std::process::exit(1);
        }
    }
}

/// 装配并校验主 agent 存在（无 LLM 配置 = 关键组件缺失 → 启动失败，fail loud）。
fn require_assembled(result: Result<cos::Assembled, cos::AppError>) -> cos::Assembled {
    match result {
        Ok(assembled) => match assembled.agent() {
            Ok(_) => assembled,
            Err(error) => fail(error),
        },
        Err(error) => fail(error),
    }
}

fn fail(error: cos::AppError) -> ! {
    eprintln!("启动失败: {error}");
    std::process::exit(1);
}
