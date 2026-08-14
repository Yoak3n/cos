//! cos CLI 入口（P6）：
//! `cos --config <cordis.yml> [--dump-config] [--session <id>] [--prompt <text>] [--no-save]`。
//! Ctrl-C → 取消活动 turn → 优雅退出（全插件逆序卸载）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cos::{RunConfig, run};

struct Args {
    config_path: String,
    dump_config: bool,
    session_id: String,
    prompt: String,
    session_path: Option<String>,
}

const USAGE: &str = "用法: cos --config <cordis.yml> [--dump-config] [--session <id>] [--prompt <text>] [--no-save]";

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        config_path: "cordis.yml".into(),
        dump_config: false,
        session_id: "demo".into(),
        prompt: "帮我记一条演示 todo".into(),
        session_path: Some("sessions/demo.jsonl".into()),
    };
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
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("未知参数: {other}")),
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
