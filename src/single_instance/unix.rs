//! 单实例 unix(macOS)实现:Unix domain socket 收发 argv + 激活主窗口。
//!
//! - [`acquire`]:bind `{tmp}/{app_id}_si.sock`。成功=首实例(listener 暂存于全局,
//!   待 [`install_listener`] 取走);地址被占则试连一次以区分「活着的首实例」与「残留 socket」。
//! - [`forward`]:二次实例 connect 后写入编码 argv。
//! - [`install_listener`]:首实例在 UI 线程存回调,另起线程 accept;收到 argv 后经
//!   libdispatch 派回主线程调 on_second + 激活主窗口。
//!
//! # 为什么 macOS 上「.app 天然单实例」还需要这一层
//!
//! LaunchServices 确实不会为同一个 .app 启第二个进程,但它**丢弃**第二次启动带的
//! arguments,只把已有窗口拉到前台。于是「打开设置的词库页」在设置程序已开着时就只是
//! 闪一下窗口、页不动。argv 必须由我们自己转发过去。
//!
//! # 为什么回调要派回主线程
//!
//! `on_second` 会碰 UI 状态(切页 / 路由深链),而 accept 在后台线程。对照 win32 版:
//! 那边靠 message-only 窗口的 wndproc 天然落在 UI 线程,这里用 `dispatch_async_f`
//! 到主队列达到同一效果(与 `platform::macos::window::MacWake` 同一手法)。

use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;

use super::{decode_argv, encode_argv, socket_path};

/// `acquire` 里 bind 成功的 listener,暂存至 `install_listener` 取走。
///
/// 两者之间隔着窗口创建(见 `platform::macos::run`),不能在 acquire 里就开始 accept:
/// 那时 `on_second` 还没交过来,收到的 argv 无处可去。
static PENDING_LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);

/// 首实例上下文(UI 线程局部):二次实例回调 + 主窗口 `NSWindow` 指针(as usize)。
struct SiCtx {
    on_second: Box<dyn FnMut(Vec<String>)>,
    main_window: usize,
}
thread_local! {
    static SI_CTX: RefCell<Option<SiCtx>> = const { RefCell::new(None) };
}

/// 检测单实例。返回 true=首实例(已 bind 并暂存 listener);false=已有实例在运行。
pub(crate) fn acquire(app_id: &str) -> bool {
    let path = socket_path(app_id);
    match UnixListener::bind(&path) {
        Ok(l) => {
            *PENDING_LISTENER.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // socket 文件存在不等于首实例还活着:进程被 kill / 崩溃后文件会留下。
            // 试连一次来区分——连得上说明对端在 accept(真首实例),连不上就是残留,
            // 删掉重 bind。少了这一步,一次异常退出就能把程序永久挡在门外。
            if UnixStream::connect(&path).is_ok() {
                return false;
            }
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(l) => {
                    *PENDING_LISTENER.lock().unwrap_or_else(|e| e.into_inner()) = Some(l);
                    true
                }
                // 竞态:另一个进程抢在我们前面 bind 上了,它是首实例。
                Err(_) => false,
            }
        }
        // 其它错误(目录不可写等)保守按首实例处理,不阻塞启动——代价只是失去单实例,
        // 比"程序打不开"轻得多。此时 PENDING_LISTENER 为空,install_listener 会安静跳过。
        Err(_) => true,
    }
}

/// 二次实例:把 argv 写给首实例。返回是否成功送达(失败时调用方回退为正常启动)。
pub(crate) fn forward(app_id: &str, argv: &[String]) -> bool {
    let Ok(mut s) = UnixStream::connect(socket_path(app_id)) else {
        return false;
    };
    // 首实例可能正在退出(还在 listen 但已不再 accept):加写超时,免得二次实例陪着挂住。
    let _ = s.set_write_timeout(Some(std::time::Duration::from_secs(3)));
    s.write_all(&encode_argv(argv)).is_ok()
}

/// 首实例:在 UI 线程存下回调,另起线程 accept 二次实例的 argv。
/// `main_window` 为主窗口 `NSWindow` 指针数值(0=不激活),`on_second` 在主线程回调。
pub(crate) fn install_listener(
    _app_id: &str,
    main_window: isize,
    on_second: Box<dyn FnMut(Vec<String>)>,
) {
    let Some(listener) = PENDING_LISTENER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    else {
        // acquire 未能 bind(见其 Err(_) 分支):没有 listener 可用,单实例降级为不生效。
        return;
    };
    SI_CTX.with(|c| {
        *c.borrow_mut() = Some(SiCtx {
            on_second,
            main_window: main_window as usize,
        })
    });
    std::thread::Builder::new()
        .name("windui-single-instance".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
                let mut buf = Vec::new();
                if s.read_to_end(&mut buf).is_err() || buf.is_empty() {
                    continue;
                }
                dispatch_to_main(decode_argv(&buf));
            }
        })
        .ok();
}

/// 外部来源的 argv(macOS URL scheme)走与二次实例同一条主线程通路。
pub(crate) fn deliver_argv(argv: Vec<String>) {
    dispatch_to_main(argv);
}

// ── 主线程蹦床 ────────────────────────────────────────────────────────────

// libdispatch FFI:与 `platform::macos::window` 同一套(那边用于跨线程标脏一帧)。
#[cfg(target_os = "macos")]
unsafe extern "C" {
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: *const std::ffi::c_void,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
}

/// 把 argv 派回主线程处理。`Vec<String>` 装箱后以裸指针过队列,由蹦床取回所有权。
#[cfg(target_os = "macos")]
fn dispatch_to_main(argv: Vec<String>) {
    let ctx = Box::into_raw(Box::new(argv)) as *mut std::ffi::c_void;
    unsafe { dispatch_async_f(std::ptr::addr_of!(_dispatch_main_q), ctx, on_main) };
}

#[cfg(not(target_os = "macos"))]
fn dispatch_to_main(_argv: Vec<String>) {
    // 本 crate 目前只有 win32 / macOS 两个 GUI 后端；其它 unix 无主线程可派。
}

/// 主线程:调 on_second 并把主窗口带到前台。
#[cfg(target_os = "macos")]
extern "C" fn on_main(ctx: *mut std::ffi::c_void) {
    let argv = *unsafe { Box::from_raw(ctx as *mut Vec<String>) };
    SI_CTX.with(|c| {
        // 先 take 释放借用再调回调:回调内若再触及 SI_CTX,同线程二次 borrow_mut 会 panic
        // (与 win32 版同样的处理)。
        let maybe_ctx = c.borrow_mut().take();
        let Some(mut ctx) = maybe_ctx else { return };
        let main_window = ctx.main_window;
        // catch_unwind 防止回调 panic 穿越 extern "C" 边界导致 UB。
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (ctx.on_second)(argv);
        }));
        let mut guard = c.borrow_mut();
        if guard.is_none() {
            *guard = Some(ctx);
        }
        drop(guard);
        activate(main_window);
    });
}

/// 激活主窗口:取消最小化 + 带到前台 + **标脏一帧** + 让 app 成为前台应用。
///
/// macOS 无 Windows 那套「前台激活权」限制,`activate` 直接生效,不需要二次实例先授权。
///
/// # 为什么必须显式标脏
///
/// `on_second` 里写 Signal 触发的是 `anim::request_repaint`,而它只置线程局部脏标志、
/// 等宿主在帧收尾时消费 —— 空闲时 macOS 后端零唤醒,根本没有帧在跑,没人来消费。窗口
/// 若本就在前台,`makeKeyAndOrderFront` 也是 no-op,于是页切了、界面纹丝不动(要等用户
/// 晃一下鼠标才跳过去)。故这里照 `MacWake::wake_on_main` 的做法对 contentView
/// `setNeedsDisplay:` —— 与后台数据经 channel 回 UI 线程后标脏是同一条理。
///
/// win32 那边不需要这一步: `SetForegroundWindow`/`BringWindowToTop` 顺带就引发了重绘。
#[cfg(target_os = "macos")]
fn activate(main_window: usize) {
    use objc2_app_kit::{NSApplication, NSWindow};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    if main_window != 0 {
        // 指针来自 run_windowed 里存活至进程退出的 NSWindow(对照 MacWake 持视图指针)。
        let w: &NSWindow = unsafe { &*(main_window as *const NSWindow) };
        if w.isMiniaturized() {
            w.deminiaturize(None);
        }
        w.makeKeyAndOrderFront(None);
        if let Some(v) = w.contentView() {
            v.setNeedsDisplay(true);
        }
    }
    NSApplication::sharedApplication(mtm).activate();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PENDING_LISTENER` 是进程级单例（生产上一个进程只有一个 App，这是对的），
    /// 于是并发跑的用例会互相清掉对方的 listener。用例间必须串行。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 每个用例再用独立 app_id，避免与同机其它测试进程抢同一个 socket 文件。
    fn app_id(tag: &str) -> String {
        format!("windui_si_test_{tag}_{}", std::process::id())
    }

    fn cleanup(id: &str) {
        *PENDING_LISTENER.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *crate::single_instance::HELD
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        let _ = std::fs::remove_file(socket_path(id));
    }

    /// `claim_instance` 取得单实例后，`run` 内的仲裁必须直接放行。
    ///
    /// 少了这层记忆就会 `acquire` 第二次 —— 那次连得上的是本进程**自己** bind 的
    /// socket，于是把自己判成二次实例、把 argv forward 给自己，`run` 随即返回，
    /// 窗口永不出现。整个程序打不开，且看不出任何报错。
    #[test]
    fn claim_then_arbitrate_does_not_hand_off_to_self() {
        let _g = lock();
        let id = app_id("claim");
        cleanup(&id);
        assert_eq!(
            crate::single_instance::claim_instance(&id),
            crate::single_instance::InstanceRole::First
        );
        assert!(
            crate::single_instance::arbitrate(&id),
            "已 claim 的进程必须被放行"
        );
        cleanup(&id);
    }

    /// 没 claim 过的进程走原路：首次仲裁取得单实例，之后同 app_id 的仲裁一律判二次实例
    /// （此处 forward 送得进 backlog，故返回 false = 调用方应退出）。
    #[test]
    fn arbitrate_without_claim_still_gates() {
        let _g = lock();
        let id = app_id("arb");
        cleanup(&id);
        assert!(crate::single_instance::arbitrate(&id), "首次仲裁应放行");
        assert!(
            !crate::single_instance::arbitrate(&id),
            "已有实例在跑时应判二次实例"
        );
        cleanup(&id);
    }

    #[test]
    fn first_instance_acquires_second_does_not() {
        let _g = lock();
        let id = app_id("dup");
        cleanup(&id);
        assert!(acquire(&id), "首次应取得单实例");
        // 首实例的 listener 已在 backlog 上，二次 acquire 的探测连接能连上 → false。
        assert!(!acquire(&id), "已有实例在运行时不得再取得");
        cleanup(&id);
    }

    #[test]
    fn stale_socket_is_reclaimed() {
        let _g = lock();
        // 进程被 kill 后 socket 文件会留下。若不区分「残留」与「活着的首实例」，
        // 一次异常退出就能把程序永久挡在门外。
        let id = app_id("stale");
        cleanup(&id);
        let path = socket_path(&id);
        std::fs::write(&path, b"").expect("造一个残留文件");
        assert!(acquire(&id), "残留 socket 应被回收并重新 bind");
        cleanup(&id);
    }

    #[test]
    fn forward_delivers_argv_to_listener() {
        let _g = lock();
        let id = app_id("fwd");
        cleanup(&id);
        assert!(acquire(&id));
        let listener = PENDING_LISTENER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap();

        let argv = vec!["exe".to_string(), "--page=dict".to_string()];
        let sent = argv.clone();
        let id2 = id.clone();
        let t = std::thread::spawn(move || forward(&id2, &sent));

        let (mut s, _) = listener.accept().expect("accept 二次实例");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).expect("读 argv");
        assert_eq!(decode_argv(&buf), argv);
        assert!(t.join().unwrap(), "forward 应报告送达");
        cleanup(&id);
    }

    #[test]
    fn forward_fails_when_nobody_listens() {
        let _g = lock();
        let id = app_id("nolisten");
        cleanup(&id);
        assert!(
            !forward(&id, &["exe".to_string()]),
            "无首实例时应报告未送达"
        );
    }
}
