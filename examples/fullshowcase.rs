//! 综合示例：一个"偏好设置"小工具，集中展示 windui 全部控件。
//!
//! 运行：    cargo run --release --example fullshowcase
//! 截屏：    cargo run --example fullshowcase -- --screenshot artifacts/showcase.png
//! 对话框：  cargo run --example fullshowcase -- --dialog --screenshot artifacts/showcase_dialog.png

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;
const CARD: u32 = 0xFFFFFF;
const BG: u32 = 0xEEF1F5;

/// 生成 w×h 对角渐变 RGBA8（演示图，免捆绑资源）。
fn gradient(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / (w - 1).max(1) as f32;
            let fy = y as f32 / (h - 1).max(1) as f32;
            v.extend_from_slice(&[(220.0 * (1.0 - fx)) as u8, (200.0 * fy) as u8, (220.0 * fx + 40.0) as u8, 255]);
        }
    }
    v
}

/// 生成 size×size 纯色图标（标签图标演示用）。
fn solid(size: u32, hex: u32) -> Vec<u8> {
    let (r, g, b) = (((hex >> 16) & 0xff) as u8, ((hex >> 8) & 0xff) as u8, (hex & 0xff) as u8);
    [r, g, b, 255].repeat((size * size) as usize)
}

/// 一行设置项：左标签 + 右控件。
fn row(label: &str, control: Element) -> Element {
    Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .spacing(12)
        .child(Element::label(label).font_size(14.0).fg(Color::hex(FG)).width(110))
        .child(control)
}

fn card(title: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .bg(Color::hex(CARD))
        .corner(10.0)
        .padding(16)
        .spacing(8)
        .child(Element::label(title).font_size(16.0).fg(Color::hex(FG)).height(24).width_match())
        .child(Element::divider())
        .child(body)
}

fn main() {
    let name = Rc::new(RefCell::new(String::from("我的设备")));
    let pwd = Rc::new(RefCell::new(String::from("hunter2")));
    let notes = Rc::new(RefCell::new(String::from(
        "这是一个多行文本框示例。\n超过宽度的长行会自动软换行，无需手动断行，体验接近现代编辑器。\n按 Enter 可换行。",
    )));
    let dark = Rc::new(Cell::new(true));
    let notify = Rc::new(Cell::new(true));
    let beta = Rc::new(Cell::new(false));
    let quality = Rc::new(Cell::new(1usize));
    let lang = Rc::new(Cell::new(0usize));
    let volume = Rc::new(Cell::new(0.7f32));
    let show_about = Rc::new(Cell::new(std::env::args().any(|a| a == "--dialog")));

    // 设置页（内容较多，包进滚动容器）
    let settings_body = Element::col()
        .width_match()
        .spacing(14)
        .child(card(
            "常规",
            Element::col()
                .width_match()
                .spacing(6)
                .child(row("设备名称", Element::text_input(name.clone(), "输入名称").width_match()))
                .child(row("访问密码", Element::text_input(pwd.clone(), "输入密码").password().width_match()))
                .child(row("界面语言", Element::dropdown(vec!["简体中文", "English", "日本語"], lang.clone()).width_match()))
                .child(row("深色主题", Element::switch(dark.clone())))
                .child(row("接收通知", Element::checkbox("启用推送通知", notify.clone())))
                .child(row("测试版", Element::checkbox("加入 Beta 通道", beta.clone()))),
        ))
        .child(card(
            "渲染",
            Element::col()
                .width_match()
                .spacing(6)
                .child(row("音量", Element::slider(volume.clone()).width_match()))
                .child(row(
                    "质量",
                    Element::row()
                        .spacing(16)
                        .child(Element::radio("低", quality.clone(), 0))
                        .child(Element::radio("中", quality.clone(), 1))
                        .child(Element::radio("高", quality.clone(), 2)),
                )),
        ))
        .child(card(
            "备注",
            Element::text_input(notes.clone(), "输入备注")
                .multiline()
                .width_match()
                .height(96),
        ));
    let settings = Element::scroll().fill().child(settings_body);

    // 列表页（滚动）
    let mut list = Element::scroll().fill().bg(Color::hex(CARD)).corner(10.0);
    for i in 0u32..24 {
        list = list.child(
            Element::row()
                .width_match()
                .height(38)
                .cross(Align::Center)
                .padding_xy(14, 0)
                .bg(if i.is_multiple_of(2) { Color::hex(CARD) } else { Color::hex(0xF6F8FA) })
                .child(Element::label(format!("历史记录 {i:02}")).font_size(14.0).fg(Color::hex(FG)).weight(1.0))
                .child(Element::label("查看").font_size(13.0).fg(Color::hex(0x4C8BF5))),
        );
    }

    let about_show = show_about.clone();
    let about = Element::col()
        .fill()
        .spacing(12)
        .child(card(
            "关于 windui",
            Element::col()
                .width_match()
                .spacing(8)
                .child(Element::label("轻量 Windows 桌面 GUI 框架").font_size(15.0).fg(Color::hex(FG)).height(22).width_match())
                .child(Element::label("Win32 窗口 + GDI 呈现 + tiny-skia 图形 + DirectWrite 文字").font_size(13.0).fg(Color::hex(SUB)).height(20).width_match())
                .child(Element::label("目标内存占用 2–5MB，无运行时、无 GC。").font_size(13.0).fg(Color::hex(SUB)).height(20).width_match())
                .child(Element::button("打开对话框").on_click(move |_| about_show.set(true))),
        ));

    // 控件页（新控件集中展示，内容可滚动便于后续扩充）。
    let prog = Rc::new(Cell::new(0.45f32));
    let qty = Rc::new(Cell::new(3.0f64));
    let zoom = Rc::new(Cell::new(1.0f64));
    let picked = Rc::new(Cell::new(1usize));
    // 分段控制器演示状态（输入法常见的二/三选一切换）。
    let zh_form = Rc::new(Cell::new(0usize)); // 简体/繁体
    let width_mode = Rc::new(Cell::new(0usize)); // 半角/全角
    let pinyin = Rc::new(Cell::new(0usize)); // 全拼/双拼/笔画
    // 可折叠分组 + 导航行演示。
    let adv_expand = Rc::new(Cell::new(true));
    let nav_msg = Rc::new(RefCell::new(String::from("（点下方导航行试试）")));
    // 链接 on_click 演示：点击计数写入动态标签。
    let link_msg = Rc::new(RefCell::new(String::from("（点下方“点我计数”试试）")));
    let link_n = Rc::new(Cell::new(0u32));
    let (lm, ln) = (link_msg.clone(), link_n.clone());
    let components_body = Element::col()
        .width_match()
        .spacing(14)
        .child(card(
            "分段控制器（连体多段单选，点击/方向键切换）",
            Element::col()
                .width_match()
                .spacing(6)
                .child(row("简繁切换", Element::segmented(vec!["简体", "繁体"], zh_form.clone())))
                .child(row("半全角", Element::segmented(vec!["半角", "全角"], width_mode.clone())))
                .child(row("输入方案", Element::segmented(vec!["全拼", "双拼", "笔画"], pinyin.clone())))
                .child(row(
                    "禁用态",
                    Element::segmented(vec!["开", "关"], Rc::new(Cell::new(0usize))).disabled(true),
                )),
        ))
        .child(card(
            "可折叠分组 + 导航行（点标题展开/收起，行尾 > 钻入子页）",
            Element::col().width_match().spacing(4).child(Element::collapsible(
                "高级设置",
                adv_expand.clone(),
                Element::col()
                    .width_match()
                    .child({
                        let m = nav_msg.clone();
                        Element::nav_row("双拼方案设定").on_click(move |_| *m.borrow_mut() = "已进入：双拼方案设定".into())
                    })
                    .child({
                        let m = nav_msg.clone();
                        Element::nav_row("模糊音设置").on_click(move |_| *m.borrow_mut() = "已进入：模糊音设置".into())
                    })
                    .child({
                        let m = nav_msg.clone();
                        Element::nav_row("拼音纠错设置").on_click(move |_| *m.borrow_mut() = "已进入：拼音纠错设置".into())
                    }),
            ))
            .child(Element::label_rc(nav_msg.clone()).font_size(13.0).fg(Color::hex(SUB)).height(18).width_match()),
        ))
        .child(card(
            "悬停提示 Tooltip（任意元素 .tooltip(...)，停留约 0.5s 弹出）",
            Element::col()
                .width_match()
                .spacing(10)
                .child(row("按钮", Element::button("悬停我").tooltip("这是按钮的悬停说明")))
                .child(row(
                    "帮助图标",
                    Element::label("(?)").font_size(14.0).fg(Color::hex(SUB)).tooltip("把鼠标停在元素上片刻即可看到提示"),
                )),
        ))
        .child(card(
            "进度条",
            Element::col()
                .width_match()
                .spacing(8)
                .child(Element::label("确定 45%").font_size(13.0).fg(Color::hex(SUB)).height(18).width_match())
                .child(Element::progress(prog.clone()).width_match())
                .child(Element::label("不确定（忙碌动画）").font_size(13.0).fg(Color::hex(SUB)).height(18).width_match())
                .child(Element::progress_indeterminate().width_match()),
        ))
        .child(card(
            "数字步进",
            Element::col()
                .width_match()
                .spacing(10)
                .child(row("数量", Element::stepper(qty.clone(), 0.0, 99.0, 1.0).width(120)))
                .child(row("缩放", Element::stepper(zoom.clone(), 0.5, 3.0, 0.25).width(120))),
        ))
        .child(card(
            "列表",
            Element::list(
                vec!["收件箱", "已发送", "草稿箱", "垃圾邮件", "归档", "重要", "已加星标"],
                picked.clone(),
            )
            .height(160)
            .width_match()
            .bg(Color::hex(0xF6F8FA))
            .corner(8.0),
        ))
        .child(card(
            "禁用态（核心统一管理：不可交互 + 置灰 + 跳 Tab）",
            Element::col()
                .width_match()
                .spacing(8)
                .child(row("按钮", Element::button("不可点").disabled(true)))
                .child(row("开关", Element::switch(Rc::new(Cell::new(true))).disabled(true)))
                .child(row("勾选", Element::checkbox("已禁用", Rc::new(Cell::new(true))).disabled(true)))
                .child(row("滑块", Element::slider(Rc::new(Cell::new(0.5))).disabled(true).width_match()))
                .child(row(
                    "下拉",
                    Element::dropdown(vec!["选项 A", "选项 B"], Rc::new(Cell::new(0))).disabled(true).width_match(),
                ))
                .child(row("步进", Element::stepper(Rc::new(Cell::new(3.0)), 0.0, 9.0, 1.0).disabled(true).width(120)))
                .child(row(
                    "输入",
                    Element::text_input(Rc::new(RefCell::new("只读内容".into())), "").disabled(true).width_match(),
                )),
        ))
        .child(card(
            "链接（链接色 + 下划线 + 悬停手型，点击/回车激活）",
            Element::col()
                .width_match()
                .spacing(8)
                .child(Element::link("打开 windui 官网（用系统浏览器）").url("https://example.com").font_size(14.0).height(20))
                .child(
                    Element::row()
                        .spacing(20)
                        .cross(Align::Center)
                        .child(Element::link("无下划线样式").underline(false).font_size(14.0).height(20))
                        .child(Element::link("已禁用链接").url("https://example.com").disabled(true).font_size(14.0).height(20)),
                )
                .child(Element::link("点我计数（自定义 on_click）").font_size(14.0).height(20).on_click(move |_| {
                    ln.set(ln.get() + 1);
                    *lm.borrow_mut() = format!("已点击 {} 次", ln.get());
                }))
                .child(Element::label_rc(link_msg.clone()).font_size(13.0).fg(Color::hex(SUB)).height(18).width_match()),
        ));
    let components = Element::scroll().fill().child(components_body);

    // 图片页：适配模式 + 圆角 + 占位 + Button 图标。
    let grad = gradient(64, 48);
    let img_cell = |label: &str, e: Element| {
        Element::col()
            .spacing(4)
            .child(e.width(84).height(60).bg(Color::hex(0xF6F8FA)).border(Color::hex(0xDDDDDD), 1))
            .child(Element::label(label).font_size(12.0).fg(Color::hex(SUB)).height(16))
    };
    let images_body = Element::col()
        .width_match()
        .spacing(14)
        .child(card(
            "适配模式（源图 4:3）",
            Element::row()
                .spacing(10)
                .child(img_cell("Contain", Element::image_rgba(64, 48, &grad).fit(Fit::Contain)))
                .child(img_cell("Cover", Element::image_rgba(64, 48, &grad).fit(Fit::Cover)))
                .child(img_cell("Fill", Element::image_rgba(64, 48, &grad).fit(Fit::Fill))),
        ))
        .child(card(
            "圆角 & 占位 & 图标",
            Element::row()
                .spacing(12)
                .cross(Align::Center)
                .child(img_cell("圆角", Element::image_rgba(64, 48, &grad).fit(Fit::Cover).corner(12.0)))
                .child(img_cell("占位", Element::image("不存在.png")))
                .child(Element::button("新建").icon_rgba(64, 48, &grad))
                .child(Element::button("禁用").icon_rgba(64, 48, &grad).disabled(true)),
        ));
    let images = Element::scroll().fill().child(images_body);

    let tab = Rc::new(Cell::new(0usize));
    let dot = |hex: u32| ImageContent::from_rgba(16, 16, &solid(16, hex));
    let tabs = Element::tabs_icons(
        tab.clone(),
        vec![
            ("设置", dot(0x4C8BF5), settings),
            ("控件", dot(0x2EC48B), components),
            ("图片", dot(0xF5A623), images),
            ("历史", dot(0x9B59B6), Element::col().fill().child(list)),
            ("关于", dot(0xE5484D), about),
        ],
    );

    // 关于对话框
    let close = show_about.clone();
    let dialog = Element::dialog(
        show_about.clone(),
        Element::col()
            .width(320)
            .bg(Color::hex(CARD))
            .corner(14.0)
            .padding(22)
            .spacing(14)
            .child(Element::label("windui v0.1").font_size(20.0).fg(Color::hex(FG)).height(28).width_match())
            .child(Element::label("一个用 Rust 编写的轻量桌面 GUI 框架，适合做内存友好的小工具。").font_size(14.0).fg(Color::hex(SUB)).height(44).width_match())
            .child(
                Element::row()
                    .width_match()
                    .height(40)
                    .child(Element::label("").weight(1.0))
                    .child(Element::button("知道了").on_click(move |_| close.set(false))),
            ),
    );

    let ui = Element::stack()
        .fill()
        .bg(Color::hex(BG))
        .child(
            Element::col()
                .fill()
                .padding(18)
                .spacing(12)
                .child(Element::label("偏好设置").font_size(24.0).fg(Color::hex(0x1A1A2E)).height(34).width_match())
                // tabs 用 weight 占据标题以下的剩余高度（纵向 Match 会降级为 Wrap，需 weight 才填充）。
                .child(tabs.weight(1.0)),
        )
        .child(dialog);

    App::new("windui — 综合示例", 520, 560)
        .bg(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
