//! macOS 「用本应用打开文件」接收：Finder 双击关联文件、「打开方式」、拖到 Dock 图标。
//!
//! 与 URL scheme（见 `url_scheme`）同一类问题：LaunchServices 不把文件放进 argv，而是发
//! `odoc` Apple Event，首次启动与已在运行时都一样。区别在于 `odoc` 是 AppKit 自己接管的
//! 核心事件——它在 `finishLaunching` 里注册处理器（覆盖掉事件循环之前装的任何 Carbon
//! 处理器），再把文件交给 app delegate 的 `application:openURLs:`；delegate 没实现这个
//! 方法时，AppKit 会退到文档控制器并弹「无法打开此类文件」。所以这里不走 Carbon，而是
//! 装一个只实现这一个方法的 delegate。
//!
//! 收到的文件按 `[exe, path…]` 拼成 argv 交给单实例的主线程通路（`deliver_argv`），
//! 对应用而言就是「被带参数再次启动」，复用 `App::single_instance` 的 `on_second`。

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSApplicationDelegate};
use objc2_foundation::{NSArray, NSURL};

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WindUiAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(application:openURLs:))]
        fn application_open_urls(&self, _application: &NSApplication, urls: &NSArray<NSURL>) {
            let paths: Vec<String> = (0..urls.count())
                .map(|i| urls.objectAtIndex(i))
                .filter(|u| u.isFileURL())
                .filter_map(|u| u.path())
                .map(|p| p.to_string())
                .collect();
            if paths.is_empty() {
                return;
            }
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut argv = Vec::with_capacity(paths.len() + 1);
            argv.push(exe);
            argv.extend(paths);
            crate::single_instance::deliver_argv(argv);
        }
    }
);

/// 装上 delegate。**须在 `app.run()` 之前**：冷启动时的 `odoc` 在 `finishLaunching` 期间
/// 派发，那时 delegate 必须已经就位。NSApplication 对 delegate 是弱引用，返回值要由调用方
/// 持有到事件循环结束。
pub(crate) fn install(mtm: MainThreadMarker, app: &NSApplication) -> Retained<NSObject> {
    let delegate: Retained<AppDelegate> = unsafe { msg_send![AppDelegate::alloc(mtm), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    delegate.into_super()
}
