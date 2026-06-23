//! 跨线程唤醒原语：Waker 延迟绑定平台句柄，窗口建好前的 wake 走 pending 兜底。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 平台唤醒句柄：win32 持 HWND 数值并 post 自定义消息、macOS dispatch。Send 由各实现保证。
pub(crate) trait RawWakeSignal: Send {
    fn signal(&self);
}
pub(crate) type RawWake = Box<dyn RawWakeSignal>;

pub(crate) struct WakerShared {
    raw: Mutex<Option<RawWake>>,
    pending: AtomicBool,
}

/// 跨线程唤醒句柄：Send + Sync + Clone，交后台线程。
#[derive(Clone)]
pub struct Waker {
    inner: Arc<WakerShared>,
}

impl WakerShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self { raw: Mutex::new(None), pending: AtomicBool::new(false) })
    }
    /// 窗口建好后回填平台句柄；若此前有积压 wake，立即补发一次。
    pub(crate) fn bind(self: &Arc<Self>, raw: RawWake) {
        let pending = self.pending.swap(false, Ordering::SeqCst);
        if pending {
            raw.signal();
        }
        *self.raw.lock().unwrap() = Some(raw);
    }
    pub(crate) fn waker(self: &Arc<Self>) -> Waker {
        Waker { inner: self.clone() }
    }
}

impl Waker {
    /// 唤醒 UI 一帧。句柄未绑定（run 前）时置 pending，绑定时补发。
    pub fn wake(&self) {
        let guard = self.inner.raw.lock().unwrap();
        match guard.as_ref() {
            Some(raw) => raw.signal(),
            None => self.inner.pending.store(true, Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    struct CountSignal(Arc<AtomicU32>);
    impl RawWakeSignal for CountSignal {
        fn signal(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn wake_before_bind_is_pending_then_flushed() {
        let shared = WakerShared::new();
        let waker = shared.waker();
        waker.wake(); // 未绑定 → pending
        let count = Arc::new(AtomicU32::new(0));
        shared.bind(Box::new(CountSignal(count.clone())));
        assert_eq!(count.load(Ordering::SeqCst), 1, "绑定时补发积压 wake");
        waker.wake(); // 已绑定 → 直接 signal
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn waker_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Waker>();
    }
}
