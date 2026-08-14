//! 交互式 REPL（`cos --config cordis.yml` 无 `--prompt` 时默认启动）。
//!
//! 输入一行 → 跑一轮 turn → **流式显示**：文本增量实时打印、工具调用/结果即时浮现
//! （读会话日志的 `assistant/chunk` / `tool/call` / `tool/result` 增量）；`/exit` 或
//! Ctrl-D 退出；Ctrl-C 在回复中取消当前 turn（回到提示符），在提示符处退出。

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use cos_llm::{ChunkDelta, UserMessage};
use cos_session::{Session, SessionEventData};
use tokio::io::AsyncBufReadExt;

use crate::{AppError, Assembled, last_turn, run_turn, wait_for_cancel};

/// 流式显示一轮 turn：文本增量实时打印、工具调用/结果即时浮现；本轮 `turn/end`
/// 出现后收束。返回是否打印过文本（供调用方决定是否补打整段回复兜底）。
///
/// 从 `seen`（已消费的 seq）开始增量读会话日志；事件按追加序处理，
/// `turn/end` 是本轮最后一个事件（先写日志再行动），收束前不会漏事件。
/// 持有会话与输出器的所有权（`tokio::spawn` 需要 `'static`）。
async fn watch_turn<W: Write + Send>(
    session: Session,
    turn: u32,
    mut seen: u64,
    mut out: W,
) -> bool {
    let mut streamed_text = false;
    let mut at_line_start = true; // 上次输出是否以换行结尾（工具行前需要换行）
    let mut pending: HashMap<String, String> = HashMap::new(); // call_id → 已打印的工具行前缀
    loop {
        let events = session.events_after(seen);
        let mut ended = false;
        for event in &events {
            seen = event.seq;
            match &event.data {
                SessionEventData::TurnEnd { turn: t, .. } if *t == turn => ended = true,
                SessionEventData::AssistantChunk { turn: t, chunk, .. } if *t == turn => {
                    if let ChunkDelta::Text { text } = &chunk.delta
                        && !text.is_empty()
                    {
                        streamed_text = true;
                        at_line_start = text.ends_with('\n');
                        write!(out, "{text}").expect("写入 stdout");
                        out.flush().expect("flush stdout");
                    }
                }
                SessionEventData::ToolCall {
                    turn: t,
                    call_id,
                    name,
                    arguments,
                    ..
                } if *t == turn => {
                    if !at_line_start {
                        writeln!(out).expect("写入 stdout");
                    }
                    let prefix = format!("  ↳ {name} {arguments}");
                    pending.insert(call_id.clone(), prefix.clone());
                    write!(out, "{prefix}").expect("写入 stdout");
                    out.flush().expect("flush stdout");
                    at_line_start = false;
                }
                SessionEventData::ToolResult {
                    turn: t,
                    call_id,
                    message,
                    ..
                } if *t == turn => match pending.remove(call_id) {
                    Some(prefix) => {
                        writeln!(out, "{prefix} → {}\n", message.content).expect("写入 stdout");
                        at_line_start = true;
                    }
                    None => {
                        writeln!(out, "  ↳ → {}\n", message.content).expect("写入 stdout");
                        at_line_start = true;
                    }
                },
                _ => {}
            }
        }
        if ended {
            // 文本流未以换行收尾 → 补一个换行，保证提示符另起一行
            if streamed_text && !at_line_start {
                writeln!(out).expect("写入 stdout");
            }
            return streamed_text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
}

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
        // 流式显示：从当前日志末尾起增量读本轮事件（文本/工具调用实时打印）
        let turn = last_turn(assembled.agent.session()) + 1;
        let seen = assembled.agent.session().last_seq();
        let stream_job = tokio::spawn(watch_turn(
            assembled.agent.session().clone(),
            turn,
            seen,
            std::io::stdout(),
        ));
        let summary = run_turn(&assembled.agent, UserMessage::new(text), cancel.as_ref()).await;
        // watcher 看到本轮 turn/end 即收束；run_turn 返回时 turn/end 已落日志
        let streamed = stream_job.await.unwrap_or(false);
        if let Some(flag) = &cancel {
            flag.store(false, Ordering::Release);
        }
        if summary.cancelled {
            println!("（已取消）");
            continue;
        }
        if let Some(error) = &summary.error {
            println!("[错误] {error}");
        }
        if !streamed && !summary.reply.is_empty() {
            // 兜底：watcher 未打印任何文本（极端竞态）才补打整段回复
            println!("{}", summary.reply);
        }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cos_llm::{StreamChunk, ToolResultMessage};
    use cos_session::{Session, SessionEventData, TurnEndReason};

    use super::watch_turn;

    /// 文本增量 + 工具调用/结果按序实时浮现；turn/end 后收束。
    #[tokio::test]
    async fn watch_turn_streams_text_and_tools_then_ends() {
        let session = Session::new("t");
        let seen = session.last_seq();
        let feed = {
            let session = session.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                session.append(SessionEventData::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::text("你"),
                });
                session.append(SessionEventData::AssistantChunk {
                    turn: 1,
                    step: 1,
                    chunk: StreamChunk::text("好"),
                });
                session.append(SessionEventData::ToolCall {
                    turn: 1,
                    step: 2,
                    call_id: "c1".into(),
                    name: "recall".into(),
                    arguments: "{}".into(),
                });
                session.append(SessionEventData::ToolResult {
                    turn: 1,
                    step: 2,
                    call_id: "c1".into(),
                    message: ToolResultMessage::new("无相关记忆"),
                    error: None,
                });
                session.append(SessionEventData::AssistantChunk {
                    turn: 1,
                    step: 2,
                    chunk: StreamChunk::text("回答"),
                });
                session.append(SessionEventData::TurnEnd {
                    turn: 1,
                    reason: TurnEndReason::Completed,
                });
            })
        };

        let mut out = Vec::new();
        let streamed = watch_turn(session.clone(), 1, seen, &mut out).await;
        feed.await.unwrap();

        assert!(streamed, "有文本增量");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("你好"), "文本增量实时打印: {text}");
        assert!(text.contains("  ↳ recall {}"), "工具调用即时浮现: {text}");
        assert!(text.contains("→ 无相关记忆"), "工具结果即时浮现: {text}");
        assert!(text.contains("回答"), "{text}");
        // 流式文本与工具行之间应有换行分隔
        assert!(text.contains("好\n  ↳ recall"), "工具行前换行: {text}");
    }

    /// 只发生工具调用、无文本 → 返回 false（不补打空回复）。
    #[tokio::test]
    async fn watch_turn_tool_only_reports_no_text() {
        let session = Session::new("t");
        // seen 必须在事件追加之前捕获（watcher 只消费 seen 之后的新事件）
        let seen = session.last_seq();
        session.append(SessionEventData::TurnStart { turn: 1 });
        session.append(SessionEventData::ToolCall {
            turn: 1,
            step: 1,
            call_id: "c1".into(),
            name: "recall".into(),
            arguments: "{}".into(),
        });
        session.append(SessionEventData::ToolResult {
            turn: 1,
            step: 1,
            call_id: "c1".into(),
            message: ToolResultMessage::new("无"),
            error: None,
        });
        session.append(SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        });

        let mut out = Vec::new();
        let streamed = watch_turn(session.clone(), 1, seen, &mut out).await;
        assert!(!streamed, "无文本 → false");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("↳ recall"), "{text}");
        assert!(text.contains("→ 无"), "{text}");
    }

    /// 早于 `seen` 的事件不重复打印（增量读取语义）：只看 turn 2，turn 1 的旧文本不重放。
    #[tokio::test]
    async fn watch_turn_skips_events_before_seen() {
        let session = Session::new("t");
        session.append(SessionEventData::TurnStart { turn: 1 });
        session.append(SessionEventData::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: StreamChunk::text("旧"),
        });
        session.append(SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        });
        let seen = session.last_seq();
        session.append(SessionEventData::TurnStart { turn: 2 });
        session.append(SessionEventData::AssistantChunk {
            turn: 2,
            step: 1,
            chunk: StreamChunk::text("新"),
        });
        session.append(SessionEventData::TurnEnd {
            turn: 2,
            reason: TurnEndReason::Completed,
        });

        let mut out = Vec::new();
        let streamed = watch_turn(session.clone(), 2, seen, &mut out).await;
        assert!(streamed);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("新"), "{text}");
        assert!(!text.contains("旧"), "不应重放 seen 之前的事件: {text}");
    }
}
