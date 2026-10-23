//! 可输入的下拉框 ComboBox：文本框 + 右端下拉箭头，既能直接输入，也能从列表里选。
//!
//! 由两个节点组成：
//!
//! - [`ComboField`]：锚点。包一个 `TextInput`（与 stepper 中部数字框同一种「同节点包一层」
//!   写法，命中 / 输入法 / 光标的一切内部假设照旧成立），另外接管下拉箭头的点击与列表开着
//!   时的 ↑↓ / Enter / Esc。
//! - [`ComboList`]：挂在锚点上的树内浮层（[`Element::popup`]），自绘选项行。
//!
//! 列表**不走宿主菜单**（`show_dropdown_menu`）：宿主菜单打开后独占键盘，字符键被当成助记
//! 字母吃掉，「边打字边过滤」就无从谈起。树内浮层不抢焦点——键盘一直在输入框里，列表只是
//! 跟着正文变的一块显示。
//!
//! 过滤口径：**展开方式决定过滤与否**。点箭头 / 按 ↓ 展开时列出全部选项（用户想看有哪些）；
//! 打字时按前缀（不区分大小写）过滤并自动展开，没有匹配项就收起。打字不会预选任何一项，
//! 于是 Enter 提交的始终是框里打的字，只有用 ↑↓ 明确挑中了一项时 Enter 才取那一项（指针
//! 碰巧停在列表上不算挑选，要选就点下去）。
//!
//! 主题：不另设覆盖层。输入框部分就是 `TextInput`（读 `theme.input`），箭头读
//! `theme.dropdown.chevron`，列表读 `theme.menu`——与 `Dropdown` 收起态和它展开的列表是
//! 同一组颜色，两者并排时必须一致，单独开一套只会多一处可能失配的配置。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, KeyEvent, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::{signal, Signal};
use crate::spec::Align;
use crate::style::Style;
use crate::text::{TextEngine, TextStyle};
use crate::ui::inputs::{TextInput, TEXT_PAD};
use crate::ui::{Element, TextContent};

/// 下拉箭头区宽（自右沿量起）：右留白 12 + 箭头 18，与 `Dropdown` 的箭头同位置。
const CHEVRON_ZONE: i32 = 30;
const CHEVRON_W: i32 = 18;
const CHEVRON_PAD_X: i32 = 12;
/// 列表行规格与宿主菜单一致（`app/menu.rs` 的 `MENU_ITEM_H` / `MENU_VPAD` / `MENU_PAD_X` /
/// 勾选列 / 间距）：组合框的列表与 `Dropdown` 展开的列表并排出现时，行高差一像素都看得出。
const ROW_H: i32 = 32;
const VPAD: i32 = 6;
const PAD_X: i32 = 12;
const CHECK_W: i32 = 16;
const GAP: i32 = 8;
/// 列表最多同时显示的行数，多出来的滚动。
const MAX_ROWS: usize = 8;

/// 选项来源：构建期的静态表（[`TextContent`]，`t!` 换语言即跟随）或绑定的信号。
enum ComboOptions {
    Static(Vec<TextContent>),
    Bound(Signal<Vec<String>>),
}

/// 可见选项的缓存键：选项来源的版本 + 过滤词。指针每动一下都要读可见表，不缓存就是
/// 每个 Move 克隆整张表、逐项转小写——几百项的字体表上明显发涩。
struct VisibleCache {
    ver: u64,
    query: Option<String>,
    list: Rc<Vec<String>>,
}

/// 锚点与浮层之间共享的状态。都是控件内部装配细节，只在这两个节点间流动，故用
/// `Cell`/`RefCell` 而不是信号（理由同 stepper 的 `Shared`）；唯一的信号 `open` 是
/// [`Element::popup`] 要的展开开关。
pub(crate) struct ComboState {
    text: Signal<String>,
    options: RefCell<ComboOptions>,
    open: Signal<bool>,
    /// 过滤词：`None` = 列出全部（点箭头 / ↓ 展开），`Some` = 打字触发的前缀过滤。
    query: RefCell<Option<String>>,
    /// 高亮行（在**当前可见列表**里的下标）。键盘与指针共用一个，与宿主菜单一致。
    hover: Cell<Option<usize>>,
    /// 高亮是键盘挑的（↑↓ / 展开时落在当前值上）。只有它才让 Enter 取那一项：
    /// 指针只是碰巧停在列表上就被 Enter 采纳，会把用户打的字悄悄换掉。
    kb_hover: Cell<bool>,
    /// 首个可见行（超过 [`MAX_ROWS`] 时滚动）。
    top: Cell<usize>,
    /// 上次对齐下标时见到的选项版本：选项换了，旧高亮下标指的就是另一项了。
    seen_ver: Cell<u64>,
    cache: RefCell<Option<VisibleCache>>,
    /// 列表行被点中、待锚点落实的选项文字。浮层不碰 `TextInput`，选中要经锚点走
    /// 「用户编辑 + 提交」那条路，故只登记，由冒泡上来的同一次 Up 交给锚点办。
    pick: RefCell<Option<String>>,
}

impl ComboState {
    fn new(text: Signal<String>, options: Vec<TextContent>, open: Signal<bool>) -> Self {
        Self {
            text,
            options: RefCell::new(ComboOptions::Static(options)),
            open,
            query: RefCell::new(None),
            hover: Cell::new(None),
            kb_hover: Cell::new(false),
            top: Cell::new(0),
            seen_ver: Cell::new(0),
            cache: RefCell::new(None),
            pick: RefCell::new(None),
        }
    }

    /// 选项来源的版本。静态表恒为 0（换语言的译文变化由每次展开时清缓存兜住）。
    fn options_ver(&self) -> u64 {
        match &*self.options.borrow() {
            ComboOptions::Static(_) => 0,
            ComboOptions::Bound(sig) => sig.version(),
        }
    }

    /// 当前可见的选项（按过滤词筛过），带缓存。
    fn visible(&self) -> Rc<Vec<String>> {
        let ver = self.options_ver();
        let query = self.query.borrow().clone();
        if let Some(c) = &*self.cache.borrow() {
            if c.ver == ver && c.query == query {
                return c.list.clone();
            }
        }
        let keep = |o: &str| match &query {
            None => true,
            Some(q) => o.to_lowercase().starts_with(&q.to_lowercase()),
        };
        let list: Vec<String> = match &*self.options.borrow() {
            ComboOptions::Static(v) => v
                .iter()
                .map(|t| t.resolve().into_owned())
                .filter(|o| keep(o))
                .collect(),
            ComboOptions::Bound(sig) => {
                sig.with(|v| v.iter().filter(|o| keep(o)).cloned().collect())
            }
        };
        let list = Rc::new(list);
        self.cache.replace(Some(VisibleCache {
            ver,
            query,
            list: list.clone(),
        }));
        list
    }

    /// 与正文完全相同的那一项（「当前值在列表里」），供高亮与打勾。
    fn current_in(&self, list: &[String]) -> Option<usize> {
        self.text.with(|t| list.iter().position(|o| o == t))
    }

    fn is_open(&self) -> bool {
        self.open.get()
    }

    fn set_open(&self, on: bool) {
        // `Signal::set` 不做相等短路，同值写入也会标脏。
        if self.open.get() != on {
            self.open.set(on);
        }
    }

    fn close(&self) {
        self.set_open(false);
        self.query.replace(None);
        self.hover.set(None);
        self.kb_hover.set(false);
    }

    fn max_top(n: usize) -> usize {
        n.saturating_sub(MAX_ROWS)
    }

    /// 把高亮与滚动位置对齐到当前可见表：选项换了（展开着被 `options_signal` 换成更短 /
    /// 重排过的表）时，旧下标要么越界——行点了没反应、Enter 取不到项——要么指着另一项，
    /// Enter 静默选错。读可见表的每条路径（测量、绘制、事件）都先过这一道。
    fn settle(&self) -> usize {
        let n = self.visible().len();
        let ver = self.options_ver();
        if self.seen_ver.replace(ver) != ver || self.hover.get().is_some_and(|h| h >= n) {
            self.hover.set(None);
            self.kb_hover.set(false);
        }
        if self.top.get() > Self::max_top(n) {
            self.top.set(Self::max_top(n));
        }
        n
    }

    /// 让高亮行落在可见窗口里。
    fn reveal(&self, n: usize) {
        let top = self.top.get().min(Self::max_top(n));
        self.top.set(match self.hover.get() {
            Some(h) if h < top => h,
            Some(h) if h >= top + MAX_ROWS => h + 1 - MAX_ROWS,
            _ => top,
        });
    }
}

/// 锚点：文本框 + 下拉箭头。见模块文档。
pub struct ComboField {
    inner: TextInput,
    state: Rc<ComboState>,
    /// 指针停在箭头区：光标换成箭头而不是 I 形。
    over_chevron: Cell<bool>,
    /// 本次按下落在箭头区。松手时只认它：在文字区按下拖选、松手碰巧落到箭头区的那次
    /// Up 必须交还文本框，否则它的拖选与指针捕获永远不释放，整个窗口再也点不动。
    chevron_down: bool,
    /// 选中列表项后全选正文（[`Element::select_all_on_pick`]）；默认光标落到末尾。
    select_all_on_pick: bool,
}

impl ComboField {
    fn new(text: Signal<String>, state: Rc<ComboState>) -> Self {
        let mut inner = TextInput::new(text, String::new());
        // 文字在箭头左侧留 4px 截止。
        inner.config_mut().trail = CHEVRON_ZONE - TEXT_PAD + 4;
        Self {
            inner,
            state,
            over_chevron: Cell::new(false),
            chevron_down: false,
            select_all_on_pick: false,
        }
    }

    /// 内含的文本框（`Element` 上的 text_input 修饰符经它落到组合框里）。
    pub(crate) fn inner_mut(&mut self) -> &mut TextInput {
        &mut self.inner
    }

    pub(crate) fn set_select_all_on_pick(&mut self, on: bool) {
        self.select_all_on_pick = on;
    }

    pub(crate) fn set_options_signal(&mut self, options: Signal<Vec<String>>) {
        self.state.options.replace(ComboOptions::Bound(options));
        self.state.cache.replace(None);
    }

    fn in_chevron(&self, bounds: Rect, p: crate::geometry::Point) -> bool {
        bounds.w >= CHEVRON_ZONE && bounds.contains(p) && p.x >= bounds.right() - CHEVRON_ZONE
    }

    /// 列出全部选项展开，高亮当前值。没有选项、或只读，就不展开；返回是否展开了。
    fn open_full(&self, ctx: &mut EventCtx) -> bool {
        if self.inner.is_read_only() {
            return false;
        }
        let st = &self.state;
        st.query.replace(None);
        // 静态表的译文可能随语言换了：展开时现取一遍。
        st.cache.replace(None);
        let list = st.visible();
        if list.is_empty() {
            return false;
        }
        st.seen_ver.set(st.options_ver());
        st.hover.set(st.current_in(&list));
        st.kb_hover.set(st.hover.get().is_some());
        st.top.set(0);
        st.reveal(list.len());
        st.set_open(true);
        // 浮层的显隐与高度都变了：重排 + 整窗（浮层不在本节点的脏区里）。
        ctx.mark_dirty_all();
        true
    }

    fn close_list(&self, ctx: &mut EventCtx) {
        if self.state.is_open() {
            self.state.close();
            ctx.mark_dirty_all();
        }
    }

    /// ↑↓ 移动高亮，首尾回绕（与宿主菜单一致）。
    fn move_hover(&self, ctx: &mut EventCtx, down: bool) {
        let st = &self.state;
        let n = st.settle();
        if n == 0 {
            return;
        }
        let next = match (st.hover.get(), down) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(h), true) => (h + 1) % n,
            (Some(h), false) => (h + n - 1) % n,
        };
        st.hover.set(Some(next));
        st.kb_hover.set(true);
        st.reveal(n);
        ctx.mark_dirty_all();
    }

    /// 选中一项：以用户编辑的身份写进正文、收起列表、提交。
    fn apply_pick(&mut self, ctx: &mut EventCtx, s: &str) {
        self.state.close();
        ctx.mark_dirty_all();
        if self.inner.is_read_only() {
            return;
        }
        self.inner.replace_as_edit(s, self.select_all_on_pick);
        self.inner.commit_now(ctx);
    }

    /// 用户改了正文：按前缀过滤，有匹配就展开、没有（或删空了）就收起。不预选任何
    /// 一项——否则 Enter 会把用户打的字替换成第一个匹配项。
    fn on_typed(&self, ctx: &mut EventCtx) {
        let st = &self.state;
        let was_open = st.is_open();
        let q = st.text.get();
        if q.is_empty() {
            st.close();
        } else {
            st.query.replace(Some(q));
            st.hover.set(None);
            st.kb_hover.set(false);
            st.top.set(0);
            if st.visible().is_empty() {
                st.close();
            } else {
                st.set_open(true);
            }
        }
        // 列表收着、也继续收着时不升整窗：打字仍是本框的局部帧。
        if was_open || st.is_open() {
            ctx.mark_dirty_all();
        }
    }

    fn on_key(&mut self, ctx: &mut EventCtx, k: &KeyEvent) -> Option<bool> {
        if !k.pressed || k.ctrl || k.meta {
            return None;
        }
        let open = self.state.is_open();
        if k.alt {
            // Alt+↓：Windows 组合框的标准展开键。其余带 Alt 的键照常交出去。
            return match k.key {
                Key::Down if !open => self.open_full(ctx).then_some(true),
                _ => None,
            };
        }
        match k.key {
            // 没有可展开的选项时不吞，↓ 照常交给文本框（`on_nav_key`）。
            Key::Down if !open => self.open_full(ctx).then_some(true),
            Key::Down => {
                self.move_hover(ctx, true);
                Some(true)
            }
            Key::Up if open => {
                self.move_hover(ctx, false);
                Some(true)
            }
            Key::Enter if open => {
                let st = &self.state;
                st.settle();
                let picked = st
                    .hover
                    .get()
                    .filter(|_| st.kb_hover.get())
                    .and_then(|h| st.visible().get(h).cloned());
                match picked {
                    Some(s) => {
                        // 吃掉这次 Enter：它是「选这一项」，不该再去按对话框的默认按钮。
                        self.apply_pick(ctx, &s);
                        Some(true)
                    }
                    // 没挑中任何项：收起，Enter 照常交给文本框（提交 / on_submit）。
                    None => {
                        self.close_list(ctx);
                        None
                    }
                }
            }
            Key::Escape if open => {
                self.close_list(ctx);
                Some(true)
            }
            // Tab 离开：收起，焦点导航照常。
            Key::Tab if open => {
                self.close_list(ctx);
                None
            }
            _ => None,
        }
    }
}

/// 这个键交给文本框后若改了正文，算不算「用户在打字」。
///
/// 只看正文版本号分不出来：Enter 会跑 `on_commit` / `on_submit`，导航键会跑
/// `on_nav_key`，应用在这些回调里改写正文（`on_commit` 里规整格式正是 `numeric()`
/// 文档推荐的写法）也会让版本号变——那不是打字，不该再按新正文展开过滤列表。
fn is_edit_key(k: Key) -> bool {
    !matches!(
        k,
        Key::Enter | Key::Tab | Key::Up | Key::Down | Key::PageUp | Key::PageDown | Key::Escape
    )
}

impl Widget for ComboField {
    fn frame_corner(&self, bounds: Rect) -> Option<f32> {
        self.inner.frame_corner(bounds)
    }
    fn frame_shows_focus(&self) -> bool {
        self.inner.frame_shows_focus()
    }

    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        self.inner.measure(avail, style, text)
    }

    fn paint(
        &self,
        bounds: Rect,
        content: Rect,
        focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        self.inner
            .paint(bounds, content, focused, enabled, canvas, style);
        // 下拉箭头 ▼：位置、线宽与 `Dropdown` 相同。展开时尖朝上，提示「再点收起」。
        if bounds.w < CHEVRON_ZONE {
            return;
        }
        let th = crate::theme::current();
        // 只读与禁用一样画弱化色：两者都展不开列表。
        let color = if enabled && !self.inner.is_read_only() {
            th.dropdown.chevron(&th.palette)
        } else {
            th.palette.text_disabled
        };
        let cx = bounds.right() as f32 - CHEVRON_PAD_X as f32 - CHEVRON_W as f32 / 2.0;
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        let d = if self.state.is_open() { -1.0 } else { 1.0 };
        canvas.draw_polyline(
            &[
                (cx - 4.0, cy - 2.0 * d),
                (cx, cy + 3.0 * d),
                (cx + 4.0, cy - 2.0 * d),
            ],
            1.6,
            &Paint::fill(color),
        );
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => {
                // 列表行被点中：浮层登记了选项，冒泡到这里由锚点落实。
                if p.kind == PointerKind::Up {
                    let pick = self.state.pick.borrow_mut().take();
                    if let Some(s) = pick {
                        self.apply_pick(ctx, &s);
                        return true;
                    }
                }
                let b = ctx.bounds();
                match p.kind {
                    PointerKind::Move => self.over_chevron.set(self.in_chevron(b, p.pos)),
                    PointerKind::Leave => self.over_chevron.set(false),
                    _ => {}
                }
                let left = p.button == crate::event::MouseButton::Left;
                match p.kind {
                    PointerKind::Down if left && self.in_chevron(b, p.pos) => {
                        self.chevron_down = true;
                        ctx.request_focus();
                        if self.state.is_open() {
                            self.close_list(ctx);
                        } else {
                            self.open_full(ctx);
                        }
                        return true;
                    }
                    PointerKind::Up if std::mem::take(&mut self.chevron_down) => return true,
                    _ => {}
                }
                self.inner.on_event(ctx, ev)
            }
            Event::Key(k) => {
                if let Some(consumed) = self.on_key(ctx, k) {
                    return consumed;
                }
                let v0 = self.state.text.version();
                let consumed = self.inner.on_event(ctx, ev);
                if k.pressed && is_edit_key(k.key) && self.state.text.version() != v0 {
                    self.on_typed(ctx);
                }
                consumed
            }
        }
    }

    /// 展开着的列表随选项变化对齐：选项被清空就收起（否则剩一块 0 高的面板）。
    fn on_update(&mut self, _ctx: &mut EventCtx) {
        if self.state.is_open() && self.state.settle() == 0 {
            self.state.close();
        }
    }

    fn on_blur(&mut self, ctx: &mut EventCtx) {
        self.inner.on_blur(ctx);
    }
    fn focus_lost(&mut self) {
        self.state.close();
        self.chevron_down = false;
        self.inner.focus_lost();
    }
    fn reset_interaction(&mut self) {
        self.state.close();
        self.chevron_down = false;
        self.inner.reset_interaction();
    }
    fn cursor(&self) -> CursorShape {
        if self.over_chevron.get() {
            CursorShape::Arrow
        } else {
            self.inner.cursor()
        }
    }

    // ---- 以下纯转发 ----

    fn focusable(&self) -> bool {
        self.inner.focusable()
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        self.inner.ime_caret()
    }
    fn set_composing(&mut self, composing: bool) {
        self.inner.set_composing(composing);
    }
    fn set_preedit(&mut self, pe: &crate::event::Preedit) {
        self.inner.set_preedit(pe);
    }
    fn selection_range(&self) -> Option<(usize, usize)> {
        self.inner.selection_range()
    }
    fn ime_text(&self) -> Option<String> {
        self.inner.ime_text()
    }
    fn ime_hints(&self) -> crate::event::ImeHints {
        self.inner.ime_hints()
    }
    fn accepts_ime(&self) -> bool {
        self.inner.accepts_ime()
    }
    fn accepts_text(&self) -> bool {
        self.inner.accepts_text()
    }
    fn keys_damage_self_only(&self) -> bool {
        self.inner.keys_damage_self_only()
    }
    fn wants_right_click(&self) -> bool {
        self.inner.wants_right_click()
    }
    fn take_click(&mut self, f: crate::core::ClickFn) {
        self.inner.take_click(f);
    }
}

/// 浮层里的选项列表。见模块文档。
struct ComboList {
    state: Rc<ComboState>,
}

impl ComboList {
    /// 指针所在的行（可见列表下标）。
    fn row_at(&self, b: Rect, y: i32, n: usize) -> Option<usize> {
        let dy = y - (b.y + VPAD);
        if dy < 0 {
            return None;
        }
        // `top` 已由调用方经 `settle` 对齐到当前可见表——绘制与命中用的是同一个值。
        let top = self.state.top.get();
        let i = top + (dy / ROW_H) as usize;
        (i < n && i < top + MAX_ROWS).then_some(i)
    }
}

impl Widget for ComboList {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        let n = self.state.settle().min(MAX_ROWS) as i32;
        Size::new(avail.w, if n == 0 { 0 } else { n * ROW_H + 2 * VPAD })
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        _enabled: bool,
        canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
        let th = crate::theme::current();
        let (pal, mt) = (&th.palette, &th.menu);
        let r = bounds;
        let corner = th.metrics.corner_lg;
        let (x, y, w, h) = (r.x as f32, r.y as f32, r.w as f32, r.h as f32);
        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(mt.bg(pal)));
        canvas.stroke_round_rect(
            x,
            y,
            w,
            h,
            corner,
            th.metrics.border_width.to_logical(canvas.dpi_scale()),
            &Paint::fill(mt.border(pal)),
        );

        self.state.settle();
        let list = self.state.visible();
        let n = list.len();
        let cur = self.state.current_in(&list);
        let hover = self.state.hover.get();
        let top = self.state.top.get();
        let ts = TextStyle::new(th.metrics.font_md);
        let label_x = r.x + PAD_X + CHECK_W + GAP;
        canvas.save();
        canvas.clip_rect(Rect::new(r.x, r.y + VPAD, r.w, r.h - 2 * VPAD));
        for (row, i) in (top..n.min(top + MAX_ROWS)).enumerate() {
            let ry = r.y + VPAD + row as i32 * ROW_H;
            if hover == Some(i) {
                canvas.fill_round_rect(
                    (r.x + 4) as f32,
                    (ry + 1) as f32,
                    (r.w - 8) as f32,
                    (ROW_H - 2) as f32,
                    th.metrics.corner_md,
                    &Paint::fill(mt.hover(pal)),
                );
            }
            if cur == Some(i) {
                canvas.draw_text(
                    "\u{2713}",
                    Rect::new(r.x + PAD_X, ry, CHECK_W, ROW_H),
                    mt.accent(pal),
                    Align::Center,
                    &ts,
                );
            }
            canvas.draw_text(
                &list[i],
                Rect::new(label_x, ry, (r.right() - PAD_X - label_x).max(0), ROW_H),
                mt.text(pal),
                Align::Start,
                &ts,
            );
        }
        canvas.restore();

        // 超出可见行数：右侧画一根滚动指示条（与宿主菜单同款）。
        if n > MAX_ROWS {
            let track = (r.h - 2 * VPAD) as f32;
            let thumb_h = (track * MAX_ROWS as f32 / n as f32).max(16.0);
            let thumb_y =
                (r.y + VPAD) as f32 + (track - thumb_h) * top as f32 / (n - MAX_ROWS) as f32;
            canvas.fill_round_rect(
                (r.right() - 8) as f32,
                thumb_y,
                5.0,
                thumb_h,
                2.5,
                &Paint::fill(mt.border(pal)),
            );
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else {
            return false;
        };
        let b = ctx.bounds();
        let n = self.state.settle();
        // 右键：吃掉，不让它冒泡到锚点的文本框去弹剪贴板菜单、挪光标。
        if p.button != crate::event::MouseButton::Left
            && matches!(p.kind, PointerKind::Down | PointerKind::Up)
        {
            return true;
        }
        match p.kind {
            PointerKind::Move => {
                let row = self.row_at(b, p.pos.y, n);
                if row.is_some() && row != self.state.hover.get() {
                    self.state.hover.set(row);
                    self.state.kb_hover.set(false);
                    ctx.mark_dirty();
                }
                true
            }
            // 指针挑的高亮随指针离开而撤掉；键盘挑的留着。
            PointerKind::Leave => {
                if !self.state.kb_hover.get() && self.state.hover.get().is_some() {
                    self.state.hover.set(None);
                    ctx.mark_dirty();
                }
                true
            }
            PointerKind::Wheel(d) if n > MAX_ROWS => {
                let max_top = n - MAX_ROWS;
                let top = self.state.top.get();
                let next = if d > 0 {
                    top.saturating_sub(1)
                } else {
                    (top + 1).min(max_top)
                };
                if next != top {
                    self.state.top.set(next);
                    ctx.mark_dirty();
                }
                true
            }
            PointerKind::Up => {
                match self.row_at(b, p.pos.y, n) {
                    Some(i) if b.contains(p.pos) => {
                        self.state
                            .pick
                            .replace(self.state.visible().get(i).cloned());
                        // 不消费：冒泡到父节点（锚点）由它落实选中，见 `ComboState::pick`。
                        false
                    }
                    _ => true,
                }
            }
            // 吃掉按下：不让它冒泡到锚点的文本框里去定位光标。
            PointerKind::Down => true,
            _ => false,
        }
    }

    fn wants_right_click(&self) -> bool {
        true
    }
}

/// 组装组合框（见 [`Element::combo_box`]）。
pub(crate) fn build(text: Signal<String>, options: Vec<TextContent>) -> Element {
    let open = signal(false);
    let state = Rc::new(ComboState::new(text, options, open));
    let th = crate::theme::current();
    let list = Element::leaf()
        .widget(ComboList {
            state: state.clone(),
        })
        // 与锚点同宽（浮层的 `width_match` 对齐锚点，见 `Element::popup`）。
        .width_match()
        .corner(th.metrics.corner_lg)
        .shadow(th.menu.shadow());
    Element::leaf()
        .widget(ComboField::new(text, state))
        // 选项信号在展开时被换掉，要在重排前把高亮 / 滚动位置对齐（见 `on_update`）。
        .reactive()
        .popup(open, list)
}
