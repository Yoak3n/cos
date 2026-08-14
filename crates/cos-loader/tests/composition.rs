//! P2 验收：loader-composition 集成测试。
//!
//! 真实 yaml 装配 3-4 个插件（fixtures/demo.yml）：断言服务存在、
//! 依赖序（llm 先于 agent-loop）、环/缺依赖/重复 provide 报错可读、卸载逆序。

use std::sync::{Mutex, OnceLock};

use cos_core::{Context, CoreError, EffectHandle, Plugin, Service, Validate};
use cos_loader::{self as loader, LoadError, Profile};
use serde::Deserialize;

// —— 服务 ——

struct LlmService {
    model: String,
}
impl Service for LlmService {
    const NAME: &'static str = "llm";
}

struct AgentLoopService;
impl Service for AgentLoopService {
    const NAME: &'static str = "agent-loop";
}

// —— 卸载日志（仅 happy-path 测试断言，文件内测试串行化防干扰）——

fn test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 取测试串行化锁；poison 恢复（单个测试 panic 不级联）。
fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
    test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unload_log() -> &'static Mutex<Vec<&'static str>> {
    static LOG: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Vec::new()))
}

fn push_unload(name: &'static str) {
    unload_log().lock().unwrap().push(name);
}

// —— 配置 ——

#[derive(Deserialize, Default)]
struct NoConfig;
impl Validate for NoConfig {}

#[derive(Deserialize)]
struct LlmConfig {
    #[serde(default)]
    model: String,
}
impl Validate for LlmConfig {}

#[derive(Deserialize)]
struct DemoConfig {
    #[serde(default)]
    message: String,
}
impl Validate for DemoConfig {
    fn validate(&self) -> Result<(), CoreError> {
        if self.message == "bad" {
            Err(CoreError::Other("message 不能为 bad".into()))
        } else {
            Ok(())
        }
    }
}

// —— 插件 ——

#[derive(Default)]
struct LlmPlugin;
impl Plugin for LlmPlugin {
    fn id(&self) -> &'static str {
        "plugin-llm"
    }
    type Config = LlmConfig;
    fn provide(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn apply(&self, ctx: &Context, config: &Self::Config) -> Result<(), CoreError> {
        ctx.provide(LlmService {
            model: config.model.clone(),
        })?;
        ctx.fiber().push(EffectHandle::new(|| push_unload("llm")));
        Ok(())
    }
}
loader::plugin!("llm", LlmPlugin);

#[derive(Default)]
struct AgentLoopPlugin;
impl Plugin for AgentLoopPlugin {
    fn id(&self) -> &'static str {
        "plugin-agent-loop"
    }
    type Config = NoConfig;
    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn provide(&self) -> &'static [&'static str] {
        &["agent-loop"]
    }
    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        // 依赖就绪才激活：apply 时 llm 必须已在仓库里。
        ctx.get::<LlmService>()?;
        ctx.provide(AgentLoopService)?;
        ctx.fiber()
            .push(EffectHandle::new(|| push_unload("agent-loop")));
        Ok(())
    }
}
loader::plugin!("agent-loop", AgentLoopPlugin);

#[derive(Default)]
struct TodoPlugin;
impl Plugin for TodoPlugin {
    fn id(&self) -> &'static str {
        "plugin-todo"
    }
    type Config = NoConfig;
    fn inject(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        ctx.get::<LlmService>()?;
        ctx.fiber().push(EffectHandle::new(|| push_unload("todo")));
        Ok(())
    }
}
loader::plugin!("todo", TodoPlugin);

#[derive(Default)]
struct DemoPlugin;
impl Plugin for DemoPlugin {
    fn id(&self) -> &'static str {
        "plugin-demo"
    }
    type Config = DemoConfig;
    fn apply(&self, ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        ctx.fiber().push(EffectHandle::new(|| push_unload("demo")));
        Ok(())
    }
}
loader::plugin!("demo", DemoPlugin);

#[derive(Default)]
struct FailingPlugin;
impl Plugin for FailingPlugin {
    fn id(&self) -> &'static str {
        "plugin-failing"
    }
    type Config = NoConfig;
    fn apply(&self, _ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        Err(CoreError::Other("boom".into()))
    }
}
loader::plugin!("failing", FailingPlugin);

#[derive(Default)]
struct CyclicA;
impl Plugin for CyclicA {
    fn id(&self) -> &'static str {
        "plugin-cycler-a"
    }
    type Config = NoConfig;
    fn inject(&self) -> &'static [&'static str] {
        &["svc-b"]
    }
    fn provide(&self) -> &'static [&'static str] {
        &["svc-a"]
    }
    fn apply(&self, _ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        Ok(())
    }
}
loader::plugin!("cycler-a", CyclicA);

#[derive(Default)]
struct CyclicB;
impl Plugin for CyclicB {
    fn id(&self) -> &'static str {
        "plugin-cycler-b"
    }
    type Config = NoConfig;
    fn inject(&self) -> &'static [&'static str] {
        &["svc-a"]
    }
    fn provide(&self) -> &'static [&'static str] {
        &["svc-b"]
    }
    fn apply(&self, _ctx: &Context, _config: &Self::Config) -> Result<(), CoreError> {
        Ok(())
    }
}
loader::plugin!("cycler-b", CyclicB);

// —— 条目默认值 ——

#[test]
fn entry_defaults_are_applied() {
    let profile = Profile::parse("- name: x\n").unwrap();
    let entry = &profile.0[0];
    assert_eq!(entry.id(), "x");
    assert_eq!(entry.config, serde_json::Value::Null);
    assert!(entry.inject.is_empty());
    assert!(!entry.disabled);
}

// —— 验收：真实 yaml 装配 ——

#[test]
fn loads_real_yaml_with_dependency_order() {
    let _guard = lock_tests();
    unload_log().lock().unwrap().clear();

    let profile = Profile::parse(include_str!("fixtures/demo.yml")).unwrap();
    let root = Context::root();
    let app = loader::load(&root, &profile).unwrap();

    let names: Vec<&str> = app.instances().iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["llm", "demo", "agent-loop", "todo"]);

    // 服务存在（依赖就绪才激活的产物）
    let llm = app.root().get::<LlmService>().unwrap();
    assert_eq!(llm.model, "mock");
    assert!(app.root().get::<AgentLoopService>().is_ok());

    // disabled 的未知插件被跳过
    assert!(!names.contains(&"nope"));

    // 卸载：apply 逆序
    app.dispose();
    assert_eq!(
        *unload_log().lock().unwrap(),
        vec!["todo", "agent-loop", "demo", "llm"]
    );
}

// —— 错误路径：全部 fail loud at load ——

#[test]
fn unknown_plugin_reports_available_names() {
    let _guard = lock_tests();
    let profile = Profile::parse("- name: nope\n").unwrap();
    let err = loader::load(&Context::root(), &profile).unwrap_err();
    match err {
        LoadError::UnknownPlugin { name, available } => {
            assert_eq!(name, "nope");
            assert!(available.contains(&"llm"));
            assert!(available.contains(&"agent-loop"));
        }
        other => panic!("期望 UnknownPlugin，实际 {other:?}"),
    }
}

#[test]
fn missing_dependency_reports_plugin_and_service() {
    let _guard = lock_tests();
    let profile = Profile::parse("- name: demo\n  inject: [missing-svc]\n").unwrap();
    let err = loader::load(&Context::root(), &profile).unwrap_err();
    match err {
        LoadError::MissingDependency {
            ref plugin,
            ref service,
        } => {
            assert_eq!(plugin.as_str(), "demo");
            assert_eq!(service.as_str(), "missing-svc");
        }
        other => panic!("期望 MissingDependency，实际 {other:?}"),
    }
    assert!(err.to_string().contains("missing-svc"));
    assert!(err.to_string().contains("demo"));
}

#[test]
fn dependency_cycle_reports_readable_error() {
    let _guard = lock_tests();
    let profile = Profile::parse("- name: cycler-a\n- name: cycler-b\n").unwrap();
    let err = loader::load(&Context::root(), &profile).unwrap_err();
    match err {
        LoadError::DependencyCycle { ref cycle } => {
            assert_eq!(cycle.len(), 2);
            assert!(cycle.contains(&"cycler-a".to_string()));
            assert!(cycle.contains(&"cycler-b".to_string()));
        }
        other => panic!("期望 DependencyCycle，实际 {other:?}"),
    }
    let text = err.to_string();
    assert!(text.contains("依赖环"), "报错应可读: {text}");
    assert!(text.contains("cycler-a"));
    assert!(text.contains("cycler-b"));
}

#[test]
fn duplicate_provide_is_rejected_with_both_ids() {
    let _guard = lock_tests();
    let profile = Profile::parse("- id: l1\n  name: llm\n- id: l2\n  name: llm\n").unwrap();
    let err = loader::load(&Context::root(), &profile).unwrap_err();
    match err {
        LoadError::DuplicateProvide { service, plugins } => {
            assert_eq!(service, "llm");
            assert_eq!(plugins, vec!["l1".to_string(), "l2".to_string()]);
        }
        other => panic!("期望 DuplicateProvide，实际 {other:?}"),
    }
}

#[test]
fn config_parse_error_is_attributed_to_entry() {
    let _guard = lock_tests();
    let profile = Profile::parse("- name: llm\n  config:\n    model: 123\n").unwrap();
    let err = loader::load(&Context::root(), &profile).unwrap_err();
    assert!(
        matches!(&err, LoadError::ConfigParse { id, name, .. } if id == "llm" && name == "plugin-llm"),
        "实际 {err:?}"
    );
    assert!(err.to_string().contains("failed to apply loader entry llm"));
}

#[test]
fn validation_failure_is_attributed_to_entry() {
    let _guard = lock_tests();
    let profile = Profile::parse("- name: demo\n  config:\n    message: bad\n").unwrap();
    let err = loader::load(&Context::root(), &profile).unwrap_err();
    assert!(
        matches!(&err, LoadError::ConfigInvalid { id, .. } if id == "demo"),
        "实际 {err:?}"
    );
    assert!(err.to_string().contains("bad"));
}

#[test]
fn apply_failure_rolls_back_already_applied_plugins() {
    let _guard = lock_tests();
    let profile = Profile::parse("- name: llm\n  config: {}\n- name: failing\n").unwrap();
    let root = Context::root();
    let err = loader::load(&root, &profile).unwrap_err();
    assert!(
        matches!(&err, LoadError::Apply { id, .. } if id == "failing"),
        "实际 {err:?}"
    );
    // 已 apply 的 llm 被回滚：服务不可见
    assert!(root.get::<LlmService>().is_err());
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 测试串行化锁：跨 await 持有是有意为之（与同步测试互斥共享日志）
async fn dispose_async_unloads_in_reverse_order() {
    let _guard = lock_tests();
    unload_log().lock().unwrap().clear();
    let profile = Profile::parse("- name: agent-loop\n- name: llm\n  config: {}\n").unwrap();
    let app = loader::load(&Context::root(), &profile).unwrap();
    app.dispose_async().await;
    assert_eq!(*unload_log().lock().unwrap(), vec!["agent-loop", "llm"]);
}
