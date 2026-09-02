//! 轻量富文本控件 `RichText`。
//!
//! 定位：词典条目这类「一段内容里混多种字号/字重/颜色/胶囊标签」的静态排版，
//! 用一个控件 + span 数据模型解决，而非拼接大量 Label。设计要点：
//!
//! - **数据模型**：`RichDoc` = 块（段落 / 分隔线 / 可折叠 Section）的树；段落由
//!   `RichSpan` 组成，样式为全 Option 覆盖（None 继承控件 `Style`）。命名样式表
//!   （[`RichDoc::style`]）让调用方只标语义（"headword"/"pos"…），视觉集中定义。
//! - **布局**：控件自做行内流式布局（与多行 TextInput 的"自绘视觉行"同一范式）：
//!   span 切碎片（Latin 按空格、CJK 逐字可断、`\n` 强制换行），贪心装行；
//!   同行混字号靠 [`TextEngine::line_metrics`] 基线对齐——行基线 = max(各碎片
//!   ascent)，碎片矩形 top = 基线 − 自身 ascent、高 = 自然行高，引擎"矩形内垂直
//!   居中"的绘制约定在矩形高恰为自然行高时退化为顶对齐，字形落在正确基线。
//! - **折叠**：Section 的展开态是 `Signal<bool>`（状态与文档分离，翻转不失效
//!   碎片测量缓存）；折叠 = 布局器不下钻子块——不产出碎片即不测量、不绘制、
//!   不命中。头部整行可点击（悬停手型光标）；含 Section 时控件可 Tab 聚焦，
//!   ↑↓ 在折叠头间移动、Enter/Space 翻转（聚焦头绘焦点框）。展开/收起带
//!   卷帘高度动画（`AnimTheme::normal` 时长）：子块按目标状态完整排版、对外
//!   只占补间高度，溢出部分 paint 按 `ClipRegion` 裁剪；动画期布局缓存恒
//!   miss（每帧重排，落定后再排一次得稳定布局）；全局动画关闭时瞬时落定。
//! - **主题**：颜色用 [`RichColor`] 语义角色（paint 时按当前 palette 解析，
//!   运行时换主题自动跟随）或固定色；控件自身 chrome（箭头/分隔线/chip 默认色/
//!   间距）走 `RichTheme` 覆盖层。chip 默认前景按 WCAG AA 对比度自适应派生。
//! - **span 点击**：[`Para::span_id`]/`styled_id` 标注 id，回调经
//!   [`super::Element::on_span_click`] 挂控件层（文档保持纯数据）；悬停手型 +
//!   前景提亮。词典交叉引用即此。
//! - **中文排版**：CJK 避头尾（闭合标点不落行首、开括不孤悬行尾）、悬挂缩进
//!   [`Para::hanging`]（编号义项续行对齐释义首字）、段距按段覆盖
//!   [`Para::spacing_before`]。
//! - **划选复制**：碎片级选区（CJK 逐字成片＝中文天然字符级；Latin 整词吸附；
//!   chip 整体）——拖拽高亮、原地单击清除、**双击选词**（CJK 取连续汉字串至
//!   标点/空白止，Latin 单词）、**三击选段落**（跨软换行，同浏览器）、Ctrl+A 全选；Ctrl+C 复制选区
//!   （无选区复制全文，Ctrl+Shift+C 强制全文）；右键菜单随选区态提供「复制/复制全部/全选」
//!   （`Element::copy_menu(false)` 关闭）。拼装经 Frag 的 line/block 源锚点：跨块补
//!   换行、块内软换行按 CJK/Latin 边界补空格。选区随**真**重排失效（碎片下标不稳定）——
//!   宽度落在布局的等价区间内不算重排（见 `RichLayout::wrap_hi`）：Wrap 宽下 measure
//!   拿 avail.w、paint 拿 content.w，两者天然不等，按相等判缓存会让选区活不过一帧。
//! - **行数截断**：[`Para::clamp`]（长释义预览）——未展开时最多排 N 行，截断处
//!   缀「… 展开」可点击标记；展开态是 `Signal<bool>`，同折叠的状态分离纪律。
//! - **动态文档**：[`super::Element::rich_signal`] 绑定 `Signal<RichDoc>`（词典切
//!   词条），信号版本变化时整篇换文档、失效布局缓存与选区。
//!
//! 已知限制（后续分期）：Latin 词内字符级选区（需每碎片字符偏移表）；动画期
//! 隐藏带内的碎片仍参与命中测试（约 200ms 的暂态，可接受）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, KeyEvent, MenuItem, MouseButton, PointerKind};
use crate::geometry::{Color, Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::style::Style;
use crate::text::{LineMetrics, TextEngine, TextStyle};
use crate::theme::{Palette, Theme};

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

/// 富文本颜色：主题语义角色（paint 时按当前 palette 解析，换主题自动跟随）
/// 或固定色。`Color` 可经 `From` 直接传入。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RichColor {
    /// 正文色（palette.text）。
    Text,
    /// 次要文字（palette.text_muted）。
    Muted,
    /// 强调色（palette.accent）。
    Accent,
    /// 危险色（palette.danger）。
    Danger,
    /// 固定颜色（不随主题变化）。
    Fixed(Color),
}

impl RichColor {
    fn resolve(self, p: &Palette) -> Color {
        match self {
            RichColor::Text => p.text,
            RichColor::Muted => p.text_muted,
            RichColor::Accent => p.accent,
            RichColor::Danger => p.danger,
            RichColor::Fixed(c) => c,
        }
    }
}

impl From<Color> for RichColor {
    fn from(c: Color) -> Self {
        RichColor::Fixed(c)
    }
}

/// span 样式：全 Option 覆盖，`None` 继承控件 `Style` / 主题默认。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpanStyle {
    size: Option<f32>,
    weight: Option<u16>,
    family: Option<String>,
    fg: Option<RichColor>,
    bg: Option<RichColor>,
    italic: bool,
    underline: bool,
    strike: bool,
    chip: bool,
}

impl SpanStyle {
    pub fn new() -> Self {
        Self::default()
    }
    /// 字号（逻辑 dp）。
    pub fn size(mut self, size: f32) -> Self {
        self.size = Some(size);
        self
    }
    /// 字重（400 常规 / 600 半粗 / 700 粗）。
    pub fn weight(mut self, weight: u16) -> Self {
        self.weight = Some(weight);
        self
    }
    /// 粗体（weight 700）。
    pub fn bold(self) -> Self {
        self.weight(700)
    }
    /// 字族（音标等特殊字体场景）。
    pub fn family(mut self, family: impl Into<String>) -> Self {
        self.family = Some(family.into());
        self
    }
    /// 前景色（语义角色或固定色）。
    pub fn fg(mut self, color: impl Into<RichColor>) -> Self {
        self.fg = Some(color.into());
        self
    }
    /// 背景色。非 chip 时为文字底色高亮；chip 时为胶囊底色。
    pub fn bg(mut self, color: impl Into<RichColor>) -> Self {
        self.bg = Some(color.into());
        self
    }
    /// 斜体。
    ///
    /// 词典正文里斜体承载语义而非装饰：例句、语体标注（*informal*）、拉丁学名都靠它。
    /// 剥掉 CSS 的 HTML 里，`<em>`/`<i>` 往往是**唯一**幸存的语义信号。
    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    /// 下划线。
    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    /// 删除线。
    pub fn strike(mut self) -> Self {
        self.strike = true;
        self
    }
    /// 胶囊（pill）：加内边距 + 全圆角底色，整体不随行拆分。
    /// 词性标签、领域标签等即此。未指定 fg/bg 时用 `RichTheme` 的 chip 默认色。
    pub fn chip(mut self) -> Self {
        self.chip = true;
        self
    }

    /// 以 `base` 为底、本样式的显式字段覆盖（命名样式 + 内联覆盖的合并规则）。
    fn over(&self, base: &SpanStyle) -> SpanStyle {
        SpanStyle {
            size: self.size.or(base.size),
            weight: self.weight.or(base.weight),
            family: self.family.clone().or_else(|| base.family.clone()),
            fg: self.fg.or(base.fg),
            bg: self.bg.or(base.bg),
            // 与 underline/strike 同为「或」而非「覆盖」：它们是开关不是取值，
            // 命名样式开了斜体、内联再开一次不该把它关掉。
            italic: self.italic || base.italic,
            underline: self.underline || base.underline,
            strike: self.strike || base.strike,
            chip: self.chip || base.chip,
        }
    }
}

/// 一段带样式的文字。经 [`Para`] 的 builder 方法构造。
#[derive(Clone, Debug)]
struct RichSpan {
    text: String,
    /// 命名样式（在 `RichDoc` 样式表中查找作为基底）。
    named: Option<String>,
    /// 内联样式（覆盖命名样式的对应字段）。
    style: SpanStyle,
    /// 交互标识：设置后该 span 可点击（词典交叉引用），点击经
    /// [`super::Element::on_span_click`] 回调携带此 id。文档保持纯数据——
    /// 回调挂控件层而非 span 上，`RichDoc` 因此仍可 Clone/比较/缓存。
    id: Option<Rc<str>>,
}

/// 段落：span 序列 + 段级排版参数。
#[derive(Clone, Default)]
pub struct Para {
    spans: Vec<RichSpan>,
    /// 段首行缩进（逻辑 px，相对当前块缩进基线）。
    indent: i32,
    /// 续行缩进（悬挂缩进；None = 与 `indent` 相同即整段缩进）。
    hanging: Option<i32>,
    /// 段前间距覆盖（None = 用 `RichTheme::para_spacing` 全局值）。
    spacing_before: Option<i32>,
    /// 行数截断：未展开时最多排这么多行，截断处缀「… 展开」可点击标记。
    max_lines: Option<usize>,
    /// clamp 展开态（true = 已展开不截断）。状态与文档分离，同 Section 折叠。
    clamp_expanded: Option<Signal<bool>>,
}

impl Para {
    pub fn new() -> Self {
        Self::default()
    }
    /// 默认样式文字。
    pub fn text(mut self, s: impl Into<String>) -> Self {
        self.spans.push(RichSpan {
            text: s.into(),
            named: None,
            style: SpanStyle::default(),
            id: None,
        });
        self
    }
    /// 命名样式文字（样式名在 [`RichDoc::style`] 注册）。
    pub fn styled(mut self, name: impl Into<String>, s: impl Into<String>) -> Self {
        self.spans.push(RichSpan {
            text: s.into(),
            named: Some(name.into()),
            style: SpanStyle::default(),
            id: None,
        });
        self
    }
    /// 内联样式文字。
    pub fn span(mut self, s: impl Into<String>, style: SpanStyle) -> Self {
        self.spans.push(RichSpan {
            text: s.into(),
            named: None,
            style,
            id: None,
        });
        self
    }
    /// 命名样式 + 内联覆盖（内联显式字段优先）。
    pub fn styled_span(
        mut self,
        name: impl Into<String>,
        s: impl Into<String>,
        style: SpanStyle,
    ) -> Self {
        self.spans.push(RichSpan {
            text: s.into(),
            named: Some(name.into()),
            style,
            id: None,
        });
        self
    }
    /// 可点击文字（内联样式）：`id` 经 [`super::Element::on_span_click`] 回调传出。
    /// 词典交叉引用（"参见 X""近义词 Y"）即此。悬停显示手型 + 前景提亮。
    pub fn span_id(
        mut self,
        id: impl Into<String>,
        s: impl Into<String>,
        style: SpanStyle,
    ) -> Self {
        self.spans.push(RichSpan {
            text: s.into(),
            named: None,
            style,
            id: Some(Rc::from(id.into())),
        });
        self
    }
    /// 可点击文字（命名样式）：同 [`Para::span_id`]，样式取自样式表。
    pub fn styled_id(
        mut self,
        name: impl Into<String>,
        id: impl Into<String>,
        s: impl Into<String>,
    ) -> Self {
        self.spans.push(RichSpan {
            text: s.into(),
            named: Some(name.into()),
            style: SpanStyle::default(),
            id: Some(Rc::from(id.into())),
        });
        self
    }
    /// 段缩进（逻辑 px）。
    pub fn indent(mut self, px: i32) -> Self {
        self.indent = px;
        self
    }
    /// 悬挂缩进：换行后的续行左缘（逻辑 px，相对当前块缩进基线）。
    /// 编号义项场景："1. 释义…" 设 hanging 为编号宽，续行对齐释义首字。
    pub fn hanging(mut self, px: i32) -> Self {
        self.hanging = Some(px);
        self
    }
    /// 段前间距覆盖（逻辑 px）：如词头段与释义段之间要比释义段间更宽/更窄。
    pub fn spacing_before(mut self, px: i32) -> Self {
        self.spacing_before = Some(px);
        self
    }
    /// 行数截断（长释义预览）：`expanded` 为 false 时最多排 `max_lines` 行，
    /// 截断处缀「… 展开」可点击标记（点击置 true，段落展开为全文）。
    /// 状态与文档分离——同 Section 折叠的 Signal 纪律。
    pub fn clamp(mut self, max_lines: usize, expanded: Signal<bool>) -> Self {
        self.max_lines = Some(max_lines.max(1));
        self.clamp_expanded = Some(expanded);
        self
    }
}

impl From<&str> for Para {
    fn from(s: &str) -> Self {
        Para::new().text(s)
    }
}
impl From<String> for Para {
    fn from(s: String) -> Self {
        Para::new().text(s)
    }
}

/// 块：段落 / 分隔线 / 可折叠 Section。
#[derive(Clone)]
enum RichBlock {
    Para(Para),
    Divider,
    Section(Section),
}

/// 可折叠区：头部（自动加折叠箭头）+ 子块（折叠时不参与布局）。
#[derive(Clone)]
struct Section {
    header: Para,
    children: Vec<RichBlock>,
    collapsed: Signal<bool>,
}

/// 富文本文档：块序列 + 命名样式表。经 builder 构造后交给 [`super::Element::rich`]。
#[derive(Clone, Default)]
pub struct RichDoc {
    blocks: Vec<RichBlock>,
    styles: HashMap<String, SpanStyle>,
}

impl RichDoc {
    pub fn new() -> Self {
        Self::default()
    }
    /// 注册命名样式（语义样式表）。span 经 [`Para::styled`] 引用；
    /// 未注册的名字按默认样式处理。
    pub fn style(mut self, name: impl Into<String>, style: SpanStyle) -> Self {
        self.styles.insert(name.into(), style);
        self
    }
    /// 追加一个段落（`&str` 可直接传入成为单 span 段落）。
    pub fn para(mut self, p: impl Into<Para>) -> Self {
        self.blocks.push(RichBlock::Para(p.into()));
        self
    }
    /// 追加一条分隔线（义项之间的细线，宽度撑满控件）。
    pub fn divider(mut self) -> Self {
        self.blocks.push(RichBlock::Divider);
        self
    }
    /// 全文纯文本（复制用）：段落/折叠头逐行拼接，chip 文字包含在内；
    /// 折叠区内容**包含**——复制语义取全文，与视觉折叠态无关。
    pub fn plain_text(&self) -> String {
        fn walk(blocks: &[RichBlock], out: &mut String) {
            for b in blocks {
                match b {
                    RichBlock::Para(p) => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        for s in &p.spans {
                            out.push_str(&s.text);
                        }
                    }
                    RichBlock::Divider => {}
                    RichBlock::Section(sec) => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        for s in &sec.header.spans {
                            out.push_str(&s.text);
                        }
                        walk(&sec.children, out);
                    }
                }
            }
        }
        let mut out = String::new();
        walk(&self.blocks, &mut out);
        out
    }

    /// 追加一个可折叠区。`collapsed` 为展开态信号（true = 收起），头部点击自动翻转；
    /// 子块经嵌套 builder 构造（其命名样式并入本文档，同名后定义覆盖）。
    pub fn section(
        mut self,
        header: impl Into<Para>,
        collapsed: Signal<bool>,
        children: impl FnOnce(RichDoc) -> RichDoc,
    ) -> Self {
        let inner = children(RichDoc::new());
        self.styles.extend(inner.styles);
        self.blocks.push(RichBlock::Section(Section {
            header: header.into(),
            children: inner.blocks,
            collapsed,
        }));
        self
    }
}

// ---------------------------------------------------------------------------
// 布局
// ---------------------------------------------------------------------------

/// 测量抽象：布局算法同时服务 measure（`TextEngine`）与 paint（`Canvas`）两条路径。
trait Measurer {
    fn size(&mut self, text: &str, ts: &TextStyle) -> Size;
    fn metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics;
    /// DPI 缩放因子：测量结果随之有物理取整差异，须入布局缓存键。
    fn scale(&self) -> f32;
}

/// 碎片内每个**字符边界**距文字起点的横向偏移。长度 = 字符数 + 1，首元素恒为 0、
/// 末元素恒为 `full_w`。
///
/// 逐前缀测量，与 `TextInput` 的 `prefix` 是同一套做法（见 `ui/inputs.rs` 里
/// `wrap_paragraph` 的入参）——同一件事用两种算法，会让同一段文字在两个控件里落点
/// 不同，而那种差异只有拿尺子量才看得出来。
///
/// 单字符碎片直接给 `[0, full_w]`：CJK 逐字成片，正文里绝大多数碎片都是这一种，
/// 为它们各跑一次 measure 是纯浪费。
///
/// 末元素取传入的 `full_w` 而不是再测一次整串：布局已经按那个宽度摆好了碎片，这里
/// 若测出个差一像素的值，选区右边界就会与碎片右边界对不齐。
fn char_offsets(m: &mut dyn Measurer, text: &str, ts: &TextStyle, full_w: i32) -> Vec<i32> {
    let n = text.chars().count();
    if n == 0 {
        return vec![0];
    }
    if n == 1 {
        return vec![0, full_w];
    }
    let mut out = Vec::with_capacity(n + 1);
    out.push(0);
    for (b, _) in text.char_indices().skip(1) {
        out.push(m.size(&text[..b], ts).w);
    }
    out.push(full_w);
    out
}

/// 绝对坐标 → 相对某个控件 content 原点的局部坐标。
fn local_of(pos: Point, content: Rect) -> Point {
    Point::new(pos.x - content.x, pos.y - content.y)
}

/// 局部坐标最近的碎片。**吸附**：点落在碎片之外也归到最近的那个，于是拖到行尾空白
/// 处、段落之间的空隙里，选区照样跟着走。
///
/// 拆成自由函数而不是留作方法：跨控件划选时，捕获指针的那个控件要在**别人**的布局上
/// 做同一件事，而那时它手上只有一个 `&RichLayout`。
fn frag_near_in(lay: &RichLayout, local: Point) -> Option<usize> {
    let mut best: Option<(i32, i32, usize)> = None;
    for (i, f) in lay.frags.iter().enumerate() {
        let dy = if local.y < f.rect.y {
            f.rect.y - local.y
        } else if local.y >= f.rect.y + f.rect.h {
            local.y - (f.rect.y + f.rect.h) + 1
        } else {
            0
        };
        let dx = if local.x < f.rect.x {
            f.rect.x - local.x
        } else if local.x >= f.rect.x + f.rect.w {
            local.x - (f.rect.x + f.rect.w) + 1
        } else {
            0
        };
        if best.map(|(by, bx, _)| (dy, dx) < (by, bx)).unwrap_or(true) {
            best = Some((dy, dx, i));
        }
    }
    best.map(|(_, _, i)| i)
}

/// 局部坐标最近的选区端点：先由 [`frag_near_in`] 选碎片，再在碎片内落到最近的字符
/// 边界上。
fn caret_near_in(lay: &RichLayout, local: Point) -> Option<Caret> {
    let i = frag_near_in(lay, local)?;
    let f = lay.frags.get(i)?;
    let x = local.x - f.text_rect.x;
    let last = f.char_x.len().saturating_sub(1);
    // chip 不可分：词性胶囊那样的东西是一个整体的视觉块，从中间切开既难看又没有
    // 意义。落到就近的那一端，于是它要么整个进选区、要么整个不进。
    if f.style.chip {
        let ch = if x * 2 >= f.text_rect.w { last } else { 0 };
        return Some(Caret { frag: i, ch });
    }
    // 最近的那个边界——两个边界的中点为界，与文本控件放光标的手感一致。
    let mut best = 0usize;
    let mut bd = i32::MAX;
    for (k, cx) in f.char_x.iter().enumerate() {
        let d = (x - cx).abs();
        if d < bd {
            bd = d;
            best = k;
        }
    }
    Some(Caret { frag: i, ch: best })
}

/// 一段选区的纯文本：按阅读序拼接，跨块补换行，块内软换行按 CJK/Latin 边界决定要不要
/// 补空格（折行时被丢弃的词间空格在此还原）。箭头与「… 展开」标记不入文。
///
/// 自由函数而非方法：选择域要在**别的成员**的布局上做同一件事，那时手上只有一个
/// `&RichLayout`。
fn slice_text(lay: &RichLayout, a: Caret, b: Caret) -> Option<String> {
    let (a, b) = (a.min(b), a.max(b));
    let mut out = String::new();
    let mut prev: Option<&Frag> = None;
    for i in a.frag..=b.frag.min(lay.frags.len().saturating_sub(1)) {
        let f = lay.frags.get(i)?;
        if f.chevron || f.expand.is_some() {
            continue;
        }
        // 首尾两个碎片只取选中的那一段，中间的整片都要。
        let from = if i == a.frag { a.ch } else { 0 };
        let to = if i == b.frag {
            b.ch
        } else {
            f.char_x.len().saturating_sub(1)
        };
        // 空片不入文，**也不更新 `prev`**：它没有贡献任何字符，让它去参与「要不要补
        // 空格/换行」的判断只会凭空多出一个分隔。
        if to <= from {
            continue;
        }
        if let Some(p) = prev {
            if f.block != p.block {
                out.push('\n');
            } else if f.line != p.line {
                let cjk_join = p.text.chars().last().map(is_cjk).unwrap_or(false)
                    && f.text.chars().next().map(is_cjk).unwrap_or(false);
                if !cjk_join {
                    out.push(' ');
                }
            }
        }
        out.extend(f.text.chars().skip(from).take(to - from));
        prev = Some(f);
    }
    (!out.is_empty()).then_some(out)
}

struct EngineMeasurer<'a>(&'a mut dyn TextEngine);
impl Measurer for EngineMeasurer<'_> {
    fn size(&mut self, text: &str, ts: &TextStyle) -> Size {
        self.0.measure(text, ts, None)
    }
    fn metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
        self.0.line_metrics(text, ts)
    }
    fn scale(&self) -> f32 {
        self.0.scale()
    }
}

struct CanvasMeasurer<'a>(&'a mut dyn Canvas);
impl Measurer for CanvasMeasurer<'_> {
    fn size(&mut self, text: &str, ts: &TextStyle) -> Size {
        self.0.measure_text(text, ts)
    }
    fn metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
        self.0.text_line_metrics(text, ts)
    }
    fn scale(&self) -> f32 {
        self.0.dpi_scale()
    }
}

/// 碎片的已解析样式（family 已合并控件 Style，颜色保留语义角色供 paint 时解析）。
#[derive(Clone, Debug)]
struct FragStyle {
    size: f32,
    weight: u16,
    family: Option<String>,
    line_height: Option<f32>,
    fg: Option<RichColor>,
    bg: Option<RichColor>,
    italic: bool,
    underline: bool,
    strike: bool,
    chip: bool,
}

impl FragStyle {
    fn ts(&self) -> TextStyle<'_> {
        TextStyle {
            family: self.family.as_deref(),
            size: self.size,
            weight: self.weight,
            italic: self.italic,
            line_height: self.line_height,
        }
    }
}

/// 选区的一个端点：第几个碎片的第几个**字符边界**。
///
/// `ch` 是字符边界下标而非字节偏移——它直接拿去 [`Frag::char_x`] 取横坐标，也直接
/// 拿去 `chars().skip/take` 切文本，两处用的是同一把尺子。
///
/// 派生的 `Ord` 恰好就是阅读序（先比碎片、再比字符），选区排序直接用它。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
struct Caret {
    frag: usize,
    ch: usize,
}

/// 已排版碎片。坐标相对控件 content 左上角（逻辑 px）。
struct Frag {
    text: String,
    /// 碎片全框（chip 含内边距）。
    rect: Rect,
    /// 文字矩形：高恰为该文字自然行高——引擎"矩形内垂直居中"即顶对齐，落在基线上。
    text_rect: Rect,
    /// 所在视觉行的行盒纵向范围（行顶 y、行盒高 = 全行 max ascent + max descent）。
    /// 选区高亮按行盒铺满而非按碎片 `rect`：混排字号/chip 时同行选区顶底齐平、
    /// 下伸部（`p`/`{`）完整包住，且相邻行首尾相接——多行选中无横向白缝。
    line_top: i32,
    line_h: i32,
    /// 基线距 text_rect 顶（画下划线/删除线用）。
    ascent: f32,
    style: FragStyle,
    /// Section 折叠箭头（fg 走 RichTheme.chevron）。
    chevron: bool,
    /// 字符边界距 `text_rect` 左缘的横向偏移，长度 = 字符数 + 1。选区要能落在碎片
    /// **内部**：一个 Latin 单词就是一个碎片，没有这份数据，「standby」这样的词头
    /// 就只能整块选或者一点也选不中。见 [`char_offsets`]。
    char_x: Vec<i32>,
    /// 可点击 span 的标识（词典交叉引用）。同一 span 换行拆出的多个碎片共享同一 Rc。
    id: Option<Rc<str>>,
    /// clamp 截断标记「… 展开」：点击置该信号为 true（段落展开）。
    expand: Option<Signal<bool>>,
    /// 源锚点：视觉行号 / 块号（文档序）。划选复制阶段的基础设施预埋——
    /// 拼装选中文本时按行号插换行、按块号插段落分隔，不必重新铺映射管道。
    #[allow(dead_code)]
    line: u32,
    #[allow(dead_code)]
    block: u32,
}

/// Section 高度补间状态。以折叠 Signal 为身份键——跨重排、跨 `rich_signal` 换文档
/// 前后都稳定（应用复用同一信号时动画自然衔接）。`Transition` 是 Copy，
/// 布局器按下标先取副本、排完子块再写回，避开与 `&mut self` 的借用冲突。
#[derive(Clone, Copy)]
struct SectAnim {
    sig: Signal<bool>,
    /// 展示高度补间：目标 = 展开时子块全高、收起时 0。`from` 为 NAN 表示尚未
    /// 初始化（首次遇到以目标值静止落定，首帧不动画）。
    h: Transition<f32>,
}

/// 动画期的子块裁剪区：`[lo, hi)` 碎片区间、区起 y0、当前露出高、全高。
/// paint 据此对区间内碎片 clip（卷帘渐次露出），分隔线落在隐藏带的跳过。
#[derive(Clone, Copy)]
struct ClipRegion {
    lo: usize,
    hi: usize,
    y0: i32,
    reveal: i32,
    full: i32,
}

/// 布局缓存键。任何影响几何的输入都在此；颜色不在（paint 时解析，换主题不重排）。
#[derive(Clone, PartialEq, Debug)]
struct LayoutKey {
    wrap_w: Option<i32>,
    family: Option<String>,
    size_bits: u32,
    weight: u16,
    line_height_bits: Option<u32>,
    /// DPI 缩放（测量结果随之有物理取整差异；跨 DPI 显示器拖动时借此失效重排）。
    scale_bits: u32,
    /// 各 Section 折叠态 + 各段 clamp 展开态快照（文档序，与布局遍历一致）。
    collapsed: Vec<bool>,
    /// 主题间距参数（para_spacing, section_indent）。
    spacing: (i32, i32),
}

/// 布局产物。
struct RichLayout {
    frags: Vec<Frag>,
    /// 动画期的子块裁剪区（空 = 无动画进行中）。
    clips: Vec<ClipRegion>,
    /// 本布局产于动画进行期：缓存命中判定视为恒 miss，直到动画落定后
    /// 再重排一次得到稳定布局（否则会冻结在最后一个动画帧上）。
    animated: bool,
    /// 折叠头命中区（相对 content；宽度撑满可用宽）+ 对应折叠信号。
    headers: Vec<(Rect, Signal<bool>)>,
    /// 分隔线 (x缩进, y)；绘制时延展到 content 右缘。
    dividers: Vec<(i32, i32)>,
    /// 自然尺寸（最宽行 × 总高）。
    size: Size,
    /// 本布局有效的约束宽上界（无约束排出时为 `i32::MAX`）。任何落在
    /// `[size.w, wrap_hi]` 内的约束宽都产出**同一份**布局：贪心逐 token 装行下，
    /// 每行宽 ≤ `size.w` ≤ 新约束宽，而每处换行都因"再加一个 token 会超 `wrap_hi`
    /// （≥ 新约束宽）"发生——换行点不变，碎片与坐标全等。
    ///
    /// 这条区间是选区能活过一帧的前提：`measure` 拿父给的 `avail.w`、`paint` 拿
    /// 分配到的 `content.w`，Wrap 宽控件下两者天然不等（如 500 vs 自然宽 93）。
    /// 若按"宽度必须相等"判缓存，则 measure/paint 每帧交替重排、`sel` 随之清空，
    /// 而宿主对 Down/Up/按键都置 `needs_relayout`——全选与拖选的高亮永远等不到下一帧。
    wrap_hi: i32,
    key: LayoutKey,
}

/// 行内待排项（碎片测量结果 + 盒参数）。
struct Item {
    text: String,
    style: FragStyle,
    chevron: bool,
    /// 空白碎片：行首丢弃、不触发换行、行尾裁剪；行中照常产出 Frag
    /// （划选复制靠它保住词间空格，选区高亮靠它填词隙）。
    space: bool,
    text_w: i32,
    text_h: i32,
    ascent: f32,
    /// chip 内边距 (横, 纵)；非 chip 为 0。
    pad: (i32, i32),
    /// 可点击 span 标识（透传到 Frag）。
    id: Option<Rc<str>>,
    /// clamp 展开标记（透传到 Frag）。
    expand: Option<Signal<bool>>,
    /// 字符边界横向偏移（透传到 Frag）。见 [`char_offsets`]。
    char_x: Vec<i32>,
}

impl Item {
    fn box_w(&self) -> i32 {
        self.text_w + 2 * self.pad.0
    }
    fn box_h(&self) -> i32 {
        self.text_h + 2 * self.pad.1
    }
    fn box_ascent(&self) -> f32 {
        self.ascent + self.pad.1 as f32
    }
    fn box_descent(&self) -> f32 {
        (self.text_h as f32 - self.ascent) + self.pad.1 as f32
    }
}

/// 闭合类标点：不得落在行首（避头）。仅单字符 token 有意义——ASCII 标点通常
/// 随 Latin 词成一个 token，列入无害。
fn is_close_punct(s: &str) -> bool {
    let mut ch = s.chars();
    match (ch.next(), ch.next()) {
        (Some(c), None) => matches!(
            c,
            '。' | '；'
                | '，'
                | '、'
                | '！'
                | '？'
                | '：'
                | '）'
                | '」'
                | '』'
                | '】'
                | '〉'
                | '》'
                | '％'
                | '%'
                | '…'
                | ','
                | '.'
                | ';'
                | ':'
                | '!'
                | '?'
                | ')'
                | ']'
                | '}'
        ),
        _ => false,
    }
}

/// 开括类标点：不得孤悬行尾（避尾）。
fn is_open_punct(s: &str) -> bool {
    let mut ch = s.chars();
    match (ch.next(), ch.next()) {
        (Some(c), None) => matches!(c, '（' | '「' | '『' | '【' | '〈' | '《' | '(' | '[' | '{'),
        _ => false,
    }
}

/// CJK 及东亚全角字符：行内任意处可断行。
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{2E80}'..='\u{303F}'   // 部首扩展 + CJK 符号标点
        | '\u{3040}'..='\u{30FF}' // 平/片假名
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}' // 谚文音节
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}' // 全角形式
        | '\u{20000}'..='\u{2FA1F}')
}

/// 碎片种类（切分结果）。
enum TokKind {
    Word,
    Space,
    Newline,
}

/// 把 span 文本切成不可再分碎片：Latin 词 / 空白串 / 单个 CJK 字 / 强制换行。
fn tokenize(s: &str) -> Vec<(TokKind, &str)> {
    fn flush<'a>(out: &mut Vec<(TokKind, &'a str)>, s: &'a str, from: usize, to: usize, sp: bool) {
        if to > from {
            out.push((
                if sp { TokKind::Space } else { TokKind::Word },
                &s[from..to],
            ));
        }
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut word = false; // 当前是否在累积 Latin 词
    let mut space = false; // 当前是否在累积空白串
    for (i, c) in s.char_indices() {
        if c == '\n' {
            flush(&mut out, s, start, i, space);
            out.push((TokKind::Newline, ""));
            start = i + c.len_utf8();
            word = false;
            space = false;
        } else if c.is_whitespace() {
            if !space {
                flush(&mut out, s, start, i, false);
                start = i;
            }
            word = false;
            space = true;
        } else if is_cjk(c) {
            flush(&mut out, s, start, i, space);
            let end = i + c.len_utf8();
            out.push((TokKind::Word, &s[i..end]));
            start = end;
            word = false;
            space = false;
        } else {
            if !word {
                flush(&mut out, s, start, i, space);
                start = i;
            }
            word = true;
            space = false;
        }
    }
    if start < s.len() {
        out.push((
            if space { TokKind::Space } else { TokKind::Word },
            &s[start..],
        ));
    }
    out
}

/// 布局器：一次 `layout_doc` 的可变状态。
struct Walker<'a> {
    m: &'a mut dyn Measurer,
    th: &'a Theme,
    wrap_w: Option<i32>,
    frags: Vec<Frag>,
    headers: Vec<(Rect, Signal<bool>)>,
    dividers: Vec<(i32, i32)>,
    y: i32,
    natural_w: i32,
    /// 是否已排过块（控制段前间距）。
    any_block: bool,
    /// 视觉行 / 块计数（Frag 源锚点）。
    line_no: u32,
    block_no: u32,
    /// 各 Section 高度补间（控件持有，跨帧存活）。
    anims: &'a mut Vec<SectAnim>,
    /// 动画期裁剪区。
    clips: Vec<ClipRegion>,
}

impl Walker<'_> {
    /// 解析 span → FragStyle（命名样式为基底、内联覆盖，再回退控件 Style）。
    fn resolve(
        &self,
        span: &RichSpan,
        styles: &HashMap<String, SpanStyle>,
        base: &Style,
    ) -> FragStyle {
        let named = span
            .named
            .as_ref()
            .and_then(|n| styles.get(n))
            .cloned()
            .unwrap_or_default();
        let s = span.style.over(&named);
        FragStyle {
            size: s.size.unwrap_or(base.font_size),
            weight: s.weight.unwrap_or(base.font_weight),
            family: s.family.or_else(|| base.font_family.clone()),
            line_height: base.line_height,
            fg: s.fg,
            bg: s.bg,
            italic: s.italic,
            underline: s.underline,
            strike: s.strike,
            chip: s.chip,
        }
    }

    /// 测量一个碎片为待排项。
    fn item(
        &mut self,
        text: &str,
        style: &FragStyle,
        chevron: bool,
        space: bool,
        id: Option<Rc<str>>,
    ) -> Item {
        let ts = style.ts();
        let sz = self.m.size(text, &ts);
        let lm = self.m.metrics(text, &ts);
        let char_x = char_offsets(&mut *self.m, text, &ts, sz.w);
        let pad = if style.chip {
            (
                (style.size * 0.45).round() as i32,
                (style.size * 0.15).round().max(1.0) as i32,
            )
        } else {
            (0, 0)
        };
        Item {
            text: text.to_string(),
            style: style.clone(),
            chevron,
            space,
            text_w: sz.w,
            text_h: sz.h,
            ascent: lm.ascent,
            pad,
            id,
            expand: None,
            char_x,
        }
    }

    /// 落定一行：基线对齐，产出 Frag，推进 y。
    fn flush_line(&mut self, line: &mut Vec<Item>, x0: i32) {
        // 行尾空白不参与行宽（但仍产出前进过的 x——已在装行时计入，无需回退视觉）。
        while line.last().map(|it| it.space).unwrap_or(false) {
            line.pop();
        }
        if line.is_empty() {
            return;
        }
        let asc = line.iter().map(Item::box_ascent).fold(0.0f32, f32::max);
        let desc = line.iter().map(Item::box_descent).fold(0.0f32, f32::max);
        // 行盒：所有碎片共用，供选区高亮铺满整行（碎片自身 rect 只有各自文字高）。
        // 与下方 `self.y` 的推进量同源，故相邻行的选区首尾相接、不留白缝。
        let line_top = self.y;
        let line_h = (asc + desc).ceil() as i32;
        let mut x = x0;
        for it in line.drain(..) {
            let top = self.y + (asc - it.box_ascent()).round() as i32;
            let rect = Rect::new(x, top, it.box_w(), it.box_h());
            x += it.box_w();
            // 空白碎片也产出：绘制阶段跳过其文字，但划选复制需要它保住词间空格，
            // 选区高亮也靠它填满词隙。（行尾空白已在上方 pop，不会残留。）
            let text_rect = Rect::new(rect.x + it.pad.0, rect.y + it.pad.1, it.text_w, it.text_h);
            self.frags.push(Frag {
                text: it.text,
                rect,
                text_rect,
                char_x: it.char_x,
                line_top,
                line_h,
                ascent: it.ascent,
                style: it.style,
                chevron: it.chevron,
                id: it.id,
                expand: it.expand,
                line: self.line_no,
                block: self.block_no,
            });
        }
        self.natural_w = self.natural_w.max(x);
        self.y += (asc + desc).ceil() as i32;
        self.line_no += 1;
    }

    /// 排一个段落（含 Section 头部复用：`extra` 为前置附加项，如折叠箭头）。
    fn para(
        &mut self,
        p: &Para,
        styles: &HashMap<String, SpanStyle>,
        base: &Style,
        indent: i32,
        extra: Option<Item>,
    ) {
        // 悬挂缩进：首行用 indent，续行用 hanging（未设时同 indent）。
        // 词典编号义项即此："1. 释义…" 换行后续行对齐释义首字而非编号。
        let x_first = indent + p.indent;
        let x_rest = indent + p.hanging.unwrap_or(p.indent);
        let mut x0 = x_first;
        let mut line: Vec<Item> = Vec::new();
        let mut x = x0;
        if let Some(it) = extra {
            x += it.box_w();
            line.push(it);
        }
        // clamp：未展开时只排 max_lines 行，最后一行将溢出处截断缀「… 展开」。
        let clamp = match (p.max_lines, p.clamp_expanded) {
            (Some(n), Some(sig)) if !sig.get() => Some((n, sig)),
            _ => None,
        };
        let start_line = self.line_no;
        let spans = p.spans.clone();
        'spans: for span in &spans {
            let fs = self.resolve(span, styles, base);
            if fs.chip {
                // 胶囊整体不拆分。
                let it = self.item(&span.text, &fs, false, false, span.id.clone());
                if let Some((n, sig)) = clamp {
                    if self.clamp_would_exceed(&line, x, n, start_line, &it) {
                        self.truncate_line(&mut line, &mut x, base, sig);
                        break 'spans;
                    }
                }
                self.place(&mut line, &mut x, &mut x0, x_rest, it);
                continue;
            }
            for (kind, tok) in tokenize(&span.text) {
                match kind {
                    TokKind::Newline => {
                        if let Some((n, sig)) = clamp {
                            // 硬换行也吃行数：已在最后一行则就地截断。
                            if (self.line_no - start_line) as usize + 1 >= n {
                                self.truncate_line(&mut line, &mut x, base, sig);
                                break 'spans;
                            }
                        }
                        self.flush_line(&mut line, x0);
                        // 连续空行：flush 空行无高度，显式补一行高。
                        if line.is_empty() && x == x0 {
                            let lh = fs.ts().line_height_px().unwrap_or(fs.size).ceil() as i32;
                            self.y += lh;
                        }
                        x0 = x_rest;
                        x = x0;
                    }
                    TokKind::Space => {
                        // 行首空白丢弃；空白不触发换行。
                        if line.is_empty() {
                            continue;
                        }
                        let it = self.item(tok, &fs, false, true, span.id.clone());
                        x += it.box_w();
                        line.push(it);
                    }
                    TokKind::Word => {
                        let it = self.item(tok, &fs, false, false, span.id.clone());
                        if let Some((n, sig)) = clamp {
                            if self.clamp_would_exceed(&line, x, n, start_line, &it) {
                                self.truncate_line(&mut line, &mut x, base, sig);
                                break 'spans;
                            }
                        }
                        self.place(&mut line, &mut x, &mut x0, x_rest, it);
                    }
                }
            }
        }
        self.flush_line(&mut line, x0);
    }

    /// clamp 判定：正在排最后一个允许行、且放入 `it` 会触发换行。
    /// 闭合标点走避头附着不换行，放行。
    fn clamp_would_exceed(
        &self,
        line: &[Item],
        x: i32,
        max_lines: usize,
        start_line: u32,
        it: &Item,
    ) -> bool {
        let Some(w) = self.wrap_w else { return false };
        if ((self.line_no - start_line) as usize + 1) < max_lines {
            return false;
        }
        !line.is_empty() && x + it.box_w() > w && !is_close_punct(&it.text)
    }

    /// 截断收尾：行尾腾出宽度，缀可点击的「… 展开」标记。
    fn truncate_line(
        &mut self,
        line: &mut Vec<Item>,
        x: &mut i32,
        base: &Style,
        sig: Signal<bool>,
    ) {
        let fs = FragStyle {
            size: base.font_size,
            weight: base.font_weight,
            family: base.font_family.clone(),
            line_height: base.line_height,
            fg: Some(RichColor::Accent),
            bg: None,
            italic: false,
            underline: false,
            strike: false,
            chip: false,
        };
        let mut ell = self.item("… 展开", &fs, false, false, None);
        ell.expand = Some(sig);
        if let Some(w) = self.wrap_w {
            // 从行尾弹出内容给标记腾位（至少留一项，防清空整行）。
            while line.len() > 1 && *x + ell.box_w() > w {
                if let Some(popped) = line.pop() {
                    *x -= popped.box_w();
                }
            }
            // 腾位后行尾若是空白，一并去掉（标记不该跟在空格后）。
            while line.len() > 1 && line.last().map(|i| i.space).unwrap_or(false) {
                if let Some(popped) = line.pop() {
                    *x -= popped.box_w();
                }
            }
        }
        *x += ell.box_w();
        line.push(ell);
    }

    /// 装行：放不下且行非空 → 先落定当前行再放（贪心断行）。
    ///
    /// CJK 避头尾：闭合标点（。；，、）等）不落行首——触发换行时强制附着当前
    /// 行尾（溢出一字宽可接受，好过标点孤悬行首）；行尾的开括类标点（（「『等）
    /// 随后续内容一起下行（不孤悬行尾）。
    fn place(&mut self, line: &mut Vec<Item>, x: &mut i32, x0: &mut i32, x_rest: i32, it: Item) {
        if let Some(w) = self.wrap_w {
            let overflow = !line.is_empty() && *x + it.box_w() > w;
            if overflow {
                if is_close_punct(&it.text) {
                    // 避头：附着当前行尾。
                    *x += it.box_w();
                    line.push(it);
                    return;
                }
                // 避尾：行尾连续开括类标点随新行携带（至少给原行留一项，防空转）。
                let mut carried: Vec<Item> = Vec::new();
                while line.len() > 1 {
                    let Some(last) = line.last() else { break };
                    if !last.space && is_open_punct(&last.text) {
                        carried.push(line.pop().unwrap());
                    } else {
                        break;
                    }
                }
                self.flush_line(line, *x0);
                *x0 = x_rest;
                *x = *x0;
                for c in carried.into_iter().rev() {
                    *x += c.box_w();
                    line.push(c);
                }
            }
        }
        *x += it.box_w();
        line.push(it);
    }

    /// 排块序列（Section 递归）。
    fn blocks(
        &mut self,
        blocks: &[RichBlock],
        styles: &HashMap<String, SpanStyle>,
        base: &Style,
        indent: i32,
    ) {
        let spacing = self.th.rich.para_spacing();
        for b in blocks {
            self.block_no += 1;
            match b {
                RichBlock::Para(p) => {
                    if self.any_block {
                        // 段前间距：段级覆盖优先，回退主题全局值。
                        self.y += p.spacing_before.unwrap_or(spacing);
                    }
                    self.any_block = true;
                    self.para(p, styles, base, indent, None);
                }
                RichBlock::Divider => {
                    // 分隔线自带上下留白，不叠加段前间距。
                    self.y += spacing;
                    self.dividers.push((indent, self.y));
                    self.y += 1 + spacing;
                    self.any_block = true;
                }
                RichBlock::Section(sec) => {
                    if self.any_block {
                        self.y += spacing;
                    }
                    self.any_block = true;
                    let collapsed = sec.collapsed.get();
                    // 头部 = 折叠箭头 + 头部段落；整行区域记为命中区。
                    let y0 = self.y;
                    let glyph = if collapsed { "▸ " } else { "▾ " };
                    let mut fs = self.resolve(
                        &RichSpan {
                            text: String::new(),
                            named: None,
                            style: SpanStyle::default(),
                            id: None,
                        },
                        styles,
                        base,
                    );
                    fs.fg = None;
                    let chev = self.item(glyph, &fs, true, false, None);
                    self.para(&sec.header, styles, base, indent, Some(chev));
                    // 命中区宽度：宽度受限时撑满可用宽（好点）；Wrap 宽在收尾统一补齐。
                    let w = self.wrap_w.map(|w| w - indent).unwrap_or(0);
                    self.headers
                        .push((Rect::new(indent, y0, w, self.y - y0), sec.collapsed));
                    // —— 展开高度动画（卷帘）——
                    // 子块始终按目标状态完整排版；对外只占用补间后的高度，
                    // 溢出部分由 paint 按 ClipRegion 裁剪渐次露出/收拢。
                    let ai = match self.anims.iter().position(|a| a.sig == sec.collapsed) {
                        Some(i) => i,
                        None => {
                            self.anims.push(SectAnim {
                                sig: sec.collapsed,
                                h: Transition::new(f32::NAN),
                            });
                            self.anims.len() - 1
                        }
                    };
                    let mut anim = self.anims[ai].h;
                    // 收拢动画进行中也要排出子块（正在被卷起的内容仍可见）。
                    let closing = collapsed && anim.value().is_finite() && anim.value() > 0.5;
                    let cy0 = self.y;
                    let frag_lo = self.frags.len();
                    if !collapsed || closing {
                        self.blocks(
                            &sec.children,
                            styles,
                            base,
                            indent + self.th.rich.section_indent(),
                        );
                    }
                    let full = (self.y - cy0) as f32;
                    let target = if collapsed { 0.0 } else { full };
                    if !anim.value().is_finite() {
                        // 首次遇到：静止落定，首帧不动画。
                        anim = Transition::new(target);
                    } else if (anim.target() <= 0.5) != (target <= 0.5) {
                        // 折叠态翻转：起卷帘动画。
                        anim.retarget(target, self.th.anim.normal(), Easing::EaseInOut);
                    } else if (anim.target() - target).abs() > 0.5 && !anim.is_active() {
                        // 内容/宽度变化导致全高漂移（非折叠切换）：静止跟随，不动画。
                        anim = Transition::new(target);
                    }
                    // 布局期只读取值（无副作用）：下一帧的请求由 paint 经
                    // anim::request_relayout 走宿主 needs_relayout 正规门发出，
                    // 保证动画每帧都执行结构签名升级与 hover 重同步。
                    let reveal = anim.value().clamp(0.0, full);
                    let reveal_px = reveal.round() as i32;
                    if (reveal + 0.5) < full {
                        self.clips.push(ClipRegion {
                            lo: frag_lo,
                            hi: self.frags.len(),
                            y0: cy0,
                            reveal: reveal_px,
                            full: full as i32,
                        });
                        self.y = cy0 + reveal_px;
                    }
                    self.anims[ai].h = anim;
                }
            }
        }
    }
}

/// 全文布局。`wrap_w` 为可用宽度（None = 不限宽，逐段单行）。
fn layout_doc(
    doc: &RichDoc,
    key: LayoutKey,
    base: &Style,
    m: &mut dyn Measurer,
    th: &Theme,
    anims: &mut Vec<SectAnim>,
) -> RichLayout {
    let mut w = Walker {
        m,
        th,
        wrap_w: key.wrap_w,
        frags: Vec::new(),
        headers: Vec::new(),
        dividers: Vec::new(),
        y: 0,
        natural_w: 0,
        any_block: false,
        line_no: 0,
        block_no: 0,
        anims,
        clips: Vec::new(),
    };
    w.blocks(&doc.blocks, &doc.styles, base, 0);
    let natural_w = w.natural_w;
    let mut headers = w.headers;
    // Wrap 宽（无约束）时头部命中区宽度补齐到自然宽。
    for (r, _) in headers.iter_mut() {
        if r.w <= 0 {
            r.w = (natural_w - r.x).max(0);
        }
    }
    let clips = w.clips;
    let animated = !clips.is_empty() || w.anims.iter().any(|a| a.h.is_active());
    RichLayout {
        frags: w.frags,
        clips,
        animated,
        headers,
        dividers: w.dividers,
        size: Size::new(natural_w, w.y),
        wrap_hi: key.wrap_w.unwrap_or(i32::MAX),
        key,
    }
}

/// 收集影响布局的运行态快照（文档序，与布局遍历一致）：Section 折叠态 +
/// 段落 clamp 展开态。折叠区内的状态仅在展开时收集（与布局跳过一致）。
fn collect_collapsed(blocks: &[RichBlock], out: &mut Vec<bool>) {
    for b in blocks {
        match b {
            RichBlock::Para(p) => {
                if let Some(sig) = p.clamp_expanded {
                    out.push(sig.get());
                }
            }
            RichBlock::Divider => {}
            RichBlock::Section(sec) => {
                // 头部段落无论折叠与否都参与布局，其 clamp 信号必须入快照，
                // 否则点击头部的「… 展开」后缓存恒判命中（stale）、视觉无反应。
                if let Some(sig) = sec.header.clamp_expanded {
                    out.push(sig.get());
                }
                let c = sec.collapsed.get();
                out.push(c);
                if !c {
                    collect_collapsed(&sec.children, out);
                }
            }
        }
    }
}

/// 当前运行态是否与快照一致（与 `collect_collapsed` 同序遍历，**零分配**——
/// 缓存命中判定的快路径，避免每帧为比对而堆分配 Vec）。
fn collapsed_matches(blocks: &[RichBlock], snap: &[bool]) -> bool {
    fn walk(blocks: &[RichBlock], snap: &[bool], i: &mut usize) -> bool {
        for b in blocks {
            match b {
                RichBlock::Para(p) => {
                    if let Some(sig) = p.clamp_expanded {
                        if snap.get(*i) != Some(&sig.get()) {
                            return false;
                        }
                        *i += 1;
                    }
                }
                RichBlock::Divider => {}
                RichBlock::Section(sec) => {
                    if let Some(sig) = sec.header.clamp_expanded {
                        if snap.get(*i) != Some(&sig.get()) {
                            return false;
                        }
                        *i += 1;
                    }
                    let c = sec.collapsed.get();
                    if snap.get(*i) != Some(&c) {
                        return false;
                    }
                    *i += 1;
                    if !c && !walk(&sec.children, snap, i) {
                        return false;
                    }
                }
            }
        }
        true
    }
    let mut i = 0;
    walk(blocks, snap, &mut i) && i == snap.len()
}

/// 文档是否含可折叠 Section（决定控件是否参与 Tab 聚焦）。
fn has_section(blocks: &[RichBlock]) -> bool {
    blocks.iter().any(|b| matches!(b, RichBlock::Section(_)))
}

/// 通道向白插值（悬停提亮）。
fn lighten(c: Color, t: f32) -> Color {
    let ch = |x: u8| (x as f32 + (255.0 - x as f32) * t).round() as u8;
    Color::rgba(ch(c.r), ch(c.g), ch(c.b), c.a)
}

// ---------------------------------------------------------------------------
// 控件
// ---------------------------------------------------------------------------

/// 富文本控件（见模块文档）。经 [`super::Element::rich`] 构造。
pub struct RichText {
    doc: RichDoc,
    /// 布局缓存。
    ///
    /// 包一层 `Rc` 是为了**选择域**：跨控件划选时，捕获指针的那个控件要拿全局坐标去
    /// 问别的成员「这落在你第几个字上」，而成员的碎片几何全在这里。`Rc` 让那次询问
    /// 是零拷贝的；否则每帧都要把一屏几百个碎片的文本与字符偏移复制一份，而拖选期间
    /// 每一帧都在重绘。
    cache: RefCell<Option<Rc<RichLayout>>>,
    /// 最近一帧 paint 的 content 绝对矩形（事件坐标换算用）。
    last_content: Cell<Rect>,
    /// 悬停中的折叠头下标（headers 序）。
    hover_header: Cell<Option<usize>>,
    /// 按下时锁定的折叠头下标。
    pressed_header: Cell<Option<usize>>,
    /// 键盘焦点指向的折叠头下标（↑↓ 移动、Enter/Space 翻转；使用时按 headers 长度钳制）。
    focus_header: Cell<usize>,
    /// 悬停中的可点击 span 碎片下标（frags 序；视觉提亮 + 手型光标）。
    hover_span: Cell<Option<usize>>,
    /// 按下时锁定的可点击 span 碎片下标。
    pressed_span: Cell<Option<usize>>,
    /// span 点击回调（`Element::on_span_click` 注入；参数为 span 的 id）。
    on_span_click: Option<SpanClickFn>,
    /// 划选选区：(锚点, 延伸点)（无序存储，使用时排序）。
    ///
    /// 粒度为**字符级**。此前是碎片级，而碎片的切法是「Latin 按空格断、CJK 逐字断」
    /// ——于是 `standby` 这样的词头、`[ˈstændbaɪ]` 这样的音标各自只有一个碎片，拖动时
    /// 延伸点永远等于锚点，选区恒为空：那些内容只能双击整选，拖不动。中文因为逐字
    /// 成片才碰巧是可拖的。
    ///
    /// 布局重排（宽度/折叠/字体变化）时失效清空（碎片下标已不稳定）。
    sel: Cell<Option<(Caret, Caret)>>,
    /// 是否正在拖拽划选（Down 起、Up 止，期间 Move 更新延伸点）。
    selecting: Cell<bool>,
    /// 拖选锚点（Down 只记录、不落选区——拖出锚点字符才成选区，按下即选中一个字
    /// 不符合通用划选手感）。
    drag_anchor: Cell<Option<Caret>>,
    /// 指针悬停在正文文字上（I 形光标；span/折叠头的手型优先）。
    hover_text: Cell<bool>,
    /// 指针悬停在「… 展开」标记上（手型光标）。
    hover_exp: Cell<bool>,
    /// 是否内建右键「复制全部」菜单（默认开；`Element::copy_menu(false)` 关闭，
    /// 以便应用挂自己的 `on_context_menu`）。
    copy_menu: bool,
    /// 各 Section 高度补间（以折叠 Signal 为身份，跨重排存活）。
    sect_anims: RefCell<Vec<SectAnim>>,
    /// 动态文档源（`Element::rich_signal` 绑定）：`on_update` 检测版本变化换文档。
    doc_sig: Option<Signal<RichDoc>>,
    /// 已消化的文档信号版本。
    doc_version: Cell<u64>,
    /// 所属选择域（`Element::selection_scope` 注入）。
    ///
    /// 挂了域之后，选区的存放、绘制、复制全部改走域；本地的 `sel` 闲置。不挂域的控件
    /// 行为一字不变——windui 的别的用处（对话框正文、说明段落）用不着跨控件选择，不该
    /// 为此背上一份共享状态。
    scope: Option<ScopeMember>,
}

/// 控件与所属选择域的连接。
struct ScopeMember {
    scope: SelectionScope,
    cell: Rc<MemberCell>,
}

/// span 点击回调类型：`ctx` 在前（全库回调统一），其后是被点 span 的 id。
pub type SpanClickFn = Box<dyn FnMut(&mut EventCtx, &str)>;

impl RichText {
    pub fn new(doc: RichDoc) -> Self {
        Self {
            doc,
            cache: RefCell::new(None),
            last_content: Cell::new(Rect::new(0, 0, 0, 0)),
            hover_header: Cell::new(None),
            pressed_header: Cell::new(None),
            focus_header: Cell::new(0),
            hover_span: Cell::new(None),
            pressed_span: Cell::new(None),
            on_span_click: None,
            sel: Cell::new(None),
            selecting: Cell::new(false),
            drag_anchor: Cell::new(None),
            hover_text: Cell::new(false),
            hover_exp: Cell::new(false),
            copy_menu: true,
            sect_anims: RefCell::new(Vec::new()),
            doc_sig: None,
            doc_version: Cell::new(0),
            scope: None,
        }
    }

    /// 加入一个选择域（供 `Element::selection_scope`）。
    pub fn set_selection_scope(&mut self, scope: SelectionScope) {
        let cell = scope.join();
        self.scope = Some(ScopeMember { scope, cell });
    }

    /// 本控件当前该高亮的那段选区：挂了域就问域，否则用本地的。
    fn effective_sel(&self) -> Option<(Caret, Caret)> {
        match &self.scope {
            Some(sm) => sm.scope.local_sel(&sm.cell),
            None => self.sel.get(),
        }
    }

    /// 清掉选区并请求重绘。挂了域时脏的是**整个域**的范围——高亮可能落在别的控件上，
    /// 而 `ctx.mark_dirty()` 只标记自己那一块，用它的话别人身上的高亮会留在屏幕上不走。
    fn clear_sel(&self, ctx: &mut EventCtx) {
        match &self.scope {
            Some(sm) => {
                if sm.scope.clear() {
                    match sm.scope.bounds() {
                        Some(r) => ctx.mark_dirty_rect(r),
                        None => ctx.mark_dirty(),
                    }
                }
            }
            None => {
                if self.sel.take().is_some() {
                    ctx.mark_dirty();
                }
            }
        }
    }

    /// 绑定动态文档信号（词典切词条）：`layout_root` 前经 `on_update` 检测版本、
    /// 换入新文档并失效布局缓存/选区。须配合 `Element::reactive()`（`rich_signal` 已代办）。
    pub fn new_dynamic(sig: Signal<RichDoc>) -> Self {
        let mut rt = Self::new(sig.get());
        rt.doc_version = Cell::new(sig.version());
        rt.doc_sig = Some(sig);
        rt
    }

    /// 注入 span 点击回调（供 `Element::on_span_click`）。
    pub fn set_on_span_click(&mut self, f: SpanClickFn) {
        self.on_span_click = Some(f);
    }
    /// 内建右键复制菜单开关（供 `Element::copy_menu`）。
    pub fn set_copy_menu(&mut self, on: bool) {
        self.copy_menu = on;
    }

    fn layout_key(&self, wrap_w: Option<i32>, style: &Style, th: &Theme, scale: f32) -> LayoutKey {
        let mut collapsed = Vec::new();
        collect_collapsed(&self.doc.blocks, &mut collapsed);
        LayoutKey {
            wrap_w,
            family: style.font_family.clone(),
            size_bits: style.font_size.to_bits(),
            weight: style.font_weight,
            line_height_bits: style.line_height.map(f32::to_bits),
            scale_bits: scale.to_bits(),
            collapsed,
            spacing: (th.rich.para_spacing(), th.rich.section_indent()),
        }
    }

    /// 确保缓存布局与 (宽度, 字体, DPI, 折叠/展开态, 主题间距) 匹配，不匹配则重排。
    /// 命中判定走引用比较（零分配）——这是每帧 measure/paint 的常态路径；
    /// 只有真正 miss 时才构造拥有所有权的 `LayoutKey` 并重排。
    fn ensure_layout(&self, wrap_w: Option<i32>, style: &Style, m: &mut dyn Measurer) {
        let th = crate::theme::current();
        let scale = m.scale();
        let mut cache = self.cache.borrow_mut();
        // 无约束（Wrap 宽）视作上界无穷大，与有约束宽走同一条区间判定。
        let want_w = wrap_w.unwrap_or(i32::MAX);
        let hit = cache.as_ref().is_some_and(|l| {
            let k = &l.key;
            // 动画期产物恒 miss：高度补间每帧变化，且落定后还需一次重排
            // 得到无裁剪的稳定布局（否则冻结在最后一个动画帧）。
            !l.animated
                // 宽度按等价区间判定而非相等（见 `RichLayout::wrap_hi`）：measure 的
                // avail.w 与 paint 的 content.w 在 Wrap 宽下天然不等，按相等判会
                // 每帧交替重排、把选区一并清掉。
                && l.size.w <= want_w
                && want_w <= l.wrap_hi
                && k.family.as_deref() == style.font_family.as_deref()
                && k.size_bits == style.font_size.to_bits()
                && k.weight == style.font_weight
                && k.line_height_bits == style.line_height.map(f32::to_bits)
                && k.scale_bits == scale.to_bits()
                && k.spacing == (th.rich.para_spacing(), th.rich.section_indent())
                && collapsed_matches(&self.doc.blocks, &k.collapsed)
        });
        if !hit {
            let key = self.layout_key(wrap_w, style, &th, scale);
            let mut anims = self.sect_anims.borrow_mut();
            *cache = Some(Rc::new(layout_doc(
                &self.doc, key, style, m, &th, &mut anims,
            )));
            // 重排后碎片下标不再稳定，选区随之失效。域里那份一并清掉：它记的是
            // 「第几个成员的第几个碎片」，本控件重排之后那个下标已经指向别的字了。
            self.sel.set(None);
            if let Some(sm) = &self.scope {
                sm.scope.clear();
            }
        }
    }

    /// 翻转第 `idx` 个折叠头的信号（Signal 写入自动重绘；折叠态入布局键，下帧重排）。
    fn toggle_header(&self, idx: usize) {
        let sig = {
            let cache = self.cache.borrow();
            cache
                .as_ref()
                .and_then(|l| l.headers.get(idx))
                .map(|(_, s)| *s)
        };
        if let Some(sig) = sig {
            sig.set(!sig.get());
        }
    }

    /// 命中测试折叠头（`pos` 为绝对坐标）。
    fn header_at(&self, pos: Point) -> Option<usize> {
        let content = self.last_content.get();
        let local = Point::new(pos.x - content.x, pos.y - content.y);
        let cache = self.cache.borrow();
        let lay = cache.as_ref()?;
        lay.headers.iter().position(|(r, _)| r.contains(local))
    }

    /// 命中测试可点击 span 碎片（`pos` 为绝对坐标）。比折叠头更具体，优先判定。
    fn span_at(&self, pos: Point) -> Option<usize> {
        let content = self.last_content.get();
        let local = Point::new(pos.x - content.x, pos.y - content.y);
        let cache = self.cache.borrow();
        let lay = cache.as_ref()?;
        lay.frags
            .iter()
            .position(|f| f.id.is_some() && f.rect.contains(local))
    }

    /// 取第 `idx` 个碎片的 span id（克隆 Rc，借用即释）。
    fn span_id_of(&self, idx: usize) -> Option<Rc<str>> {
        let cache = self.cache.borrow();
        cache.as_ref()?.frags.get(idx)?.id.clone()
    }

    /// 命中「… 展开」标记（`pos` 为绝对坐标），返回其展开信号。
    fn expander_at(&self, pos: Point) -> Option<Signal<bool>> {
        let content = self.last_content.get();
        let local = Point::new(pos.x - content.x, pos.y - content.y);
        let cache = self.cache.borrow();
        cache
            .as_ref()?
            .frags
            .iter()
            .find(|f| f.expand.is_some() && f.rect.contains(local))
            .and_then(|f| f.expand)
    }

    /// 最近碎片（划选定位）：先按垂直距离找行、再按水平距离找片——指针在行间
    /// 空隙或行首行尾外侧时吸附到最近处，与文本编辑器的划选手感一致。
    fn frag_near(&self, pos: Point) -> Option<usize> {
        let content = self.last_content.get();
        let cache = self.cache.borrow();
        frag_near_in(cache.as_ref()?, local_of(pos, content))
    }

    /// 指针最近的选区端点：先由 [`Self::frag_near`] 选碎片（那套「最近碎片」的吸附
    /// 规则原样不动），再在碎片内落到最近的字符边界上。
    fn caret_near(&self, pos: Point) -> Option<Caret> {
        let content = self.last_content.get();
        let cache = self.cache.borrow();
        caret_near_in(cache.as_ref()?, local_of(pos, content))
    }

    /// 把一段碎片范围整体转成选区端点对（双击选词、三击选段、Ctrl+A 全选都用它）。
    fn whole_frags(&self, a: usize, b: usize) -> Option<(Caret, Caret)> {
        let cache = self.cache.borrow();
        let end = cache.as_ref()?.frags.get(b)?.char_x.len().saturating_sub(1);
        Some((Caret { frag: a, ch: 0 }, Caret { frag: b, ch: end }))
    }

    /// 指针是否精确落在某个碎片上（I 形光标判定；不吸附）。
    fn over_frag(&self, pos: Point) -> bool {
        let content = self.last_content.get();
        let local = Point::new(pos.x - content.x, pos.y - content.y);
        let cache = self.cache.borrow();
        cache
            .as_ref()
            .map(|l| l.frags.iter().any(|f| f.rect.contains(local)))
            .unwrap_or(false)
    }

    /// 双击选词：命中 CJK 字则向两侧吞并同块内连续的 CJK 字碎片（到标点/空白/
    /// 样式边界止——中文无空格分词，连续汉字串即编辑器惯例的"词"）；Latin 词
    /// 经 tokenize 本就是单碎片，standalone 标点/chip 选自身。
    fn word_range_at(&self, idx: usize) -> (usize, usize) {
        fn cjk_word(f: &Frag) -> bool {
            !f.chevron
                && f.expand.is_none()
                && !f.style.chip
                && f.text.chars().next().map(is_cjk).unwrap_or(false)
                && !is_close_punct(&f.text)
                && !is_open_punct(&f.text)
        }
        let cache = self.cache.borrow();
        let Some(lay) = cache.as_ref() else {
            return (idx, idx);
        };
        let frags = &lay.frags;
        let Some(f0) = frags.get(idx) else {
            return (idx, idx);
        };
        if !cjk_word(f0) {
            return (idx, idx);
        }
        let block = f0.block;
        let mut lo = idx;
        while lo > 0 && frags[lo - 1].block == block && cjk_word(&frags[lo - 1]) {
            lo -= 1;
        }
        let mut hi = idx;
        while hi + 1 < frags.len() && frags[hi + 1].block == block && cjk_word(&frags[hi + 1]) {
            hi += 1;
        }
        (lo, hi)
    }

    /// 三击选段：命中碎片所在**段落**（block）的全部碎片，含软换行的续行——
    /// 与浏览器三击行为一致（"选视觉行"是代码编辑器的习惯，阅读型内容跟网页）。
    fn para_range_at(&self, idx: usize) -> (usize, usize) {
        let cache = self.cache.borrow();
        let Some(lay) = cache.as_ref() else {
            return (idx, idx);
        };
        let frags = &lay.frags;
        let Some(f0) = frags.get(idx) else {
            return (idx, idx);
        };
        let block = f0.block;
        let mut lo = idx;
        while lo > 0 && frags[lo - 1].block == block {
            lo -= 1;
        }
        let mut hi = idx;
        while hi + 1 < frags.len() && frags[hi + 1].block == block {
            hi += 1;
        }
        (lo, hi)
    }

    /// 选区纯文本：按阅读序拼接选中碎片；跨块补换行，块内软换行按 CJK/Latin
    /// 边界决定是否补空格（折行时被丢弃的词间空格在此还原）。箭头不入文。
    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.sel.get()?;
        let cache = self.cache.borrow();
        slice_text(cache.as_ref()?, a, b)
    }

    /// 复制：有选区取选区，否则取全文。
    ///
    /// 挂了域就以域为准。用户拖过好几段，Ctrl+C 只给当前这一段，是把一次明确的意图
    /// 砍掉大半。
    fn copy_text(&self) -> String {
        if let Some(sm) = &self.scope {
            if let Some(t) = sm.scope.selected_text() {
                return t;
            }
        }
        self.selected_text()
            .unwrap_or_else(|| self.doc.plain_text())
    }

    /// 当前有没有选区（挂了域就问域）。
    fn has_any_sel(&self) -> bool {
        match &self.scope {
            Some(sm) => sm.scope.has_sel(),
            None => self.sel.get().is_some(),
        }
    }
}

// ── 跨控件选择域 ────────────────────────────────────────────────────────────
//
// 一个 `RichText` 自己是一个独立的选择域。而一屏内容常常被拆成好几个控件——词典条目
// 里词头一个、音标一个、每段释义一个（词头要与星标按钮并排，而富文本里放不进按钮），
// 于是选区跨不过去：用户拖过词头再往下拖，释义那段不会被选中。网页没有这个毛病，因为
// 整页共用一份 selection。
//
// 做法：成员在 `paint` 时把自己的布局（`Rc<RichLayout>`，零拷贝）连同 content 的绝对
// 矩形交给选择域。拖拽时指针被起始控件捕获、事件全送给它，它拿**全局坐标**去问选择域
// 「这落在谁的第几个字上」——不必访问别的控件，只读那份共享布局。

/// 选择域里的一个位置。
///
/// `member` 是**视觉序下标**，不是成员的身份。成员集合一变（查了新词、装卸词典），
/// 这个下标的含义就变了——而那时旧选区本就该作废，故不是缺陷：结果都换了，选区留着
/// 只会指到别的东西上。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct ScopeCaret {
    member: usize,
    frag: usize,
    ch: usize,
}

/// 一个成员交给选择域的快照。
struct MemberSnap {
    lay: Rc<RichLayout>,
    /// content 区的**绝对**矩形（`paint` 收到的那个）。
    content: Rect,
}

/// 成员在选择域里的槽位。
///
/// 控件持 `Rc`、域持 `Weak`：控件被销毁（重建结果区、切词条）后域这边自然失效，不需要
/// 谁去注销它——控件的析构没有一个能拿到域的可靠时机，而漏注销会让域里堆着一串指向
/// 旧布局的成员，选区于是落在早就不在屏幕上的东西上。
struct MemberCell {
    snap: RefCell<Option<MemberSnap>>,
}

#[derive(Default)]
struct ScopeState {
    members: RefCell<Vec<Weak<MemberCell>>>,
    /// 全局选区：(锚点, 延伸点)，无序存储、取用时排序。
    sel: Cell<Option<(ScopeCaret, ScopeCaret)>>,
    /// 拖拽锚点。
    anchor: Cell<Option<ScopeCaret>>,
}

/// 跨控件选择域：让若干富文本共用一份选区，拖拽可以从一个控件一路延伸到另一个。
///
/// 克隆即共享同一个域。用 [`Element::selection_scope`](super::Element::selection_scope)
/// 把控件挂进来：
///
/// ```no_run
/// use windui::prelude::*;
///
/// let scope = SelectionScope::new();
/// let page = Element::col()
///     .child(Element::rich(RichDoc::new().para("标题")).selection_scope(scope.clone()))
///     .child(Element::rich(RichDoc::new().para("正文")).selection_scope(scope.clone()));
/// ```
///
/// 没挂域的控件行为完全不变，仍是各管各的选区。
#[derive(Clone, Default)]
pub struct SelectionScope(Rc<ScopeState>);

impl SelectionScope {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入一个成员，拿回它的槽位。
    fn join(&self) -> Rc<MemberCell> {
        let cell = Rc::new(MemberCell {
            snap: RefCell::new(None),
        });
        self.0.members.borrow_mut().push(Rc::downgrade(&cell));
        cell
    }

    /// 成员按**视觉序**（上、左）。顺带把已销毁的槽位清掉。
    ///
    /// 视觉序而不是注册序：注册序取决于首次 `paint` 的次序，而那个次序在滚动、重建
    /// 之后并不稳定。对垂直流式布局，视觉序就是阅读序。
    ///
    /// 每次询问都重排一遍。成员是一屏之内的几十个，这点开销远小于「维护一份增量有序
    /// 表」要付的正确性代价——那份表得在每个成员每次移动时更新，漏一次就错到底。
    fn ordered(&self) -> Vec<(Rc<MemberCell>, Rect)> {
        let mut ms = self.0.members.borrow_mut();
        ms.retain(|w| w.strong_count() > 0);
        let mut out: Vec<(Rc<MemberCell>, Rect)> = ms
            .iter()
            .filter_map(|w| {
                let m = w.upgrade()?;
                // 还没画过的成员没有几何可言，跳过——它进不了选区，也不该占一个下标。
                let r = m.snap.borrow().as_ref().map(|s| s.content)?;
                Some((m, r))
            })
            .collect();
        out.sort_by_key(|(_, r)| (r.y, r.x));
        out
    }

    /// 全局坐标 → 域里的位置。
    ///
    /// 先挑成员：落在谁的 content 里就是谁；都不在则取**垂直距离最近**的那个。拖到
    /// 卡片之间的空隙、拖出结果区的上下边界时，选区仍要跟着走——这和浏览器一致，也是
    /// 「从头拖到尾」这个动作能成立的前提（中途必然扫过控件之间的缝）。
    fn caret_at(&self, pos: Point) -> Option<ScopeCaret> {
        let ms = self.ordered();
        let mut best: Option<(i32, usize)> = None;
        for (i, (_, r)) in ms.iter().enumerate() {
            let dy = if pos.y < r.y {
                r.y - pos.y
            } else if pos.y >= r.y + r.h {
                pos.y - (r.y + r.h) + 1
            } else {
                0
            };
            if best.map(|(bd, _)| dy < bd).unwrap_or(true) {
                best = Some((dy, i));
            }
        }
        let (_, i) = best?;
        let (m, content) = &ms[i];
        let snap = m.snap.borrow();
        let lay = &snap.as_ref()?.lay;
        let c = caret_near_in(lay, local_of(pos, *content))?;
        Some(ScopeCaret {
            member: i,
            frag: c.frag,
            ch: c.ch,
        })
    }

    /// 起划选：只记锚点，不落选区（拖出锚点字符才成选区）。
    fn begin(&self, pos: Point) {
        self.0.anchor.set(self.caret_at(pos));
    }

    /// 延伸到新位置。返回选区是否变了（变了才需要重绘）。
    fn extend(&self, pos: Point) -> bool {
        let (Some(a), Some(c)) = (self.0.anchor.get(), self.caret_at(pos)) else {
            return false;
        };
        let new = (c != a).then_some((a, c));
        if new != self.0.sel.get() {
            self.0.sel.set(new);
            return true;
        }
        false
    }

    fn end(&self) {
        self.0.anchor.set(None);
    }

    /// 清掉选区，返回原先是否有选区。
    fn clear(&self) -> bool {
        self.0.sel.take().is_some()
    }

    fn has_sel(&self) -> bool {
        self.0.sel.get().is_some()
    }

    /// 把某个成员内部的一段碎片范围提升为全域选区（双击选词、三击选段）。
    fn set_within(&self, me: &Rc<MemberCell>, a: Caret, b: Caret) -> bool {
        let Some(i) = self.index_of(me) else {
            return false;
        };
        self.0.sel.set(Some((
            ScopeCaret {
                member: i,
                frag: a.frag,
                ch: a.ch,
            },
            ScopeCaret {
                member: i,
                frag: b.frag,
                ch: b.ch,
            },
        )));
        true
    }

    /// 全选整个域。Ctrl+A 在域里的意思是「这一屏」，不是「我碰巧聚焦的这一段」。
    fn select_all(&self) -> bool {
        let ms = self.ordered();
        let Some((last, _)) = ms.last() else {
            return false;
        };
        let snap = last.snap.borrow();
        let Some(sn) = snap.as_ref() else {
            return false;
        };
        let n = sn.lay.frags.len();
        if n == 0 {
            return false;
        }
        let end = ScopeCaret {
            member: ms.len() - 1,
            frag: n - 1,
            ch: sn.lay.frags[n - 1].char_x.len().saturating_sub(1),
        };
        self.0.sel.set(Some((
            ScopeCaret {
                member: 0,
                frag: 0,
                ch: 0,
            },
            end,
        )));
        true
    }

    fn index_of(&self, me: &Rc<MemberCell>) -> Option<usize> {
        self.ordered().iter().position(|(m, _)| Rc::ptr_eq(m, me))
    }

    /// 全局选区落在某个成员身上的那一段（用它自己的碎片下标表示）。
    fn local_sel(&self, me: &Rc<MemberCell>) -> Option<(Caret, Caret)> {
        let ms = self.ordered();
        let idx = ms.iter().position(|(m, _)| Rc::ptr_eq(m, me))?;
        let (a, b) = self.0.sel.get()?;
        let (a, b) = (a.min(b), a.max(b));
        if idx < a.member || idx > b.member {
            return None;
        }
        let snap = me.snap.borrow();
        let lay = &snap.as_ref()?.lay;
        let n = lay.frags.len();
        if n == 0 {
            return None;
        }
        // 首尾成员按各自的端点切，中间的成员整个进选区。
        let start = if idx == a.member {
            Caret {
                frag: a.frag,
                ch: a.ch,
            }
        } else {
            Caret { frag: 0, ch: 0 }
        };
        let end = if idx == b.member {
            Caret {
                frag: b.frag,
                ch: b.ch,
            }
        } else {
            Caret {
                frag: n - 1,
                ch: lay.frags[n - 1].char_x.len().saturating_sub(1),
            }
        };
        Some((start, end))
    }

    /// 全域选区的纯文本。成员之间补换行——它们在界面上本就是分开的块。
    fn selected_text(&self) -> Option<String> {
        let ms = self.ordered();
        let (a, b) = self.0.sel.get()?;
        let (a, b) = (a.min(b), a.max(b));
        let mut out = String::new();
        for idx in a.member..=b.member.min(ms.len().saturating_sub(1)) {
            let (m, _) = ms.get(idx)?;
            let Some((s, e)) = self.local_sel(m) else {
                continue;
            };
            let snap = m.snap.borrow();
            let Some(sn) = snap.as_ref() else { continue };
            if let Some(t) = slice_text(&sn.lay, s, e) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&t);
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// 所有成员的并集矩形。
    ///
    /// 拿它去 `mark_dirty_rect`：清选区、跨控件延伸时脏的是**别人**的像素，而
    /// `ctx.mark_dirty()` 只标记自己那一块——用它的话，别的控件上的高亮会留在屏幕上
    /// 不走。
    fn bounds(&self) -> Option<Rect> {
        let ms = self.ordered();
        let mut it = ms.iter().map(|(_, r)| *r);
        let first = it.next()?;
        let (mut x0, mut y0) = (first.x, first.y);
        let (mut x1, mut y1) = (first.x + first.w, first.y + first.h);
        for r in it {
            x0 = x0.min(r.x);
            y0 = y0.min(r.y);
            x1 = x1.max(r.x + r.w);
            y1 = y1.max(r.y + r.h);
        }
        Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
    }
}

impl Widget for RichText {
    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        // 与 Label 同约定：宽度受限时按其换行；换行准确性仅保证于显式宽度
        //（width/width_match/weight），纯 Wrap 宽下为逐段单行的自然尺寸。
        let wrap_w = (avail.w > 0).then_some(avail.w);
        let mut m = EngineMeasurer(text);
        self.ensure_layout(wrap_w, style, &mut m);
        self.cache
            .borrow()
            .as_ref()
            .map(|l| l.size)
            .unwrap_or(Size::ZERO)
    }

    fn paint(
        &self,
        _bounds: Rect,
        content: Rect,
        focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        self.last_content.set(content);
        {
            let mut m = CanvasMeasurer(canvas);
            self.ensure_layout(Some(content.w), style, &mut m);
        }
        let th = crate::theme::current();
        let pal = &th.palette;
        let cache = self.cache.borrow();
        let Some(lay) = cache.as_ref() else { return };

        // 把这一帧的几何交给选择域。`Rc::clone` 而不是拷贝碎片：拖选期间每帧都在重绘，
        // 一屏几百个碎片的文本与字符偏移复制一遍是白花的钱。
        //
        // 写完立刻放开借用：下面 `effective_sel` 要读回所有成员的快照（含自己这一份），
        // 借用还攥在手里就会撞上 `RefCell` 的运行期检查。
        if let Some(sm) = &self.scope {
            *sm.cell.snap.borrow_mut() = Some(MemberSnap {
                lay: Rc::clone(lay),
                content,
            });
        }

        // 高度动画进行中：请求下一帧重排（经正规门，见 anim::request_relayout）。
        if lay.animated {
            crate::anim::request_relayout();
        }

        // 选区高亮：先于文字整片铺底（空白碎片也在碎片列表里，词隙不断档）。
        // 纵向按行盒铺满（非碎片自身高）：对齐系统文本控件——下伸部完整包住、
        // 同行混排字号顶底齐平、多行选中行与行之间无白缝。
        if let Some((a, b)) = self.effective_sel() {
            let (a, b) = (a.min(b), a.max(b));
            let selc = th.rich.selection(pal);
            for i in a.frag..=b.frag.min(lay.frags.len().saturating_sub(1)) {
                let Some(f) = lay.frags.get(i) else { continue };
                // `char_x` 相对 `text_rect`，而高亮铺在 `rect` 上（chip 含内边距），
                // 故要把那段 padding 加回去。
                let pad = f.text_rect.x - f.rect.x;
                let x0 = if i == a.frag {
                    pad + f.char_x.get(a.ch).copied().unwrap_or(0)
                } else {
                    0
                };
                let x1 = if i == b.frag {
                    pad + f.char_x.get(b.ch).copied().unwrap_or(f.text_rect.w)
                } else {
                    f.rect.w
                };
                if x1 <= x0 {
                    continue;
                }
                canvas.fill_rect(
                    (content.x + f.rect.x + x0) as f32,
                    (content.y + f.line_top) as f32,
                    (x1 - x0) as f32,
                    f.line_h as f32,
                    &Paint::fill(selc),
                );
            }
        }

        // 悬停的可点击 span：同 id 的所有碎片（换行拆片）一起提亮。
        let hovered_id = self
            .hover_span
            .get()
            .and_then(|i| lay.frags.get(i))
            .and_then(|f| f.id.clone());

        for (idx, f) in lay.frags.iter().enumerate() {
            // 动画期：落在收拢/展开区间内的碎片按当前露出高度裁剪（卷帘）。
            let mut saves = 0;
            for r in &lay.clips {
                if idx >= r.lo && idx < r.hi {
                    canvas.save();
                    canvas.clip_rect(Rect::new(content.x, content.y + r.y0, content.w, r.reveal));
                    saves += 1;
                }
            }
            let st = &f.style;
            let rect = Rect::new(
                content.x + f.rect.x,
                content.y + f.rect.y,
                f.rect.w,
                f.rect.h,
            );
            // 背景 / 胶囊底。
            let bg = match (st.bg, st.chip) {
                (Some(rc), _) => Some(rc.resolve(pal)),
                (None, true) => Some(th.rich.chip_bg(pal)),
                (None, false) => None,
            };
            if let Some(bg) = bg {
                let radius = if st.chip { rect.h as f32 / 2.0 } else { 2.0 };
                canvas.fill_round_rect(
                    rect.x as f32,
                    rect.y as f32,
                    rect.w as f32,
                    rect.h as f32,
                    radius,
                    &Paint::fill(bg),
                );
            }
            // 前景：禁用统一置灰（与 Label 同纪律）。
            let mut fg = if !enabled {
                pal.text_disabled
            } else if f.chevron {
                th.rich.chevron(pal)
            } else {
                match st.fg {
                    Some(rc) => rc.resolve(pal),
                    None if st.chip => th.rich.chip_fg(pal),
                    None => super::text_fg(true, style, &th),
                }
            };
            // 悬停提亮（可点击 span）：向白插值 25%——亮暗主题下都表现为「变亮」，
            // 与 accent 家族的 hover 变体观感一致。
            if enabled && f.id.is_some() && f.id == hovered_id {
                fg = lighten(fg, 0.25);
            }
            let text_rect = Rect::new(
                content.x + f.text_rect.x,
                content.y + f.text_rect.y,
                f.text_rect.w,
                f.text_rect.h,
            );
            if !f.text.trim().is_empty() {
                canvas.draw_text(&f.text, text_rect, fg, crate::spec::Align::Start, &st.ts());
            }
            // 下划线贴基线下缘、删除线穿 x 高中部；色随前景。
            let x0 = text_rect.x as f32;
            let x1 = (text_rect.x + text_rect.w) as f32;
            if st.underline {
                let y = text_rect.y as f32
                    + f.ascent
                    + ((f.text_rect.h as f32 - f.ascent) * 0.35).max(1.0);
                canvas.draw_line(x0, y, x1, y, 1.0, &Paint::fill(fg));
            }
            if st.strike {
                let y = text_rect.y as f32 + f.ascent * 0.66;
                canvas.draw_line(x0, y, x1, y, 1.0, &Paint::fill(fg));
            }
            for _ in 0..saves {
                canvas.restore();
            }
        }
        // 分隔线延展到 content 右缘。
        let dcol = if enabled {
            th.rich.divider(pal)
        } else {
            pal.divider
        };
        for &(dx, dy) in &lay.dividers {
            // 动画期：落在收拢隐藏带内的分隔线不画。
            if lay
                .clips
                .iter()
                .any(|r| dy >= r.y0 + r.reveal && dy < r.y0 + r.full)
            {
                continue;
            }
            let y = (content.y + dy) as f32 + 0.5;
            canvas.draw_line(
                (content.x + dx) as f32,
                y,
                (content.x + content.w) as f32,
                y,
                1.0,
                &Paint::fill(dcol),
            );
        }
        // 键盘焦点：给当前聚焦的折叠头描 accent 细框（Tab 聚焦后 ↑↓/Enter 可视化）。
        if focused && enabled && !lay.headers.is_empty() {
            let idx = self.focus_header.get().min(lay.headers.len() - 1);
            let (r, _) = &lay.headers[idx];
            canvas.stroke_round_rect(
                (content.x + r.x) as f32 - 2.0,
                (content.y + r.y) as f32 - 2.0,
                (r.w + 4) as f32,
                (r.h + 4) as f32,
                4.0,
                1.0,
                &Paint::fill(pal.accent),
            );
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Key(k) = ev {
            if !k.pressed {
                return false;
            }
            // Ctrl+C：有选区复制选区、无则复制全文；Ctrl+Shift+C：强制复制全文。
            // 内建右键菜单经 SendKey 回投到此（VK_C=0x43）。
            if k.ctrl && k.key == Key::Other(0x43) {
                let text = if k.shift {
                    self.doc.plain_text()
                } else {
                    self.copy_text()
                };
                ctx.clipboard_set(&text);
                return true;
            }
            // Ctrl+A：全选（VK_A=0x41）。
            if k.ctrl && k.key == Key::Other(0x41) {
                let n = self
                    .cache
                    .borrow()
                    .as_ref()
                    .map(|l| l.frags.len())
                    .unwrap_or(0);
                match &self.scope {
                    // 进了域，Ctrl+A 的意思是「这一屏」，不是「我碰巧聚焦的这一段」
                    // ——焦点落在哪一段是上一次点击的副产物，用户按全选时并不在想它。
                    Some(sm) => {
                        if sm.scope.select_all() {
                            match sm.scope.bounds() {
                                Some(r) => ctx.mark_dirty_rect(r),
                                None => ctx.mark_dirty(),
                            }
                        }
                    }
                    None => {
                        if n > 0 {
                            self.sel.set(self.whole_frags(0, n - 1));
                            ctx.mark_dirty();
                        }
                    }
                }
                return true;
            }
            // ↑↓ 在折叠头间移动焦点，Enter/Space 翻转当前头（与 Accordion 约定一致）。
            let n = self
                .cache
                .borrow()
                .as_ref()
                .map(|l| l.headers.len())
                .unwrap_or(0);
            if n == 0 {
                return false;
            }
            let cur = self.focus_header.get().min(n - 1);
            return match k.key {
                Key::Enter | Key::Space => {
                    self.focus_header.set(cur);
                    self.toggle_header(cur);
                    ctx.mark_dirty();
                    true
                }
                Key::Up if cur > 0 => {
                    self.focus_header.set(cur - 1);
                    ctx.mark_dirty();
                    true
                }
                Key::Down if cur + 1 < n => {
                    self.focus_header.set(cur + 1);
                    ctx.mark_dirty();
                    true
                }
                _ => false,
            };
        }
        let Event::Pointer(p) = ev else { return false };
        match p.kind {
            PointerKind::Move | PointerKind::Enter => {
                // 拖拽划选中：更新延伸点（capture 保证界外 Move 也送达）。
                // 未拖出锚点碎片前不产生选区；拖回锚点碎片则选区消失。
                if self.selecting.get() {
                    match &self.scope {
                        // 挂了域：拿**全局坐标**去问域。指针被本控件捕获，界外的 Move
                        // 也照样送到这里，于是拖到别的成员身上时那个坐标依然有效——跨
                        // 控件划选就成立在这一点上。
                        Some(sm) => {
                            if sm.scope.extend(p.pos) {
                                match sm.scope.bounds() {
                                    Some(r) => ctx.mark_dirty_rect(r),
                                    None => ctx.mark_dirty(),
                                }
                            }
                        }
                        None => {
                            if let (Some(anchor), Some(i)) =
                                (self.drag_anchor.get(), self.caret_near(p.pos))
                            {
                                let new = (i != anchor).then_some((anchor, i));
                                if new != self.sel.get() {
                                    self.sel.set(new);
                                    ctx.mark_dirty();
                                }
                            }
                        }
                    }
                    return true;
                }
                let over_span = self.span_at(p.pos);
                if over_span != self.hover_span.get() {
                    self.hover_span.set(over_span);
                    // 可点击 span 有提亮反馈，悬停变化需重绘；折叠头无 hover 视觉则不必。
                    ctx.mark_dirty();
                }
                let over = self.header_at(p.pos);
                if over != self.hover_header.get() {
                    self.hover_header.set(over);
                }
                self.hover_exp.set(self.expander_at(p.pos).is_some());
                self.hover_text.set(self.over_frag(p.pos));
                false
            }
            PointerKind::Leave => {
                if self.hover_span.take().is_some() {
                    ctx.mark_dirty();
                }
                self.hover_header.set(None);
                self.hover_text.set(false);
                self.hover_exp.set(false);
                false
            }
            PointerKind::Down if p.button == MouseButton::Right => {
                // 内建右键复制：先聚焦（菜单项以 SendKey 回投焦点节点），再弹菜单。
                // 右键不清选区——「划选 → 右键 → 复制」是主路径。
                if !self.copy_menu {
                    return false;
                }
                ctx.request_focus();
                let key = |vk: u32, shift: bool| KeyEvent {
                    key: Key::Other(vk),
                    pressed: true,
                    shift,
                    ctrl: true,
                };
                let mut items = Vec::new();
                if self.has_any_sel() {
                    items.push(MenuItem::key("复制", key(0x43, false), true));
                    items.push(MenuItem::key("复制全部", key(0x43, true), true));
                } else {
                    items.push(MenuItem::key("复制全部", key(0x43, false), true));
                }
                items.push(MenuItem::key("全选", key(0x41, false), true));
                ctx.show_context_menu(p.pos, items);
                true
            }
            PointerKind::Down if p.button == MouseButton::Left => {
                // 任何左键按下先清旧选区（与编辑器习惯一致）。
                self.clear_sel(ctx);
                // 「… 展开」标记最具体，按下即展开（Signal 自动重绘，布局键随
                // 展开态失效重排）。
                if let Some(sig) = self.expander_at(p.pos) {
                    sig.set(true);
                    return true;
                }
                // 可点击 span 比折叠头更具体，优先命中（折叠头内也可嵌交叉引用）。
                if let Some(idx) = self.span_at(p.pos) {
                    self.pressed_span.set(Some(idx));
                    ctx.capture();
                    return true;
                }
                if let Some(idx) = self.header_at(p.pos) {
                    self.pressed_header.set(Some(idx));
                    ctx.capture();
                    return true;
                }
                // 正文区：双击选词 / 三击选行（不进入拖选态——Up 各分支均不命中，
                // 选区得以保留；交互控件的连点仍走上面的单击路径，行为不变）。
                if p.click_count >= 2 {
                    let Some(i) = self.frag_near(p.pos) else {
                        return false;
                    };
                    ctx.request_focus();
                    let (ra, rb) = if p.click_count >= 3 {
                        self.para_range_at(i)
                    } else {
                        self.word_range_at(i)
                    };
                    let range = self.whole_frags(ra, rb);
                    match (&self.scope, range) {
                        // 双击/三击选中的仍是本控件内的一段，但它得记进域里——否则
                        // 域里那份选区还是空的，Ctrl+C 会拿不到刚选中的词。
                        (Some(sm), Some((a, b))) => {
                            sm.scope.set_within(&sm.cell, a, b);
                        }
                        _ => self.sel.set(range),
                    }
                    ctx.mark_dirty();
                    return true;
                }
                // 起划选：只记锚点，不落选区（拖出锚点字符才出现高亮）。
                // 先聚焦——物理 Ctrl+C 与菜单 SendKey 都路由到焦点节点。
                let Some(i) = self.caret_near(p.pos) else {
                    return false;
                };
                ctx.request_focus();
                if let Some(sm) = &self.scope {
                    sm.scope.begin(p.pos);
                }
                self.drag_anchor.set(Some(i));
                self.selecting.set(true);
                ctx.capture();
                true
            }
            PointerKind::Up if p.button == MouseButton::Left => {
                if self.selecting.get() {
                    // 锚点延迟落地后，原地单击本就无选区，Up 只收尾。
                    self.selecting.set(false);
                    self.drag_anchor.set(None);
                    if let Some(sm) = &self.scope {
                        sm.scope.end();
                    }
                    ctx.release_capture();
                    ctx.mark_dirty();
                    return true;
                }
                if let Some(idx) = self.pressed_span.take() {
                    ctx.release_capture();
                    // 同一 id 内抬起即触发（换行拆片后跨碎片抬起也算点中）。
                    let pressed_id = self.span_id_of(idx);
                    let over_id = self.span_at(p.pos).and_then(|i| self.span_id_of(i));
                    if let (Some(a), Some(b)) = (pressed_id, over_id) {
                        if a == b {
                            if let Some(cb) = self.on_span_click.as_mut() {
                                cb(ctx, &a);
                            }
                        }
                    }
                    return true;
                }
                let Some(idx) = self.pressed_header.take() else {
                    return false;
                };
                ctx.release_capture();
                if self.header_at(p.pos) == Some(idx) {
                    // 鼠标操作同步键盘焦点位置，避免随后按 Enter 翻转到别的头。
                    self.focus_header.set(idx);
                    self.toggle_header(idx);
                }
                true
            }
            _ => false,
        }
    }

    fn on_update(&mut self, _ctx: &mut EventCtx) {
        // 动态文档：信号版本变化 → 换文档、清布局缓存与选区（碎片全部作废）。
        // 展开/折叠 Signal 由应用与文档一起管理，跨词条自然复位或延续由应用决定。
        let Some(sig) = self.doc_sig else { return };
        let v = sig.version();
        if v != self.doc_version.get() {
            self.doc_version.set(v);
            self.doc = sig.get();
            *self.cache.borrow_mut() = None;
            self.sel.set(None);
            if let Some(sm) = &self.scope {
                sm.scope.clear();
            }
            // 换文档不做高度动画：清空补间状态，新文档首帧静止落定。
            self.sect_anims.borrow_mut().clear();
            // 悬停/按下/键盘焦点都是旧文档碎片下标——不复位会在新文档上产生
            // 幽灵提亮/手型（切词条时鼠标恰停在被点 span 上，必现场景）。
            self.reset_interaction();
            self.focus_header.set(0);
        }
    }

    fn reset_interaction(&mut self) {
        // 显隐翻转时复位交互态：拖选/按下若残留，再次显示后单纯悬停就会
        // 误入"延伸选区"分支（capture 已被别处接管、Up 永远到不了本控件）。
        self.selecting.set(false);
        self.drag_anchor.set(None);
        self.pressed_span.set(None);
        self.pressed_header.set(None);
        self.hover_span.set(None);
        self.hover_header.set(None);
        self.hover_text.set(false);
        self.hover_exp.set(false);
    }

    fn focusable(&self) -> bool {
        // 仅当文档含可折叠 Section 时参与 Tab 导航（纯静态文本不占焦点位）。
        has_section(&self.doc.blocks)
    }

    fn cursor(&self) -> CursorShape {
        if self.hover_span.get().is_some()
            || self.hover_header.get().is_some()
            || self.hover_exp.get()
        {
            CursorShape::Hand
        } else if self.hover_text.get() {
            // 正文可划选，I 形光标提示。
            CursorShape::Text
        } else {
            CursorShape::Arrow
        }
    }

    fn wants_right_click(&self) -> bool {
        // 内建「复制全部」右键菜单（可经 Element::copy_menu(false) 关闭）。
        self.copy_menu
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod italic_tests {
    use super::*;

    /// 斜体必须传到 `TextStyle`，否则引擎收不到它。
    #[test]
    fn 斜体传得到文字样式() {
        let fs = FragStyle {
            size: 14.0,
            weight: 400,
            family: None,
            line_height: None,
            fg: None,
            bg: None,
            italic: true,
            underline: false,
            strike: false,
            chip: false,
        };
        assert!(fs.ts().italic, "FragStyle 的斜体没传进 TextStyle");
    }

    /// 斜体是开关不是取值：命名样式开了，内联样式不该把它关掉。
    ///
    /// 若写成 `self.italic`（覆盖语义），命名样式里的斜体会被任何一个内联样式清掉，
    /// 而 underline / strike 早已按「或」处理——三者必须一致，否则同一份文档里
    /// 三个开关的行为不同。
    #[test]
    fn 斜体按或合并而非覆盖() {
        let 底 = SpanStyle::new().italic();
        let 上 = SpanStyle::new().bold();
        assert!(上.over(&底).italic, "命名样式的斜体被内联样式清掉了");
        assert!(SpanStyle::new().italic().over(&SpanStyle::new()).italic);
        assert!(!SpanStyle::new().over(&SpanStyle::new()).italic);
    }

    /// 斜体与字重正交——「粗斜体」必须能同时表达。
    #[test]
    fn 粗体与斜体可以并存() {
        let s = SpanStyle::new().bold().italic();
        assert!(s.italic);
        assert!(s.weight.is_some_and(|w| w > crate::text::WEIGHT_NORMAL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Tree;
    use crate::event::{MouseButton, PointerEvent};
    use crate::signal::signal;
    use crate::ui::Element;

    /// NullTextEngine 尺寸约定：宽 = 字符数 × size × 0.6 向上取整；高 = size；
    /// 基线 = 高 × 0.8（trait 默认近似）。默认字号 14 → 单 CJK 字 9×14。
    /// 根节点会被拉伸到窗口尺寸，故把 rich 包进 col、返回其子节点 id 供断言。
    fn build(el: Element, w: i32, h: i32) -> (Tree, crate::core::NodeId) {
        let mut tree = Tree::new();
        let root = Element::col().child(el).build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(w, h), &mut crate::text::NullTextEngine);
        let child = tree.get(root).unwrap().children[0];
        (tree, child)
    }

    fn node_h(tree: &Tree, id: crate::core::NodeId) -> i32 {
        tree.get(id).unwrap().bounds.h
    }

    #[test]
    fn cjk_wraps_at_width() {
        // 20 个 CJK 字、每字 9px：95px 宽一行放 10 字（90≤95，第 11 字 99>95）→ 2 行。
        let doc = RichDoc::new().para("汉".repeat(20));
        let (tree, root) = build(Element::rich(doc).width(95), 300, 300);
        assert_eq!(node_h(&tree, root), 28, "20 字在 95px 宽应折成 2 行 × 14px");
    }

    #[test]
    fn newline_forces_break() {
        let doc = RichDoc::new().para("a\nb");
        let (tree, root) = build(Element::rich(doc).width(200), 300, 300);
        assert_eq!(node_h(&tree, root), 28, "\\n 应强制换行为 2 行");
    }

    #[test]
    fn mixed_sizes_align_on_baseline() {
        // 14px 与 28px 同行：行高 = ceil(max asc 22.4 + max desc 5.6) = 28；
        // 小字碎片 top = round(22.4 − 11.2) = 11（基线对齐产生的下沉）。
        let doc = RichDoc::new().para(Para::new().text("a").span("b", SpanStyle::new().size(28.0)));
        let rt = RichText::new(doc);
        let style = Style::default();
        let sz = rt.measure(Size::new(500, 0), &style, &mut crate::text::NullTextEngine);
        assert_eq!(sz.h, 28, "行高应取大字号的自然行高");
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        assert_eq!(frags.len(), 2);
        assert_eq!(frags[0].rect.y, 11, "小字应下沉到公共基线");
        assert_eq!(frags[1].rect.y, 0, "大字决定行盒、顶对齐");
    }

    #[test]
    fn selection_box_covers_full_line_height() {
        // 回归：选区曾按碎片自身 rect 铺，混排字号时小字高亮只有 11..25、
        // 与大字顶底参差，下伸部也露在高亮外。行盒（0..28）才是正确铺底范围。
        let doc = RichDoc::new().para(Para::new().text("a").span("b", SpanStyle::new().size(28.0)));
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(500, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        for (i, f) in frags.iter().enumerate() {
            assert_eq!(f.line_top, 0, "碎片 {i} 行盒顶应为行起点");
            assert_eq!(f.line_h, 28, "碎片 {i} 应共用大字号决定的行盒高");
        }
        assert!(
            frags[0].line_h > frags[0].rect.h,
            "小字碎片的行盒应高于其自身框（14），否则高亮仍参差"
        );
    }

    #[test]
    fn adjacent_line_selection_boxes_touch() {
        // 回归：多行选中的行与行之间不得留白缝——上一行行盒底须正好是下一行行盒顶。
        // 宽 20 只装两个 CJK 字（9px/字），"汉汉汉汉" 排成两行、行高各 14。
        let doc = RichDoc::new().para("汉汉汉汉");
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(20, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        let second = frags.iter().find(|f| f.line == 1).expect("应折出第二行");
        assert_eq!(
            second.line_top,
            frags[0].line_top + frags[0].line_h,
            "下一行行盒顶应紧接上一行行盒底"
        );
        // 同一行内所有碎片共用行盒（横向连片，无高低差）。
        for f in frags.iter().filter(|f| f.line == 0) {
            assert_eq!(f.line_top, frags[0].line_top);
            assert_eq!(f.line_h, frags[0].line_h);
        }
    }

    #[test]
    fn chip_adds_padding_box() {
        // chip 12px："n." 文字 15×12，pad = (5,2) → 盒 25×16。
        let doc = RichDoc::new().para(Para::new().span("n.", SpanStyle::new().size(12.0).chip()));
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(500, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let f = &cache.as_ref().unwrap().frags[0];
        assert_eq!((f.rect.w, f.rect.h), (25, 16), "chip 盒应含内边距");
        assert_eq!((f.text_rect.w, f.text_rect.h), (15, 12), "文字矩形内缩 pad");
    }

    #[test]
    fn section_collapse_shrinks_and_click_toggles() {
        // 本测试断言折叠后的最终高度，关掉高度动画使补间瞬时落定
        //（thread-local 开关，不影响并行测试线程；尾部复位）。
        crate::anim::set_enabled(false);
        let collapsed = signal(false);
        let doc = RichDoc::new()
            .para("正文")
            .section("例句", collapsed, |d| d.para("第一句"));
        // 展开：正文 14 + 间距 6 + 头 14 + 间距 6 + 子段 14 = 54；收起：34。
        let (mut tree, root) = build(Element::rich(doc).width(200), 300, 300);
        assert_eq!(node_h(&tree, root), 54, "展开高度");

        // 点击折叠头（y ∈ [20,34)）。
        let (mut hover, mut cap) = (None, None);
        let at = crate::geometry::Point::new(10, 25);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        assert!(collapsed.get(), "点击头部应翻转折叠信号");

        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(node_h(&tree, root), 34, "收起后高度只剩正文 + 头");
        crate::anim::set_enabled(true);
    }

    #[test]
    fn section_height_animates_on_collapse() {
        crate::anim::set_enabled(true);
        crate::anim::set_clock_ms(1_000);
        let collapsed = signal(false);
        let doc = RichDoc::new()
            .para("正文")
            .section("例句", collapsed, |d| d.para("第一句"));
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        assert_eq!(node_h(&tree, node), 54, "首帧静止落定，不动画");

        let dur = crate::theme::current().anim.normal() as u64;
        collapsed.set(true);
        // 切换帧：补间刚起步，高度仍在起点。
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(node_h(&tree, node), 54, "动画起点应保持展开高度");

        // 半程：高度介于两态之间（EaseInOut 半程恰为中点）。
        crate::anim::set_clock_ms(1_000 + dur / 2);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        let mid = node_h(&tree, node);
        assert!(mid > 34 && mid < 54, "半程高度应介于 34..54，实得 {mid}");

        // 结束后：落定收起高度，且再排一次得到无裁剪稳定布局。
        crate::anim::set_clock_ms(1_000 + dur + 100);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(node_h(&tree, node), 34, "动画结束应落定收起高度");
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(node_h(&tree, node), 34, "落定后布局应稳定");
    }

    #[test]
    fn hanging_indent_applies_to_continuation_lines() {
        // "aa bb cc" 宽 40：aa+空(26) 后 bb 溢出换行；续行左缘应为 hanging=10。
        let doc = RichDoc::new().para(Para::new().text("aa bb cc").hanging(10));
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(40, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        assert_eq!(frags[0].rect.x, 0, "首行从 indent 起");
        assert!(
            frags[1..].iter().all(|f| f.rect.x >= 10),
            "续行应从 hanging=10 起"
        );
        assert_eq!(frags[1].rect.x, 10, "续行行首对齐 hanging");
    }

    #[test]
    fn spacing_before_overrides_theme_default() {
        let doc = RichDoc::new()
            .para("a")
            .para(Para::new().text("b").spacing_before(20));
        let (tree, node) = build(Element::rich(doc).width(200), 300, 300);
        // 14 + 20 + 14 = 48（默认段距 6 时应为 34）。
        assert_eq!(node_h(&tree, node), 48, "段级间距覆盖应生效");
    }

    #[test]
    fn kinsoku_close_punct_sticks_to_line_end() {
        // 宽 20 只装下两个 CJK 字（18px）；"，" 本会掉行首，避头规则应附着行尾。
        let doc = RichDoc::new().para("汉汉，");
        let rt = RichText::new(doc);
        let style = Style::default();
        let sz = rt.measure(Size::new(20, 0), &style, &mut crate::text::NullTextEngine);
        assert_eq!(sz.h, 14, "闭合标点应附着行尾，不产生第二行");
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        assert!(frags.iter().all(|f| f.rect.y == 0), "三个碎片同在首行");
    }

    #[test]
    fn kinsoku_open_punct_carries_to_next_line() {
        // 宽 20："汉（" 装满首行后下个 "汉" 换行——"（" 不得孤悬行尾，应随之下行。
        let doc = RichDoc::new().para("汉（汉");
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(20, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        let open = frags.iter().find(|f| f.text == "（").unwrap();
        assert_eq!((open.rect.x, open.rect.y), (0, 14), "开括应移到次行行首");
    }

    #[test]
    fn plain_text_includes_chips_and_collapsed_sections() {
        let collapsed = signal(true);
        let doc = RichDoc::new()
            .para(
                Para::new()
                    .span("n.", SpanStyle::new().chip())
                    .text(" 苹果"),
            )
            .section("例句", collapsed, |d| d.para("An apple a day."));
        assert_eq!(
            doc.plain_text(),
            "n. 苹果\n例句\nAn apple a day.",
            "chip 文字与折叠区内容都应包含"
        );
    }

    #[test]
    fn span_click_fires_with_id_and_plain_text_ignores() {
        let hit = signal(0);
        let h2 = hit;
        let doc = RichDoc::new().para(Para::new().text("参见 ").span_id(
            "fruit",
            "fruit",
            SpanStyle::new().underline(),
        ));
        let (mut tree, _node) = build(
            Element::rich(doc).on_span_click(move |_, id| {
                assert_eq!(id, "fruit");
                h2.set(h2.get() + 1);
            }),
            300,
            300,
        );
        let (mut hover, mut cap) = (None, None);
        // "参见 " 宽 3*9=27（空格并入 x 前进），fruit 从 x=36 起宽 5*9=45。
        let on_span = crate::geometry::Point::new(40, 7);
        let off_span = crate::geometry::Point::new(5, 7);
        for at in [off_span, on_span] {
            tree.dispatch_pointer(
                PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
                &mut hover,
                &mut cap,
            );
            tree.dispatch_pointer(
                PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
                &mut hover,
                &mut cap,
            );
        }
        assert_eq!(hit.get(), 1, "仅点中标 id 的文字触发一次回调");
    }

    #[test]
    fn right_click_offers_copy_and_ctrl_c_copies_plain_text() {
        use std::cell::RefCell as StdRefCell;
        use std::rc::Rc as StdRc;
        struct Clip(StdRc<StdRefCell<String>>);
        impl crate::core::ClipboardProvider for Clip {
            fn get_text(&self) -> Option<String> {
                Some(self.0.borrow().clone())
            }
            fn set_text(&self, text: &str) {
                *self.0.borrow_mut() = text.to_string();
            }
        }
        let doc = RichDoc::new().para("苹果").para("释义");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = StdRc::new(StdRefCell::new(String::new()));
        tree.clipboard = Some(Box::new(Clip(clip.clone())));

        // 右键应弹出「复制全部」菜单。
        let (mut hover, mut cap) = (None, None);
        let at = crate::geometry::Point::new(10, 5);
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Right),
            &mut hover,
            &mut cap,
        );
        let menu = res.menu.expect("右键应请求菜单");
        assert_eq!(menu.items.len(), 2, "无选区：复制全部 + 全选");
        assert_eq!(menu.items[0].label, "复制全部");
        assert_eq!(menu.items[1].label, "全选");

        // 菜单项经 SendKey(Ctrl+C) 回投焦点节点 → 剪贴板收到纯文本。
        tree.dispatch_key(
            crate::event::KeyEvent {
                key: Key::Other(0x43),
                pressed: true,
                shift: false,
                ctrl: true,
            },
            Some(node),
        );
        assert_eq!(&*clip.borrow(), "苹果\n释义", "Ctrl+C 应复制全文纯文本");
    }

    /// 测试用剪贴板桩。
    struct TestClip(std::rc::Rc<std::cell::RefCell<String>>);
    impl crate::core::ClipboardProvider for TestClip {
        fn get_text(&self) -> Option<String> {
            Some(self.0.borrow().clone())
        }
        fn set_text(&self, text: &str) {
            *self.0.borrow_mut() = text.to_string();
        }
    }

    fn ctrl_key(vk: u32, shift: bool) -> crate::event::KeyEvent {
        crate::event::KeyEvent {
            key: Key::Other(vk),
            pressed: true,
            shift,
            ctrl: true,
        }
    }

    fn press_at(tree: &mut Tree, kind: PointerKind, x: i32, y: i32) {
        let (mut hover, mut cap) = (None, None);
        // 划选序列需要跨调用保持 capture，故调用方自管 hover/cap 时不用本助手。
        tree.dispatch_pointer(
            PointerEvent::single(kind, crate::geometry::Point::new(x, y), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
    }

    /// 拖选跨越多个碎片，终点落在「词」的右半边 —— 该字整个进选区。
    ///
    /// 终点取 26 而不是 20：选区改成字符级之后，延伸点吸附到**最近的字符边界**，
    /// 20 落在「词」的左半边（18..27 的中点是 22.5），最近边界是它的左缘，于是
    /// 「词」不进选区、复制出来只有「汉字」。这不是回归，是字符级选区该有的样子
    /// ——浏览器里也是拖过字的一半才选中它。
    #[test]
    fn drag_selection_copies_fragment_range() {
        // "汉字词典" 单行 4 片（每片 9px：汉 0..9、字 9..18、词 18..27、典 27..36）。
        let doc = RichDoc::new().para("汉字词典");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));

        let (mut hover, mut cap) = (None, None);
        let pt = |x| crate::geometry::Point::new(x, 7);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, pt(2), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, pt(26), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, pt(26), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(
            &*clip.borrow(),
            "汉字词",
            "拖过「词」的右半边，该字应整个进选区"
        );
    }

    /// 一个 Latin 单词就是一个碎片，选区必须能落在它**内部**。
    ///
    /// 这是选区从碎片级改成字符级的**全部理由**。此前选区要求延伸点与锚点不在同一
    /// 碎片，于是 `standby` 这样的词头、`[ˈstændbaɪ]` 这样的音标怎么拖都选不出东西
    /// ——它们各自只有一个碎片，`i != anchor` 永远不成立。用户看到的是「这行字只能
    /// 双击整选，拖不动」，而中文因为逐字成片碰巧是可拖的，于是这个毛病看起来像
    /// 「英文那块坏了」。
    #[test]
    fn drag_inside_one_latin_word_selects_characters() {
        // Null 引擎每字符 9px（字号 15 × 0.6）：s0 t9 a18 n27 d36 b45 y54，末缘 63。
        let doc = RichDoc::new().para("standby");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        press_at(&mut tree, PointerKind::Down, 0, 7);
        press_at(&mut tree, PointerKind::Move, 45, 7);
        press_at(&mut tree, PointerKind::Up, 45, 7);
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "stand", "单个碎片内也要能拖出选区");
    }

    /// 首尾都落在碎片内部：两端都要按字符切，不能整片取。
    #[test]
    fn drag_selection_trims_both_ends() {
        let doc = RichDoc::new().para("standby");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        press_at(&mut tree, PointerKind::Down, 45, 7);
        press_at(&mut tree, PointerKind::Move, 18, 7);
        press_at(&mut tree, PointerKind::Up, 18, 7);
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        // 反向拖（右→左）同样成立：选区存的是锚点与延伸点，取用时才排序。
        assert_eq!(&*clip.borrow(), "and", "两端都该按字符边界切");
    }

    /// 双击仍然整词选中：字符级不该把「双击选词」也变成半个词。
    #[test]
    fn double_click_still_takes_whole_word() {
        let doc = RichDoc::new().para("standby");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(
            multi_click(crate::geometry::Point::new(30, 7), 2),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "standby", "双击应取整词");
    }

    #[test]
    fn select_all_copy_preserves_spaces_and_paragraph_breaks() {
        let doc = RichDoc::new().para("aa bb").para("汉汉");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        tree.dispatch_key(ctrl_key(0x41, false), Some(node));
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "aa bb\n汉汉", "词间空格保留、段间换行分隔");
    }

    #[test]
    fn soft_wrap_copy_joins_lines_sensibly() {
        // Latin 折行（空格在行尾被丢）：复制时按边界补回空格。
        let doc = RichDoc::new().para("aa bb");
        let (mut tree, node) = build(Element::rich(doc).width(40), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        tree.dispatch_key(ctrl_key(0x41, false), Some(node));
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "aa bb", "Latin 软换行应补空格");

        // CJK 折行：直接续排，不插分隔。
        let doc = RichDoc::new().para("汉汉汉汉");
        let (mut tree, node) = build(Element::rich(doc).width(20), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        tree.dispatch_key(ctrl_key(0x41, false), Some(node));
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "汉汉汉汉", "CJK 软换行不应插分隔符");
    }

    #[test]
    fn click_clears_selection_and_copy_falls_back_to_all() {
        let doc = RichDoc::new().para("汉").para("字");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));

        // 拖选第一段（frag0 → frag0 不算：拖到段内另一点仍是同片会被清，
        // 故拖跨两片再拖回单片场景不在此测；直接 Ctrl+A 后局部收窄不易，
        // 用 Down(片0)+Move(片0 外仍片0)+Up 得单片=清空来验证"原地单击不留选区"）。
        press_at(&mut tree, PointerKind::Down, 4, 7);
        press_at(&mut tree, PointerKind::Up, 4, 7);
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "汉\n字", "原地单击不留选区，复制回退全文");
    }

    /// 测试里手搓选区用：碎片 `a` 的首字符 → 碎片 `b` 的末字符。
    ///
    /// 样例文本都是 CJK（逐字成片），故每个碎片只有一个字符、末边界恒为 1。
    fn cjk_sel(a: usize, b: usize) -> (Caret, Caret) {
        (Caret { frag: a, ch: 0 }, Caret { frag: b, ch: 1 })
    }

    #[test]
    fn relayout_invalidates_selection() {
        let doc = RichDoc::new().para("汉汉汉汉");
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(200, 0), &style, &mut crate::text::NullTextEngine);
        rt.sel.set(Some(cjk_sel(0, 3)));
        // 收窄到自然宽（4×9=36）以下 → 折行点变 → 真重排 → 选区失效。
        rt.measure(Size::new(20, 0), &style, &mut crate::text::NullTextEngine);
        assert!(rt.sel.get().is_none(), "重排后选区应清空");
    }

    #[test]
    fn measure_paint_width_gap_keeps_selection() {
        // 回归：Wrap 宽控件下 measure 收到父给的 avail.w、paint 收到分配到的
        // content.w（= 自然宽），二者天然不等。曾按"宽度必须相等"判缓存，于是
        // 每帧交替重排、顺手清空选区——而宿主对 Down/Up/按键都置 needs_relayout，
        // 结果 Ctrl+A 全选与拖选松手后的高亮永远等不到下一帧（对话框内必现）。
        let doc = RichDoc::new().para("汉汉汉汉");
        let rt = RichText::new(doc);
        let style = Style::default();
        // measure：父给 500 可用宽。
        let sz = rt.measure(Size::new(500, 0), &style, &mut crate::text::NullTextEngine);
        assert!(sz.w < 500, "自然宽应小于可用宽，否则测不到本回归");
        rt.sel.set(Some(cjk_sel(0, 3)));
        // paint：以自然宽（节点实际分配宽）再确保一次布局。
        {
            let mut m = EngineMeasurer(&mut crate::text::NullTextEngine);
            rt.ensure_layout(Some(sz.w), &style, &mut m);
        }
        assert_eq!(
            rt.sel.get(),
            Some(cjk_sel(0, 3)),
            "paint 的 content.w 不应清掉选区"
        );
        // 下一帧 measure 又回到 avail.w：同样不得重排。
        rt.measure(Size::new(500, 0), &style, &mut crate::text::NullTextEngine);
        assert_eq!(
            rt.sel.get(),
            Some(cjk_sel(0, 3)),
            "回到 avail.w 也不应清掉选区"
        );
    }

    #[test]
    fn right_click_with_selection_offers_copy_selection() {
        let doc = RichDoc::new().para("汉字词典");
        let (mut tree, _node) = build(Element::rich(doc).width(200), 300, 300);
        let (mut hover, mut cap) = (None, None);
        let pt = |x| crate::geometry::Point::new(x, 7);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, pt(2), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, pt(20), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, pt(20), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, pt(10), MouseButton::Right),
            &mut hover,
            &mut cap,
        );
        let menu = res.menu.expect("右键应请求菜单");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["复制", "复制全部", "全选"],
            "有选区时应提供三项且右键不清选区"
        );
    }

    #[test]
    fn selection_survives_host_relayout_on_wrap_width() {
        // 端到端回归（对话框内 Wrap 宽 RichText 的必现场景）：宿主对按键与
        // Down/Up 都置 `needs_relayout`（src/app/mod.rs），下一帧 layout_root
        // 以 avail.w 再 measure 一次。曾因 measure/paint 宽度不等而每帧重排，
        // Ctrl+A 全选的选区在同一帧就被清掉——高亮永远不出现。
        // 注意本例刻意不设 width：显式宽下 avail.w == content.w，测不到本回归。
        let doc = RichDoc::new().para("汉字词典");
        let (mut tree, node) = build(Element::rich(doc), 300, 300);
        tree.dispatch_key(ctrl_key(0x41, false), Some(node));
        // 宿主本帧重排。
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        // 菜单项数随选区态变化，借它观测选区是否还在。
        let (mut hover, mut cap) = (None, None);
        let res = tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Down,
                crate::geometry::Point::new(10, 7),
                MouseButton::Right,
            ),
            &mut hover,
            &mut cap,
        );
        let menu = res.menu.expect("右键应请求菜单");
        let labels: Vec<&str> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["复制", "复制全部", "全选"],
            "全选后经宿主重排，选区应存活"
        );
    }

    #[test]
    fn clamp_truncates_with_expander_and_click_expands() {
        // 宽 60、每字 9px：一行 6 字。"汉"×20 全文 4 行（56px）；clamp(2) 截为 2 行，
        // 末行腾位后缀「… 展开」（4 字符 → 34px）。
        let expanded = signal(false);
        let doc = RichDoc::new().para(Para::new().text("汉".repeat(20)).clamp(2, expanded));
        let (mut tree, node) = build(Element::rich(doc).width(60), 300, 300);
        assert_eq!(node_h(&tree, node), 28, "clamp(2) 应只排两行");
        // 点击「… 展开」标记（次行 x≈18..52、y∈[14,28)）。
        press_at(&mut tree, PointerKind::Down, 30, 20);
        press_at(&mut tree, PointerKind::Up, 30, 20);
        assert!(expanded.get(), "点击展开标记应置信号为 true");
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(node_h(&tree, node), 56, "展开后应排出全文四行");
    }

    #[test]
    fn clamp_expander_excluded_from_copy() {
        let expanded = signal(false);
        let doc = RichDoc::new().para(Para::new().text("汉".repeat(20)).clamp(2, expanded));
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(60, 0), &style, &mut crate::text::NullTextEngine);
        let n = rt.cache.borrow().as_ref().unwrap().frags.len();
        rt.sel.set(rt.whole_frags(0, n - 1));
        let text = rt.selected_text().unwrap();
        assert!(!text.contains("展开"), "「… 展开」标记不应进入复制文本");
        assert!(text.starts_with("汉"), "选区应为正文内容");
    }

    #[test]
    fn dpi_scale_change_invalidates_layout() {
        // 自定义 Measurer 模拟 DPI 变化（Null 引擎恒 1.0，无法直接驱动）。
        struct ScaledNull(f32);
        impl Measurer for ScaledNull {
            fn size(&mut self, text: &str, ts: &TextStyle) -> Size {
                crate::text::NullTextEngine.measure(text, ts, None)
            }
            fn metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
                crate::text::NullTextEngine.line_metrics(text, ts)
            }
            fn scale(&self) -> f32 {
                self.0
            }
        }
        let rt = RichText::new(RichDoc::new().para("汉汉"));
        let style = Style::default();
        rt.ensure_layout(Some(200), &style, &mut ScaledNull(1.0));
        rt.sel.set(Some(cjk_sel(0, 1)));
        // 同宽同字体、仅 scale 变 → 必须 miss 重排（测量物理取整随 DPI 而变）。
        rt.ensure_layout(Some(200), &style, &mut ScaledNull(1.5));
        assert!(
            rt.sel.get().is_none(),
            "DPI 变化应使布局缓存失效（选区被清即证明重排）"
        );
    }

    #[test]
    fn rich_signal_swaps_document_on_signal_change() {
        let doc_sig = signal(RichDoc::new().para("一"));
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::rich_signal(doc_sig).width(200))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        let node = tree.get(root).unwrap().children[0];
        assert_eq!(node_h(&tree, node), 14, "初始单段一行");

        doc_sig.set(RichDoc::new().para("一").para("二").para("三"));
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(
            node_h(&tree, node),
            14 + 6 + 14 + 6 + 14,
            "信号换文档后应按新文档重排（三段 + 两处段距）"
        );
    }

    #[test]
    fn section_header_clamp_signal_enters_snapshot() {
        // 头部段落设 clamp：其信号必须进布局快照，否则展开后缓存恒 stale。
        let exp = signal(false);
        let doc = RichDoc::new().section(
            Para::new().text("很长的头部".repeat(10)).clamp(1, exp),
            signal(false),
            |d| d.para("体"),
        );
        let mut snap = Vec::new();
        collect_collapsed(&doc.blocks, &mut snap);
        assert_eq!(snap.len(), 2, "快照应含头部 clamp + 折叠态两项");
        assert!(collapsed_matches(&doc.blocks, &snap), "快照应自洽");
        exp.set(true);
        assert!(
            !collapsed_matches(&doc.blocks, &snap),
            "头部 clamp 信号翻转后旧快照必须失配（触发重排）"
        );
    }

    #[test]
    fn rich_signal_doc_swap_resets_hover_state() {
        // 切词条时鼠标常停在被点 span 上：换文档必须复位悬停态，否则新文档
        // 同下标碎片会被幽灵提亮/显示手型。
        let doc_sig = signal(RichDoc::new().para(Para::new().span_id(
            "x",
            "链接",
            SpanStyle::new().underline(),
        )));
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::rich_signal(doc_sig).width(200))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        let node = tree.get(root).unwrap().children[0];

        // 悬停到可点击 span 上 → 手型。
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                crate::geometry::Point::new(5, 7),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        assert_eq!(tree.cursor_at(node), CursorShape::Hand, "悬停 span 应手型");

        // 换文档（新文档无可点击 span）→ 悬停态应被复位，不再手型。
        doc_sig.set(RichDoc::new().para("纯文本"));
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(
            tree.cursor_at(node),
            CursorShape::Arrow,
            "换文档后旧悬停下标应复位（无幽灵手型）"
        );
    }

    /// 构造带连击计数的按下事件。
    /// 真跑一帧绘制。
    ///
    /// 选择域的几何是在 `paint` 里登记的，不画一帧就一个成员也没有——这与真实运行
    /// 一致：控件没画出来之前，它在屏幕上没有位置可言。
    fn paint_once(tree: &mut Tree, w: i32, h: i32) {
        let mut pm = tiny_skia::Pixmap::new(w as u32, h as u32).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text_offset(
            &mut pm,
            &mut eng,
            1.0,
            crate::geometry::Point::new(0, 0),
        );
        tree.paint(&mut canvas);
    }

    /// 一次完整拖拽，**全程保持指针捕获**。
    ///
    /// 跨控件划选全靠这一点：Down 时控件调 `ctx.capture()`，之后即便指针移到别的控件
    /// 上，事件仍送回它手里（`Tree::dispatch_pointer` 里 `capture.or_else(hit_test)`）。
    /// 测试若像 `press_at` 那样每次新建一个空的 capture 槽，Move 就改走命中测试落到
    /// 别人身上，跨控件那条路根本走不到。
    fn drag(tree: &mut Tree, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (mut hover, mut cap) = (None, None);
        for (kind, x, y) in [
            (PointerKind::Down, x0, y0),
            (PointerKind::Move, x1, y1),
            (PointerKind::Up, x1, y1),
        ] {
            tree.dispatch_pointer(
                PointerEvent::single(kind, crate::geometry::Point::new(x, y), MouseButton::Left),
                &mut hover,
                &mut cap,
            );
        }
    }

    /// 两段文字挂同一个选择域，各占一行（每行 14px 高、每字 9px 宽）。
    fn two_member_scope() -> (
        Tree,
        crate::core::NodeId,
        SelectionScope,
        Rc<RefCell<String>>,
    ) {
        let scope = SelectionScope::new();
        let a = Element::rich(RichDoc::new().para("汉字"))
            .width(200)
            .selection_scope(scope.clone());
        let b = Element::rich(RichDoc::new().para("词典"))
            .width(200)
            .selection_scope(scope.clone());
        let mut tree = Tree::new();
        let root = Element::col().child(a).child(b).build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        paint_once(&mut tree, 300, 300);
        let first = tree.get(root).unwrap().children[0];
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        (tree, first, scope, clip)
    }

    /// 选择域的正题：拖拽从一个控件延伸到另一个，复制拿到两段。
    ///
    /// 一个 `RichText` 本身是一个独立的选择域，而一屏内容常被拆成好几个控件（词典条目
    /// 里词头一个、音标一个、每段释义一个）。没有域的话，用户拖过词头再往下拖，释义那
    /// 段不会被选中——他得一段一段复制。
    #[test]
    fn scope_drag_spans_two_widgets() {
        let (mut tree, first, _scope, clip) = two_member_scope();
        // 从第一段的「汉」起，拖到第二段末尾之外（吸附到末字符边界）。
        drag(&mut tree, 0, 7, 60, 21);
        tree.dispatch_key(ctrl_key(0x43, false), Some(first));
        assert_eq!(
            &*clip.borrow(),
            "汉字\n词典",
            "选区应跨过控件边界，成员之间补换行"
        );
    }

    /// 域里的 Ctrl+A 是「这一屏」，不是「我碰巧聚焦的这一段」。
    #[test]
    fn scope_select_all_covers_every_member() {
        let (mut tree, first, _scope, clip) = two_member_scope();
        tree.dispatch_key(ctrl_key(0x41, false), Some(first));
        tree.dispatch_key(ctrl_key(0x43, false), Some(first));
        assert_eq!(&*clip.borrow(), "汉字\n词典", "全选应覆盖域里每个成员");
    }

    /// 在域里单击一下就清掉选区——包括落在**别的**成员身上的那部分高亮。
    #[test]
    fn scope_click_clears_selection_everywhere() {
        let (mut tree, first, scope, _clip) = two_member_scope();
        tree.dispatch_key(ctrl_key(0x41, false), Some(first));
        assert!(scope.has_sel(), "全选之后该有选区");
        press_at(&mut tree, PointerKind::Down, 0, 7);
        press_at(&mut tree, PointerKind::Up, 0, 7);
        assert!(!scope.has_sel(), "原地单击应清掉整个域的选区");
    }

    /// 双击选词也要记进域里，否则 Ctrl+C 拿不到刚选中的那个词。
    #[test]
    fn scope_double_click_lands_in_scope() {
        let (mut tree, first, scope, clip) = two_member_scope();
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(
            multi_click(crate::geometry::Point::new(4, 7), 2),
            &mut hover,
            &mut cap,
        );
        assert!(scope.has_sel(), "双击的结果应落在域里");
        tree.dispatch_key(ctrl_key(0x43, false), Some(first));
        assert_eq!(
            &*clip.borrow(),
            "汉字",
            "双击取连续汉字串（至标点/空白止），不是单字"
        );
    }

    fn multi_click(pos: crate::geometry::Point, count: u8) -> PointerEvent {
        PointerEvent {
            kind: PointerKind::Down,
            pos,
            button: MouseButton::Left,
            click_count: count,
        }
    }

    #[test]
    fn press_without_drag_selects_nothing() {
        // 左键按下（未拖动）不得产生选区——按下即选中单字不符合通用手感。
        let doc = RichDoc::new().para("苹果").para("第二段");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Down,
                crate::geometry::Point::new(4, 7),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        // 按住未拖：Ctrl+C 应复制全文（无选区），而非按下处的单字。
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "苹果\n第二段", "按下未拖动不应产生选区");
    }

    #[test]
    fn drag_back_to_anchor_clears_selection() {
        let doc = RichDoc::new().para("苹果很甜");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        let (mut hover, mut cap) = (None, None);
        let pt = |x| crate::geometry::Point::new(x, 7);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, pt(4), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, pt(20), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        // 拖回锚点碎片：选区应消失。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, pt(4), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, pt(4), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(
            &*clip.borrow(),
            "苹果很甜",
            "拖回锚点后无选区，复制回退全文"
        );
    }

    #[test]
    fn double_click_selects_cjk_word_run() {
        // "苹果，很甜"：苹果 | ， | 很甜 —— 双击"苹"应选到"，"为止的连续汉字。
        let doc = RichDoc::new().para("苹果，很甜");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));

        let (mut hover, mut cap) = (None, None);
        let at = crate::geometry::Point::new(5, 7);
        // 双击 = 单击 Down/Up 后再来一发 count=2 的 Down。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(multi_click(at, 2), &mut hover, &mut cap);
        // Up 不应清掉双击选区。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "苹果", "双击应选中标点前的连续汉字串");
    }

    #[test]
    fn double_click_on_latin_selects_single_word() {
        let doc = RichDoc::new().para("hello world");
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        let (mut hover, mut cap) = (None, None);
        // "hello" 宽 5×8.4→42px，点 (10,7) 落在词内。
        tree.dispatch_pointer(
            multi_click(crate::geometry::Point::new(10, 7), 2),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(&*clip.borrow(), "hello", "双击 Latin 词应只选该词");
    }

    #[test]
    fn triple_click_selects_whole_paragraph() {
        // 首段 8 字在 40px 宽折成两行（软换行）；三击应选**整段**（跨软换行、
        // 不跨段）——与浏览器三击选段落的习惯一致。
        let doc = RichDoc::new().para("苹果很甜苹果很甜").para("第二段");
        let (mut tree, node) = build(Element::rich(doc).width(40), 300, 300);
        let clip = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        tree.clipboard = Some(Box::new(TestClip(clip.clone())));
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(
            multi_click(crate::geometry::Point::new(5, 7), 3),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(ctrl_key(0x43, false), Some(node));
        assert_eq!(
            &*clip.borrow(),
            "苹果很甜苹果很甜",
            "三击应选整段（含软换行续行），不含第二段"
        );
    }

    /// 反向：纯静态文本经 `.focusable(true)` **进得了**焦点环。
    ///
    /// 这条不是为对称而写。`RichText::focusable()` 默认只认「含可折叠 Section」，于是
    /// 一整篇没有 Section 的正文拿不到焦点——而它的 `on_event` 里 Ctrl+C / Ctrl+A 是
    /// 齐全的，键盘事件只发给焦点节点，那段代码就永远跑不到：鼠标划得动、右键菜单复制
    /// 得了，独独 Ctrl+C 没反应。词典类应用（wind-dict）正是靠这个覆盖把复制快捷键接
    /// 起来的，故它是**被依赖的契约**，不能随默认值一起改掉。
    #[test]
    fn focusable_override_adds_static_text_to_tab_order() {
        let doc = RichDoc::new().para("一段没有可折叠区的正文");
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::rich(doc).focusable(true))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert_eq!(
            tree.focusable_order().len(),
            1,
            ".focusable(true) 应让纯静态富文本进入 Tab 焦点环，否则 Ctrl+C 无处可去"
        );
    }

    #[test]
    fn focusable_override_removes_from_tab_order() {
        let doc = RichDoc::new().section("头", signal(false), |d| d.para("体"));
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::rich(doc).focusable(false))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 300), &mut crate::text::NullTextEngine);
        assert!(
            tree.focusable_order().is_empty(),
            ".focusable(false) 应使含 Section 的富文本退出 Tab 焦点环"
        );
    }

    #[test]
    fn enter_key_toggles_focused_section() {
        let collapsed = signal(false);
        let doc = RichDoc::new().section("例句", collapsed, |d| d.para("第一句"));
        let (mut tree, node) = build(Element::rich(doc).width(200), 300, 300);
        tree.dispatch_key(
            crate::event::KeyEvent {
                key: Key::Enter,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(node),
        );
        assert!(collapsed.get(), "Enter 应翻转聚焦的折叠头");
    }

    #[test]
    fn focusable_only_with_sections() {
        let plain = RichText::new(RichDoc::new().para("纯文本"));
        assert!(!plain.focusable(), "无 Section 的富文本不应占 Tab 焦点位");
        let with = RichText::new(RichDoc::new().section("头", signal(false), |d| d.para("体")));
        assert!(with.focusable(), "含 Section 的富文本应可聚焦");
    }

    #[test]
    fn collapsed_section_children_produce_no_frags() {
        let collapsed = signal(true);
        let doc = RichDoc::new().section("头", collapsed, |d| d.para("隐藏内容"));
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(300, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        // 只有箭头 + 头文字两个碎片；子内容不产出。
        assert_eq!(frags.len(), 2, "折叠区子内容不应产出碎片");
    }

    #[test]
    fn named_style_resolves_and_inline_overrides() {
        let doc = RichDoc::new()
            .style("big", SpanStyle::new().size(20.0).bold())
            .para(Para::new().styled_span("big", "x", SpanStyle::new().size(30.0)));
        let rt = RichText::new(doc);
        let style = Style::default();
        rt.measure(Size::new(300, 0), &style, &mut crate::text::NullTextEngine);
        let cache = rt.cache.borrow();
        let f = &cache.as_ref().unwrap().frags[0];
        assert_eq!(f.style.size, 30.0, "内联字号应覆盖命名样式");
        assert_eq!(f.style.weight, 700, "未覆盖字段继承命名样式");
    }

    #[test]
    fn spaces_do_not_trigger_wrap_and_drop_at_line_start() {
        // "aa bb"（词 17px+空 9? — 空格 1 字 → ceil(0.6*14)=9；aa=17,bb=17）宽 40：
        // aa(17)+空(9)=26，bb 需 26+17=43>40 → 换行，bb 行首无空格。
        let doc = RichDoc::new().para("aa bb");
        let rt = RichText::new(doc);
        let style = Style::default();
        let sz = rt.measure(Size::new(40, 0), &style, &mut crate::text::NullTextEngine);
        assert_eq!(sz.h, 28, "应折成两行");
        let cache = rt.cache.borrow();
        let frags = &cache.as_ref().unwrap().frags;
        assert_eq!(frags.len(), 2, "空白碎片不应产出");
        assert_eq!(frags[1].rect.x, 0, "第二行行首不应残留空格缩进");
    }
}
