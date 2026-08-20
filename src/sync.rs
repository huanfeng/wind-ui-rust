//! 跨线程唤醒原语：Waker 延迟绑定平台句柄，窗口建好前的 wake 走 pending 兜底。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 平台唤醒句柄：win32 持 HWND 数值并 post 自定义消息、macOS dispatch。Send 由各实现保证。
pub(crate) trait RawWakeSignal: Send {
    fn signal(&self);
}
pub(crate) type RawWake = Box<dyn RawWakeSignal>;

pub use std::sync::mpsc::SendError;

/// 跨线程消息发送端：Send + Sync + Clone。send = 入队 + 唤醒 UI 一帧。
pub struct Sender<Msg> {
    tx: std::sync::mpsc::Sender<Msg>,
    waker: Waker,
}

impl<Msg> Clone for Sender<Msg> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            waker: self.waker.clone(),
        }
    }
}

impl<Msg> Sender<Msg> {
    /// 入队一条消息并唤醒 UI 一帧。接收端（窗口）已关闭时返回 Err。
    pub fn send(&self, msg: Msg) -> Result<(), SendError<Msg>> {
        self.tx.send(msg)?;
        self.waker.wake();
        Ok(())
    }
}

/// 类型擦除的通道排空器（供 UiHost 每帧调用）：借宿主的树与 App 级 `self_id` 逐条
/// 派送积压消息，每条产出一份 [`DispatchResult`] 交宿主消费。
///
/// 为什么把树传进来而不是让 pump 只调回调：`on_message` 收 `&mut EventCtx`，而
/// `EventCtx` 只能由 [`Tree::run_detached`] 借出。**逐条**借（而非整批借一次）是为了
/// 让每条消息的副作用互不覆盖——`DispatchResult` 里 toast/dialog 都是 `Option`，
/// 一批消息共用一份就只剩最后一条的 toast，"三个任务完成弹三条提示"会静默丢两条。
pub(crate) type ChannelPump =
    Box<dyn FnMut(&mut crate::core::Tree, crate::core::NodeId) -> Vec<crate::core::DispatchResult>>;

// ── App 级通道登记表 ────────────────────────────────────────────────────────
//
// pump 挂在**应用**而非某个窗口上，理由与托盘/全局热键/跨线程唤醒挪进 `AppHost` 时
// 完全相同：后台线程发来的消息是给「这个应用」的，不是给某个窗口的。此前它们挂在
// `UiHost` 上，于是「窗口」与「应用」两个生命周期被迫重合——主窗一关，`App::channel`
// 注册的所有通道就随之失效，而应用可能还有别的窗口开着、还要继续收数据。
//
// 唤醒早已是 App 级（`RawWakeSignal` 投给 message-only 宿主，标脏所有窗口），消费却
// 还留在窗口级，这本身就是条断裂：被唤醒之后要消费的东西随窗口死了。

thread_local! {
    /// 本线程（= 本应用）注册的全部通道排空器。
    static APP_PUMPS: std::cell::RefCell<Vec<ChannelPump>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// 当前持有消费权的宿主序号。`None` = 无人持有，下一个出帧的窗口接管。
    ///
    /// 需要「代表窗口」而不是让每个窗口都排空：pump 借的是**调用方的树**，
    /// `toast` / `focus` / `menu` 这些副作用会落到那棵树所属的窗口上。若谁先渲染谁消费，
    /// 同一条消息的提示会随窗口的渲染次序在窗口之间跳。
    static CHANNEL_OWNER: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    /// 宿主序号发号器（主窗最小，故它天然先抢到消费权）。
    static NEXT_HOST_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 注册一个通道排空器（`App::channel` 调用，建窗前）。
pub(crate) fn register_pump(pump: ChannelPump) {
    APP_PUMPS.with(|p| p.borrow_mut().push(pump));
}

/// 领一个宿主序号。
pub(crate) fn next_host_id() -> u64 {
    NEXT_HOST_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

/// 本宿主是否该在本帧排空通道：无人持有则就地接管，否则仅持有者为真。
pub(crate) fn claim_channel_owner(host_id: u64) -> bool {
    CHANNEL_OWNER.with(|o| match o.get() {
        Some(cur) => cur == host_id,
        None => {
            o.set(Some(host_id));
            true
        }
    })
}

/// 释放消费权（宿主析构时调用）。非持有者调用无副作用。
///
/// 释放而不是转交给某个具体窗口：核心层不知道还有谁活着。置空之后，下一个出帧的窗口
/// 会经 [`claim_channel_owner`] 就地接管——而"还在出帧"恰好就是"还活着"的证据。
pub(crate) fn release_channel_owner(host_id: u64) {
    CHANNEL_OWNER.with(|o| {
        if o.get() == Some(host_id) {
            o.set(None);
        }
    });
}

/// 取走全部 pump（跑完须经 [`put_pumps`] 放回）。
///
/// 借出—放回而不是就地遍历：pump 要 `&mut Tree`，而它产出的副作用要整个 `&mut UiHost`
/// 才能落地，同时持有两者过不了借用检查。
pub(crate) fn take_pumps() -> Vec<ChannelPump> {
    APP_PUMPS.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// 放回 pump。期间若有新注册（`App::channel` 只在建窗前可调，运行期不会发生），
/// 拼接而不是覆盖，避免静默丢通道。
pub(crate) fn put_pumps(mut pumps: Vec<ChannelPump>) {
    APP_PUMPS.with(|p| {
        let mut slot = p.borrow_mut();
        pumps.append(&mut slot);
        *slot = pumps;
    });
}

/// 清空登记表与消费权（**仅测试**：用例之间彼此隔离）。
///
/// 刻意不在 `App::new` / `App::run` 里调：`App::channel` 是在 `run` **之前**注册的，
/// 在 run 里清一次正好把刚注册的通道全丢掉；而 `App` 本就是一个进程一个、`run` 进消息
/// 循环直到退出，运行期没有"换一个 App"这回事。
#[cfg(test)]
pub(crate) fn reset_pumps() {
    APP_PUMPS.with(|p| p.borrow_mut().clear());
    CHANNEL_OWNER.with(|o| o.set(None));
}

/// 建一个 typed channel：返回发送端 + 类型擦除的排空 pump（供 UiHost 每帧调用）。
pub(crate) fn new_channel<Msg: Send + 'static>(
    waker: Waker,
    mut on_message: impl FnMut(&mut crate::core::EventCtx, Msg) + 'static,
) -> (Sender<Msg>, ChannelPump) {
    let (tx, rx) = std::sync::mpsc::channel::<Msg>();
    let pump: ChannelPump = Box::new(move |tree, id| {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(tree.run_detached(id, |ctx| on_message(ctx, m)));
        }
        out
    });
    (Sender { tx, waker }, pump)
}

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
        Arc::new(Self {
            raw: Mutex::new(None),
            pending: AtomicBool::new(false),
        })
    }
    /// 窗口建好后回填平台句柄；若此前有积压 wake，立即补发一次。
    pub(crate) fn bind(self: &Arc<Self>, raw: RawWake) {
        // 全程持锁：与同样持锁的 wake() 串行化 raw 的读写，消除「pending 已读、raw 未装」的窗口。
        let mut guard = self.raw.lock().unwrap();
        *guard = Some(raw);
        if self.pending.swap(false, Ordering::SeqCst) {
            guard.as_ref().unwrap().signal();
        }
    }
    pub(crate) fn waker(self: &Arc<Self>) -> Waker {
        Waker {
            inner: self.clone(),
        }
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

    /// 一棵只有根节点的最小树，供 pump 借出 `EventCtx`。
    fn tiny_tree() -> (crate::core::Tree, crate::core::NodeId) {
        let mut tree = crate::core::Tree::new();
        let id = crate::ui::Element::col().build(&mut tree);
        tree.root = Some(id);
        (tree, id)
    }

    #[test]
    fn channel_pump_drains_in_order_across_thread() {
        let shared = WakerShared::new();
        let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u32>::new()));
        let g2 = got.clone();
        let (tx, mut pump) =
            new_channel::<u32>(shared.waker(), move |_ctx, m| g2.borrow_mut().push(m));
        let t = std::thread::spawn(move || {
            tx.send(1).unwrap();
            tx.send(2).unwrap();
            tx.send(3).unwrap();
        });
        t.join().unwrap();
        let (mut tree, root) = tiny_tree();
        let out = pump(&mut tree, root);
        assert_eq!(*got.borrow(), vec![1, 2, 3]);
        assert_eq!(out.len(), 3, "每条消息各产出一份可消费的副作用");
    }

    /// 逐条借 ctx 而非整批借一次：`DispatchResult` 的 toast/dialog 是 `Option`，
    /// 共用一份会让一批消息里只剩最后一条的提示。
    #[test]
    fn each_message_gets_its_own_dispatch_result() {
        let shared = WakerShared::new();
        let (tx, mut pump) = new_channel::<u32>(shared.waker(), |ctx, m| ctx.toast(m.to_string()));
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        let (mut tree, root) = tiny_tree();
        let out = pump(&mut tree, root);
        let texts: Vec<String> = out
            .into_iter()
            .filter_map(|r| r.toast.map(|t| t.text))
            .collect();
        assert_eq!(texts, vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn send_after_receiver_dropped_errs() {
        let shared = WakerShared::new();
        let (tx, pump) = new_channel::<u32>(shared.waker(), |_ctx, _m: u32| {});
        drop(pump); // 接收端 rx 随 pump 一起销毁
        assert!(tx.send(9).is_err(), "接收端关闭后 send 返回 Err");
    }
}
