//! 菜单栏：一排标题，点开即宿主浮层菜单（与右键菜单、下拉同一套面板）。
//!
//! 控件自己只做两件事：画标题、收第一下点击。展开之后指针与键盘都归宿主浮层独占，
//! 控件再也收不到事件——"滑到相邻标题自动切换、←→ 跨菜单、点标题收起、F10 / 单击
//! Alt 激活、Alt+助记键直开"这些原生手感全在宿主里（`app::menu`），控件经
//! [`MenuBarLink`] 把各标题的位置与项生成器交出去。
//!
//! 标题**每次展开现建**项：启用 / 勾选态因此总反映当前状态，调用方不必维护信号。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{
    mnemonic_split, Event, MenuBarLink, MenuBarSlot, MenuItem, MouseButton, PointerKind,
};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::{TextEngine, TextStyle};

/// 标题左右内边距。
const TITLE_PAD_X: i32 = 10;
/// 标题高亮块相对栏上下的内缩。
const TITLE_INSET_Y: i32 = 3;

/// 菜单栏的一个标题：文字、助记字母、项生成器。
///
/// 助记字母不从标题文字里解析（`&File` 那套）：中文标题没有可下划线的字母，惯例是
/// `"文件(F)"` 配 `.mnemonic('F')`——括号里的字母会被画上线，Alt+F 打开它。
pub struct MenuBarEntry {
    title: crate::ui::TextContent,
    mnemonic: Option<char>,
    build: Rc<dyn Fn() -> Vec<MenuItem>>,
}

impl MenuBarEntry {
    /// 标题 + 项生成器（每次展开调用）。
    ///
    /// 标题收 [`TextContent`](crate::ui::TextContent)：可以是 `&str` / `String` /
    /// `Signal<String>` / [`t!`](crate::t)，与控件文案同一套规则。菜单栏是自绘的，标题在
    /// 每次 measure/paint 时现取，于是换语言自动跟随；下拉项由 `build` 每次展开时生成，
    /// 那里用 `tr!` 即可（同右键菜单）。
    pub fn new(
        title: impl Into<crate::ui::TextContent>,
        build: impl Fn() -> Vec<MenuItem> + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            mnemonic: None,
            build: Rc::new(build),
        }
    }
    /// 助记字母：Alt+字母直接展开本菜单；键盘激活态下按字母亦然。
    pub fn mnemonic(mut self, c: char) -> Self {
        self.mnemonic = Some(c);
        self
    }
}

/// 菜单栏控件（见模块文档）。
pub struct MenuBar {
    entries: Vec<MenuBarEntry>,
    /// 上一帧各标题的窗口矩形（paint 是 `&self`，故用 `RefCell`）。
    rects: RefCell<Vec<Rect>>,
    hover: Option<usize>,
    /// 与宿主共享的"当前展开 / 键盘激活的标题"（见 [`MenuBarLink::open`]）。
    open: Rc<Cell<Option<usize>>>,
    /// 与宿主共享的"助记字母下划线是否显示"（见 [`MenuBarLink::mnemonics`]）。
    mnemonics: Rc<Cell<bool>>,
}

impl MenuBar {
    pub fn new(entries: Vec<MenuBarEntry>) -> Self {
        Self {
            entries,
            rects: RefCell::new(Vec::new()),
            hover: None,
            open: Rc::new(Cell::new(None)),
            mnemonics: Rc::new(Cell::new(false)),
        }
    }

    fn slot_at(&self, p: crate::geometry::Point) -> Option<usize> {
        self.rects.borrow().iter().position(|r| r.contains(p))
    }

    /// 打包交给宿主的联动信息。矩形取自上一帧绘制；尚未绘制过（矩形为空）则为 `None`。
    fn link(&self) -> Option<MenuBarLink> {
        let rects = self.rects.borrow();
        if rects.len() != self.entries.len() || rects.is_empty() {
            return None;
        }
        Some(MenuBarLink {
            slots: self
                .entries
                .iter()
                .zip(rects.iter())
                .map(|(e, r)| MenuBarSlot {
                    rect: *r,
                    mnemonic: e.mnemonic,
                    build: e.build.clone(),
                })
                .collect(),
            current: 0,
            keyboard: false,
            open: self.open.clone(),
            mnemonics: self.mnemonics.clone(),
        })
    }
}

impl Widget for MenuBar {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let ts = TextStyle::of(style);
        let w: i32 = self
            .entries
            .iter()
            .map(|e| text.measure(&e.title.resolve(), &ts, None).w + 2 * TITLE_PAD_X)
            .sum();
        Size::new(w, style.font_size as i32 + 14)
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (pal, mt) = (&th.palette, &th.menu);
        let ts = TextStyle::of(style);
        let open = self.open.get();
        let mnemonics = self.mnemonics.get();
        let mut rects = Vec::with_capacity(self.entries.len());
        let mut x = bounds.x;
        for (i, e) in self.entries.iter().enumerate() {
            // 现取一次，本轮循环里复用：标题可能绑在信号或译文上，同一帧内多处引用
            // （量宽、画字、助记下划线）必须是同一份值，否则下划线会画错位置。
            let title = e.title.resolve();
            let w = canvas.measure_text(&title, &ts).w + 2 * TITLE_PAD_X;
            let r = Rect::new(
                x,
                bounds.y + TITLE_INSET_Y,
                w,
                (bounds.h - 2 * TITLE_INSET_Y).max(0),
            );
            // 展开 / 键盘激活的标题按下态；否则悬停高亮。宿主展开期间控件收不到
            // Leave，`hover` 可能是陈旧值——按下态存在时一律不画悬停。
            let active = open == Some(i);
            let hovered = open.is_none() && self.hover == Some(i) && enabled;
            if active || hovered {
                canvas.fill_round_rect(
                    r.x as f32,
                    r.y as f32,
                    r.w as f32,
                    r.h as f32,
                    th.metrics.corner_sm,
                    &Paint::fill(mt.hover(pal)),
                );
            }
            let color = if !enabled {
                mt.text_disabled(pal)
            } else if active {
                mt.accent(pal)
            } else {
                mt.text(pal)
            };
            canvas.draw_text(&title, r, color, Align::Center, &ts);
            // 助记字母下划线（标题居中绘制：先算出文字起点）。只在键盘触达过菜单栏后
            // 才画：一排常驻的下划线会把标题栏搅得很花，而鼠标用户永远用不上它们。
            if !mnemonics {
                rects.push(r);
                x += w;
                continue;
            }
            if let Some((pre, ch, _)) = e.mnemonic.and_then(|m| mnemonic_split(&title, m)) {
                let tw = canvas.measure_text(&title, &ts).w;
                let x0 = r.x + (r.w - tw) / 2 + canvas.measure_text(pre, &ts).w;
                let cw = canvas.measure_text(ch, &ts).w;
                let y = r.y + (r.h + style.font_size as i32) / 2 + 1;
                canvas.fill_rect(x0 as f32, y as f32, cw as f32, 1.0, &Paint::fill(color));
            }
            rects.push(r);
            x += w;
        }
        *self.rects.borrow_mut() = rects;
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else {
            return false;
        };
        match p.kind {
            PointerKind::Move | PointerKind::Enter => {
                let h = self.slot_at(p.pos);
                if h != self.hover {
                    self.hover = h;
                    ctx.mark_dirty();
                }
                h.is_some()
            }
            PointerKind::Leave => {
                if self.hover.is_some() {
                    self.hover = None;
                    ctx.mark_dirty();
                }
                true
            }
            // 按下即展开（原生菜单栏是按下展开，不等抬起）。展开后宿主独占指针、控件
            // 收不到 Leave，先清掉悬停免得残留。
            PointerKind::Down if p.button == MouseButton::Left => {
                let Some(i) = self.slot_at(p.pos) else {
                    return false;
                };
                self.hover = None;
                ctx.mark_dirty();
                if let Some(mut link) = self.link() {
                    link.current = i;
                    ctx.show_menu_bar(link);
                }
                true
            }
            _ => false,
        }
    }

    fn menu_bar_link(&self) -> Option<MenuBarLink> {
        self.link()
    }

    fn preserves_focus(&self) -> bool {
        true // 打开菜单不该让别处失焦，见 `Widget::preserves_focus`
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 标题跟随换语言：菜单栏是自绘的，标题在每次 measure/paint 时现取。
    ///
    /// 判据取**量出来的宽度**而不是标题字符串本身：后者在"构建时就定格成 String"的
    /// 旧实现里也能对（只要重建控件），唯有同一个控件实例量出不同宽度，才说明它真的
    /// 每次都去查了当前语言。
    #[test]
    fn 标题跟随换语言() {
        crate::i18n::install(
            crate::i18n::Locales::builder()
                .embed("[meta]\nlocale = \"zh-CN\"\n[m]\nfile = \"文件\"\n")
                .embed("[meta]\nlocale = \"en\"\n[m]\nfile = \"File and a much longer title\"\n")
                .initial(crate::i18n::Initial::Fixed("zh-CN".into()))
                .build(),
        );
        let bar = MenuBar::new(vec![MenuBarEntry::new(crate::t!("m.file"), Vec::new)]);
        let style = Style::default();
        let mut eng = crate::text::PlatformTextEngine::default();
        let zh = bar.measure(Size::new(1000, 100), &style, &mut eng).w;

        assert!(crate::i18n::LocaleHandle::new().set("en"));
        let en = bar.measure(Size::new(1000, 100), &style, &mut eng).w;

        assert!(
            en > zh,
            "换成更长的译文后同一个菜单栏应量得更宽（{zh} → {en}）"
        );

        // 复原线程局部的语言状态，理由见 `platform::tray` 里同名的那一处。
        crate::i18n::install(crate::i18n::Locales::default());
    }

    #[test]
    fn 助记字母按不分大小写切分标签() {
        assert_eq!(mnemonic_split("File", 'f'), Some(("", "F", "ile")));
        assert_eq!(mnemonic_split("文件(F)", 'F'), Some(("文件(", "F", ")")));
        assert_eq!(
            mnemonic_split("文件", 'F'),
            None,
            "标签里没有该字母就不画线"
        );
    }
}
