//! P8 试点端到端（P9 服务直连验证）：spawn 真实二进制 + 临时 yml
//! （opencode Provider 插件 + `name: ./target/debug/plugin_todo_dlopen.dll`），
//! `--llm-*` 指向本地回环 chat/completions 服务器（脚本：tool_use 调 todo_write → 文本）
//! → **dlopen 工具**经 C 回调执行（内含 get_service/service_call 查询宿主 tools 桥）→
//! 结果回流会话日志；`finish` 卸载时插件 disposer 写 marker 文件
//! （验证效果卸载链 + 配置 JSON 传递）。
//!
//! 前置：`cargo build -p plugin-todo-dlopen`（或任一 dev profile 全量构建）先生成
//! `target/debug/plugin_todo_dlopen.dll`——`cargo test` 的 test profile 产物带哈希、
//! 不产生该未哈希文件名，测试运行时依赖它已存在（相对路径相对测试 cwd = 工作区根）。

use cos_test_support::{ChatReply, spawn_sync};
use std::process::Command;

#[test]
fn dlopen_plugin_loads_tool_executes_and_disposes() {
    let marker = std::env::temp_dir().join(format!("cos-dlopen-marker-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let yml = std::env::temp_dir().join(format!("cos-dlopen-{}.yml", std::process::id()));
    // 相对路径相对测试 cwd（= 工作区根）；配置 JSON 原样传给插件（marker 路径）
    std::fs::write(
        &yml,
        format!(
            "- name: opencode-provider\n- name: ./target/debug/plugin_todo_dlopen.dll\n  config:\n    marker: {}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    // 会话默认落盘 sessions/demo.jsonl（验证工具结果回流）
    let session_log = "sessions/demo.jsonl";
    let _ = std::fs::remove_file(session_log);

    // 本地 chat/completions 服务器：tool_use(todo_write) → 文本回复（真实适配器协议）
    let (port, server_thread) = spawn_sync(vec![
        ChatReply::ToolUse {
            id: "c1".into(),
            name: "todo_write".into(),
            arguments: r#"{"todos":[{"content":"演示任务","status":"in_progress"}]}"#.into(),
        },
        ChatReply::Text("已记录。".into()),
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args([
            "--config",
            yml.to_str().unwrap(),
            "--prompt",
            "帮我记一条演示 todo",
            "--llm-base-url",
            &format!("http://127.0.0.1:{port}/v1"),
            "--llm-model",
            "test-model",
            "--llm-api-key",
            "test-key",
            "--llm-no-stream",
        ])
        .output()
        .expect("spawn cos");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "dlopen 装载/运行失败: {stdout} {stderr}"
    );

    // 全链路走通：不变量全过、卸载顺序含 dlopen 插件
    assert!(stdout.contains("不变量: 全部通过"), "不变量未过: {stdout}");
    assert!(
        stdout.contains("dlopen 插件") || stdout.contains("plugin_todo_dlopen"),
        "卸载顺序应含 dlopen 插件: {stdout}"
    );

    // dlopen 工具经 C 回调执行的结果已回流会话日志（ToolOutcome JSON → 宿主解析）
    let log = std::fs::read_to_string(session_log).unwrap_or_default();
    assert!(
        log.contains("已写入 1 条任务"),
        "dlopen 工具结果未回流: {log}"
    );
    assert!(
        log.contains("\"todo_write\""),
        "会话日志应含 todo_write 调用: {log}"
    );
    // P9 服务直连：工具回调内经 get_service/service_call 查询宿主 tools 桥，
    // 工具清单数量已并入结果文本（此时仅 todo_write 一个工具）
    assert!(
        log.contains("tools=1"),
        "dlopen 工具应经 JSON 桥查询到宿主工具清单: {log}"
    );

    // finish 卸载逆序 → 插件 disposer 已执行（marker 文件）
    assert!(
        marker.exists(),
        "disposer 应已执行（效果卸载链）: {marker:?}"
    );

    let _ = std::fs::remove_file(&yml);
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(session_log);
    let _ = server_thread.join(); // 服务器脚本已消费完，线程收束
}
