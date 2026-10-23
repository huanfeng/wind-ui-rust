//! 综合示例：一个"偏好设置"小工具，集中展示 windui 全部控件。
//!
//! 运行：    cargo run --release --example fullshowcase
//! 截屏：    cargo run --example fullshowcase -- --screenshot artifacts/showcase.png
//! 对话框：  cargo run --example fullshowcase -- --dialog --screenshot artifacts/showcase_dialog.png

use windui::prelude::*;

#[path = "common/mod.rs"]
mod common;
use common::{page_title, theme_toggle, Shell};

/// 内联 SVG 演示资源（含 `#` 颜色值，故用 br##"..."## 定界）。渐变圆 + 单色对勾。
const SVG_CIRCLE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#ff6b9d"/><stop offset="1" stop-color="#4c8bf5"/></linearGradient></defs><circle cx="32" cy="32" r="28" fill="url(#g)"/></svg>"##;
const SVG_CHECK: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" fill="#000000"/></svg>"##;

/// 生成 w×h 对角渐变 RGBA8（演示图，免捆绑资源）。
fn gradient(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / (w - 1).max(1) as f32;
            let fy = y as f32 / (h - 1).max(1) as f32;
            v.extend_from_slice(&[
                (220.0 * (1.0 - fx)) as u8,
                (200.0 * fy) as u8,
                (220.0 * fx + 40.0) as u8,
                255,
            ]);
        }
    }
    v
}

/// 生成 size×size 纯色图标（标签图标演示用）。
fn solid(size: u32, hex: u32) -> Vec<u8> {
    let (r, g, b) = (
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    );
    [r, g, b, 255].repeat((size * size) as usize)
}

/// 行高与限宽演示用的中文长句：这两项的差别只有在多行正文上才看得出来。
const CJK_SAMPLE: &str = "行高决定中文正文的呼吸感。这段字用来对比：未设行高时按字体自带行距排版，设为 1.8 后行与行之间明显松开，长段落的可读性差别很大。";

/// 可排序列表的一行：名称 + 副标题 + 右侧开关。
///
/// 刻意做成"行内带交互控件 + 高度不等"的形态——这正是拖拽手柄存在的理由：
/// 整行拖拽会与开关抢事件，而让位算法也必须按各行实际高度重新堆叠。
fn scheme_row(name: &str, sub: &str, on: Signal<bool>) -> Element {
    Element::hstack()
        .width_match()
        .padding_xy(8, 6)
        .child(
            Element::col()
                .weight(1.0)
                .spacing(2)
                .child(Element::label(name).fg_role(Role::Text))
                .child(Element::label(sub).font_size(11.0).fg_role(Role::TextMuted)),
        )
        .child(Element::switch(on))
}

/// 数据驱动重排演示的出厂顺序。
fn default_dict_order() -> Vec<String> {
    ["系统词库", "用户词库", "网络流行语", "专业术语"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 某项在当前顺序中的下标（找不到当 0）。副标题借它显示实时优先级。
fn pos_of(order: &[String], name: &str) -> usize {
    order.iter().position(|x| x == name).unwrap_or(0)
}

/// 多边形填充演示（`Canvas::fill_polygon`）：凹多边形、反向内圈挖洞、斜边三角、带孔齿轮，
/// 外加一块渐变填充。四个格子各 72×72，颜色取主题色。
struct PolygonDemo;

/// `q` 在 `body` 里的全部命中，按**字符**索引（`marks` / `select_when` 的单位）。
fn find_ranges(body: &str, q: &str) -> Vec<std::ops::Range<usize>> {
    if q.is_empty() {
        return Vec::new();
    }
    let n = q.chars().count();
    body.match_indices(q)
        .map(|(b, _)| {
            let s = body[..b].chars().count();
            s..s + n
        })
        .collect()
}

/// 正多边形近似的圆轮廓；`ccw` 反向绕——放进同一次 `fill_polygon` 里就是洞。
fn ring_contour(cx: f32, cy: f32, r: f32, ccw: bool) -> Vec<(f32, f32)> {
    (0..48)
        .map(|i| {
            let t = i as f32 / 48.0 * std::f32::consts::TAU * if ccw { -1.0 } else { 1.0 };
            (cx + r * t.cos(), cy + r * t.sin())
        })
        .collect()
}

impl windui::core::Widget for PolygonDemo {
    fn measure(
        &self,
        _avail: Size,
        _style: &windui::style::Style,
        _text: &mut dyn windui::text::TextEngine,
    ) -> Size {
        Size::new(4 * 72 + 3 * 16, 72)
    }

    fn paint(
        &self,
        _bounds: Rect,
        content: Rect,
        _focused: bool,
        _enabled: bool,
        canvas: &mut dyn windui::render::Canvas,
        _style: &windui::style::Style,
    ) {
        use std::f32::consts::{FRAC_PI_2, TAU};
        use windui::render::Paint;
        let pal = windui::theme::current().palette.clone();
        let cell = |i: usize| {
            (
                content.x as f32 + i as f32 * 88.0 + 36.0,
                content.y as f32 + 36.0,
            )
        };

        // ① 凹五角星（10 个顶点交替取外 / 内半径），渐变填充。
        let (cx, cy) = cell(0);
        let star: Vec<(f32, f32)> = (0..10)
            .map(|i| {
                let r = if i % 2 == 0 { 34.0 } else { 14.0 };
                let t = i as f32 / 10.0 * TAU - FRAC_PI_2;
                (cx + r * t.cos(), cy + r * t.sin())
            })
            .collect();
        let grad = Gradient::linear(
            (0.0, 0.0),
            (1.0, 1.0),
            vec![(0.0, pal.accent), (1.0, pal.text)],
        );
        canvas.fill_polygon(&[&star], &Paint::gradient(grad));

        // ② 圆环：外圈顺时针、内圈逆时针，一次调用挖出洞。
        let (cx, cy) = cell(1);
        let (outer, inner) = (
            ring_contour(cx, cy, 33.0, false),
            ring_contour(cx, cy, 18.0, true),
        );
        canvas.fill_polygon(&[&outer, &inner], &Paint::fill(pal.accent));

        // ③ 播放三角：斜边靠抗锯齿，分数缩放下不发锯齿。
        let (cx, cy) = cell(2);
        let tri = [
            (cx - 22.0, cy - 30.0),
            (cx + 32.0, cy),
            (cx - 22.0, cy + 30.0),
        ];
        canvas.fill_polygon(&[&tri], &Paint::fill(pal.text));

        // ④ 齿轮：12 个齿的外轮廓 + 反向的中心孔。
        let (cx, cy) = cell(3);
        let teeth = 12;
        let gear: Vec<(f32, f32)> = (0..teeth * 4)
            .map(|i| {
                let r = if (i % 4) < 2 { 34.0 } else { 26.0 };
                let t = (i as f32 + 0.5) / (teeth * 4) as f32 * TAU;
                (cx + r * t.cos(), cy + r * t.sin())
            })
            .collect();
        let hole = ring_contour(cx, cy, 10.0, true);
        canvas.fill_polygon(&[&gear, &hole], &Paint::fill(pal.text_muted));
    }
}

fn main() {
    let name = signal(String::from("我的设备"));
    let pwd = signal(String::from("hunter2"));
    let notes = signal(String::from(
        "这是一个多行文本框示例。\n超过宽度的长行会自动软换行，无需手动断行，体验接近现代编辑器。\n按 Enter 可换行。",
    ));
    let dark = signal(false);
    let notify = signal(true);
    let beta = signal(false);
    let (hide_disabled, show_special) = (signal(true), signal(false));
    let quality = signal(1usize);
    let lang = signal(0usize);
    let volume = signal(0.7f32);
    let show_about = signal(std::env::args().any(|a| a == "--dialog"));

    // 520×560 时首屏只露得出一张半卡片，读起来像"截了一角"。760×700 是两头的折中：
    // 够七个分页的标签一行排开、首屏装得下一张完整卡片，又不至于宽到让 `field` 那种
    // "控件紧跟标签"的行右侧空出半屏——那个 API 的标签列是定宽的，窗口再宽控件也不跟着长。
    let mut app = App::new("windui — 综合示例", 760, 700)
        .icon(brand_icon())
        .frameless();
    let th = app.theme_handle();

    // 设置页（内容较多，包进滚动容器）

    // 列表页（滚动）
    let mut list = Element::scroll().fill().bg_role(Role::Surface).corner(10.0);
    for i in 0u32..24 {
        list = list.child(
            Element::row()
                .width_match()
                .height(38)
                .cross(Align::Center)
                .padding_xy(14, 0)
                .bg_role(if i.is_multiple_of(2) {
                    Role::Surface
                } else {
                    Role::SurfaceAlt
                })
                .child(
                    Element::label(format!("历史记录 {i:02}"))
                        .fg_role(Role::Text)
                        .weight(1.0),
                )
                .child(Element::label("查看").font_size(13.0).fg_role(Role::Accent)),
        );
    }

    let about_show = show_about;
    let about = Element::col().fill().spacing(12).child(Element::card(
        "关于 windui",
        Element::col()
            .width_match()
            .spacing(8)
            .child(
                Element::label("轻量 Windows 桌面 GUI 框架")
                    .font_size(15.0)
                    .fg_role(Role::Text)
                    .width_match(),
            )
            .child(
                Element::label("Win32 窗口 + GDI 呈现 + tiny-skia 图形 + DirectWrite 文字")
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(
                Element::label("目标内存占用 2–5MB，无运行时、无 GC。")
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(Element::button("打开对话框").on_click(move |_| about_show.set(true))),
    ));

    // 控件页（新控件集中展示，内容可滚动便于后续扩充）。
    let prog = signal(0.45f32);
    let qty = signal(3.0f64);
    let zoom = signal(1.0f64);
    let picked = signal(1usize);
    // 分段控制器演示状态（输入法常见的二/三选一切换）。
    let zh_form = signal(0usize); // 简体/繁体
    let width_mode = signal(0usize); // 半角/全角
    let pinyin = signal(0usize); // 全拼/双拼/笔画
                                 // 可折叠分组 + 导航行演示。
    let adv_expand = signal(true);
    // 手风琴：单开互斥共享索引（初值 0 = 默认展开第一面板）。
    let acc_sel = signal(Some(0usize));
    let nav_msg = signal(String::from("（点下方导航行试试）"));
    // 富文本演示：例句组折叠态 + 长释义 clamp 展开态。
    let rich_collapsed = signal(false);
    let rich_expanded = signal(false);
    // 链接 on_click 演示：点击计数写入动态标签。
    let link_msg = signal(String::from("（点下方“点我计数”试试）"));
    let link_n = signal(0u32);
    let (lm, ln) = (link_msg, link_n);
    // 拖拽重排演示：三个输入方案，行内各带一个开关。
    let scheme_a = signal(true);
    let scheme_b = signal(false);
    let scheme_c = signal(true);
    let order_msg = signal(String::from("（按住左侧手柄上下拖动即可调整顺序）"));
    let om = order_msg;
    // 数据驱动重排演示：顺序存在信号里，故「恢复默认」这类反向同步才做得到。
    let dict_order = signal(default_dict_order());
    // 虚拟滚动演示数据：十万行。存的是 usize 而非预格式化的 String——十万个 String
    // 会平白多占十几 MB，而本库的立身之本正是极低内存。
    let huge_rows = signal((0..100_000usize).collect::<Vec<_>>());

    // 图片页：适配模式 + 圆角 + 占位 + Button 图标。
    let grad = gradient(64, 48);
    let img_cell = |label: &str, e: Element| {
        Element::col()
            .spacing(4)
            .child(
                e.width(84)
                    .height(60)
                    .bg_role(Role::SurfaceAlt)
                    .border_role(Role::Border, 1),
            )
            .child(
                Element::label(label)
                    .font_size(12.0)
                    .fg_role(Role::TextMuted),
            )
    };

    // 表格页（表格功能较多，集中于独立 tab）。
    let file_rows = || {
        vec![
            vec!["report.pdf", "1280", "2026-05-01"],
            vec!["notes.txt", "3", "2026-06-18"],
            vec!["photo.png", "845", "2026-04-22"],
            vec!["archive.zip", "20480", "2026-06-30"],
            vec!["readme.md", "12", "2026-05-15"],
        ]
    };
    let file_cols = || vec![("名称", 2.0), ("大小(KB)", 1.0), ("修改日期", 1.5)];
    // 可排序 + 多选：每行一个选择信号，选中集可被 app 读取。
    let sel: Vec<Signal<bool>> = (0..file_rows().len()).map(|_| signal(false)).collect();
    let sel_count = signal(String::from("已选 0 项"));
    // 虚拟表格演示数据：1 万行——正是非虚拟表格开始滚不动的量级（实测稳态重排 47ms/帧）。
    let huge_table = signal(
        (0..10_000)
            .map(|i| {
                vec![
                    format!("file_{i:05}.dat"),
                    format!("{}", i * 37 % 9999),
                    format!("2026-{:02}-{:02}", i % 12 + 1, i % 28 + 1),
                ]
            })
            .collect::<Vec<_>>(),
    );
    let sc_2 = Element::card(
        "常规",
        Element::col()
            .width_match()
            .spacing(6)
            .on_context_menu(|| {
                vec![
                    MenuItem::run("复制", |ctx| ctx.toast("已复制"), false)
                        .icon("⧉")
                        .shortcut("Ctrl+C"),
                    MenuItem::run("重命名…", |ctx| ctx.toast("重命名"), false).icon("✎"),
                    MenuItem::separator(),
                    // 危险项：intent 压过悬停，指向时仍是红的。
                    MenuItem::run("删除…", |ctx| ctx.toast_err("已删除"), false)
                        .icon("🗑")
                        .danger(),
                ]
            })
            .child(Element::field(
                "设备名称",
                Element::text_input(name, "输入名称").width_match(),
            ))
            .child(Element::field(
                "访问密码",
                Element::text_input(pwd, "输入密码")
                    .password()
                    .width_match(),
            ))
            .child(Element::field(
                "界面语言",
                Element::dropdown(vec!["简体中文", "English", "日本語"], lang).width_match(),
            ))
            .child(Element::field(
                "列表显示",
                Element::check_menu(
                    "列表显示",
                    vec![
                        CheckMenuItem::check("隐藏未启用", hide_disabled),
                        CheckMenuItem::check("显示特殊项", show_special),
                        CheckMenuItem::separator(),
                        CheckMenuItem::action("恢复默认", |_ctx| {}),
                    ],
                )
                .summary(|on| match on.len() {
                    0 => "列表显示".to_string(),
                    n => format!("列表显示 ({n})"),
                })
                .width_match(),
            ))
            .child(Element::field("深色主题", Element::switch(dark)))
            .child(Element::field(
                "接收通知",
                Element::checkbox("启用推送通知", notify),
            ))
            .child(Element::field(
                "测试版",
                Element::checkbox("加入 Beta 通道", beta),
            ))
            .child(
                Element::label("在本卡片上右击：复制 / 重命名… / 删除…（末项 danger 显红）")
                    .font_size(12.0)
                    .fg_role(Role::TextSubtle)
                    .width_match(),
            ),
    );

    let sc_3 = Element::card(
        "渲染",
        Element::col()
            .width_match()
            .spacing(6)
            .child(Element::field(
                "音量",
                Element::slider(volume).width_match(),
            ))
            .child(Element::field(
                "质量",
                Element::hstack()
                    .spacing(16)
                    .child(Element::radio("低", quality, 0))
                    .child(Element::radio("中", quality, 1))
                    .child(Element::radio("高", quality, 2)),
            )),
    );

    let sc_4 = Element::card(
        "备注",
        Element::col()
            .width_match()
            .spacing(6)
            .child(
                Element::text_input(notes, "输入备注")
                    .multiline()
                    .width_match()
                    .height(96),
            )
            // 只读：可点选、拖选、Ctrl+C / 右键复制，不可编辑（日志、诊断信息这类场景）。
            .child(
                Element::text_input(
                    signal(String::from(
                        "只读多行框：可以拖选局部、Ctrl+C 或右键「复制」，\n但键入、粘贴、剪切都不会改动内容。",
                    )),
                    "",
                )
                .multiline()
                .wrap(true)
                .read_only()
                .width_match()
                .height(64),
            ),
    );

    // 查找命中：所有命中由框架画底色（`marks`），「下一处」把选区摆到那一段、焦点不动
    // （`select_without_focus_when`）——叠着选区色的那一条就是当前这一处。命中表只在点「下一处」时
    // 重算：跳转后在正文里改字，标记会错位到下次点击为止（越界有钳制，不会 panic）。
    let find_body = signal(String::from(
        "风起于青萍之末，浪成于微澜之间。\n风停了，浪还在；浪平了，风又起。",
    ));
    let find_query = signal(String::from("风"));
    let find_marks = signal(Vec::<TextMark>::new());
    let (find_jump, find_hit, find_k) = (signal(0u64), signal((0usize, 0usize)), signal(0usize));
    let find_status = signal(String::from("回车或点「下一处」"));
    let find_next = move || {
        let hits = find_body.with(|b| find_query.with(|q| find_ranges(b, q)));
        find_marks.set(hits.iter().cloned().map(TextMark::new).collect());
        if hits.is_empty() {
            find_status.set("没有命中".into());
            return;
        }
        let k = find_k.get() % hits.len();
        find_hit.set((hits[k].start, hits[k].end));
        find_jump.update(|n| *n += 1);
        find_k.set(k + 1);
        find_status.set(format!("第 {} / {} 处", k + 1, hits.len()));
    };
    let sc_find = Element::card(
        "查找命中（marks 区间底色 + select_without_focus_when 运行期设选区）",
        Element::col()
            .width_match()
            .spacing(6)
            .child(
                Element::hstack()
                    .width_match()
                    .child(
                        // 焦点一直留在查询框：Enter 一次跳一处（正文框用的是不夺焦点版）。
                        Element::text_input(find_query, "要找的字")
                            .weight(1.0)
                            .on_submit(move |_| find_next()),
                    )
                    .child(Element::button("下一处").on_click(move |_| find_next()))
                    // 定宽：状态文字长短一变，按钮就跟着挪位置。
                    .child(
                        Element::label_signal(find_status)
                            .width(120)
                            .fg_role(Role::TextMuted),
                    ),
            )
            .child(
                Element::text_input(find_body, "")
                    .multiline()
                    .wrap(true)
                    .width_match()
                    .height(64)
                    .marks(find_marks)
                    .select_without_focus_when(find_jump, find_hit),
            ),
    );

    let cp_1 = Element::card(
        "拖拽重排 reorder_list（按住手柄拖动；行内开关照常可点，拖动中按 Esc 取消）",
        Element::col()
            .width_match()
            .spacing(6)
            .child(
                Element::reorder_list(vec![
                    scheme_row("拼音方案", "全拼 · 默认", scheme_a),
                    scheme_row("五笔方案", "86 版", scheme_b),
                    scheme_row("双拼方案", "小鹤双拼", scheme_c),
                ])
                .on_reorder(move |_ctx, from, to| {
                    om.set(format!("已把第 {} 项移到第 {} 位", from + 1, to + 1));
                }),
            )
            .child(
                Element::label_signal(order_msg)
                    .font_size(12.0)
                    .fg_role(Role::TextMuted),
            ),
    );

    let cp_2 = Element::card(
        "数据驱动重排 reorder_list_signal（顺序的真相源在信号里，可被「恢复默认」推回）",
        Element::col()
            .width_match()
            .spacing(6)
            .child(
                Element::reorder_list_signal(dict_order, {
                    let d = dict_order;
                    move |name: String, handle| {
                        // 手柄由行自己安放——这里放行首，整行可点的场景则应放进
                        // stack 覆盖层（手柄不能是 clickable 容器的后代）。
                        let sub = format!("优先级 {}", pos_of(&d.get(), &name) + 1);
                        Element::row()
                            .width_match()
                            .cross(Align::Center)
                            .child(handle)
                            .child(
                                Element::col()
                                    .weight(1.0)
                                    .spacing(2)
                                    .padding_xy(8, 6)
                                    .child(Element::label(name).fg_role(Role::Text))
                                    .child(
                                        Element::label(sub)
                                            .font_size(11.0)
                                            .fg_role(Role::TextMuted),
                                    ),
                            )
                    }
                })
                .on_reorder({
                    let d = dict_order;
                    move |_ctx, from, to| {
                        d.update(|v| {
                            let x = v.remove(from);
                            v.insert(to.min(v.len()), x);
                        })
                    }
                }),
            )
            .child(
                Element::button("恢复默认顺序")
                    .small()
                    .outline_soft()
                    .on_click(move |_| dict_order.set(default_dict_order())),
            ),
    );

    let cp_3 = Element::card(
            "富文本 RichText（span 混排基线对齐 + 胶囊 + 分隔线 + 可折叠例句组）",
            Element::rich(
                RichDoc::new()
                    .style("headword", SpanStyle::new().size(24.0).bold())
                    .style("phonetic", SpanStyle::new().size(13.0).fg(RichColor::Muted))
                    .style("pos", SpanStyle::new().size(11.0).bold().chip())
                    .style("example", SpanStyle::new().size(13.0).fg(RichColor::Muted))
                    .para(
                        Para::new()
                            .styled("headword", "apple")
                            .text("  ")
                            .styled("phonetic", "/ˈæp.əl/"),
                    )
                    .style("ref", SpanStyle::new().fg(RichColor::Accent).underline())
                    .para(
                        Para::new()
                            .styled("pos", "n.")
                            .text(" 苹果；苹果树。参见 ")
                            .styled_id("ref", "fruit", "fruit")
                            .text(" 词条。"),
                    )
                    .para(
                        Para::new()
                            .text("1. 悬挂缩进演示：这一条编号义项足够长，换行后的续行会对齐到释义首字而不是编号底下。")
                            .hanging(14),
                    )
                    .para(
                        Para::new()
                            .styled("example", "行数截断演示：这一段长释义默认只显示两行，超出的内容被截断，行尾出现可点击的展开标记；点击后整段展开为全文，再看就是完整内容了。ECDICT 的 translation 字段偶尔很长，侧栏预览正需要这种收敛。")
                            .clamp(2, rich_expanded),
                    )
                    .divider()
                    .para(
                        Para::new()
                            .styled("pos", "习语")
                            .text(" the apple of one's eye 掌上明珠 ")
                            .span("apple of the eye", SpanStyle::new().strike().fg(RichColor::Muted)),
                    )
                    .section("例句（点击标题折叠）", rich_collapsed, |s| {
                        s.para(Para::new().styled(
                            "example",
                            "An apple a day keeps the doctor away. 一天一苹果，医生远离我。",
                        ))
                        .para(Para::new().styled(
                            "example",
                            "She bought a pound of apples. 她买了一磅苹果。",
                        ))
                    }),
            )
            .on_span_click(|ctx, id| ctx.toast(format!("跳转词条：{id}")))
            .width_match(),
        );

    let cp_4 = Element::card(
        "按钮风格（intent：primary / neutral / danger + accent 扩展）",
        Element::hstack()
            .child(Element::button("主操作"))
            .child(Element::button("次要").neutral())
            .child(Element::button("删除").danger())
            .child(Element::button("品牌").accent(Color::hex(0x2E9E5B)))
            .child(Element::button("禁用").danger().disabled(true)),
    );

    let cp_5 = Element::card(
        "轻提示 Toast（居中浮层 + 淡入淡出 + 定时消失，回调内 ctx.toast*）",
        Element::hstack()
            .child(Element::button("成功提示").on_click(|ctx| ctx.toast_ok("已添加到剪贴板")))
            .child(
                Element::button("普通提示")
                    .neutral()
                    .on_click(|ctx| ctx.toast("已保存设置")),
            )
            .child(
                Element::button("错误提示")
                    .danger()
                    .on_click(|ctx| ctx.toast_err("操作失败，请重试")),
            ),
    );

    let cp_6 = Element::card(
        "描边按钮 Outline + 胶囊徽章 Badge",
        Element::hstack()
            .child(Element::button("检查更新").outline())
            .child(Element::button("次要").neutral().outline())
            .child(Element::button("删除").danger().outline())
            .child(Element::badge("v0.0.0-alpha"))
            .child(Element::badge_intent(
                "稳定",
                Intent::Custom(Color::hex(0x2EA043)),
            ))
            .child(Element::badge_intent("废弃", Intent::Danger)),
    );

    let cp_7 = Element::card(
        "可点击容器 clickable（hover/press 叠层 + 键盘激活 + 手型光标）",
        Element::hstack()
            .clickable()
            .on_click(|ctx| ctx.toast_ok("卡片被点击"))
            .width_match()
            .padding(12)
            .corner(10.0)
            .bg_role(Role::Surface)
            .border_role(Role::Border, 1)
            .child(
                Element::label("整行可点击 — 悬停高亮 / 回车激活 / 点击弹 Toast")
                    .fg_role(Role::Text)
                    .weight(1.0),
            )
            .child(
                Element::label("›")
                    .font_size(20.0)
                    .fg_role(Role::TextSubtle),
            ),
    );

    let cp_8 = Element::card(
        "图标按钮 IconButton / 标签 chip / 标签字段 tag_field",
        Element::hstack()
            .width_match()
            .child(Element::icon_button("\u{25B2}").fg_role(Role::TextSubtle))
            .child(Element::icon_button("\u{25BC}").fg_role(Role::TextSubtle))
            .child(Element::icon_button("\u{24D8}").fg_role(Role::TextSubtle))
            .child(Element::icon_button("\u{2715}").fg_role(Role::TextSubtle))
            .child(
                Element::tag_field(
                    "添加触发键…",
                    vec![
                        Element::chip("分号(;)", |ctx| ctx.toast("移除：分号")),
                        Element::chip("逗号(,)", |ctx| ctx.toast("移除：逗号")),
                    ],
                )
                .weight(1.0),
            ),
    );

    let cp_9 = Element::card(
        "网格 grid（每行 2 列等宽）",
        Element::grid(
            2,
            10,
            vec![
                {
                    let s = signal(true);
                    Element::checkbox("（ ） 圆括号", s)
                },
                {
                    let s = signal(true);
                    Element::checkbox("【 】 方括号", s)
                },
                {
                    let s = signal(false);
                    Element::checkbox("｛ ｝ 花括号", s)
                },
                {
                    let s = signal(true);
                    Element::checkbox("《 》 书名号", s)
                },
            ],
        ),
    );

    let cp_10 = Element::card(
        "复选框增强（受控点击拦截 + 危险 / 自定义强调色）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(Element::field("危险项", {
                let s = signal(true);
                Element::checkbox("删除我的所有数据", s).danger()
            }))
            .child(Element::field("自定义色", {
                let s = signal(true);
                Element::checkbox("绿色强调（accent 覆盖）", s).accent(Color::hex(0x00A86B))
            }))
            .child(Element::field("浅色自适应", {
                let s = signal(true);
                Element::checkbox("浅色 accent（对勾自动转深）", s).accent(Color::hex(0xFFD54F))
            }))
            .child(Element::field("受控", {
                let s = signal(false);
                let s2 = s;
                // 受控：点击不自动翻转，交回调决定（此处演示直接翻转；真实场景可先弹确认再 set）。
                Element::checkbox("点击交给 app 决定", s).on_toggle(move |_| s2.set(!s2.get()))
            })),
    );

    let cp_11 = Element::card(
        "复选框尺寸（Normal 18px vs Small 14px）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(Element::field("默认", {
                let s = signal(true);
                Element::checkbox("Normal（18px）", s)
            }))
            .child(Element::field("小尺寸", {
                let s = signal(true);
                Element::checkbox("Small（14px）", s).small()
            }))
            .child(Element::field("小+危险", {
                let s = signal(true);
                Element::checkbox("Small danger", s).small().danger()
            }))
            .child(Element::field("小+自定义色", {
                let s = signal(false);
                Element::checkbox("Small accent", s)
                    .small()
                    .accent(Color::hex(0x00A86B))
            }))
            .child(Element::field("小+禁用", {
                Element::checkbox("Small disabled", signal(true))
                    .small()
                    .disabled(true)
            })),
    );

    let cp_12 = Element::card(
        "开关尺寸（Normal 44×24 vs Small 36×20）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(Element::field("默认", Element::switch(signal(true))))
            .child(Element::field(
                "小尺寸",
                Element::switch(signal(true)).small(),
            ))
            .child(Element::field(
                "小+关态",
                Element::switch(signal(false)).small(),
            ))
            .child(Element::field(
                "小+禁用",
                Element::switch(signal(true)).small().disabled(true),
            )),
    );

    let cp_13 = Element::card(
        "文字行高（line_height：倍数，随字号与 DPI 缩放）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(
                Element::label("默认行距")
                    .font_size(12.0)
                    .fg_role(Role::TextMuted),
            )
            .child(Element::label(CJK_SAMPLE).width_match())
            .child(
                Element::label("行高 1.8")
                    .font_size(12.0)
                    .fg_role(Role::TextMuted),
            )
            .child(Element::label(CJK_SAMPLE).width_match().line_height(1.8)),
    );

    let cp_14 = Element::card(
        "正文限宽（max_width：在上界内换行，而非排完再裁）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(
                Element::label("不限宽：行长随窗口，越宽越难回到行首")
                    .font_size(12.0)
                    .fg_role(Role::TextMuted),
            )
            .child(Element::label(CJK_SAMPLE).width_match())
            .child(
                Element::label("限宽 320")
                    .font_size(12.0)
                    .fg_role(Role::TextMuted),
            )
            .child(Element::label(CJK_SAMPLE).width_match().max_width(320)),
    );

    let cp_15 = Element::card(
        "单边边框（Edges：不参与布局，替代 1px 色块）",
        Element::col()
            .width_match()
            .spacing(12)
            .child(
                Element::label("仅底边——页签下划线、分区底线用")
                    .width_match()
                    .padding(8)
                    .border_role(Role::Accent, 2)
                    .border_edges(Edges::BOTTOM),
            )
            .child(
                Element::label("上下双边（Edges::TOP | Edges::BOTTOM）")
                    .width_match()
                    .padding(8)
                    .border_role(Role::Divider, 1)
                    .border_edges(Edges::TOP | Edges::BOTTOM),
            )
            .child(
                Element::label("仅左边——引用块、侧栏标记用")
                    .width_match()
                    .padding(8)
                    .border_role(Role::Accent, 3)
                    .border_edges(Edges::LEFT),
            )
            .child(
                Element::label("四边齐全时仍走圆角描边（对照）")
                    .width_match()
                    .padding(8)
                    .corner(8.0)
                    .border_role(Role::Border, 1),
            ),
    );

    let cp_16 = Element::card(
        "分段控制器（连体多段单选，点击/方向键切换）",
        Element::col()
            .width_match()
            .spacing(6)
            .child(Element::field(
                "简繁切换",
                Element::segmented(vec!["简体", "繁体"], zh_form),
            ))
            .child(Element::field(
                "半全角",
                Element::segmented(vec!["半角", "全角"], width_mode),
            ))
            .child(Element::field(
                "输入方案",
                Element::segmented(vec!["全拼", "双拼", "笔画"], pinyin),
            ))
            .child(Element::field(
                "禁用态",
                Element::segmented(vec!["开", "关"], signal(0usize)).disabled(true),
            )),
    );

    let cp_17 = Element::card(
        "可折叠分组 + 导航行（点标题展开/收起，行尾 > 钻入子页）",
        Element::col()
            .width_match()
            .spacing(4)
            .child(Element::collapsible(
                "高级设置",
                adv_expand,
                Element::col()
                    .width_match()
                    .child({
                        let m = nav_msg;
                        Element::nav_row("双拼方案设定")
                            .on_click(move |_| m.set("已进入：双拼方案设定".into()))
                    })
                    .child({
                        let m = nav_msg;
                        Element::nav_row("模糊音设置")
                            .on_click(move |_| m.set("已进入：模糊音设置".into()))
                    })
                    .child({
                        let m = nav_msg;
                        Element::nav_row("拼音纠错设置")
                            .on_click(move |_| m.set("已进入：拼音纠错设置".into()))
                    }),
            ))
            .child(
                Element::label_signal(nav_msg)
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            ),
    );

    let cp_18 = Element::card(
        "手风琴 Accordion（卡片多面板；单开互斥 / 多开独立）",
        Element::col()
            .width_match()
            .spacing(12)
            .child(
                Element::label("单开互斥（展开一个自动收起其它）")
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(Element::accordion(
                acc_sel,
                vec![
                    (
                        "什么是双拼？",
                        Element::label("双拼用两键拼出一个音节，减少击键。")
                            .width_match()
                            .padding_xy(12, 4),
                    ),
                    (
                        "如何切换方案？",
                        Element::label("在“高级设置 → 双拼方案设定”里选择。")
                            .width_match()
                            .padding_xy(12, 4),
                    ),
                    (
                        "支持自定义吗？",
                        Element::label("支持，导入自定义码表即可。")
                            .width_match()
                            .padding_xy(12, 4),
                    ),
                ],
            ))
            .child(
                Element::label("多开独立（各面板互不影响）")
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(Element::accordion_multi(vec![
                (
                    "常规",
                    Element::label("常规设置项……")
                        .width_match()
                        .padding_xy(12, 4),
                ),
                (
                    "外观",
                    Element::label("外观设置项……")
                        .width_match()
                        .padding_xy(12, 4),
                ),
            ])),
    );

    let cp_19 = Element::card(
        "悬停提示 Tooltip（任意元素 .tooltip(...)，停留约 0.5s 弹出）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(Element::field(
                "按钮",
                Element::button("悬停我").tooltip("这是按钮的悬停说明"),
            ))
            .child(Element::field(
                "帮助图标",
                Element::label("(?)")
                    .fg_role(Role::TextMuted)
                    .tooltip("把鼠标停在元素上片刻即可看到提示"),
            )),
    );

    let cp_20 = Element::card(
        "进度条",
        Element::col()
            .width_match()
            .spacing(8)
            .child(
                Element::label("确定 45%")
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(Element::progress(prog).width_match())
            .child(
                Element::label("不确定（忙碌动画）")
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(Element::progress_indeterminate().width_match()),
    );

    let cp_21 = Element::card(
        "数字步进",
        Element::col()
            .width_match()
            .spacing(10)
            .child(Element::field(
                "数量",
                Element::stepper(qty, 0.0, 99.0, 1.0).width(120),
            ))
            .child(Element::field(
                "缩放",
                Element::stepper(zoom, 0.5, 3.0, 0.25).width(120),
            )),
    );

    // 颜色选择器：面板走锚定浮层，会盖住卡片下方的内容——这正是它与普通子节点的分别。
    let brand_color = signal(Color::hex(0x4C8BF5));
    let pen_color = signal(Color::rgba(0xE0, 0x31, 0x31, 0xCC));
    let cp_27 = Element::card(
        "颜色选择器（锚定浮层面板：SV 方块 + 色相/透明度条 + HEX + 预设）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(Element::field("主题色", Element::color_picker(brand_color)))
            .child(Element::field(
                "画笔色（工具栏形态：只留色块）",
                Element::color_picker_opts(
                    pen_color,
                    ColorPickerOpts::default().trigger_text(false).presets(vec![
                        Color::hex(0x000000),
                        Color::hex(0xE03131),
                        Color::hex(0x1971C2),
                        Color::hex(0x2F9E44),
                        Color::hex(0xF5A524),
                        Color::hex(0x6741D9),
                    ]),
                ),
            ))
            .child(
                Element::label_signal(
                    brand_color.map(|c| format!("拖面板即时联动：主题色 = {}", c.to_hex_string())),
                )
                .font_size(12.0)
                .fg_role(Role::TextMuted)
                .width_match()
                .height(20),
            ),
    );

    let cp_22 = Element::card(
        "列表",
        Element::list(
            vec![
                "收件箱",
                "已发送",
                "草稿箱",
                "垃圾邮件",
                "归档",
                "重要",
                "已加星标",
            ],
            picked,
        )
        .height(160)
        .width_match()
        .bg_role(Role::SurfaceAlt)
        .corner(8.0),
    );

    let cp_23 = Element::card(
        "禁用态（核心统一管理：不可交互 + 置灰 + 跳 Tab）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(Element::field(
                "按钮",
                Element::button("不可点").disabled(true),
            ))
            .child(Element::field(
                "开关",
                Element::switch(signal(true)).disabled(true),
            ))
            .child(Element::field(
                "勾选",
                Element::checkbox("已禁用", signal(true)).disabled(true),
            ))
            .child(Element::field(
                "滑块",
                Element::slider(signal(0.5)).disabled(true).width_match(),
            ))
            .child(Element::field(
                "下拉",
                Element::dropdown(vec!["选项 A", "选项 B"], signal(0))
                    .disabled(true)
                    .width_match(),
            ))
            .child(Element::field(
                "步进",
                Element::stepper(signal(3.0), 0.0, 9.0, 1.0)
                    .disabled(true)
                    .width(120),
            ))
            .child(Element::field(
                "输入",
                Element::text_input(signal("只读内容".into()), "")
                    .disabled(true)
                    .width_match(),
            )),
    );

    let cp_24 = Element::card(
        "链接（链接色 + 下划线 + 悬停手型，点击/回车激活）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(
                Element::link("打开 windui 官网（用系统浏览器）")
                    .url("https://example.com")
                    .font_size(14.0),
            )
            .child(
                Element::row()
                    .spacing(20)
                    .cross(Align::Center)
                    .child(
                        Element::link("无下划线样式")
                            .underline(false)
                            .font_size(14.0),
                    )
                    .child(
                        Element::link("已禁用链接")
                            .url("https://example.com")
                            .disabled(true)
                            .font_size(14.0),
                    ),
            )
            .child(
                Element::link("点我计数（自定义 on_click）")
                    .font_size(14.0)
                    .on_click(move |_| {
                        ln.set(ln.get() + 1);
                        lm.set(format!("已点击 {} 次", ln.get()));
                    }),
            )
            .child(
                Element::label_signal(link_msg)
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            ),
    );

    let cp_25 = Element::card(
            "标签省略（max_lines + truncate）",
            Element::col()
                .width_match()
                .spacing(8)
                .child(Element::field("End", Element::label("这是一段很长很长的文本，用来演示末尾省略号效果，超出部分会被截断显示为 …").max_lines(1).truncate(Truncate::End).fg_role(Role::Text).weight(1.0)))
                .child(Element::field("Start", Element::label("这是一段很长很长的文本，用来演示开头省略号效果，超出部分会在开头显示为 …").max_lines(1).truncate(Truncate::Start).fg_role(Role::Text).weight(1.0)))
                .child(Element::field("Middle", Element::label("这是一段很长很长的文本，用来演示中间省略号效果，超出部分在中间被截断显示为 …").max_lines(1).truncate(Truncate::Middle).fg_role(Role::Text).weight(1.0)))
                .child(Element::field("2行裁剪", Element::label("行一：这是第一行内容。\n行二：这是第二行内容。\n行三：这一行被 max_lines(2) 裁剪不显示。").max_lines(2).fg_role(Role::Text).weight(1.0))),
        );

    let cp_26 = Element::card(
            "虚拟滚动列表 virtual_list（10 万行；只构建视口内那几行，滚多远都不变慢）",
            Element::col()
                .width_match()
                .spacing(8)
                .child(
                    Element::label(
                        "常规列表每行都是真实节点，行数一多建树/重排/绘制三条 O(N) 成本一条都躲不掉，\
                         且滚一格就全付一遍。虚拟列表两端用空占位撑出未渲染部分的高度，滚动条与钳制照常正确。",
                    )
                    .font_size(12.5)
                    .fg_role(Role::TextMuted)
                    .width_match(),
                )
                .child(
                    // 数据是 Vec<usize>，行文本在构建行时才生成——不必预先格式化十万个 String。
                    Element::virtual_list(huge_rows, 32, |i, v: usize| {
                        Element::hstack()
                            .width_match()
                            .padding_xy(10, 0)
                            .child(
                                Element::label(format!("{:>6}", i + 1))
                                    .font_size(12.5)
                                    .fg_role(Role::TextMuted)
                                    .width(54),
                            )
                            .child(
                                Element::label(format!("第 {} 项 · 值 {}", i + 1, v * 7 % 100_000))
                                    .font_size(13.5)
                                    .weight(1.0),
                            )
                    })
                    .height(220),
                ),
        );

    // 横向 / 双向滚动：容器仍纵向堆叠子节点，`scroll_axis` 决定哪个方向不设上限、可滚动。
    // 滚轮：Shift+滚轮或触控板横滑滚横向；只能横滚的容器上，竖滚轮也转成横滚。
    let mut strip = Element::hstack().padding_xy(2, 4);
    for i in 1..=18 {
        strip = strip.child(Element::button(format!("标签 {i}")).outline());
    }
    let mut grid = Element::col().spacing(4);
    for r in 0..14 {
        let mut line = Element::row().spacing(4);
        for c in 0..12 {
            line = line.child(
                Element::label(format!("{}{}", (b'A' + c as u8) as char, r + 1))
                    .font_size(12.5)
                    .text_align(Align::Center)
                    .width(64)
                    .height(28)
                    .bg_role(Role::SurfaceAlt)
                    .border_role(Role::Divider, 1),
            );
        }
        grid = grid.child(line);
    }
    let cp_28 = Element::card(
        "横向 / 双向滚动（scroll_axis）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(
                Element::label("Horizontal：一行标签放不下就横向滚，竖滚轮在这里也会转成横滚。")
                    .font_size(12.5)
                    .fg_role(Role::TextMuted),
            )
            .child(
                Element::scroll()
                    .scroll_axis(ScrollAxis::Horizontal)
                    .width_match()
                    .child(strip),
            )
            .child(
                Element::label(
                    "Both：表格两个方向都溢出，右侧与底部各一条滚动条，在右下角互相让位。",
                )
                .font_size(12.5)
                .fg_role(Role::TextMuted),
            )
            .child(
                Element::scroll()
                    .scroll_axis(ScrollAxis::Both)
                    .width_match()
                    .height(180)
                    .child(grid),
            ),
    );

    let im_1 = Element::card(
        "适配模式（源图 4:3）",
        Element::hstack()
            .child(img_cell(
                "Contain",
                Element::image_rgba(64, 48, &grad).fit(Fit::Contain),
            ))
            .child(img_cell(
                "Cover",
                Element::image_rgba(64, 48, &grad).fit(Fit::Cover),
            ))
            .child(img_cell(
                "Fill",
                Element::image_rgba(64, 48, &grad).fit(Fit::Fill),
            )),
    );

    let im_2 = Element::card(
        "圆角 & 占位 & 图标",
        Element::hstack()
            .child(img_cell(
                "圆角",
                Element::image_rgba(64, 48, &grad)
                    .fit(Fit::Cover)
                    .corner(12.0),
            ))
            .child(img_cell("占位", Element::image("不存在.png")))
            .child(Element::button("新建").icon_rgba(64, 48, &grad))
            .child(
                Element::button("禁用")
                    .icon_rgba(64, 48, &grad)
                    .disabled(true),
            ),
    );

    let im_3 = Element::card(
        "SVG 矢量（resvg）",
        Element::hstack()
            .child(img_cell(
                "渐变圆",
                Element::image_svg(SVG_CIRCLE, Some(120)).fit(Fit::Contain),
            ))
            .child(img_cell(
                "着色对勾",
                Element::image_svg(SVG_CHECK, Some(64))
                    .fit(Fit::Contain)
                    .tint(Color::hex(0x4C8BF5)),
            ))
            .child(Element::button("SVG 图标").icon_svg(SVG_CHECK, Some(32))),
    );

    let tb_1 = Element::card(
        "数据表格 table（固定表头 + 滚动 + 斑马纹 + 行悬停高亮）",
        Element::table(
            vec![("字符", 1.0), ("半角", 1.0), ("全角", 1.0)],
            vec![
                vec!["!", "!", "！"],
                vec!["@", "@", "＠"],
                vec!["#", "#", "＃"],
                vec!["$", "￥", "￥"],
            ],
        )
        .height(160),
    );

    let tb_2 = Element::card(
        "可排序表格 table_sortable（点表头循环 无→升→降；数值列按数值比较）",
        Element::table_sortable(file_cols(), file_rows(), signal(Some(SortKey::asc(0))))
            .height(200),
    );

    let tb_3 = Element::card(
        "操作列 .actions（末列自定义控件：查看/编辑/删除；回调按原始行下标绑定，排序后仍正确）",
        Element::table_sortable(
            // 窄窗下用两数据列 + 操作列，避免挤压换行；操作列做法与列数无关。
            vec![("名称", 2.0), ("大小(KB)", 1.0)],
            file_rows().into_iter().map(|r| vec![r[0], r[1]]).collect(),
            signal(Some(SortKey::asc(0))),
        )
        // 尾列由闭包按行生成按钮组；row 为原始行下标（Copy），各按钮 move 捕获它绑定回调。
        // 用 .small() 紧凑按钮，让三枚操作按钮在窄列内并排不溢出。
        .actions("操作", 2.6, |row| {
            Element::hstack()
                .child(
                    Element::button("查看")
                        .neutral()
                        .outline()
                        .small()
                        .on_click(move |ctx| ctx.toast(format!("查看第 {} 行", row + 1))),
                )
                .child(
                    Element::button("编辑")
                        .outline()
                        .small()
                        .on_click(move |ctx| ctx.toast(format!("编辑第 {} 行", row + 1))),
                )
                .child(
                    Element::button("删除")
                        .danger()
                        .outline()
                        .small()
                        .on_click(move |ctx| ctx.toast_err(format!("删除第 {} 行", row + 1))),
                )
        })
        .height(200),
    );

    let tb_4 = Element::card(
        "自定义单元格 .cell_render（首列边框徽章、末列彩色标签；返回 None 的列走默认文本）",
        Element::table_sortable(
            vec![("编码", 1.0), ("词条", 2.0), ("类型", 1.0)],
            vec![
                vec!["bj", "北京", "置顶"],
                vec!["sh", "上海", "删除"],
                vec!["gz", "广州", "置顶"],
            ],
            signal(None),
        )
        // 按 (行, 列, 文本) 逐格询问：Some=自定义控件，None=默认文本。排序仍按文本值。
        .cell_render(|_row, col, text| match col {
            0 => Some(
                Element::label(text)
                    .font_size(12.5)
                    .fg_role(Role::TextMuted)
                    .padding_xy(6, 2)
                    .corner(4.0)
                    .border_role(Role::Border, 1),
            ),
            2 => {
                let role = if text == "置顶" {
                    Role::Accent
                } else {
                    Role::TextMuted
                };
                Some(
                    Element::label(text)
                        .font_size(11.0)
                        .fg_role(role)
                        .padding_xy(6, 2)
                        .corner(4.0)
                        .border_role(role, 1),
                )
            }
            _ => None,
        })
        .height(200),
    );

    let tb_5 =
        Element::card(
            "可排序 + 多选 table_selectable（复选框首列 + 全选三态 + 选中行高亮）",
            Element::col()
                .width_match()
                .spacing(8)
                .child(
                    Element::table_selectable(
                        file_cols(),
                        file_rows(),
                        sel.clone(),
                        signal(Some(SortKey::asc(0))),
                    )
                    .height(200),
                )
                .child({
                    // 底部统计：点击"刷新选中数"读取选中集并写入动态标签（演示 app 读取选择）。
                    let (sel_c, msg) = (sel.clone(), sel_count);
                    Element::hstack()
                        .width_match()
                        .child(Element::button("刷新选中数").neutral().outline().on_click(
                            move |_| {
                                let n = sel_c.iter().filter(|s| s.get()).count();
                                msg.set(format!("已选 {n} 项"));
                            },
                        ))
                        .child(
                            Element::label_signal(sel_count)
                                .font_size(13.0)
                                .fg_role(Role::TextMuted)
                                .weight(1.0),
                        )
                }),
        );

    let tb_6 = Element::card(
        "服务端排序 table_sortable_server（前端不排序：点表头→回调重拉当前页）",
        {
            // 模拟「后端」全量数据（真实场景在服务器；此处放内存演示解耦流程）。
            let full: Vec<Vec<String>> = file_rows()
                .into_iter()
                .map(|r| r.into_iter().map(String::from).collect())
                .collect();
            // 「后端」按排序意图返回当前页（此处演示：全量排序后取全部；真实为 LIMIT/OFFSET）。
            let backend = move |s: Option<SortKey>| -> Vec<Vec<String>> {
                let mut rows = full.clone();
                if let Some(key) = s {
                    let col = key.column;
                    rows.sort_by(|a, b| {
                        let c = match (a[col].parse::<f64>(), b[col].parse::<f64>()) {
                            (Ok(x), Ok(y)) => {
                                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
                            }
                            _ => a[col].cmp(&b[col]),
                        };
                        if matches!(key.order, SortOrder::Desc) {
                            c.reverse()
                        } else {
                            c
                        }
                    });
                }
                rows
            };
            let sort = signal(Some(SortKey::asc(1)));
            let page = signal(backend(sort.get())); // 当前页数据信号
            Element::table_sortable_server(
                file_cols(),
                page,
                sort,
                move |_ctx, new_sort| page.set(backend(new_sort)), // 点表头→重拉
            )
            .height(200)
        },
    );

    let tb_7 = Element::card(
        "虚拟滚动表格 table_virtual（1 万行；表头/斑马纹/悬停高亮与普通表格一致）",
        Element::table_virtual(
            vec![("名称", 3.0), ("大小(KB)", 1.0), ("修改日期", 1.5)],
            huge_table,
            TABLE_ROW_H,
        )
        .height(240),
    );

    // ── 按控件族分组装页 ──
    // 卡片的 `let` 顺序与重排前完全一致（闭包捕获的 move 语义因此不变），
    // 分类只体现在下面的组装：原先「控件」一页塞了 26 张卡片，从拖拽重排到标签省略
    // 无所不包，找东西只能靠滚。现在按"这是哪一族控件"切成五页。
    let history_card = Element::card("历史记录（斑马纹行 + 滚动容器）", list.height(280));

    let forms = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .spacing(14)
            .child(sc_2)
            .child(sc_3)
            .child(sc_4)
            .child(sc_find)
            .child(cp_10)
            .child(cp_11)
            .child(cp_12)
            .child(cp_16)
            .child(cp_21)
            .child(cp_27)
            .child(cp_8),
    );

    let actions = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .spacing(14)
            .child(cp_4)
            .child(cp_6)
            .child(cp_7)
            .child(cp_5)
            .child(cp_19)
            .child(cp_20)
            .child(cp_23),
    );

    let cp_split = {
        let ratio = signal(0.4f32);
        let pane = |title: &str, tint: u32| {
            Element::col()
                .fill()
                .padding(10)
                .bg(Color::hex(tint))
                .child(Element::label(title.to_string()).width_match())
        };
        Element::card(
            "可拖动分栏 split（拖中间的分隔条，比例存在 Signal<f32> 里）",
            Element::col()
                .width_match()
                .spacing(8)
                .child(
                    Element::split(
                        Axis::Horizontal,
                        pane("左栏 · 占比跟随信号", 0xEAF2FB),
                        Element::split(
                            Axis::Vertical,
                            pane("右上", 0xFBF1E6),
                            pane("右下", 0xEDF7EC),
                            signal(0.5f32),
                        ),
                        ratio,
                    )
                    .width_match()
                    .height(160),
                )
                .child(
                    Element::hstack()
                        .width_match()
                        .child(Element::button("均分").small().on_click(move |ctx| {
                            ratio.set(0.5);
                            ctx.mark_dirty_all();
                        }))
                        .child(Element::label("比例钳在 0.1..=0.9；嵌套即多栏")),
                ),
        )
    };

    // 自由摆放 + 四角圆角：几何由数据算出的界面（卡片板一类）。卡片框占右半幅、最窄 140，
    // 高度存在信号里；贴着左侧"尾巴"的左下角切成直角，与尾巴无缝接上。
    let cp_place = {
        let card_h = signal(90);
        let board = Element::stack()
            .width_match()
            .height(190)
            .bg_role(Role::SurfaceAlt)
            .corner(8.0)
            // 尾巴：一段竖条接在卡片左下角外侧。
            .child(
                Element::leaf()
                    .bg_role(Role::Accent)
                    .place_fn(move |avail| {
                        let w = (avail.w / 2 - 16).max(140);
                        Rect::new(avail.w - w - 16 - 24, 20 + card_h.get() - 10, 24, 10)
                    }),
            )
            .child(
                Element::col()
                    .padding(10)
                    .spacing(4)
                    .bg_role(Role::Surface)
                    .border_role(Role::Accent, 1)
                    .corners(Corners {
                        bottom_left: 0.0,
                        ..Corners::all(12.0)
                    })
                    .child(Element::label("卡片框 place_fn"))
                    .child(Element::label("右半幅 · 左下直角").fg_role(Role::TextMuted))
                    .place_fn(move |avail| {
                        let w = (avail.w / 2 - 16).max(140);
                        Rect::new(avail.w - w - 16, 20, w, card_h.get())
                    }),
            )
            .child(
                Element::label("place(16, 16)")
                    .padding_xy(8, 4)
                    .bg_role(Role::Surface)
                    .corners(Corners::top(8.0))
                    .place(16, 16),
            );
        Element::card(
            "自由摆放 place / place_fn + 四角圆角 corners（几何由信号算出）",
            Element::col().width_match().spacing(8).child(board).child(
                Element::hstack()
                    .width_match()
                    .child(Element::button("拉高").small().on_click(move |_| {
                        card_h.update(|h| *h = (*h + 20).min(160));
                    }))
                    .child(Element::button("复位").small().on_click(move |_| {
                        card_h.set(90);
                    }))
                    .child(Element::label("高度在 Signal<i32> 里，改信号即重排")),
            ),
        )
    };

    let layouts = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .spacing(14)
            .child(cp_split)
            .child(cp_place)
            .child(cp_9)
            .child(cp_15)
            .child(cp_17)
            .child(cp_18),
    );

    let texts = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .spacing(14)
            .child(cp_13)
            .child(cp_14)
            .child(cp_25)
            .child(cp_3)
            .child(cp_24),
    );

    let data = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .spacing(14)
            .child(cp_22)
            .child(history_card)
            .child(cp_1)
            .child(cp_2)
            .child(cp_26)
            .child(cp_28)
            .child(tb_1)
            .child(tb_2)
            .child(tb_3)
            .child(tb_4)
            .child(tb_5)
            .child(tb_6)
            .child(tb_7),
    );

    let im_poly = Element::card(
        "多边形填充（凹形 / 挖洞 / 斜边 / 齿轮，一次 fill_polygon）",
        Element::leaf().widget(PolygonDemo),
    );

    let pics = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .spacing(14)
            .child(im_1)
            .child(im_2)
            .child(im_3)
            .child(im_poly),
    );

    let tab = signal(0usize);
    let dot = |hex: u32| ImageContent::from_rgba(16, 16, &solid(16, hex));
    let tabs = Element::tabs_icons(
        tab,
        vec![
            ("表单", dot(0x4C8BF5), forms),
            ("按钮", dot(0x2EC48B), actions),
            ("布局", dot(0x9B59B6), layouts),
            ("文字", dot(0xF5A623), texts),
            ("数据", dot(0x12B7F5), data),
            ("图片", dot(0xE0489D), pics),
            ("关于", dot(0xE5484D), about),
        ],
    );

    // 关于对话框
    let close = show_about;
    let dialog = Element::dialog(
        show_about,
        Element::col()
            .width(320)
            .bg_role(Role::Surface)
            .corner(14.0)
            .padding(22)
            .spacing(14)
            .child(
                Element::label("windui v0.1")
                    .font_size(20.0)
                    .fg_role(Role::Text)
                    .width_match(),
            )
            .child(
                Element::label("一个用 Rust 编写的轻量桌面 GUI 框架，适合做内存友好的小工具。")
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(
                Element::hstack()
                    .width_match()
                    .height(40)
                    .child(Element::label("").weight(1.0))
                    .child(Element::button("知道了").on_click(move |_| close.set(false))),
            ),
    );

    let body = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(18)
        .spacing(12)
        .child(page_title("控件总览", "七个分页按控件族分组，每页一族").height(34))
        // tabs 用 weight 占据标题以下的剩余高度（纵向 Match 会降级为 Wrap，需 weight 才填充）。
        .child(tabs.weight(1.0));

    let ui = Element::stack()
        .fill()
        .bg_role(Role::Bg)
        // 明暗切换从正文挪进标题栏：它是**整窗**开关，与页内的某一族控件无关，
        // 摆在正文标题行里会被读成"这一页的设置"。
        .child(
            Shell::new("综合示例")
                .trailing(theme_toggle(th, dark))
                .wrap(body),
        )
        .child(dialog);

    app.screenshot_from_args().content(ui).run();
}
