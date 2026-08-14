//! P1：卸载审计 —— 逆序、隔离、失效、InactiveFiber、异步 disposer。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dsh_core::{Context, CoreError, EffectHandle, EventPayload, Service};

struct RootSvc;
impl Service for RootSvc {
    const NAME: &'static str = "root-svc";
}

struct ForkSvc;
impl Service for ForkSvc {
    const NAME: &'static str = "fork-svc";
}

#[test]
fn disposing_fork_leaves_root_intact() {
    let ctx = Context::root();
    ctx.provide(RootSvc).unwrap();
    let root_hits = Arc::new(AtomicUsize::new(0));
    let r = root_hits.clone();
    ctx.on("ev", move |_| {
        r.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();

    let fork = ctx.fork();
    fork.provide(ForkSvc).unwrap();
    let fork_hits = Arc::new(AtomicUsize::new(0));
    let f = fork_hits.clone();
    fork.on("ev", move |_| {
        f.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();

    // 卸载前两边都收
    ctx.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(root_hits.load(Ordering::SeqCst), 1);
    assert_eq!(fork_hits.load(Ordering::SeqCst), 1);

    fork.fiber().dispose();

    // 卸载后：fork 的服务与监听器失效，根不受影响
    assert!(matches!(
        fork.get::<ForkSvc>(),
        Err(CoreError::ServiceNotFound("fork-svc"))
    ));
    assert!(ctx.get::<RootSvc>().is_ok());
    ctx.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(root_hits.load(Ordering::SeqCst), 2);
    assert_eq!(fork_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn registering_after_dispose_fails_loud() {
    let ctx = Context::root();
    let fork = ctx.fork();
    fork.fiber().dispose();

    assert!(matches!(
        fork.on("x", |_: &EventPayload| {}),
        Err(CoreError::InactiveFiber(_))
    ));
    assert!(matches!(
        fork.provide(ForkSvc),
        Err(CoreError::InactiveFiber(_))
    ));
    assert!(matches!(
        fork.on_bail("x", |_: &EventPayload| None),
        Err(CoreError::InactiveFiber(_))
    ));
    assert!(fork.fiber().is_disposed());
    assert!(!ctx.fiber().is_disposed()); // 兄弟 fiber 不受影响
}

#[test]
fn effect_handle_early_dispose_removes_only_itself() {
    let ctx = Context::root();
    let a_hits = Arc::new(AtomicUsize::new(0));
    let a = a_hits.clone();
    let handle = ctx
        .on("ev", move |_| {
            a.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    let b_hits = Arc::new(AtomicUsize::new(0));
    let b = b_hits.clone();
    ctx.on("ev", move |_| {
        b.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();

    handle.dispose(); // 提前反注册 A

    ctx.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(a_hits.load(Ordering::SeqCst), 0);
    assert_eq!(b_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn repeated_dispose_is_idempotent() {
    let ctx = Context::root();
    let fork = ctx.fork();
    let log = Arc::new(Mutex::new(Vec::new()));
    let l = log.clone();
    fork.fiber()
        .push(EffectHandle::new(move || l.lock().unwrap().push(1)));
    fork.fiber().dispose();
    fork.fiber().dispose();
    assert_eq!(*log.lock().unwrap(), vec![1]);
}

#[tokio::test]
async fn dispose_async_awaits_async_disposers() {
    let ctx = Context::root();
    let fork = ctx.fork();
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    fork.fiber().push_async(Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        d.store(true, Ordering::SeqCst);
    }));

    fork.fiber().dispose_async().await;
    assert!(done.load(Ordering::SeqCst), "异步 disposer 应被等待完成");
}

#[tokio::test]
async fn dispose_async_reverses_sync_then_waits_async() {
    let ctx = Context::root();
    let fork = ctx.fork();
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let l = log.clone();
    fork.fiber().push(EffectHandle::new(move || {
        l.lock().unwrap().push("s1".into())
    }));
    let l = log.clone();
    fork.fiber().push(EffectHandle::new(move || {
        l.lock().unwrap().push("s2".into())
    }));
    let l = log.clone();
    fork.fiber().push_async(Box::pin(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        l.lock().unwrap().push("a".into());
    }));

    fork.fiber().dispose_async().await;
    assert_eq!(
        *log.lock().unwrap(),
        vec!["s2".to_string(), "s1".to_string(), "a".to_string()]
    );
}
