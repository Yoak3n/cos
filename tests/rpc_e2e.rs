//! RPC 模式端到端：spawn 真实二进制（demo.yml + 确定性 mock LLM），
//! 按行 JSON-RPC 协议交互：ping / chat（工具轨迹）/ session / 未知方法 / 非法行 / exit。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// 发一行请求，读一行响应（保持顺序）。
fn send(stdin: &mut std::process::ChildStdin, line: &str) {
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
}

fn read_response(reader: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "进程提前退出（响应缺失）");
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn rpc_mode_ping_chat_session_errors_exit() {
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

    // ping
    send(&mut stdin, r#"{"id":1,"method":"ping"}"#);
    let response = read_response(&mut reader);
    assert_eq!(response["result"], "pong");

    // chat：demo mock 脚本 = todo_write 工具调用 → 文本回复
    send(
        &mut stdin,
        r#"{"id":2,"method":"chat","params":{"message":"帮我记一条演示 todo"}}"#,
    );
    let response = read_response(&mut reader);
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["reply"], "已记录演示任务。");
    assert_eq!(response["result"]["turn"], 1);
    let tools = response["result"]["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|t| t.as_str().unwrap().contains("todo_write")),
        "工具轨迹应含 todo_write: {tools:?}"
    );

    // session：事件已增长
    send(&mut stdin, r#"{"id":3,"method":"session"}"#);
    let response = read_response(&mut reader);
    assert!(response["result"]["events"].as_u64().unwrap() >= 10);
    assert!(response["result"]["messages"].as_u64().unwrap() >= 2);

    // 未知方法 → -32601
    send(&mut stdin, r#"{"id":4,"method":"nope"}"#);
    let response = read_response(&mut reader);
    assert_eq!(response["error"]["code"], -32601);

    // 非法 JSON → -32700
    send(&mut stdin, "这不是 JSON");
    let response = read_response(&mut reader);
    assert_eq!(response["error"]["code"], -32700);
    assert_eq!(response["id"], serde_json::Value::Null);

    // exit → bye 并优雅退出（exit code 0）
    send(&mut stdin, r#"{"id":5,"method":"exit"}"#);
    let response = read_response(&mut reader);
    assert_eq!(response["result"], "bye");
    let status = child.wait().unwrap();
    assert!(status.success(), "exit 后应优雅退出: {status:?}");
}
