//! 目标场景示例：一个"输入法设置"形状的界面，集中演示新控件如何组合成
//! 主从布局的设置页（侧栏导航 + 内容区分组）。
//!
//! 运行： cargo run --release --example ime_settings
//! 截屏： cargo run --example ime_settings -- --screenshot temp/ime.png
//!
//! 用到的控件：SegmentedControl（简/繁等二三选一）、Switch（开关项）、
//! Collapsible（侧栏可折叠分组）、list（侧栏选中高亮）、NavRow（钻入子页 >）。
//! `setting_row` / `section_header` 是本示例的局部便捷器（纯组合，不入库）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

const BG: u32 = 0xEEF1F5;
const SIDEBAR: u32 = 0xF7F8FA;
const CARD: u32 = 0xFFFFFF;
const FG: u32 = 0x2D3436;
const SUB: u32 = 0x8A9099;
const HEAD: u32 = 0x9AA0A6;

/// 分组小标题（灰色小号，左对齐）。
fn section_header(text: &str) -> Element {
    Element::label(text).font_size(12.0).fg(Color::hex(HEAD)).height(22).width_match()
}

/// 一行设置项：左标签 + 弹性留白 + 右侧控件（右对齐）。`indent` 为左缩进（子项用）。
fn setting_row(label: &str, indent: i32, control: Element) -> Element {
    Element::row()
        .width_match()
        .height(44)
        .cross(Align::Center)
        .child(Element::label(label).font_size(14.0).fg(Color::hex(FG)).width(180 - indent).margin_xy(indent / 2, 0))
        .child(Element::label("").weight(1.0))
        .child(control)
}

fn main() {
    // —— 状态 ——
    let nav_sel = Rc::new(Cell::new(0usize)); // 侧栏选中项（常用）
    let attr_expand = Rc::new(Cell::new(true)); // 侧栏"属性设置"展开
    let zh_form = Rc::new(Cell::new(0usize)); // 简体/繁体
    let width_mode = Rc::new(Cell::new(0usize)); // 半角/全角
    let cn_en = Rc::new(Cell::new(0usize)); // 中文/英文
    let pinyin = Rc::new(Cell::new(0usize)); // 全拼/双拼/笔画
    let hide_bar = Rc::new(Cell::new(false));
    let fullscreen_hide = Rc::new(Cell::new(true));
    let fuzzy = Rc::new(Cell::new(true));
    let status = Rc::new(RefCell::new(String::from("提示：点击带 > 的行可钻入子页")));

    // —— 侧栏：可折叠分组 + 选中高亮列表 ——
    let sidebar = Element::col()
        .width(170)
        .height_match()
        .bg(Color::hex(SIDEBAR))
        .padding(10)
        .spacing(4)
        .child(Element::label("输入法设置").font_size(15.0).fg(Color::hex(FG)).height(34).width_match())
        .child(Element::divider())
        .child(Element::collapsible(
            "属性设置",
            attr_expand.clone(),
            Element::list(
                vec!["常用", "外观", "词库", "账户", "按键", "高级"],
                nav_sel.clone(),
            )
            .width_match()
            .height(6 * 36),
        ));

    // —— 内容区：分组设置项 ——
    let (s1, s2, s3) = (status.clone(), status.clone(), status.clone());
    let content = Element::scroll().fill().weight(1.0).child(
        Element::col()
            .width_match()
            .padding(22)
            .spacing(6)
            .child(Element::label("常用").font_size(20.0).fg(Color::hex(FG)).height(34).width_match())
            .child(section_header("默认状态"))
            .child(setting_row("简体 / 繁体", 0, Element::segmented(vec!["简体", "繁体"], zh_form.clone())))
            .child(setting_row("半角 / 全角", 0, Element::segmented(vec!["半角", "全角"], width_mode.clone())))
            .child(setting_row("中文 / 英文", 0, Element::segmented(vec!["中文", "英文"], cn_en.clone())))
            .child(setting_row("隐藏状态栏", 0, Element::switch(hide_bar.clone())))
            .child(setting_row("显示输入指示器", 24, Element::switch(Rc::new(Cell::new(false))).disabled(true)))
            .child(setting_row("全屏隐藏状态栏", 0, Element::switch(fullscreen_hide.clone())))
            .child(Element::divider())
            .child(section_header("输入习惯"))
            .child(setting_row("输入方案", 0, Element::segmented(vec!["全拼", "双拼", "笔画"], pinyin.clone())))
            .child(Element::nav_row("双拼方案设定").on_click(move |_| *s1.borrow_mut() = "已进入：双拼方案设定".into()))
            .child(setting_row("拼音纠错", 0, Element::switch(fuzzy.clone())))
            .child(Element::nav_row("拼音纠错设置").on_click(move |_| *s2.borrow_mut() = "已进入：拼音纠错设置".into()))
            .child(Element::nav_row("模糊音设置").on_click(move |_| *s3.borrow_mut() = "已进入：模糊音设置".into()))
            .child(Element::divider())
            .child(Element::label_rc(status.clone()).font_size(13.0).fg(Color::hex(SUB)).height(20).width_match()),
    );

    let ui = Element::row()
        .fill()
        .bg(Color::hex(CARD))
        .child(sidebar)
        .child(content);

    App::new("输入法设置 — windui 示例", 720, 520)
        .bg(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
