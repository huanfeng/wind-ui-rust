//! 应用入口与交互宿主。
//!
//! `App` 构建器组装窗口配置与控件树；`UiHost` 持有运行期交互状态
//! （树、文字引擎、hover/capture/focus）并实现 `AppHandler` 供平台驱动。

use std::path::PathBuf;

use tiny_skia::Pixmap;

use crate::core::{NodeId, Tree};
use crate::event::{Key, MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Size};
use crate::platform::win32::{self, WindowConfig};
use crate::platform::AppHandler;
use crate::render::SkiaCanvas;
use crate::text::{DWriteEngine, TextEngine};
use crate::ui::Element;

type RenderClosure = Box<dyn FnMut(&mut Pixmap, Size)>;

/// 应用构建器。命令式 API 的根入口。
pub struct App {
    cfg: WindowConfig,
    render: Option<RenderClosure>,
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
                screenshot_scale: 1.0,
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

    /// 从命令行解析 `--screenshot <path>` 与可选 `--scale <f>`（高 DPI 截屏验证）。
    pub fn screenshot_from_args(mut self) -> Self {
        let args: Vec<String> = std::env::args().collect();
        if let Some(i) = args.iter().position(|a| a == "--screenshot") {
            if let Some(p) = args.get(i + 1) {
                self.cfg.screenshot = Some(PathBuf::from(p));
            }
        }
        if let Some(i) = args.iter().position(|a| a == "--scale") {
            if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f32>().ok()) {
                self.cfg.screenshot_scale = v;
            }
        }
        self
    }

    /// 底层渲染回调（无控件树时使用）。
    pub fn on_render(mut self, f: impl FnMut(&mut Pixmap, Size) + 'static) -> Self {
        self.render = Some(Box::new(f));
        self
    }

    /// 设置控件树根（常规入口）。
    pub fn content(mut self, root: Element) -> Self {
        self.content = Some(root);
        self
    }

    pub fn run(self) {
        let handler: Box<dyn AppHandler> = if let Some(f) = self.render {
            Box::new(ClosureHandler { f })
        } else if let Some(root) = self.content {
            Box::new(UiHost::new(root))
        } else {
            Box::new(ClosureHandler { f: Box::new(|_, _| {}) })
        };
        win32::run(self.cfg, handler);
    }
}

/// 把底层渲染闭包适配为 AppHandler（不处理输入）。
struct ClosureHandler {
    f: RenderClosure,
}

impl AppHandler for ClosureHandler {
    fn render(&mut self, pixmap: &mut Pixmap, size: Size) {
        (self.f)(pixmap, size);
    }
}

/// 控件树交互宿主：渲染 + 事件分发 + 焦点管理。
struct UiHost {
    tree: Tree,
    engine: DWriteEngine,
    hover: Option<NodeId>,
    capture: Option<NodeId>,
    focus: Option<NodeId>,
    focus_order: Vec<NodeId>,
    close: bool,
    /// DPI 缩放因子（逻辑→物理）。
    scale: f32,
    /// 焦点环是否可见：键盘 Tab 导航时 true，鼠标聚焦时 false。
    focus_visible: bool,
}

impl UiHost {
    fn new(root: Element) -> Self {
        let mut tree = Tree::new();
        tree.root = Some(root.build(&mut tree));
        tree.clipboard = Some(Box::new(crate::platform::win32::clipboard::WinClipboard));
        Self {
            tree,
            engine: DWriteEngine::new(),
            hover: None,
            capture: None,
            focus: None,
            focus_order: Vec::new(),
            close: false,
            scale: 1.0,
            focus_visible: false,
        }
    }

    /// Tab 焦点移动（forward=正向）。返回是否变化。
    fn move_focus(&mut self, forward: bool) -> bool {
        if self.focus_order.is_empty() {
            return false;
        }
        let n = self.focus_order.len();
        let cur = self.focus.and_then(|f| self.focus_order.iter().position(|&x| x == f));
        let next = match cur {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        };
        let nf = Some(self.focus_order[next]);
        let old = self.focus;
        self.tree.set_focused(nf, old);
        self.focus = nf;
        true
    }
}

impl AppHandler for UiHost {
    fn render(&mut self, pixmap: &mut Pixmap, size: Size) {
        // pixmap 是物理像素；布局用逻辑坐标（物理 / scale），绘制时再 ×scale 放大。
        let s = self.scale;
        let logical = Size::new(
            (size.w as f32 / s).round().max(1.0) as i32,
            (size.h as f32 / s).round().max(1.0) as i32,
        );
        self.tree.layout_root(logical, &mut self.engine);
        // 布局后结构稳定，刷新 Tab 焦点顺序。
        self.focus_order = self.tree.focusable_order();
        // 若当前焦点已不在可聚焦集合中（结构变更），归一化为无焦点。
        if let Some(f) = self.focus {
            if !self.focus_order.contains(&f) {
                self.tree.set_focused(None, Some(f));
                self.focus = None;
            }
        }
        self.tree.focus_ring_visible = self.focus_visible;
        let mut canvas = SkiaCanvas::with_text(pixmap, &mut self.engine, s);
        self.tree.paint(&mut canvas);
    }

    fn on_pointer(&mut self, mut ev: crate::event::PointerEvent) -> bool {
        // 物理坐标 → 逻辑坐标（布局与命中均在逻辑空间）。
        let s = self.scale;
        ev.pos = Point::new(
            (ev.pos.x as f32 / s).round() as i32,
            (ev.pos.y as f32 / s).round() as i32,
        );
        let mut hover = self.hover;
        let mut capture = self.capture;
        let res = self.tree.dispatch_pointer(ev, &mut hover, &mut capture);
        self.hover = hover;
        self.capture = capture;
        if let Some(f) = res.focus {
            let old = self.focus;
            self.tree.set_focused(Some(f), old);
            self.focus = Some(f);
            // 鼠标聚焦：不显示焦点环，保持纯鼠标操作的纯净观感。
            self.focus_visible = false;
        }
        if res.close {
            self.close = true;
        }
        res.repaint
    }

    fn on_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        // Tab 由宿主独占用于焦点导航，并启用焦点环显示。
        if ev.key == Key::Tab {
            self.focus_visible = true;
            return self.move_focus(!ev.shift);
        }
        // 其余键先交给焦点控件；未被消费的 Escape 回退为关闭窗口。
        let res = self.tree.dispatch_key(ev, self.focus);
        if res.close {
            self.close = true;
        }
        if !res.consumed && ev.key == Key::Escape {
            self.close = true;
        }
        res.repaint
    }

    fn wants_close(&self) -> bool {
        self.close
    }

    fn capture_active(&self) -> bool {
        self.capture.is_some()
    }

    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        // 文字引擎同步 scale，保证文字测量/绘制与图形缩放一致。
        self.engine.set_scale(scale);
    }

    fn on_capture_lost(&mut self) -> bool {
        // 给捕获节点派发一个远处坐标的合成 Up，复用 Up 语义让其收尾
        // （Slider 复位拖动、Button 因 inside=false 不误触发），并清逻辑捕获。
        if self.capture.is_none() {
            return false;
        }
        let ev = PointerEvent {
            kind: PointerKind::Up,
            pos: Point::new(-1_000_000, -1_000_000),
            button: MouseButton::Left,
        };
        let mut hover = self.hover;
        let mut capture = self.capture;
        let res = self.tree.dispatch_pointer(ev, &mut hover, &mut capture);
        self.hover = hover;
        self.capture = capture;
        res.repaint
    }
}
