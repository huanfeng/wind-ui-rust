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
pub const TITLEBAR_H: i32 = 44;

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
    /// 注意它落在 `window_drag()` 区域内：可聚焦的控件（按钮等）会自行吃掉按下事件、
    /// 不触发拖窗；纯 `Label` 则会连同标题栏一起被拖走——需要点击的请用 `clickable()`
    /// 容器或正经控件包一层。
    pub fn trailing(mut self, e: Element) -> Self {
        self.trailing.push(e);
        self
    }

    /// 只构建标题栏本身。需要把它塞进更复杂的层叠结构时用；
    /// 常规情况直接用 [`wrap`](Shell::wrap)。
    pub fn titlebar(self) -> Element {
        let mut bar = Element::row()
            .width_match()
            .height(TITLEBAR_H)
            .cross(Align::Center)
            .padding_xy(14, 0)
            .spacing(10)
            .bg_role(Role::SurfaceAlt)
            .window_drag()
            .child(brand_logo(22))
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
            )
            .child(Element::leaf().weight(1.0));

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
pub fn theme_toggle(th: ThemeHandle, dark: Signal<bool>) -> Element {
    Element::icon_button("◐")
        .tooltip("切换明暗主题")
        .fg_role(Role::TextMuted)
        .on_click(move |_| {
            let next = !dark.get();
            dark.set(next);
            th.set(if next {
                Theme::dark()
            } else {
                Theme::default()
            });
        })
}
