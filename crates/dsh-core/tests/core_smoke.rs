//! P0 冒烟测试：事件五种分发、fiber 逆序卸载、监听器随 fiber 失效、同名服务冲突。
//! 完整语义矩阵见 events_dispatch.rs / scope_routing.rs / unload_audit.rs（P1）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use dsh_core::{Context, CoreError, EffectHandle, EventPayload, Service};

struct ServiceA;
impl Service for ServiceA {
    const NAME: &'static str = "dup";
}

struct ServiceB;
impl Service for ServiceB {
    const NAME: &'static str = "dup";
}

#[test]
fn duplicate_service_name_conflicts() {
    let ctx = Context::root();
    ctx.provide(ServiceA).unwrap();
    assert!(matches!(
        ctx.provide(ServiceB),
        Err(CoreError::DuplicateService("dup"))
    ));
    assert!(matches!(
        ctx.provide(ServiceA),
        Err(CoreError::DuplicateService("dup"))
    ));
}

#[test]
fn emit_dispatches_synchronously_and_ignores_returns() {
    let ctx = Context::root();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    ctx.on("ping", move |payload| {
        let text = payload.downcast_ref::<String>().expect("载荷应为 String");
        assert_eq!(text, "hi");
        counter.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    ctx.emit("ping", Arc::new("hi".to_string()));
    ctx.emit("ping", Arc::new("hi".to_string()));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}

#[test]
fn bail_is_synchronous_first_non_none() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let o = order.clone();
    ctx.on_bail("b", move |_| {
        o.lock().unwrap().push(1);
        Some(Arc::new(7u32) as EventPayload)
    })
    .unwrap();
    let o = order.clone();
    ctx.on_bail("b", move |_| {
        o.lock().unwrap().push(2);
        None
    })
    .unwrap();
    let value = ctx.bail("b", Arc::new(0u32) as EventPayload).unwrap();
    assert_eq!(*value.downcast_ref::<u32>().unwrap(), 7);
    assert_eq!(*order.lock().unwrap(), vec![1]);
}

#[tokio::test]
async fn serial_returns_first_bail_value_in_order() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));

    let o = order.clone();
    ctx.on_serial("s", move |payload| {
        o.lock().unwrap().push(1);
        let n = *payload.downcast_ref::<u32>().unwrap();
        Box::pin(async move {
            if n >= 1 {
                Ok(Some(Arc::new(10u32) as EventPayload))
            } else {
                Ok(None)
            }
        })
    })
    .unwrap();
    let o = order.clone();
    ctx.on_serial("s", move |_| {
        o.lock().unwrap().push(2);
        Box::pin(async move { Ok(None) })
    })
    .unwrap();

    let result = ctx
        .serial("s", Arc::new(1u32) as EventPayload)
        .await
        .unwrap();
    assert_eq!(*result.unwrap().downcast_ref::<u32>().unwrap(), 10);
    assert_eq!(*order.lock().unwrap(), vec![1]); // 第二个监听器未执行
}

#[tokio::test]
async fn parallel_awaits_all_and_aggregates_errors() {
    let ctx = Context::root();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    ctx.on_parallel("p", move |_| {
        h.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    })
    .unwrap();
    ctx.on_parallel("p", |_| {
        Box::pin(async { Err(CoreError::ServiceNotFound("nope")) })
    })
    .unwrap();
    let err = ctx
        .parallel("p", Arc::new(()) as EventPayload)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::ListenerAggregate("p", ref errors) if errors.len() == 1),
        "应聚合 1 个错误: {err:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn waterfall_vetoes_without_next_and_delegates_default() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));

    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(1);
        d.set_value(d.value() + 1);
        Box::pin(async move { d.next().await })
    })
    .unwrap();
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |_d| {
        o.lock().unwrap().push(2);
        Box::pin(async move { 42 }) // 短路：不调 next() 直接返回值
    })
    .unwrap();
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(3);
        Box::pin(async move { d.next().await })
    })
    .unwrap();

    let result = ctx
        .waterfall("w", 1u32, |d| Box::pin(async move { d.value() + 100 }))
        .await
        .unwrap();
    assert_eq!(result, 42);
    assert_eq!(*order.lock().unwrap(), vec![1, 2]); // 3 与默认行为未执行
}

#[tokio::test]
async fn waterfall_runs_default_as_chain_tail() {
    let ctx = Context::root();
    ctx.on_waterfall::<String, String>("wf", |d| Box::pin(async move { d.next().await }))
        .unwrap();
    let result = ctx
        .waterfall("wf", "hello".to_string(), |d| {
            Box::pin(async move { format!("{}-default", d.value()) })
        })
        .await
        .unwrap();
    assert_eq!(result, "hello-default");
}

#[test]
fn fiber_disposes_in_reverse_order() {
    let ctx = Context::root();
    let fork = ctx.fork();
    let log = Arc::new(Mutex::new(Vec::new()));
    for i in 0..3 {
        let log = log.clone();
        fork.fiber()
            .push(EffectHandle::new(move || log.lock().unwrap().push(i)));
    }
    fork.fiber().dispose();
    assert_eq!(*log.lock().unwrap(), vec![2, 1, 0]);
    // dispose 幂等
    fork.fiber().dispose();
    assert_eq!(*log.lock().unwrap(), vec![2, 1, 0]);
}

#[test]
fn listener_expires_with_owning_fiber() {
    let ctx = Context::root();
    let fork = ctx.fork();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    fork.on("ev", move |_| {
        h.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    fork.emit("ev", Arc::new(1u32) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    fork.fiber().dispose();
    fork.emit("ev", Arc::new(1u32) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
