//! 应用入口（Phase 0）。
//!
//! Phase 0 仅提供「清屏 + 自定义渲染回调」的最小运行能力，Phase 1 起接入 Node 树。

use std::path::PathBuf;

use tiny_skia::Pixmap;

use crate::geometry::{Color, Size};
use crate::platform::win32::{self, RenderFn, WindowConfig};

/// 应用构建器。命令式 API 的根入口。
pub struct App {
    cfg: WindowConfig,
    render: Option<RenderFn>,
}

impl App {
    pub fn new(title: impl Into<String>, width: i32, height: i32) -> Self {
        Self {
            cfg: WindowConfig {
                title: title.into(),
                width,
                height,
                bg: Color::hex(0xF3F3F3),
                screenshot: None,
            },
            render: None,
        }
    }

    pub fn background(mut self, c: Color) -> Self {
        self.cfg.bg = c;
        self
    }

    /// 截屏模式：渲染一帧存 PNG 后退出。常用于自动化验证。
    pub fn screenshot(mut self, path: impl Into<PathBuf>) -> Self {
        self.cfg.screenshot = Some(path.into());
        self
    }

    /// 从命令行解析 `--screenshot <path>`（便于 demo 统一接入自动化）。
    pub fn screenshot_from_args(mut self) -> Self {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--screenshot") {
            if let Some(p) = args.get(i + 1) {
                self.cfg.screenshot = Some(PathBuf::from(p));
            }
        }
        self
    }

    /// 设置自定义渲染回调（Phase 0）。Phase 1 起改为 `.content(node)`。
    pub fn on_render(mut self, f: impl FnMut(&mut Pixmap, Size) + 'static) -> Self {
        self.render = Some(Box::new(f));
        self
    }

    pub fn run(self) {
        let render = self.render.unwrap_or_else(|| Box::new(|_, _| {}));
        win32::run(self.cfg, render);
    }
}
