//! 示例共用外壳：无边框窗口的自绘标题栏。
//!
//! 放在**子目录**里是刻意的：cargo 只把 `examples/*.rs` 与 `examples/*/main.rs` 当示例，
//! 这个 `common/mod.rs` 两条都不沾，故不会被当成一个缺 `main` 的示例去编译。
//! 示例侧这样接入（examples 之间不能互相 `use`，只能按路径挂模块）：
//!
//! ```ignore
//! #[path = "common/mod.rs"]
//! mod common;
//! use common::Shell;
//! ```
//!
//! 为什么要抽出来：主要示例统一走 `App::frameless()` + 自绘标题栏，README 里并排摆着，
//! 标题栏高度、三段式标题的字重分层、logo 尺寸只要有一处不一致就一眼可见。参数化一份
//! 比复制七份更禁得起后续调整。

#![allow(dead_code)]

use windui::prelude::*;

/// 标题栏高度（逻辑像素）。窗口按钮的命中区按它撑满，故改这里即可整体调整。
///
/// 38 而非 Windows 系统标题栏那个 32：这一条里还要放下 20px 的 logo，32 会让它上下
/// 只剩 6px、挤得贴边。38 与下游 wind-dict 同值，按钮成 46×38——仍是横向舒展的扁矩形，
/// 与系统的 46×32 同一个观感。**曾经是 44**，那让按钮接近正方形，一眼就不像 Windows。
pub const TITLEBAR_H: i32 = 38;

/// 示例窗口的外壳配置。
///
/// 默认三个窗口按钮齐全；卡片式小窗（`about`）那类不该最大化的，用
/// [`no_maximize`](Shell::no_maximize) 去掉中间那个。
///
/// `Element` 不是 `Clone`，故 [`titlebar`](Shell::titlebar) / [`wrap`](Shell::wrap)
/// 都**消费** `self`——一个 Shell 只出一个标题栏，这与实际用法一致。
pub struct Shell {
    subtitle: String,
    maximize: bool,
    trailing: Vec<Element>,
}

impl Shell {
    /// `subtitle` 是三段式标题的末段：标题栏显示为「windui · {subtitle}」。
    pub fn new(subtitle: impl Into<String>) -> Self {
        Self {
            subtitle: subtitle.into(),
            maximize: true,
            trailing: Vec::new(),
        }
    }

    /// 去掉最大化按钮（固定尺寸的卡片窗、向导窗用）。
    pub fn no_maximize(mut self) -> Self {
        self.maximize = false;
        self
    }

    /// 在窗口按钮**左侧**插入自定义元素（明暗切换、页内动作等）。
    ///
    /// 元素会被标题栏的 `cross(Align::Stretch)` **拉到整条满高**，与窗口按钮连成一排，
    /// 中间不留缝——这正是全包式想要的样子。所以放进来的东西得经得起拉伸：`clickable()`
    /// 容器（方角、hover 铺满）合适，`icon_button` 那类自带圆角底的**不合适**，会变成
    /// 一根竖着的圆角条杵在方角按钮旁边。要保持自然高度，自己包一层
    /// `Element::row().cross(Align::Center)`。现成的例子见 [`theme_toggle`]。
    ///
    /// 注意它落在 `window_drag()` 区域内：可聚焦的控件（按钮等）会自行吃掉按下事件、
    /// 不触发拖窗；纯 `Label` 则会连同标题栏一起被拖走——需要点击的请用 `clickable()`
    /// 容器或正经控件包一层。
    pub fn trailing(mut self, e: Element) -> Self {
        self.trailing.push(e);
        self
    }

    /// 只构建标题栏本身。需要把它塞进更复杂的层叠结构时用；
    /// 常规情况直接用 [`wrap`](Shell::wrap)。
    ///
    /// 窗口按钮走**全包式**（同 `frameless` / `light_titlebar` 两个示例，以及下游
    /// wind-dict）：按钮吃满整条标题栏的高度，最右那枚关闭键与窗口右边齐平。两处
    /// 都是布局说了算，不用改控件：
    ///
    /// - `cross(Align::Stretch)` 覆盖掉 `WindowButton::measure` 报的 `BTN_H = 32`，
    ///   按钮高度变成 `TITLEBAR_H`。此前是 `Align::Center`，按钮只有 32 高、浮在
    ///   44 高的条中间，hover 底色是一枚悬空的小方块。
    /// - 内边距挂在**左侧品牌区**这个子容器上，不挂在整条 row 上。挂 row 上是四周
    ///   一起缩，右侧按钮会被推离窗口边 14px，圆角处就空出一块底色。
    ///
    /// 关闭键的 hover 红底照旧按方角 `fill_rect` 画：窗口在 Win11 上显式声明了
    /// `DWMWCP_ROUND`（见 `platform::win32`），合成期由 DWM 裁角，落到屏上自然是
    /// 圆角。自己再画一遍圆角反而会与系统的半径对不齐。
    pub fn titlebar(self) -> Element {
        let mut bar = Element::row()
            .width_match()
            .height(TITLEBAR_H)
            .cross(Align::Stretch)
            .bg_role(Role::SurfaceAlt)
            .window_drag()
            // 品牌区：logo + 三段式标题。自带内边距与间距，见上面那段。
            // 里层重新 `cross(Center)`，免得 Stretch 把 logo 与文字拉变形。
            .child(
                Element::row()
                    .cross(Align::Center)
                    .padding_xy(14, 0)
                    .spacing(10)
                    .child(brand_logo(20))
                    // 三段式标题：产品名加粗、间隔点与副标题弱化，形成层级而不是一行同权重的字。
                    .child(
                        Element::row()
                            .cross(Align::Center)
                            .spacing(5)
                            .child(
                                Element::label("windui")
                                    .font_size(13.0)
                                    .font_weight(600)
                                    .fg_role(Role::Text),
                            )
                            .child(
                                Element::label("·")
                                    .font_size(13.0)
                                    .fg_role(Role::TextDisabled),
                            )
                            .child(
                                Element::label(self.subtitle)
                                    .font_size(13.0)
                                    .fg_role(Role::TextMuted),
                            ),
                    ),
            )
            .child(Element::leaf().weight(1.0));

        // 自定义元素直接进 row，与窗口按钮一样吃满高度、彼此贴死，连成一排。
        //
        // 曾经包了一层 `cross(Center)` 再留 8px 右缝，那是为了迁就 `icon_button` 的
        // 圆角底——结果是一枚浮在条中间的小圆角块紧挨着三枚方角满高按钮，两种视觉
        // 语言并排。与其让标题栏迁就控件，不如让控件长成标题栏该有的样子，见
        // [`theme_toggle`]；`trailing` 的文档里写明了这条约束。
        for e in self.trailing {
            bar = bar.child(e);
        }

        // 图标色走 `Role::Text`：标题栏底是 `SurfaceAlt`，暗色主题下翻深，
        // 写死深灰的话按钮会整片消失。
        bar = bar.child(Element::window_button(WindowButtonKind::Minimize).fg_role(Role::Text));
        if self.maximize {
            bar = bar.child(Element::window_button(WindowButtonKind::Maximize).fg_role(Role::Text));
        }
        bar.child(Element::window_button(WindowButtonKind::Close).fg_role(Role::Text))
    }

    /// 标题栏 + 分隔线 + 正文，构成整窗内容。
    pub fn wrap(self, body: Element) -> Element {
        Element::col()
            .fill()
            .bg_role(Role::Bg)
            .child(self.titlebar())
            .child(Element::divider())
            .child(body.weight(1.0))
    }
}

/// 内置品牌图标做成可绘制的图片元素。
///
/// 按 2 倍边长光栅化再缩到 `size`：`brand_icon_at` 是"要多大画多大"的生成器，
/// 但这里的目标框是**逻辑**像素，HiDPI 下实绘会更大——按 1 倍光栅会在 150%/200%
/// 上糊掉。2 倍是覆盖到 200% 的最小代价。
pub fn brand_logo(size: i32) -> Element {
    let px = (size.max(1) as u32) * 2;
    let icon = brand_icon_at(px);
    Element::image_rgba(icon.width(), icon.height(), icon.rgba())
        .fit(Fit::Contain)
        .size(size, size)
}

/// 页面主标题 + 副标题的成对表头。示例内容区顶部统一用它，省得每个示例各调一套字号。
pub fn page_title(title: &str, subtitle: &str) -> Element {
    Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(10)
        .child(
            Element::label(title)
                .font_size(20.0)
                .font_weight(700)
                .fg_role(Role::Text),
        )
        .child(
            Element::label(subtitle)
                .font_size(12.5)
                .fg_role(Role::TextMuted)
                .weight(1.0),
        )
}

/// 左侧竖色条 + 标题的小节头。卡片内部分区用。
pub fn section_title(title: &str) -> Element {
    Element::row()
        .cross(Align::Center)
        .spacing(10)
        .child(
            Element::leaf()
                .size(4, 18)
                .corner(2.0)
                .bg_role(Role::Accent),
        )
        .child(
            Element::label(title)
                .font_size(15.0)
                .font_weight(700)
                .fg_role(Role::Text),
        )
}

/// 标题栏上的明暗切换按钮。
///
/// 主题不是"再建一棵树"，而是 `ThemeHandle::set` 整树热切换：用 `*_role` 表达的颜色
/// 自动跟随，写死的 `Color::hex` 不跟随——这正是示例统一走 `Role` 的理由。
///
/// **为什么是 `stack().clickable()` 而不是 `icon_button`**：它要和右边三枚窗口按钮
/// 排成一列同类物，`icon_button` 三条都对不上——
/// - 圆角：`IconButton` 的半径是 `if corner_radius > 0 { 它 } else { theme.corner_sm }`，
///   即 **`.corner(0.0)` 拿不到方角**，只会回落到主题圆角。`Clickable` 直接用
///   `style.corner_radius`，默认 0，正是要的方角。
/// - 尺寸：`IconButton::measure` 报的是字号推出来的方块（下限 30×30），与 46×TITLEBAR_H
///   对不上；这里直接钉死 46 宽、`height_match()` 吃满高。
/// - 图标色：窗口按钮在 [`Shell::titlebar`] 里走 `Role::Text`，此处**曾是 `TextMuted`**，
///   天生比邻居淡一档——并排看就是「这枚是不是灰掉了」。
///
/// 仍有一处对不齐：hover 底色。`Clickable` 用 `palette.text × 0.06`，而 `WindowButton`
/// 用的是据标题栏底亮度选的写死值（亮底黑 `0x14`、暗底白 `0x20`）。方向一致，暗色主题下
/// 前者约淡一半。要精确对齐得改库（把那段叠层逻辑变成可复用的东西），不在示例里绕。
pub fn theme_toggle(th: ThemeHandle, dark: Signal<bool>) -> Element {
    Element::stack()
        .width(46)
        .height_match()
        .clickable()
        .tooltip("切换明暗主题")
        .on_click(move |_| {
            let next = !dark.get();
            dark.set(next);
            th.set(if next {
                Theme::dark()
            } else {
                Theme::default()
            });
        })
        .child(
            // `align` 是**自身**在父容器里的对齐（`Node::align` 的定义），不是容器摆
            // 子元素的方式——挂在 stack 上等于说「这个 stack 在标题栏里居中」，对图标
            // 毫无作用，实测字形贴在格子左上角（偏 dx=-16.5, dy=-10）。要居中就挂在
            // 图标自己身上：Frame 里一个 `align` 同时管两轴。
            Element::label("◐")
                .font_size(14.0)
                .fg_role(Role::Text)
                .align(Align::Center),
        )
}
