//! macOS 自定义 URL scheme（`myapp://…`）接收。
//!
//! # 为什么单实例那套转发接不住它
//!
//! 二次实例转发解决的是「LaunchServices 丢弃第二次启动的 arguments」。但 URL 打开是
//! **另一条完全不同的通路**：LaunchServices 根本不把 URL 放进 argv，而是给应用发一个
//! Apple Event（`kInternetEventClass`/`kAEGetURL`）。所以：
//!
//! - **首次**启动（应用没在跑，点链接拉起）：argv 里没有 URL，转发层也没得转 —— 唯一
//!   拿得到 URL 的地方就是这个 Apple Event。
//! - **已在运行**时点链接：同样只有 Apple Event，连新进程都不会起。
//!
//! Windows 那边靠注册表 `"<exe>" "%1"` 把 URL 塞进 argv，故只需 argv 一条路；macOS 上
//! 只写 `CFBundleURLTypes` 而不接这个事件，表现就是「链接点了毫无反应」。
//!
//! # 与单实例的关系
//!
//! 收到 URL 后按 `[exe, url]` 拼成 argv 交给单实例那条主线程通路（`deliver_argv`），
//! 复用它的「派回主线程 → 调 on_second → 激活主窗口」。对应用而言，「被 URL 打开」与
//! 「被带参数再次启动」本就是同一件事，没有理由做成两个回调。
//!
//! # 用 Carbon 的 `AEInstallEventHandler` 而不是 `NSAppleEventManager`
//!
//! 后者要一个 Objective-C target + selector，从 Rust 侧得凭空造一个 ObjC 类；前者收
//! 裸函数指针，直接可用。两者底层是同一套派发，`NSAppleEventManager` 只是它的封装。

use std::ffi::c_void;

/// 四字符码（OSType）。`'GURL'` 之类的常量在 C 头里就是这么写的。
const fn four_cc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const K_INTERNET_EVENT_CLASS: u32 = four_cc(b"GURL");
const K_AE_GET_URL: u32 = four_cc(b"GURL");
const KEY_DIRECT_OBJECT: u32 = four_cc(b"----");
const TYPE_UTF8_TEXT: u32 = four_cc(b"utf8");

/// URL 最长取这么多字节。真实的深链是几十到几百字节；给足余量的同时避免无界栈缓冲。
const MAX_URL_BYTES: usize = 4096;

#[allow(non_camel_case_types)]
type OSStatus = i32;
#[allow(non_camel_case_types)]
type AEEventHandlerProcPtr =
    extern "C" fn(event: *const c_void, reply: *mut c_void, refcon: isize) -> OSStatus;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn AEInstallEventHandler(
        event_class: u32,
        event_id: u32,
        handler: AEEventHandlerProcPtr,
        handler_refcon: isize,
        is_sys_handler: u8,
    ) -> OSStatus;

    fn AEGetParamPtr(
        the_apple_event: *const c_void,
        the_ae_keyword: u32,
        desired_type: u32,
        actual_type: *mut u32,
        data_ptr: *mut c_void,
        maximum_size: isize,
        actual_size: *mut isize,
    ) -> OSStatus;
}

/// 安装 `GURL` 事件处理器。**须在事件循环跑起来之前调用**——由 URL 拉起的那一次启动，
/// 事件已经排在队列里，处理器装晚了就直接丢掉（表现为「第一次点链接没反应，第二次才行」）。
///
/// 重复调用无害（同一 class/id 覆盖注册）。失败只记日志：URL 打不开远不到让程序起不来的地步。
pub(crate) fn install() {
    let status = unsafe {
        AEInstallEventHandler(
            K_INTERNET_EVENT_CLASS,
            K_AE_GET_URL,
            handle_get_url,
            0,
            0, // 仅本应用，不是系统级处理器
        )
    };
    if status != 0 {
        eprintln!("[windui] URL scheme: AEInstallEventHandler 失败 status={status}");
    }
}

extern "C" fn handle_get_url(
    event: *const c_void,
    _reply: *mut c_void,
    _refcon: isize,
) -> OSStatus {
    let Some(url) = (unsafe { direct_object_utf8(event) }) else {
        return 0; // 取不出就当没收到；返回非零只会让系统弹一个用户看不懂的错误
    };
    if url.is_empty() {
        return 0;
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::single_instance::deliver_argv(vec![exe, url]);
    0
}

/// 取事件的直接宾语（即 URL 串），按 UTF-8 请求——`AEGetParamPtr` 会按需做强制转换，
/// 故不必自己处理 `typeChar`/`typeUnicodeText` 等历史类型。
///
/// # Safety
/// `event` 必须是系统传入的有效 `AppleEvent` 指针。
unsafe fn direct_object_utf8(event: *const c_void) -> Option<String> {
    let mut buf = vec![0u8; MAX_URL_BYTES];
    let mut actual_type: u32 = 0;
    let mut actual_size: isize = 0;
    let status = unsafe {
        AEGetParamPtr(
            event,
            KEY_DIRECT_OBJECT,
            TYPE_UTF8_TEXT,
            &mut actual_type,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as isize,
            &mut actual_size,
        )
    };
    if status != 0 || actual_size <= 0 {
        return None;
    }
    let n = (actual_size as usize).min(buf.len());
    buf.truncate(n);
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四字符码的字节序：`'GURL'` 必须是 0x4755524C（G=0x47 在最高位）。
    /// 写反了不会有任何编译或运行期报错，只是处理器永远收不到事件。
    #[test]
    fn four_cc_is_big_endian_packed() {
        assert_eq!(four_cc(b"GURL"), 0x4755_524C);
        assert_eq!(four_cc(b"----"), 0x2D2D_2D2D);
        assert_eq!(four_cc(b"utf8"), 0x7574_6638);
    }
}
