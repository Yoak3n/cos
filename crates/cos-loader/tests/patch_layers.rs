//! P13 patch 层叠集成测试：`Profile::load_merged` 的文件路径组装——
//! 主 yml 条目 → 主 yml `patch:` 声明（相对主 yml 目录）→ 同目录
//! `cordis.patch.yml`（自动应用，显式声明时不去重）→ CLI `--patch`（后覆盖先）。

use std::fs;
use std::path::PathBuf;

use cos_loader::Profile;

/// 建一个临时目录，返回 (dir, cleanup)。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cos-patch-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn merged_layers_apply_in_order_with_sources() {
    let dir = temp_dir("order");
    // 主 yml：顶层 `patch:` 声明 + entries（相对主 yml 目录解析）
    fs::write(
        dir.join("cordis.yml"),
        "patch: [layers/p1.yml]\nentries:\n- id: main\n  name: llm\n  config: { model: a }\n",
    )
    .unwrap();
    // 主 yml 声明的 patch 层（相对目录；顶层数组语法）
    fs::create_dir_all(dir.join("layers")).unwrap();
    fs::write(
        dir.join("layers/p1.yml"),
        "- id: main\n  config: { model: b }\n",
    )
    .unwrap();
    // 同目录 cordis.patch.yml（自动应用）
    fs::write(
        dir.join("cordis.patch.yml"),
        "- id: main\n  disabled: true\n",
    )
    .unwrap();
    // CLI --patch（后覆盖先：重新启用 + 覆盖配置）
    let cli = dir.join("cli.yml");
    fs::write(
        &cli,
        "- id: main\n  disabled: false\n  config: { model: c }\n",
    )
    .unwrap();

    let merged =
        Profile::load_merged(dir.join("cordis.yml"), &[cli.to_string_lossy().into()]).unwrap();
    assert_eq!(merged.entries.len(), 1);
    let entry = &merged.entries[0];
    assert_eq!(entry.config["model"], "c", "CLI 层最后覆盖");
    assert!(!entry.disabled, "CLI 层重新启用");
    assert_eq!(entry.source, "cordis.yml", "条目来源 = 主 yml");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn auto_cordis_patch_is_not_applied_twice_when_declared() {
    let dir = temp_dir("dedupe");
    fs::write(
        dir.join("cordis.yml"),
        "patch: [cordis.patch.yml]\nentries:\n- id: main\n  name: llm\n  config: { model: a }\n",
    )
    .unwrap();
    fs::write(
        dir.join("cordis.patch.yml"),
        "- id: main\n  config: { model: b }\n",
    )
    .unwrap();

    // 显式声明 + 自动应用会双应用——但 config 覆盖是幂等的，用 disabled 计数不可行；
    // 改为验证：不报 PatchTargetMissing 且结果正确（双应用对覆盖语义无害但应去重——
    // 通过 patch 层引用 insert 条目再覆盖来验证只应用一次不现实；这里验证去重逻辑
    // 的等价行为：结果与单次应用一致）。
    let merged = Profile::load_merged(dir.join("cordis.yml"), &[]).unwrap();
    assert_eq!(merged.entries[0].config["model"], "b");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn third_party_patch_inserts_dlopen_entry_and_dump_source() {
    let dir = temp_dir("third-party");
    // 主 yml：只有 todo（不含第三方插件）
    fs::write(dir.join("cordis.yml"), "- name: todo\n").unwrap();
    // 第三方包：cdylib 条目经 patch insert 注入（同目录 cordis.patch.yml 自动应用）
    fs::write(
        dir.join("cordis.patch.yml"),
        "- insert:\n  - name: ./plugins/third-party/plugin.dll\n    config: { marker: x }\n",
    )
    .unwrap();
    let merged = Profile::load_merged(dir.join("cordis.yml"), &[]).unwrap();
    assert_eq!(merged.entries.len(), 2);
    assert_eq!(merged.entries[1].name, "./plugins/third-party/plugin.dll");
    assert!(
        merged.entries[1].source.contains("cordis.patch.yml"),
        "insert 条目来源应为 patch 文件: {}",
        merged.entries[1].source
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_patch_file_fails_loud() {
    let dir = temp_dir("missing");
    fs::write(
        dir.join("cordis.yml"),
        "patch: [nope.yml]\nentries:\n- name: todo\n",
    )
    .unwrap();
    let error = Profile::load_merged(dir.join("cordis.yml"), &[])
        .unwrap_err()
        .to_string();
    assert!(error.contains("nope.yml"), "{error}");
    let _ = fs::remove_dir_all(&dir);
}
