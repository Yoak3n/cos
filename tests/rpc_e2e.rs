//! RPC 模式端到端（pi 协议对齐）：spawn 真实二进制（demo.yml + 确定性 mock LLM），
//! 验证：prompt 异步接受 → 事件流（message_update 文本增量 / tool_execution_* /
//! turn_* / agent_start|end|settled）→ get_* 状态命令 → 错误与解析失败 → exit。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// 发一行命令（JSONL；LF 结尾）。
fn send(stdin: &mut std::process::ChildStdin, line: &str) {
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
}

/// 读一行（事件或响应）。
fn read_line(reader: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "进程提前退出（输出缺失）");
    serde_json::from_str(line.trim_end_matches(['\r', '\n'])).unwrap()
}

#[test]
fn rpc_mode_pi_protocol_prompt_events_state_exit() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args([
            "--config",
            concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo.yml"),
            "--rpc",
            "--session",
            "rpc-e2e",
            "--no-save",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn cos --rpc");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    // prompt：异步接受（响应先到，事件随后流式输出）
    send(
        &mut stdin,
        r#"{"id":"req-1","type":"prompt","message":"帮我记一条演示 todo"}"#,
    );
    let response = read_line(&mut reader);
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "prompt");
    assert_eq!(response["success"], true);
    assert_eq!(response["id"], "req-1");
    assert_eq!(
        response["data"]["messageId"], "req-1",
        "命令 id 即排队消息 id（可据此取消）"
    );

    // 事件流：读到 agent_settled 为止（pi Python 示例以 agent_end/agent_settled 收束）
    let mut text = String::new();
    let mut saw_tool = false;
    let mut saw_turn_end = false;
    let mut saw_tool_result = false;
    loop {
        let event = read_line(&mut reader);
        match event["type"].as_str().unwrap() {
            "message_update" => {
                let delta = &event["assistantMessageEvent"];
                if delta["type"] == "text_delta" {
                    text.push_str(delta["delta"].as_str().unwrap());
                }
            }
            "tool_execution_start" => {
                saw_tool = true;
                assert_eq!(event["toolName"], "todo_write");
                assert!(event["args"].is_object(), "args 应为解析后的对象");
            }
            "tool_execution_end" => {
                saw_tool_result = true;
                assert_eq!(event["isError"], false);
            }
            "turn_end" => {
                saw_turn_end = true;
                assert_eq!(event["reason"], "completed");
            }
            "agent_start" | "turn_start" | "message_start" | "message_end" | "agent_end" => {}
            "agent_settled" => break,
            other => panic!("意外事件: {other} — {event}"),
        }
    }
    assert!(saw_tool, "应看到 todo_write 工具执行");
    assert!(saw_tool_result, "应看到工具执行结束");
    assert!(saw_turn_end, "应看到 turn_end");
    assert!(text.contains("已记录演示任务"), "流式文本应含回复: {text}");

    // get_state：会话统计
    send(&mut stdin, r#"{"id":2,"type":"get_state"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "get_state");
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["sessionId"], "rpc-e2e");
    assert_eq!(response["data"]["isStreaming"], false);
    assert!(response["data"]["messageCount"].as_u64().unwrap() >= 2);
    assert_eq!(response["data"]["pendingMessageCount"], 0);

    // get_last_assistant_text
    send(&mut stdin, r#"{"id":3,"type":"get_last_assistant_text"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["success"], true);
    assert_eq!(response["data"]["text"], "已记录演示任务。");

    // get_messages：模型可见历史（user + assistant + toolResult）
    send(&mut stdin, r#"{"id":4,"type":"get_messages"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["success"], true);
    let messages = response["data"]["messages"].as_array().unwrap();
    assert!(
        messages.len() >= 3,
        "应有 user/assistant/toolResult: {messages:?}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert!(
        messages.iter().any(|m| m["role"] == "toolResult"),
        "应含工具结果消息"
    );

    // get_session_stats：计数与 token 汇总
    send(&mut stdin, r#"{"id":5,"type":"get_session_stats"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["success"], true);
    assert!(response["data"]["userMessages"].as_u64().unwrap() >= 1);
    assert!(response["data"]["assistantMessages"].as_u64().unwrap() >= 1);
    assert!(response["data"]["toolCalls"].as_u64().unwrap() >= 1);
    assert!(response["data"]["tokens"]["total"].is_u64());

    // 未知命令 → 协议兼容失败响应
    send(&mut stdin, r#"{"id":6,"type":"nope"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "nope");
    assert_eq!(response["success"], false);
    assert!(response["error"].as_str().unwrap().contains("未知命令"));

    // cancel_message：队列中无此消息 → 失败响应（已消费/未知 id）
    send(
        &mut stdin,
        r#"{"id":7,"type":"cancel_message","messageId":"m-99"}"#,
    );
    let response = read_line(&mut reader);
    assert_eq!(response["type"], "response");
    assert_eq!(response["command"], "cancel_message");
    assert_eq!(response["success"], false);
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("队列中无此消息"),
        "实际: {response}"
    );
    // messageId 缺失 → 失败响应
    send(&mut stdin, r#"{"id":8,"type":"cancel_message"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["success"], false);
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("messageId 缺失")
    );

    // 非法 JSON → command=parse 失败响应
    send(&mut stdin, "这不是 JSON");
    let response = read_line(&mut reader);
    assert_eq!(response["command"], "parse");
    assert_eq!(response["success"], false);
    assert!(
        response["error"]
            .as_str()
            .unwrap()
            .contains("Failed to parse")
    );

    // exit（cos 扩展）→ 优雅退出（exit code 0）
    send(&mut stdin, r#"{"id":9,"type":"exit"}"#);
    let response = read_line(&mut reader);
    assert_eq!(response["success"], true);
    let status = child.wait().unwrap();
    assert!(status.success(), "exit 后应优雅退出: {status:?}");
}
