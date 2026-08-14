//! 可逆效果：注册即效果，dispose 即反注册（决策 D7 的同步部分）。

use std::sync::{Arc, Mutex};

/// 一次性反注册闭包。
type Disposer = Box<dyn FnOnce() + Send>;

/// 效果句柄：持有一次性反注册闭包，[`EffectHandle::dispose`] 幂等执行。
///
/// 语义（同 dsh 的 disposer）：**显式调用才生效**——丢弃句柄克隆是无操作，
/// 自动逆序回滚由 [`crate::Fiber`] 的 `Drop` 承担（fiber 持有各注册效果的句柄克隆）。
#[derive(Clone)]
pub struct EffectHandle {
    inner: Arc<Mutex<Option<Disposer>>>,
}

impl EffectHandle {
    /// 由反注册闭包构造效果句柄。
    pub fn new<F>(disposer: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self {
            inner: Arc::new(Mutex::new(Some(Box::new(disposer)))),
        }
    }

    /// 执行反注册（幂等：重复调用无副作用）。
    pub fn dispose(&self) {
        if let Some(disposer) = self.inner.lock().unwrap().take() {
            disposer();
        }
    }

    /// 是否已反注册。
    pub fn is_disposed(&self) -> bool {
        self.inner.lock().unwrap().is_none()
    }
}

impl std::fmt::Debug for EffectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectHandle")
            .field("disposed", &self.is_disposed())
            .finish()
    }
}
