//! 单实例 unix(macOS/Linux)实现。当前为占位,后续用 Unix domain socket 完善:
//! 首实例 bind+listen `{tmp}/{app_id}_si.sock`,二次实例 connect 发 argv,
//! listener 线程收 → on_second + 激活(NSApp activate)。
#![allow(dead_code)]

pub(crate) fn acquire(_app_id: &str) -> bool {
    true
}

pub(crate) fn forward(_app_id: &str, _argv: &[String]) {}

pub(crate) fn install_listener(
    _app_id: &str,
    _main_hwnd: isize,
    _on_second: Box<dyn FnMut(Vec<String>)>,
) {
}
