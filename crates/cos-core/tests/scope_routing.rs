//! P1：scope 事件路由（语义权威：`packages/core/scope/src/index.ts`）。
//!
//! 规则：
//! - `Key(k)` 分发：无 tag 监听器全收；tag 位于 k 祖先链（含 k）的监听器收；
//! - 事件只向上流：祖先监听子孙的广播，子孙不监听祖先；
//! - `None`（unkeyed carrier）：仅无 tag 监听器；`All`：不过滤。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cos_core::{Context, CoreError, EventPayload, ScopeKey, ScopeTarget};

fn key(name: &str) -> ScopeKey {
    ScopeKey::new(name)
}

fn hit_counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

#[test]
fn untagged_listener_receives_scoped_dispatch() {
    let ctx = Context::root();
    let hits = hit_counter();
    let h = hits.clone();
    ctx.on("ev", move |_| {
        h.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();

    let target = ctx.target(ScopeTarget::Key(key("session:a")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn tagged_listener_receives_own_key_dispatch() {
    let ctx = Context::root();
    let scoped = ctx.fork_scoped(key("a"));
    let hits = hit_counter();
    let h = hits.clone();
    scoped
        .on("ev", move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    let target = ctx.target(ScopeTarget::Key(key("a")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn ancestor_listener_receives_descendant_dispatch() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("child"), key("parent")).unwrap();
    let parent_ctx = ctx.fork_scoped(key("parent"));
    let hits = hit_counter();
    let h = hits.clone();
    parent_ctx
        .on("ev", move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    // 在子 scope 上广播 → 父 scope 的监听器收到（事件向上流）
    let target = ctx.target(ScopeTarget::Key(key("child")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn descendant_listener_does_not_receive_ancestor_dispatch() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("child"), key("parent")).unwrap();
    let child_ctx = ctx.fork_scoped(key("child"));
    let hits = hit_counter();
    let h = hits.clone();
    child_ctx
        .on("ev", move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    // 在父 scope 上广播 → 子 scope 的监听器收不到（事件不向下流）
    let target = ctx.target(ScopeTarget::Key(key("parent")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn sibling_scopes_are_isolated() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("child1"), key("parent")).unwrap();
    ctx.bind_scope_parent(key("child2"), key("parent")).unwrap();
    let child1 = ctx.fork_scoped(key("child1"));
    let hits = hit_counter();
    let h = hits.clone();
    child1
        .on("ev", move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    let target = ctx.target(ScopeTarget::Key(key("child2")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 0); // 兄弟 scope 隔离
}

#[test]
fn grandparent_hears_grandchild() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("child"), key("parent")).unwrap();
    ctx.bind_scope_parent(key("grandchild"), key("child"))
        .unwrap();
    let grandparent = ctx.fork_scoped(key("parent"));
    let hits = hit_counter();
    let h = hits.clone();
    grandparent
        .on("ev", move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    let target = ctx.target(ScopeTarget::Key(key("grandchild")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn unscoped_dispatch_excludes_tagged_listeners() {
    let ctx = Context::root();
    let tagged = ctx.fork_scoped(key("a"));

    let untagged_hits = hit_counter();
    let u = untagged_hits.clone();
    ctx.on("ev", move |_| {
        u.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    let tagged_hits = hit_counter();
    let t = tagged_hits.clone();
    tagged
        .on("ev", move |_| {
            t.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    let target = ctx.target(ScopeTarget::None);
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(untagged_hits.load(Ordering::SeqCst), 1);
    assert_eq!(tagged_hits.load(Ordering::SeqCst), 0);
}

#[test]
fn all_dispatch_reaches_everyone() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("child"), key("parent")).unwrap();
    let parent = ctx.fork_scoped(key("parent"));
    let child = ctx.fork_scoped(key("child"));

    let counts = (hit_counter(), hit_counter(), hit_counter());
    let (a, _, _) = counts.clone();
    ctx.on("ev", move |_| {
        a.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    let (_, b, _) = counts.clone();
    parent
        .on("ev", move |_| {
            b.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
    let (_, _, c) = counts.clone();
    child
        .on("ev", move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    let target = ctx.target(ScopeTarget::All);
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(counts.0.load(Ordering::SeqCst), 1);
    assert_eq!(counts.1.load(Ordering::SeqCst), 1);
    assert_eq!(counts.2.load(Ordering::SeqCst), 1);
}

#[test]
fn fork_inherits_scope_tag_and_fork_scoped_overrides() {
    let ctx = Context::root();
    assert!(ctx.scope_of().is_none());

    let a = ctx.fork_scoped(key("a"));
    assert_eq!(a.scope_of(), Some(&key("a")));

    let a_child = a.fork(); // 继承 tag
    assert_eq!(a_child.scope_of(), Some(&key("a")));

    let b = a.fork_scoped(key("b")); // 重新打 tag
    assert_eq!(b.scope_of(), Some(&key("b")));
}

#[test]
fn bind_scope_parent_rejects_cycles() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("a"), key("b")).unwrap();
    assert!(matches!(
        ctx.bind_scope_parent(key("b"), key("a")),
        Err(CoreError::ScopeParentCycle(..))
    ));
}

#[test]
fn bind_scope_parent_rejects_second_bind() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("a"), key("b")).unwrap();
    assert!(matches!(
        ctx.bind_scope_parent(key("a"), key("c")),
        Err(CoreError::ScopeParentAlreadyBound(key)) if key == ScopeKey::new("a")
    ));
}

#[test]
fn scope_chain_of_is_nearest_first() {
    let ctx = Context::root();
    ctx.bind_scope_parent(key("c"), key("b")).unwrap();
    ctx.bind_scope_parent(key("b"), key("a")).unwrap();
    assert_eq!(
        ctx.scope_chain_of(&key("c")),
        vec![key("c"), key("b"), key("a")]
    );
    assert_eq!(ctx.scope_chain_of(&key("a")), vec![key("a")]);
}

#[test]
fn scoped_dispatch_reaches_untagged_and_own_tag_only() {
    // 组合断言：Key(a) 分发下，无 tag 收、a 收、无关 tag 不收。
    let ctx = Context::root();
    let other = ctx.fork_scoped(key("other"));
    let own = ctx.fork_scoped(key("a"));

    let hits = (hit_counter(), hit_counter(), hit_counter());
    let (h0, _, _) = hits.clone();
    ctx.on("ev", move |_| {
        h0.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    let (_, h1, _) = hits.clone();
    own.on("ev", move |_| {
        h1.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    let (_, _, h2) = hits.clone();
    other
        .on("ev", move |_| {
            h2.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();

    let target = ctx.target(ScopeTarget::Key(key("a")));
    target.emit("ev", Arc::new(()) as EventPayload);
    assert_eq!(hits.0.load(Ordering::SeqCst), 1);
    assert_eq!(hits.1.load(Ordering::SeqCst), 1);
    assert_eq!(hits.2.load(Ordering::SeqCst), 0);
}
