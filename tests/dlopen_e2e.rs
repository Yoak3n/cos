//! P8 试点端到端（P9 服务直连验证）：spawn 真实二进制 + 临时 yml
//! （opencode Provider 插件 + `name: ./target/debug/plugin_todo_dlopen.{dll,so}`），
//! `--llm-*` 指向本地回环 chat/completions 服务器（脚本：tool_use 调 todo_write → 文本）
//! → **dlopen 工具**经 C 回调执行（内含 get_service/service_call 查询宿主 tools 桥）→
//! 结果回流会话日志；`finish` 卸载时插件 disposer 写 marker 文件
//! （验证效果卸载链 + 配置 JSON 传递）。
//!
//! 产物定位：按平台取 `target/debug/plugin_todo_dlopen.{dll,so,dylib}`（相对测试
//! cwd = 工作区根）；`cargo test` 的 test profile 产物带哈希、不产生未哈希文件名，
//! 故缺失时先 spawn `cargo build -p plugin-todo-dlopen` 生成（dev profile 全量构建亦可）。

use cos_test_support::{ChatReply, spawn_sync};
use std::path::Path;
use std::process::Command;

#[test]
fn dlopen_plugin_loads_tool_executes_and_disposes() {
    // dlopen 插件产物：平台相关后缀；缺失则先构建（CI 冷启动场景）
    let artifact_name = if cfg!(windows) {
        "plugin_todo_dlopen.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_todo_dlopen.dylib"
    } else {
        "libplugin_todo_dlopen.so"
    };
    let artifact = Path::new("target/debug").join(artifact_name);
    if !artifact.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "plugin-todo-dlopen"])
            .status()
            .expect("spawn cargo build");
        assert!(
            status.success(),
            "cargo build -p plugin-todo-dlopen 失败: {status}"
        );
        assert!(artifact.exists(), "构建后产物缺失: {artifact:?}");
    }

    let marker = std::env::temp_dir().join(format!("cos-dlopen-marker-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let yml = std::env::temp_dir().join(format!("cos-dlopen-{}.yml", std::process::id()));
    // 相对路径相对测试 cwd（= 工作区根）；配置 JSON 原样传给插件（marker 路径）。
    // dlopen 插件声明 inject ["tools","todo-store"]（todo-store 由静态 todo 插件提供）——
    // yml 里 dlopen 在 todo **之前**，依赖边应把 todo 拓扑提前。
    std::fs::write(
        &yml,
        format!(
            "- name: ./target/debug/{artifact_name}\n  config:\n    marker: {}\n- name: opencode-provider\n- name: todo\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    // 会话默认落盘 sessions/demo.jsonl（验证工具结果回流）
    let session_log = "sessions/demo.jsonl";
    let _ = std::fs::remove_file(session_log);

    // 本地 chat/completions 服务器：tool_use(dlopen_todo) → 文本回复（真实适配器协议）
    let (port, server_thread) = spawn_sync(vec![
        ChatReply::ToolUse {
            id: "c1".into(),
            name: "dlopen_todo".into(),
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
    // P10 依赖图：dlopen 清单 inject ["todo-store"]（静态 todo 提供）→ 拓扑排序
    // 把 todo 提前——卸载逆序里 dlopen 在 todo 之前（即 apply 序 todo 先于 dlopen）
    let dlopen_name = format!("./target/debug/{artifact_name}");
    assert!(
        stdout.contains(&format!("\"{dlopen_name}\", \"todo\"")),
        "dlopen 依赖边应使 todo 先装载（卸载逆序 dlopen 在前）: {stdout}"
    );

    // dlopen 工具经 C 回调执行的结果已回流会话日志（ToolOutcome JSON → 宿主解析）
    let log = std::fs::read_to_string(session_log).unwrap_or_default();
    assert!(
        log.contains("已写入 1 条任务"),
        "dlopen 工具结果未回流: {log}"
    );
    assert!(
        log.contains("\"dlopen_todo\""),
        "会话日志应含 dlopen_todo 调用: {log}"
    );
    // P9 服务直连：工具回调内经 get_service/service_call 查询宿主 tools 桥，
    // 工具清单数量已并入结果文本（此时 todo_write + dlopen_todo 共 2 个）——
    // 能力裁剪（清单 inject ["tools"]）不阻断注入服务的访问
    assert!(
        log.contains("tools=2"),
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

/// P10 清单一等公民：--dump-config 计划里 dlopen 条目带清单 inject/provide（依赖图参与可见）。
#[test]
fn dlopen_manifest_shows_in_dump_config() {
    let artifact_name = if cfg!(windows) {
        "plugin_todo_dlopen.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_todo_dlopen.dylib"
    } else {
        "libplugin_todo_dlopen.so"
    };
    let artifact = Path::new("target/debug").join(artifact_name);
    if !artifact.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "plugin-todo-dlopen"])
            .status()
            .expect("spawn cargo build");
        assert!(status.success(), "构建 dlopen 插件失败: {status}");
    }
    let yml = std::env::temp_dir().join(format!("cos-dlopen-dump-{}.yml", std::process::id()));
    std::fs::write(
        &yml,
        format!("- name: ./target/debug/{artifact_name}\n- name: todo\n"),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args(["--config", yml.to_str().unwrap(), "--dump-config"])
        .output()
        .expect("spawn cos");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "dump-config 失败: {stdout}");
    let dump: serde_json::Value = serde_json::from_str(stdout.trim()).expect("dump 应为 JSON 数组");
    let entries = dump.as_array().expect("计划数组");
    let dlopen = entries
        .iter()
        .find(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|n| n.contains("plugin_todo_dlopen"))
        })
        .expect("计划应含 dlopen 条目");
    // 清单参与依赖图：inject/provide 出现在计划 JSON 中
    assert_eq!(dlopen["inject"][0], "tools", "{dlopen}");
    assert_eq!(dlopen["inject"][1], "todo-store", "{dlopen}");
    assert_eq!(dlopen["provide"][0], "dlopen-todo", "{dlopen}");
    let _ = std::fs::remove_file(&yml);
}

/// P12 暴露面审计：`cos_plugin_validate`（可选入口）兑现——配置预校验失败 → 启动
/// fail loud（错误文本含插件名与插件写的校验消息），不再是无操作死契约。
#[test]
fn dlopen_validate_rejects_bad_config() {
    let artifact_name = if cfg!(windows) {
        "plugin_todo_dlopen.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_todo_dlopen.dylib"
    } else {
        "libplugin_todo_dlopen.so"
    };
    let artifact = Path::new("target/debug").join(artifact_name);
    if !artifact.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "plugin-todo-dlopen"])
            .status()
            .expect("spawn cargo build");
        assert!(status.success(), "构建 dlopen 插件失败: {status}");
    }
    // 配置不是 JSON 对象（标量字符串）→ 薄壳 validate 返回 ConfigInvalid
    let yml = std::env::temp_dir().join(format!("cos-dlopen-validate-{}.yml", std::process::id()));
    std::fs::write(
        &yml,
        format!("- name: ./target/debug/{artifact_name}\n  config: \"不是对象\"\n"),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args(["--config", yml.to_str().unwrap(), "--prompt", "hi"])
        .output()
        .expect("spawn cos");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "配置预校验失败应 fail loud: {stderr}"
    );
    assert!(
        stderr.contains("配置校验失败"),
        "错误应归因到 validate 入口: {stderr}"
    );
    assert!(
        stderr.contains("配置必须是 JSON 对象"),
        "插件写的校验消息应透出: {stderr}"
    );
    let _ = std::fs::remove_file(&yml);
}

/// P12 暴露面审计回归：纯 RAII 卸载路径（装载成功 → 后续装配失败 → 应用 drop，
/// 不调 finish/dispose_async）——dlopen 插件效果（disposer 等）必须在**库卸载前**
/// 逆序注销，否则执行已卸载代码 → 访问违例（曾复现 0xC0000005）。
/// 此处断言：无 LLM 配置 → 干净失败（关键组件缺失），而非崩溃。
#[test]
fn dlopen_raii_unload_without_llm_fails_cleanly() {
    let artifact_name = if cfg!(windows) {
        "plugin_todo_dlopen.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_todo_dlopen.dylib"
    } else {
        "libplugin_todo_dlopen.so"
    };
    let artifact = Path::new("target/debug").join(artifact_name);
    if !artifact.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "plugin-todo-dlopen"])
            .status()
            .expect("spawn cargo build");
        assert!(status.success(), "构建 dlopen 插件失败: {status}");
    }
    let marker = std::env::temp_dir().join(format!("cos-raii-marker-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let yml = std::env::temp_dir().join(format!("cos-raii-{}.yml", std::process::id()));
    std::fs::write(
        &yml,
        format!(
            "- name: ./target/debug/{artifact_name}\n  config:\n    marker: {}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args(["--config", yml.to_str().unwrap(), "--prompt", "hi"])
        .output()
        .expect("spawn cos");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "RAII 卸载路径应干净失败（非崩溃）: {stderr}"
    );
    assert!(
        stderr.contains("关键组件缺失：未配置 LLM"),
        "失败原因应为无 LLM 配置: {stderr}"
    );
    // 卸载顺序不变量：disposer 在库卸载前已执行（marker 写入证明效果链跑完）
    assert!(
        marker.exists(),
        "RAII 卸载应已执行插件 disposer: {marker:?}"
    );
    let _ = std::fs::remove_file(&yml);
    let _ = std::fs::remove_file(&marker);
}

/// P13 第三方 patch 层叠：主 cordis.yml 只含 todo（不含 B 插件），第三方包
/// `third-party/cordis.patch.yml` 经 insert 注入 dlopen 条目——`--dump-config`
/// 输出**合并后的完整列表**（含 `source` 来源标注），与装载共用同一路径。
#[test]
fn dlopen_patch_injects_third_party_plugin() {
    let artifact_name = if cfg!(windows) {
        "plugin_todo_dlopen.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_todo_dlopen.dylib"
    } else {
        "libplugin_todo_dlopen.so"
    };
    let artifact = Path::new("target/debug").join(artifact_name);
    if !artifact.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "plugin-todo-dlopen"])
            .status()
            .expect("spawn cargo build");
        assert!(status.success(), "构建 dlopen 插件失败: {status}");
    }
    let dir = std::env::temp_dir().join(format!("cos-patch-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("third-party")).unwrap();
    // 第三方包：自带 cordis.patch.yml（dlopen 条目 insert；相对 cwd 的产物路径）
    std::fs::write(
        dir.join("third-party/cordis.patch.yml"),
        format!(
            "- insert:\n  - name: ./target/debug/{artifact_name}\n    config:\n      marker: patched-marker.txt\n"
        ),
    )
    .unwrap();
    // 主 yml：顶层 `patch:` 声明（相对主 yml 目录）+ 自己的条目
    let main = dir.join("cordis.yml");
    std::fs::write(
        &main,
        "patch: [third-party/cordis.patch.yml]\nentries:\n- name: todo\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_cos"))
        .args(["--config", main.to_str().unwrap(), "--dump-config"])
        .output()
        .expect("spawn cos");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "dump-config 失败: {stdout}");
    let dump: serde_json::Value = serde_json::from_str(stdout.trim()).expect("dump 应为 JSON 数组");
    let entries = dump.as_array().expect("计划数组");
    // 完整列表 = 主 yml 条目 + patch insert 条目
    assert_eq!(
        entries.len(),
        2,
        "完整列表应含主 yml + patch 注入条目: {dump}"
    );
    let todo = entries
        .iter()
        .find(|entry| entry["name"] == "todo")
        .expect("应有 todo 条目");
    assert_eq!(todo["source"], "cordis.yml", "{todo}");
    let dlopen = entries
        .iter()
        .find(|entry| {
            entry["name"]
                .as_str()
                .is_some_and(|n| n.contains("plugin_todo_dlopen"))
        })
        .expect("patch insert 的 dlopen 条目应在完整列表中");
    assert!(
        dlopen["source"]
            .as_str()
            .is_some_and(|s| s.contains("third-party/cordis.patch.yml")),
        "insert 条目来源应标注为第三方 patch: {dlopen}"
    );
    // 清单照常参与依赖图（patch 注入的条目也是完整计划成员）
    assert_eq!(dlopen["inject"][0], "tools", "{dlopen}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file("patched-marker.txt");
}
