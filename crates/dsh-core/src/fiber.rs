//! Fiber：每个插件实例（fork 出的 Context）一个，持有该实例注册的全部可逆效果。

use futures::future::BoxFuture;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::effect::EffectHandle;

/// 插件实例的可逆效果容器。
///
/// - `Drop` / [`Fiber::dispose`]：同步逆序执行 disposers（同 dsh `fiber._unload`，RAII 兜底）；
/// - [`Fiber::dispose_async`]：优雅卸载路径（loader/app 调用）——同步反注册 + 等待异步 disposer；
/// - 进入卸载后（[`Fiber::is_disposed`]）应拒绝注册新效果（同 dsh `INACTIVE_EFFECT`，
///   由 `Context` 的注册入口强制）。
///
/// 决策 D7：同步反注册由 [`EffectHandle`] 幂等执行；异步 disposer 由 fiber 统一收集等待。
pub struct Fiber {
    handles: Mutex<Vec<EffectHandle>>,
    async_disposers: Mutex<Vec<BoxFuture<'static, ()>>>,
    disposed: AtomicBool,
}

impl Default for Fiber {
    fn default() -> Self {
        Self {
            handles: Mutex::new(Vec::new()),
            async_disposers: Mutex::new(Vec::new()),
            disposed: AtomicBool::new(false),
        }
    }
}

impl Fiber {
    /// 追加一个同步效果句柄（卸载时逆序执行）。
    pub fn push(&self, handle: EffectHandle) {
        self.handles.lock().unwrap().push(handle);
    }

    /// 追加一个异步 disposer（卸载时统一等待，对应 dsh 的异步 stop 回调）。
    pub fn push_async(&self, disposer: BoxFuture<'static, ()>) {
        self.async_disposers.lock().unwrap().push(disposer);
    }

    /// 是否已进入卸载（此后注册效果应报 [`crate::CoreError::InactiveFiber`]）。
    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::Acquire)
    }

    /// 同步卸载：逆序执行同步 disposers；异步 disposer 不等待（无运行时场景的兜底路径）。
    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::Release);
        let mut handles = self.handles.lock().unwrap();
        while let Some(handle) = handles.pop() {
            handle.dispose();
        }
        self.async_disposers.lock().unwrap().clear();
    }

    /// 优雅卸载：逆序同步反注册，然后等待全部异步 disposer 完成。
    pub async fn dispose_async(&self) {
        self.disposed.store(true, Ordering::Release);
        {
            let mut handles = self.handles.lock().unwrap();
            while let Some(handle) = handles.pop() {
                handle.dispose();
            }
        }
        let disposers: Vec<_> = self.async_disposers.lock().unwrap().drain(..).collect();
        futures::future::join_all(disposers).await;
    }
}

impl Drop for Fiber {
    fn drop(&mut self) {
        self.dispose();
    }
}
