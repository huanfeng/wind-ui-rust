//! 命令式 Builder：单一 `Element` 类型贯穿所有控件，链式构建后一次性落入 `Tree`。
//!
//! 容器（`col`/`row`/`stack`）与叶子（`leaf`、Phase 2 起的 `label` 等）都返回
//! `Element`，`.child(...)` 接受任意 `Element`，构建时递归插入 arena。

pub mod containers;
pub mod inputs;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{ClickFn, EmptyWidget, EventCtx, Layout, Node, NodeId, Tree, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Color, Insets, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::{Align, Axis, Dimension};
use crate::style::Style;
use crate::text::TextEngine;

pub use inputs::{CheckBox, RadioButton, Slider, Switch, TextInput};

/// 文本叶子控件。
pub struct Label {
    text: String,
}

impl Label {
    pub fn new(text: String) -> Self {
        Self { text }
    }
}

impl Widget for Label {
    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        // 在可用宽度内换行：宽度受限时折行，宽松时单行。
        // 注意：avail/rect 均为 content-box（已扣 padding），measure 与 draw 同源故一致。
        // 已知限制：换行准确仅保证于显式宽度的 Label（width/width_match/weight）；
        // 纯 Wrap 宽度的多行 Label，draw 会在收敛后的窄宽重新换行，可能与 measure 行数不符。
        let max_w = if avail.w > 0 { Some(avail.w as f32) } else { None };
        text.measure(&self.text, style.font_family.as_deref(), style.font_size, max_w)
    }
    fn paint(&self, _bounds: Rect, content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        canvas.draw_text(
            &self.text,
            content,
            style.fg,
            style.text_align,
            style.font_family.as_deref(),
            style.font_size,
        );
    }
}

/// 按钮三态。
#[derive(PartialEq, Eq, Clone, Copy)]
enum BtnState {
    Normal,
    Hover,
    Press,
}

/// 交互按钮：hover/press 三态 + 点击/回车回调。
pub struct Button {
    label: String,
    state: BtnState,
    base: Color,
    hover: Color,
    press: Color,
    text_color: Color,
    on_click: Option<ClickFn>,
}

impl Button {
    pub fn new(label: String) -> Self {
        Self {
            label,
            state: BtnState::Normal,
            base: Color::hex(0x4C8BF5),
            hover: Color::hex(0x6BA3FF),
            press: Color::hex(0x3A6FD0),
            text_color: Color::WHITE,
            on_click: None,
        }
    }
}

impl Widget for Button {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let s = text.measure(&self.label, style.font_family.as_deref(), style.font_size, None);
        // 内置左右 16 / 上下 9 的内边距
        Size::new(s.w + 32, s.h + 18)
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let color = match self.state {
            BtnState::Normal => self.base,
            BtnState::Hover => self.hover,
            BtnState::Press => self.press,
        };
        let r = style.corner_radius.max(6.0);
        canvas.fill_round_rect(
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
            r,
            &Paint::fill(color),
        );
        canvas.draw_text(
            &self.label,
            bounds,
            self.text_color,
            Align::Center,
            style.font_family.as_deref(),
            style.font_size,
        );
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    if self.state == BtnState::Normal {
                        self.state = BtnState::Hover;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.state != BtnState::Press {
                        self.state = BtnState::Normal;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    self.state = BtnState::Press;
                    ctx.capture();
                    ctx.request_focus();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    let was_press = self.state == BtnState::Press;
                    let inside = ctx.bounds().contains(p.pos);
                    self.state = if inside { BtnState::Hover } else { BtnState::Normal };
                    ctx.release_capture();
                    ctx.mark_dirty();
                    if was_press && inside {
                        if let Some(cb) = self.on_click.as_mut() {
                            cb(ctx);
                        }
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) => {
                if k.pressed && (k.key == Key::Enter || k.key == Key::Space) {
                    if let Some(cb) = self.on_click.as_mut() {
                        cb(ctx);
                    }
                    ctx.mark_dirty();
                    true
                } else {
                    false
                }
            }
        }
    }
    fn focusable(&self) -> bool {
        true
    }
    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }
}

/// 控件构建器。可表达容器或叶子。
pub struct Element {
    width: Dimension,
    height: Dimension,
    padding: Insets,
    margin: Insets,
    align: Option<Align>,
    weight: Option<f32>,
    layout: Layout,
    style: Style,
    widget: Box<dyn Widget>,
    children: Vec<Element>,
    visible: bool,
    vis_cond: Option<Box<dyn Fn() -> bool>>,
    clip_children: bool,
    click: Option<ClickFn>,
}

impl Element {
    fn base(layout: Layout) -> Self {
        Self {
            width: Dimension::Wrap,
            height: Dimension::Wrap,
            padding: Insets::default(),
            margin: Insets::default(),
            align: None,
            weight: None,
            layout,
            style: Style::default(),
            widget: Box::new(EmptyWidget),
            children: Vec::new(),
            visible: true,
            vis_cond: None,
            clip_children: false,
            click: None,
        }
    }

    /// 垂直线性容器。
    pub fn col() -> Self {
        Self::base(Layout::Linear { axis: Axis::Vertical, spacing: 0, cross: Align::Start })
    }
    /// 水平线性容器。
    pub fn row() -> Self {
        Self::base(Layout::Linear { axis: Axis::Horizontal, spacing: 0, cross: Align::Start })
    }
    /// 叠层容器（FrameLayout）。
    pub fn stack() -> Self {
        Self::base(Layout::Frame)
    }
    /// 叶子（无子布局）。配合 `.background()` + 固定尺寸即为色块。
    pub fn leaf() -> Self {
        Self::base(Layout::None)
    }

    /// 文本标签。
    pub fn label(text: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(Label::new(text.into()))
    }

    /// 交互按钮。配合 `.on_click(...)` 设置回调。
    pub fn button(label: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(Button::new(label.into()))
    }

    /// 点击/激活回调（按钮等交互控件）。
    pub fn on_click(mut self, f: impl FnMut(&mut EventCtx) + 'static) -> Self {
        self.click = Some(Box::new(f));
        self
    }

    /// 复选框（绑定 `Rc<Cell<bool>>`）。
    pub fn checkbox(label: impl Into<String>, state: Rc<Cell<bool>>) -> Self {
        Self::base(Layout::None).widget(CheckBox::new(label.into(), state))
    }
    /// 开关（绑定 `Rc<Cell<bool>>`）。
    pub fn switch(state: Rc<Cell<bool>>) -> Self {
        Self::base(Layout::None).widget(Switch::new(state))
    }
    /// 单选按钮（共享 `Rc<Cell<usize>>` 组状态 + 本项索引）。
    pub fn radio(label: impl Into<String>, group: Rc<Cell<usize>>, index: usize) -> Self {
        Self::base(Layout::None).widget(RadioButton::new(label.into(), group, index))
    }
    /// 滑块（绑定 `Rc<Cell<f32>>`，值域 0.0..=1.0）。
    pub fn slider(value: Rc<Cell<f32>>) -> Self {
        Self::base(Layout::None).widget(Slider::new(value))
    }
    /// 单行文本输入（绑定 `Rc<RefCell<String>>`）。
    pub fn text_input(text: Rc<RefCell<String>>, placeholder: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(TextInput::new(text, placeholder.into()))
    }

    /// 运行期可见条件：闭包返回 false 时该节点本帧不显示/不命中。
    ///
    /// 契约：闭包**必须是纯函数**（仅读状态、无副作用）。它在每帧的
    /// measure/arrange/paint/hit-test/焦点收集中被多次调用，且帧内值不应变化。
    pub fn visible_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.vis_cond = Some(Box::new(f));
        self
    }

    /// 垂直滚动容器：内容超出视口时可滚轮滚动并裁剪。
    pub fn scroll() -> Self {
        let mut e = Self::base(Layout::Scroll).widget(containers::ScrollWidget::default());
        e.clip_children = true;
        e
    }

    /// 水平分隔线。
    pub fn divider() -> Self {
        Self::base(Layout::None).width_match().height(1).background(Color::hex(0xE2E6EA))
    }

    /// 标签页：顶部标签条切换、下方内容区按选中项显隐。
    /// `selected` 绑定当前选中索引，`pages` 为 (标题, 页面) 列表。
    pub fn tabs(selected: Rc<Cell<usize>>, pages: Vec<(&str, Element)>) -> Self {
        let mut bar = Element::row().width_match().height(40).spacing(6).cross(Align::Stretch);
        let mut content = Element::stack().fill().weight(1.0);
        for (i, (title, page)) in pages.into_iter().enumerate() {
            let tab = containers::TabButton::new(title.to_string(), selected.clone(), i);
            bar = bar.child(Element::base(Layout::None).widget(tab));
            let sel2 = selected.clone();
            content = content.child(page.fill().visible_when(move || sel2.get() == i));
        }
        Element::col().fill().spacing(10).child(bar).child(content)
    }

    /// 模态对话框：全窗半透明遮罩 + 居中内容，遮罩吞掉指针事件实现模态。
    /// `show` 绑定显示标志。
    pub fn dialog(show: Rc<Cell<bool>>, content: Element) -> Self {
        Element::stack()
            .fill()
            .widget(containers::ModalScrim)
            .background(Color::rgba(0, 0, 0, 120))
            .visible_when(move || show.get())
            .child(content.align(Align::Center))
    }

    /// 设置自定义内容控件（叶子）。
    pub fn widget(mut self, w: impl Widget + 'static) -> Self {
        self.widget = Box::new(w);
        self
    }

    // ---- 尺寸 ----
    pub fn width(mut self, px: i32) -> Self {
        self.width = Dimension::Px(px);
        self
    }
    pub fn height(mut self, px: i32) -> Self {
        self.height = Dimension::Px(px);
        self
    }
    pub fn size(self, w: i32, h: i32) -> Self {
        self.width(w).height(h)
    }
    pub fn width_match(mut self) -> Self {
        self.width = Dimension::Match;
        self
    }
    pub fn height_match(mut self) -> Self {
        self.height = Dimension::Match;
        self
    }
    /// 宽高都撑满父容器。
    pub fn fill(self) -> Self {
        self.width_match().height_match()
    }
    /// 主轴权重（父为线性容器时按比例瓜分剩余空间）。
    pub fn weight(mut self, w: f32) -> Self {
        self.weight = Some(w);
        self
    }

    // ---- 间距 ----
    pub fn padding(mut self, p: i32) -> Self {
        self.padding = Insets::all(p);
        self
    }
    pub fn padding_xy(mut self, h: i32, v: i32) -> Self {
        self.padding = Insets::symmetric(h, v);
        self
    }
    pub fn margin(mut self, m: i32) -> Self {
        self.margin = Insets::all(m);
        self
    }
    pub fn margin_xy(mut self, h: i32, v: i32) -> Self {
        self.margin = Insets::symmetric(h, v);
        self
    }

    // ---- 对齐/布局参数 ----
    pub fn align(mut self, a: Align) -> Self {
        self.align = Some(a);
        self
    }
    /// 线性容器主轴子间距。
    pub fn spacing(mut self, s: i32) -> Self {
        if let Layout::Linear { spacing, .. } = &mut self.layout {
            *spacing = s;
        }
        self
    }
    /// 线性容器交叉轴默认对齐。
    pub fn cross(mut self, a: Align) -> Self {
        if let Layout::Linear { cross, .. } = &mut self.layout {
            *cross = a;
        }
        self
    }

    // ---- 样式 ----
    pub fn background(mut self, c: Color) -> Self {
        self.style.bg = Some(c);
        self
    }
    pub fn border(mut self, c: Color, w: i32) -> Self {
        self.style.border = Some((c, w));
        self
    }
    pub fn corner(mut self, r: f32) -> Self {
        self.style.corner_radius = r;
        self
    }
    pub fn fg(mut self, c: Color) -> Self {
        self.style.fg = c;
        self
    }
    pub fn font_size(mut self, s: f32) -> Self {
        self.style.font_size = s;
        self
    }
    /// 文字水平对齐。
    pub fn text_align(mut self, a: Align) -> Self {
        self.style.text_align = a;
        self
    }

    // ---- 子节点 ----
    pub fn child(mut self, c: Element) -> Self {
        self.children.push(c);
        self
    }
    pub fn children(mut self, cs: impl IntoIterator<Item = Element>) -> Self {
        self.children.extend(cs);
        self
    }
    pub fn visible(mut self, v: bool) -> Self {
        self.visible = v;
        self
    }

    /// 递归落入 arena，返回根 NodeId。
    pub fn build(mut self, tree: &mut Tree) -> NodeId {
        let my_axis = match self.layout {
            Layout::Linear { axis, .. } => Some(axis),
            _ => None,
        };
        let children = std::mem::take(&mut self.children);
        // 把 Builder 上的点击回调注入控件（仅交互控件接收）。
        let mut widget = self.widget;
        if let Some(f) = self.click {
            widget.take_click(f);
        }
        let node = Node {
            parent: None,
            children: Vec::new(),
            bounds: Default::default(),
            measured: Default::default(),
            width: self.width,
            height: self.height,
            padding: self.padding,
            margin: self.margin,
            align: self.align,
            layout: self.layout,
            widget,
            style: self.style,
            visible: self.visible,
            vis_cond: self.vis_cond,
            focused: false,
            clip_children: self.clip_children,
            scroll_y: 0,
            content_h: 0,
        };
        let id = tree.insert(node);
        for mut ce in children {
            // 父为线性容器时，把请求的 weight 落到主轴维度
            if let (Some(axis), Some(w)) = (my_axis, ce.weight) {
                match axis {
                    Axis::Horizontal => ce.width = Dimension::Weight(w),
                    Axis::Vertical => ce.height = Dimension::Weight(w),
                }
            }
            let cid = ce.build(tree);
            tree.add_child(id, cid);
        }
        id
    }
}
