//! P1：五种分发的完整语义矩阵（语义权威：`vendor/cordis/src/events.ts`）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cos_core::{Context, CoreError, EventPayload};

// —— emit：注册序、同步、返回值忽略 ——

#[test]
fn emit_runs_in_registration_order_synchronously() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    for i in 0..5 {
        let o = order.clone();
        ctx.on("e", move |_| o.lock().unwrap().push(i)).unwrap();
    }
    ctx.emit("e", Arc::new(()) as EventPayload);
    assert_eq!(*order.lock().unwrap(), vec![0, 1, 2, 3, 4]);
}

#[test]
fn emit_payload_is_shared_reference() {
    // 监听器收到的是同一份 Arc（同一地址）。
    let ctx = Context::root();
    let payload: EventPayload = Arc::new(9u32);
    let seen = Arc::new(Mutex::new(Vec::<usize>::new()));
    let s = seen.clone();
    ctx.on("e", move |p| {
        s.lock().unwrap().push(Arc::as_ptr(p) as *const () as usize);
    })
    .unwrap();
    ctx.emit("e", payload.clone());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![Arc::as_ptr(&payload) as *const () as usize]
    );
}

// —— parallel：全并发、等待全部、聚合错误 ——

#[tokio::test]
async fn parallel_runs_listeners_concurrently_and_waits_all() {
    let ctx = Context::root();
    for _ in 0..3 {
        ctx.on_parallel("p", |_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(60)).await;
                Ok(())
            })
        })
        .unwrap();
    }
    let start = std::time::Instant::now();
    ctx.parallel("p", Arc::new(()) as EventPayload)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(150),
        "串行执行会 ≥180ms，实际 {elapsed:?}（应为并发）"
    );
}

#[tokio::test]
async fn parallel_aggregates_all_errors_alongside_successes() {
    let ctx = Context::root();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    ctx.on_parallel("p", move |_| {
        h.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    })
    .unwrap();
    ctx.on_parallel("p", |_| {
        Box::pin(async { Err(CoreError::ServiceNotFound("a")) })
    })
    .unwrap();
    ctx.on_parallel("p", |_| {
        Box::pin(async { Err(CoreError::ServiceNotFound("b")) })
    })
    .unwrap();

    let err = ctx
        .parallel("p", Arc::new(()) as EventPayload)
        .await
        .unwrap_err();
    assert!(
        matches!(err, CoreError::ListenerAggregate("p", ref errors) if errors.len() == 2),
        "应聚合 2 个错误: {err:?}"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1); // 成功者也跑完
}

#[tokio::test]
async fn parallel_with_no_listeners_is_ok() {
    let ctx = Context::root();
    ctx.parallel("none", Arc::new(()) as EventPayload)
        .await
        .unwrap();
}

// —— serial：异步按序、首个 bail 值、错误传播 ——

#[tokio::test]
async fn serial_returns_none_when_no_listener_bails() {
    let ctx = Context::root();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    ctx.on_serial("s", move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    })
    .unwrap();
    let c = calls.clone();
    ctx.on_serial("s", move |_| {
        c.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    })
    .unwrap();

    let result = ctx.serial("s", Arc::new(()) as EventPayload).await.unwrap();
    assert!(result.is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 2); // 全部执行
}

#[tokio::test]
async fn serial_propagates_first_error_and_stops() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let o = order.clone();
    ctx.on_serial("s", move |_| {
        o.lock().unwrap().push(1);
        Box::pin(async { Err(CoreError::ServiceNotFound("boom")) })
    })
    .unwrap();
    let o = order.clone();
    ctx.on_serial("s", move |_| {
        o.lock().unwrap().push(2);
        Box::pin(async { Ok(None) })
    })
    .unwrap();

    let err = ctx
        .serial("s", Arc::new(()) as EventPayload)
        .await
        .unwrap_err();
    assert_eq!(err, CoreError::ServiceNotFound("boom"));
    assert_eq!(*order.lock().unwrap(), vec![1]); // 错误后不再继续
}

// —— bail：同步按序、首个非 None ——

#[test]
fn bail_skips_listeners_after_first_non_none() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let o = order.clone();
    ctx.on_bail("b", move |_| {
        o.lock().unwrap().push(1);
        None
    })
    .unwrap();
    let o = order.clone();
    ctx.on_bail("b", move |_| {
        o.lock().unwrap().push(2);
        Some(Arc::new("v".to_string()) as EventPayload)
    })
    .unwrap();
    let o = order.clone();
    ctx.on_bail("b", move |_| {
        o.lock().unwrap().push(3);
        None
    })
    .unwrap();

    let value = ctx.bail("b", Arc::new(()) as EventPayload).unwrap();
    assert_eq!(*value.downcast_ref::<String>().unwrap(), "v".to_string());
    assert_eq!(*order.lock().unwrap(), vec![1, 2]); // 3 未执行
}

#[test]
fn bail_with_no_listeners_returns_none() {
    let ctx = Context::root();
    assert!(ctx.bail("none", Arc::new(()) as EventPayload).is_none());
}

// —— waterfall：链序、变换、短路、默认行为 ——

#[tokio::test]
async fn waterfall_transforms_value_through_chain() {
    let ctx = Context::root();
    ctx.on_waterfall::<u32, u32>("w", |d| {
        d.set_value(d.value() + 1);
        Box::pin(async move { d.next().await })
    })
    .unwrap();
    ctx.on_waterfall::<u32, u32>("w", |d| {
        d.set_value(d.value() * 10);
        Box::pin(async move { d.next().await })
    })
    .unwrap();

    let result = ctx
        .waterfall("w", 1u32, |d| Box::pin(async move { d.value() + 100 }))
        .await
        .unwrap();
    assert_eq!(result, 120); // (1+1)*10 + 100
}

#[tokio::test]
async fn waterfall_break_short_circuits_chain_and_default() {
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let default_ran = Arc::new(AtomicUsize::new(0));

    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(1);
        Box::pin(async move { d.next().await })
    })
    .unwrap();
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |_d| {
        o.lock().unwrap().push(2);
        Box::pin(async move { 42 }) // veto：不调 next()
    })
    .unwrap();
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(3);
        Box::pin(async move { d.next().await })
    })
    .unwrap();

    let d = default_ran.clone();
    let result = ctx
        .waterfall("w", 1u32, move |_| {
            d.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { 999u32 })
        })
        .await
        .unwrap();
    assert_eq!(result, 42);
    assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    assert_eq!(default_ran.load(Ordering::SeqCst), 0); // 默认行为被 veto
}

#[tokio::test]
async fn waterfall_return_without_next_is_veto() {
    // 不调 next() 直接返回当前值 → 短路：后续监听器与默认行为都不执行。
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(1);
        Box::pin(async move { d.next().await })
    })
    .unwrap();
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(2);
        Box::pin(async move { *d.value() }) // 静默短路：沿用当前值
    })
    .unwrap();
    let o = order.clone();
    ctx.on_waterfall::<u32, u32>("w", move |d| {
        o.lock().unwrap().push(3);
        Box::pin(async move { d.next().await })
    })
    .unwrap();

    let result = ctx
        .waterfall("w", 5u32, |d| Box::pin(async move { d.value() + 1 }))
        .await
        .unwrap();
    assert_eq!(result, 5); // 默认行为未执行，值未变
    assert_eq!(*order.lock().unwrap(), vec![1, 2]);
}

#[tokio::test]
async fn waterfall_veto_value_flows_outward() {
    // 深层监听器返回的值一路向外透传。
    let ctx = Context::root();
    let order = Arc::new(Mutex::new(Vec::new()));
    for i in 1..=3 {
        let o = order.clone();
        if i < 3 {
            ctx.on_waterfall::<u32, u32>("w", move |d| {
                o.lock().unwrap().push(i);
                Box::pin(async move { d.next().await })
            })
            .unwrap();
        } else {
            ctx.on_waterfall::<u32, u32>("w", move |_d| {
                o.lock().unwrap().push(i);
                Box::pin(async move { 99 })
            })
            .unwrap();
        }
    }

    let result = ctx
        .waterfall("w", 0u32, |_| Box::pin(async { 1u32 }))
        .await
        .unwrap();
    assert_eq!(result, 99);
    assert_eq!(*order.lock().unwrap(), vec![1, 2, 3]);
}

#[tokio::test]
async fn waterfall_no_listeners_runs_default() {
    let ctx = Context::root();
    let result = ctx
        .waterfall("w", "x".to_string(), |d| {
            Box::pin(async move { format!("{}-default", d.value()) })
        })
        .await
        .unwrap();
    assert_eq!(result, "x-default");
}
