//! 单实例 Windows 实现:命名 Mutex 检测 + message-only 窗口收 WM_COPYDATA + 激活主窗口。
//!
//! - [`acquire`]:CreateMutexW 检测;首实例持 Mutex(泄漏到进程结束),二次实例返回 false。
//! - [`forward`]:二次实例按 class 名(=app_id 派生)找首实例 message 窗口,SendMessage(WM_COPYDATA) 发 argv。
//! - [`install_listener`]:首实例在 UI 线程建 message-only 窗口;其 wndproc 收 WM_COPYDATA →
//!   解码 argv → 调 on_second + 激活主窗口。on_second 与主 hwnd 存于 UI 线程局部。

use std::cell::RefCell;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM,
    LRESULT, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, CreateWindowExW, DefWindowProcW, FindWindowW,
    GetWindowThreadProcessId, IsIconic, RegisterClassExW, SendMessageTimeoutW, SetForegroundWindow,
    ShowWindow, HWND_MESSAGE, SMTO_ABORTIFHUNG, SW_RESTORE, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_COPYDATA, WNDCLASSEXW,
};

use super::{class_name, decode_argv, encode_argv, mutex_name};

/// 首实例上下文(UI 线程局部,单窗口):二次实例消息回调 + 主窗口 HWND。
struct SiCtx {
    on_second: Box<dyn FnMut(Vec<String>)>,
    main_hwnd: isize,
}
thread_local! {
    static SI_CTX: RefCell<Option<SiCtx>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 检测单实例。返回 true=首实例(已持 Mutex);false=已有实例在运行。
pub(crate) fn acquire(app_id: &str) -> bool {
    let name = wide(&mutex_name(app_id));
    unsafe {
        // 清零 TLS 错误槽，防止 prior Win32 调用留下的 ERROR_ALREADY_EXISTS 导致误判。
        SetLastError(WIN32_ERROR(0));
        match CreateMutexW(None, false, PCWSTR(name.as_ptr())) {
            Ok(handle) => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                if already {
                    let _ = CloseHandle(handle);
                    false
                } else {
                    // 首实例：Mutex 句柄丢弃即持有至进程退出——HANDLE 是 Copy 裸句柄、无 Drop，
                    // 不会触发 CloseHandle，OS 在进程结束时释放该命名 Mutex。
                    true
                }
            }
            Err(_) => true, // 创建失败保守按首实例处理(不阻塞启动)
        }
    }
}

/// 二次实例:把 argv 发给首实例的 message 窗口(WM_COPYDATA 系统跨进程 marshal)。
///
/// 返回是否成功送达。首实例正处于退出中(消息泵已停但进程未死)时送达会失败,
/// 调用方据此回退为正常启动,避免二次实例被一个僵死的首实例永久挡在门外。
pub(crate) fn forward(app_id: &str, argv: &[String]) -> bool {
    let cls = wide(&class_name(app_id));
    unsafe {
        let Ok(hwnd) = FindWindowW(PCWSTR(cls.as_ptr()), PCWSTR::null()) else {
            return false;
        };
        if hwnd.is_invalid() {
            return false;
        }
        // 把本进程持有的前台激活权授予首实例：二次实例通常由前台进程（用户点击 →
        // ShellExecute）启动而持权，首实例仅收到 WM_COPYDATA 并**不**获得权限——
        // 不显式授权则其 SetForegroundWindow 被系统拒绝，窗口只在任务栏闪烁不到前台。
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != 0 {
            let _ = AllowSetForegroundWindow(pid);
        }
        let bytes = encode_argv(argv);
        let cds = COPYDATASTRUCT {
            dwData: 1,
            cbData: bytes.len() as u32,
            lpData: bytes.as_ptr() as *mut std::ffi::c_void,
        };
        // 用带超时的发送而非同步 SendMessageW:首实例可能已停消息泵但进程未死,
        // 同步发送会把二次实例一起挂住(表现为"再进入无响应")。SMTO_ABORTIFHUNG
        // 在目标线程无响应时立即返回;返回值为 0 表示未送达,交由调用方回退。
        let mut result: usize = 0;
        let ret = SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            3000,
            Some(&mut result as *mut usize),
        );
        ret.0 != 0
    }
}

/// 首实例:在 UI 线程建 message-only 窗口(class=app_id 派生)接收二次实例消息。
/// `main_hwnd` 主窗口句柄(数值),`on_second` 收到 argv 时回调(UI 线程)。
pub(crate) fn install_listener(
    app_id: &str,
    main_hwnd: isize,
    on_second: Box<dyn FnMut(Vec<String>)>,
) {
    SI_CTX.with(|c| {
        *c.borrow_mut() = Some(SiCtx {
            on_second,
            main_hwnd,
        })
    });
    let cls = wide(&class_name(app_id));
    unsafe {
        let hinst = HINSTANCE(
            GetModuleHandleW(None)
                .map(|h| h.0)
                .unwrap_or(std::ptr::null_mut()),
        );
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(si_wnd_proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let _ = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE), // message-only 窗口
            None,
            Some(hinst),
            None,
        );
    }
}

unsafe extern "system" fn si_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_COPYDATA {
        let pcd = lp.0 as *const COPYDATASTRUCT;
        if !pcd.is_null() && unsafe { (*pcd).dwData } == 1 {
            let cb = unsafe { (*pcd).cbData } as usize;
            let ptr = unsafe { (*pcd).lpData } as *const u8;
            if !ptr.is_null() && cb > 0 {
                let data = unsafe { std::slice::from_raw_parts(ptr, cb) };
                let argv = decode_argv(data);
                SI_CTX.with(|c| {
                    // take() 释放借用后再调回调，防止 on_second 内调 install_listener
                    // 导致同线程二次 borrow_mut panic。
                    let maybe_ctx = c.borrow_mut().take();
                    if let Some(mut ctx) = maybe_ctx {
                        let main_hwnd = ctx.main_hwnd;
                        // catch_unwind 防止回调 panic 穿越 extern "system" FFI 边界导致 UB。
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            (ctx.on_second)(argv);
                        }));
                        // 若回调未替换上下文则还原；已替换则丢弃旧值。
                        let mut guard = c.borrow_mut();
                        if guard.is_none() {
                            *guard = Some(ctx);
                        }
                        drop(guard);
                        activate(main_hwnd);
                    }
                });
            }
        }
        return LRESULT(1);
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// 激活窗口:取消最小化 + 带到前台。SetForegroundWindow 需要前台激活权——本进程在后台
/// 时默认没有;依赖二次实例在 forward 前 AllowSetForegroundWindow 授权(见 forward)。
pub(crate) fn activate(main_hwnd: isize) {
    if main_hwnd == 0 {
        return;
    }
    let hwnd = HWND(main_hwnd as *mut std::ffi::c_void);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
    }
}
