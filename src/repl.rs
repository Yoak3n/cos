//! 交互式 REPL（`cos --config cordis.yml` 无 `--prompt` 时默认启动）。
//!
//! 输入一行 → 跑一轮 turn → 打印工具轨迹与助手回复；`/exit` 或 Ctrl-D 退出；
//! Ctrl-C 在回复中取消当前 turn（回到提示符），在提示符处退出。

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cos_llm::UserMessage;
use tokio::io::AsyncBufReadExt;

use crate::{AppError, Assembled, run_turn, wait_for_cancel};

/// REPL 主循环（装配后调用；退出后由调用方统一 `finish`）。
pub async fn serve_repl(
    assembled: &Assembled,
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(), AppError> {
    let tty = std::io::stdin().is_terminal();
    if tty {
        println!(
            "cos 交互模式 —— 输入消息与 agent 对话；/help 查看命令；\
             Ctrl-C 取消当前回复；/exit 退出"
        );
        if assembled.demo_mode {
            println!(
                "注意：未配置真实 LLM（当前为确定性演示脚本）。\
                 用 --llm-* 参数、或 yml plugin-llm（providers/chains）+ --agent-llm <id> 接入真实模型。"
            );
        }
    }
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    loop {
        if tty {
            print!("你 > ");
            std::io::stdout().flush()?;
        }
        let line = match &cancel {
            Some(flag) => tokio::select! {
                line = lines.next_line() => line?,
                _ = wait_for_cancel(flag.clone()) => return Ok(()),
            },
            None => lines.next_line().await?,
        };
        let Some(text) = line else {
            break; // EOF（Ctrl-D / 管道结束）
        };
        let text = text.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if let Some(command) = text.strip_prefix('/') {
            match command {
                "exit" | "quit" => break,
                "help" => print_help(),
                "session" => print_session(assembled),
                other => println!("未知命令 /{other}（/help 查看）"),
            }
            continue;
        }
        // 消费取消信号：turn 中被 Ctrl-C 中断 → 取消该轮并回到提示符
        let summary = run_turn(&assembled.agent, UserMessage::new(text), cancel.as_ref()).await;
        if let Some(flag) = &cancel {
            flag.store(false, Ordering::Release);
        }
        for trace in &summary.tool_trace {
            println!("  ↳ {trace}");
        }
        if summary.cancelled {
            println!("（已取消）");
            continue;
        }
        if let Some(error) = &summary.error {
            println!("[错误] {error}");
        }
        println!("{}", summary.reply);
    }
    Ok(())
}

fn print_help() {
    println!(
        "命令：\n  /exit | /quit  退出\n  /session       查看会话统计\n  /help          本帮助\n\
         其它输入作为消息发给 agent；Ctrl-C 取消当前回复。"
    );
}

fn print_session(assembled: &Assembled) {
    let session = assembled.agent.session();
    println!(
        "会话 {}：事件 {} 条，模型可见消息 {} 条",
        session.id(),
        session.events().len(),
        session.derive_messages().len()
    );
}
