//! P8 试点端到端：spawn 真实二进制 + 临时 yml（`name: ./target/debug/plugin_todo_dlopen.dll`），
//! demo 模式（mock LLM 调 todo_write）→ **dlopen 工具**经 C 回调执行 → 结果回流会话日志；
//! `finish` 卸载时插件 disposer 写 marker 文件（验证效果卸载链 + 配置 JSON 传递）。
//!
//! 前置：`cargo test` 会先构建 workspace 全部成员（含 cdylib），
//! `target/debug/plugin_todo_dlopen.dll` 在测试运行时已存在。

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
            "- name: ./target/debug/plugin_todo_dlopen.dll\n  config:\n    marker: {}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    // 会话默认落盘 sessions/demo.jsonl（验证工具结果回流）
    let session_log = "sessions/demo.jsonl";
    let _ = std::fs::remove_file(session_log);

    let output = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args([
            "--config",
            yml.to_str().unwrap(),
            "--prompt",
            "帮我记一条演示 todo",
        ])
        .output()
        .expect("spawn cos");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "dlopen 装载/运行失败: {stdout} {stderr}"
    );

    // 全链路走通：会话 22 事件、不变量全过、卸载顺序含 dlopen 插件
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

    // finish 卸载逆序 → 插件 disposer 已执行（marker 文件）
    assert!(
        marker.exists(),
        "disposer 应已执行（效果卸载链）: {marker:?}"
    );

    let _ = std::fs::remove_file(&yml);
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_file(session_log);
}
