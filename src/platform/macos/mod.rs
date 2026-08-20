//! macOS 平台后端（Cocoa/AppKit + Core Text）。
//!
//! 对外暴露与 `win32` 同形的 API：`run` / `open_url` / `Tray`·`TrayCtx`·`TrayMenuItem` /
//! `clipboard::MacClipboard`。上层只依赖 `crate::platform::*`，不直接触碰本后端。
//!
//! 模块划分：
//! - `window`：`NSApplication` + `NSWindow` + 自定义翻转 `NSView`。渲染走 CPU（tiny-skia
//!   `Pixmap`），`drawRect:` 中经 `CGBitmapContext`→`CGImage`→`CGContextDrawImage` blit；
//!   鼠标/键盘/滚轮/光标/HiDPI/无边框拖动/文件拖放/输入法（`NSTextInputClient`）均在此。
//!   多窗口也在此：登记表 `WINDOWS` 兼任子窗 `NSWindow` 的所有者（对照 win32 那张只存
//!   `HWND` 的名册——差别的来由见该常量的说明）。
//! - `clipboard`：`NSPasteboard`。
//! - `tray`：`NSStatusItem` 托盘（图标 + 左键/双击 + 原生右键菜单）。
//! - 文字渲染见 `crate::text::coretext`（Core Text）。
//!
//! 逐项对照实现见 `docs/MACOS_PORTING.md` 与 `platform/win32/mod.rs`。

pub mod clipboard;
pub mod hotkey;
pub mod tray;
pub mod url_scheme;
pub mod window;

use super::{AppHandler, NewWindow, WindowConfig};

/// 运行应用：截屏模式离屏渲染存盘；否则创建窗口进入事件循环。
pub(crate) fn run(
    cfg: WindowConfig,
    mut handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    // 全局动画开关：显式配置优先；否则截屏路径恒开（保证终态稳定）、窗口路径随系统设置。
    let os_default = if cfg.screenshot.is_some() {
        true
    } else {
        os_animations_enabled()
    };
    crate::anim::set_enabled(cfg.animations.unwrap_or(os_default));
    // 光标闪烁走**插入符**设置（`NSTextInsertionPointBlinkPeriod*`），不跟"减弱动态效果"
    // 走：两者在系统里本就是分开的开关。应用显式 `animations(false)` 时连它一起关。
    crate::ui::caret::set_blink_period_ms(match cfg.animations {
        Some(false) => None,
        _ => os_caret_blink_half_ms(),
    });

    if let Some(path) = cfg.screenshot.clone() {
        // 离屏渲染走平台无关的共享实现（与 win32 后端共用）。
        super::run_offscreen(&cfg, &mut handler, &path);
        return;
    }
    // 单实例仲裁（应用若已在 main 里 claim_instance 过，这里直接放行）：二次实例把 argv
    // 转发给首实例后直接返回、不建窗口。
    //
    // ⚠ macOS 的 `.app` 由 LaunchServices 保证不会启第二个进程，但它**丢弃**第二次启动
    // 带的 arguments、只把窗口拉到前台——深链（如「打开设置的词库页」）因此在程序已开着
    // 时失效。argv 的转发必须由这一层自己做，不能指望系统的单实例语义。
    if let Some(si) = &single {
        if !crate::single_instance::arbitrate(&si.app_id) {
            return;
        }
    }
    window::run_windowed(cfg, handler, waker, single);
}

/// 把应用带到前台。**所有激活都必须走这里，不要直接 `app.activate()`。**
///
/// 无参的 `-[NSApplication activate]` 是 macOS 14 才有的 selector，而 objc2 的绑定不做
/// 运行时版本检查、直接 `msg_send`：13 及以下会 `doesNotRecognizeSelector:` 抛
/// `NSInvalidArgumentException`。Objective-C 异常穿不过 Rust 栈——它一路 unwind 到
/// `main` 的 `catch_unwind`，被判成 foreign exception 后直接 `abort()`，崩溃日志里
/// 只剩一句 "abort() called"，连异常名都没有（WindInput #67 就是这么崩的，且因为
/// `run_windowed` 在 `app.run()` 前无条件激活，13 及以下是**启动必崩**）。
///
/// 故先问过 `respondsToSelector:`，老系统回落到 10.0 就有的
/// `activateIgnoringOtherApps:`（14 起才标 deprecated，至今仍可用）。
pub(crate) fn activate_app(app: &objc2_app_kit::NSApplication) {
    use objc2::sel;
    use objc2_foundation::NSObjectProtocol;

    if app.respondsToSelector(sel!(activate)) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// 查询系统“显示动画”偏好（减弱动态效果）。占位：默认开。
fn os_animations_enabled() -> bool {
    // TODO(macos): 读取 NSWorkspace.accessibilityDisplayShouldReduceMotion，取反。
    true
}

/// 查询系统**插入符**闪烁半周期（ms）。`None` = 用户关掉了闪烁。
///
/// macOS 把它放在 `NSUserDefaults` 的 `NSTextInsertionPointBlinkPeriodOn/Off`（单位**秒**）：
/// 两者都显式设为 0 才是"不闪"；键不存在（绝大多数机器）则用系统默认 500ms。
/// 与"减弱动态效果"无关——那个管的是窗口/控件过渡。
fn os_caret_blink_half_ms() -> Option<u32> {
    use objc2_foundation::{NSString, NSUserDefaults};
    let d = NSUserDefaults::standardUserDefaults();
    let read = |k: &str| -> Option<f64> {
        let key = NSString::from_str(k);
        // objectForKey 为 None 即"用户没设过"，与"设成了 0"必须区分开。
        d.objectForKey(&key)?;
        Some(d.doubleForKey(&key))
    };
    let on = read("NSTextInsertionPointBlinkPeriodOn");
    let off = read("NSTextInsertionPointBlinkPeriodOff");
    match (on, off) {
        (None, None) => Some(crate::ui::caret::BLINK_HALF_MS as u32),
        (a, b) => {
            let on = a.unwrap_or(0.5).max(0.0);
            let off = b.unwrap_or(0.5).max(0.0);
            if on <= 0.0 && off <= 0.0 {
                return None; // 用户显式关掉闪烁
            }
            // 本库的相位模型是对称半周期，取亮/灭时长的平均。
            Some((((on + off) / 2.0) * 1000.0).round().max(1.0) as u32)
        }
    }
}

/// 用系统默认程序打开 URL/路径（链接点击）。对照 win32 `ShellExecuteW`。
pub fn open_url(url: &str) {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};

    let ns = NSString::from_str(url);
    if let Some(nsurl) = NSURL::URLWithString(&ns) {
        // fire-and-forget，忽略结果。
        let _ = NSWorkspace::sharedWorkspace().openURL(&nsurl);
    }
}
