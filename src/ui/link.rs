//! 可点击链接文本控件 `Link`。
//!
//! 视觉为链接色文本 + 下划线；交互复用 Button 范式（hover/press 三态 + 点击/回车激活）。
//! 激活时优先调用 `on_click` 回调，否则用 `url` 请求宿主打开（`EventCtx::open_url`
//! → 平台 `ShellExecute`）。悬停显示手型光标（`Widget::cursor` 返回 `Hand`）。
//! 禁用态由核心层统一管理：禁用时核心拦事件、跳 Tab，并把启用态传入 paint，
//! 据此置灰且宿主不显示手型。

use std::cell::Cell;

use crate::anim::{Easing, Transition};
use crate::core::{ClickFn, EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::style::Style;
use crate::text::TextEngine;

/// 链接三态（与 Button 同构）。
#[derive(PartialEq, Eq, Clone, Copy)]
enum LinkState {
    Normal,
    Hover,
    Press,
}

/// 链接文本控件：链接色 + 下划线，点击/回车激活。
pub struct Link {
    text: String,
    /// 激活时打开的 URL/路径（`on_click` 未设时生效）。
    url: Option<String>,
    /// 是否绘制下划线（默认 true）。
    underline: bool,
    state: LinkState,
    on_click: Option<ClickFn>,
    /// 链接色补间（hover/press 淡变）。retarget-in-paint；首帧靠 `primed` 落定。
    color_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
}

impl Link {
    pub fn new(text: String) -> Self {
        Self {
            text,
            url: None,
            underline: true,
            state: LinkState::Normal,
            on_click: None,
            color_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
        }
    }
    /// 设置激活时打开的 URL/路径（供 Builder 的 `.url()` 调用）。
    pub fn set_url(&mut self, url: String) {
        self.url = Some(url);
    }
    /// 是否绘制下划线（供 Builder 的 `.underline()` 调用）。
    pub fn set_underline(&mut self, on: bool) {
        self.underline = on;
    }
    /// 激活：优先回调，否则打开 URL（fire-and-forget 交宿主）。
    fn activate(&mut self, ctx: &mut EventCtx) {
        if let Some(cb) = self.on_click.as_mut() {
            cb(ctx);
        } else if let Some(url) = &self.url {
            ctx.open_url(url);
        }
    }
}

impl Widget for Link {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        text.measure(
            &self.text,
            style.font_family.as_deref(),
            style.font_size,
            None,
        )
    }

    fn paint(
        &self,
        _bounds: Rect,
        content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (pal, lk) = (&th.palette, &th.link);
        // 禁用：链接色降为 text_disabled；否则按三态取链接色。
        let target = if !enabled {
            pal.text_disabled
        } else {
            match self.state {
                LinkState::Normal => lk.color(pal),
                LinkState::Hover => lk.hover(pal),
                LinkState::Press => lk.pressed(pal),
            }
        };
        // 颜色补间：首帧落定，其后三态淡变。
        let mut anim = self.color_anim.get();
        if !self.primed.get() {
            anim = Transition::new(target);
            self.primed.set(true);
        } else if anim.target() != target {
            anim.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let color = anim.animate();
        self.color_anim.set(anim);
        canvas.draw_text(
            &self.text,
            content,
            color,
            style.text_align,
            style.font_family.as_deref(),
            style.font_size,
        );
        if self.underline {
            // 下划线贴文字底缘；x 跟随文字（Start 对齐），长度取文字实测宽。
            let tw = canvas
                .measure_text(&self.text, style.font_family.as_deref(), style.font_size)
                .w;
            let y = (content.y + content.h - 1) as f32;
            let x0 = content.x as f32;
            canvas.draw_line(x0, y, x0 + tw as f32, y, 1.0, &Paint::fill(color));
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        // 禁用由核心层统一拦截（call_on_event 不派发到禁用节点），此处无需判断。
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    if self.state == LinkState::Normal {
                        self.state = LinkState::Hover;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.state != LinkState::Press {
                        self.state = LinkState::Normal;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    self.state = LinkState::Press;
                    ctx.capture();
                    ctx.request_focus();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    let was_press = self.state == LinkState::Press;
                    let inside = ctx.bounds().contains(p.pos);
                    self.state = if inside {
                        LinkState::Hover
                    } else {
                        LinkState::Normal
                    };
                    ctx.release_capture();
                    ctx.mark_dirty();
                    if was_press && inside {
                        self.activate(ctx);
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && (k.key == Key::Enter || k.key == Key::Space) => {
                self.activate(ctx);
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        // 禁用链接的 Tab 跳过由核心层 collect_focusable 统一处理。
        true
    }

    fn cursor(&self) -> CursorShape {
        // 启用时手型；禁用回退由宿主统一处理（不在此判断）。
        CursorShape::Hand
    }

    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Tree;
    use crate::event::{KeyEvent, MouseButton, PointerEvent};
    use crate::geometry::Point;
    use crate::ui::Element;
    use std::cell::Cell;
    use std::rc::Rc;

    /// 构建单链接树并返回 (tree, root)。链接绝对矩形 = 0,0,w,h（根节点）。
    fn link_tree(el: Element) -> (Tree, crate::core::NodeId) {
        let mut tree = Tree::new();
        let root = el.width(120).height(20).build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 60), &mut crate::text::NullTextEngine);
        (tree, root)
    }

    #[test]
    fn click_fires_on_click_callback() {
        let hit = Rc::new(Cell::new(0));
        let h2 = hit.clone();
        let (mut tree, _root) =
            link_tree(Element::link("打开").on_click(move |_| h2.set(h2.get() + 1)));
        let mut hover = None;
        let mut capture = None;
        let at = Point::new(10, 10);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert_eq!(hit.get(), 1, "左键按下→抬起应触发一次回调");
        assert!(res.open_url.is_none(), "设了 on_click 时不应再走 open_url");
    }

    #[test]
    fn click_without_callback_requests_open_url() {
        let (mut tree, _root) = link_tree(Element::link("官网").url("https://example.com"));
        let mut hover = None;
        let mut capture = None;
        let at = Point::new(10, 10);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert_eq!(
            res.open_url.as_deref(),
            Some("https://example.com"),
            "无回调应请求打开 url"
        );
    }

    #[test]
    fn enter_key_activates() {
        let (mut tree, root) = link_tree(Element::link("官网").url("https://example.com"));
        let res = tree.dispatch_key(
            KeyEvent {
                key: Key::Enter,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(root),
        );
        assert_eq!(
            res.open_url.as_deref(),
            Some("https://example.com"),
            "回车应激活链接"
        );
    }

    #[test]
    fn link_reports_hand_cursor() {
        let (tree, root) = link_tree(Element::link("x").url("https://example.com"));
        assert_eq!(tree.cursor_at(root), CursorShape::Hand);
    }

    #[test]
    fn disabled_link_skips_open_and_cursor() {
        let (mut tree, root) = link_tree(
            Element::link("官网")
                .url("https://example.com")
                .disabled(true),
        );
        // 核心拦截禁用节点：点击不产生 open_url。
        let mut hover = None;
        let mut capture = None;
        let at = Point::new(10, 10);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert!(res.open_url.is_none(), "禁用链接点击不应打开");
        assert!(
            !tree.node_enabled(root),
            "禁用态应被核心识别（宿主据此回退箭头光标）"
        );
    }
}
