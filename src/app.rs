//! 应用入口（Phase 0）。
//!
//! Phase 0 仅提供「清屏 + 自定义渲染回调」的最小运行能力，Phase 1 起接入 Node 树。

use std::path::PathBuf;

use tiny_skia::Pixmap;

use crate::core::Tree;
use crate::geometry::{Color, Size};
use crate::platform::win32::{self, RenderFn, WindowConfig};
use crate::render::SkiaCanvas;
use crate::ui::Element;

/// 应用构建器。命令式 API 的根入口。
pub struct App {
    cfg: WindowConfig,
    render: Option<RenderFn>,
    content: Option<Element>,
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
            content: None,
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

    /// 设置自定义渲染回调（Phase 0 底层接口）。
    pub fn on_render(mut self, f: impl FnMut(&mut Pixmap, Size) + 'static) -> Self {
        self.render = Some(Box::new(f));
        self
    }

    /// 设置控件树根（Phase 1 起的常规入口）。
    pub fn content(mut self, root: Element) -> Self {
        self.content = Some(root);
        self
    }

    pub fn run(self) {
        // 优先级：显式 on_render > content 控件树 > 空。
        let render: RenderFn = if let Some(render) = self.render {
            render
        } else if let Some(root) = self.content {
            let mut tree = Tree::new();
            tree.root = Some(root.build(&mut tree));
            Box::new(move |pixmap: &mut Pixmap, size: Size| {
                tree.layout_root(size);
                let mut canvas = SkiaCanvas::new(pixmap);
                tree.paint(&mut canvas);
            })
        } else {
            Box::new(|_, _| {})
        };
        win32::run(self.cfg, render);
    }
}
