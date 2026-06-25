//! 图片控件示例：三种来源 × 四种 Fit × 圆角 × 占位框 × Button 图标。
//!
//! 运行：  cargo run --release --example image
//! 截屏：  cargo run --example image -- --screenshot artifacts/image.png
//!
//! 为免捆绑二进制资源，演示图均用 `image_rgba` 程序化生成。

use windui::prelude::*;

const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;
const CARD: u32 = 0xFFFFFF;
const BG: u32 = 0xEEF1F5;

/// 生成 w×h 的对角渐变 RGBA8（左上洋红 → 右下青）。
fn gradient(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / (w - 1).max(1) as f32;
            let fy = y as f32 / (h - 1).max(1) as f32;
            let r = (220.0 * (1.0 - fx)) as u8;
            let g = (200.0 * fy) as u8;
            let b = (220.0 * fx + 40.0) as u8;
            v.extend_from_slice(&[r, g, b, 255]);
        }
    }
    v
}

/// 生成一个简单的"加号"图标（透明底 + 白色十字），用于 Button 图标演示。
fn plus_icon(size: u32) -> Vec<u8> {
    let mut v = vec![0u8; (size * size * 4) as usize];
    let c = size / 2;
    let thick = (size / 6).max(1);
    for y in 0..size {
        for x in 0..size {
            let on = (x.abs_diff(c) <= thick) || (y.abs_diff(c) <= thick);
            if on {
                let i = ((y * size + x) * 4) as usize;
                v[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    v
}

/// 生成 size×size 纯色圆角图标（用作列表行图标）。
fn solid(size: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
    [r, g, b, 255].repeat((size * size) as usize)
}

fn card(title: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .bg(Color::hex(CARD))
        .corner(10.0)
        .padding(16)
        .spacing(10)
        .child(
            Element::label(title)
                .font_size(16.0)
                .fg(Color::hex(FG))
                .height(24)
                .width_match(),
        )
        .child(Element::divider())
        .child(body)
}

/// 一个带标题的固定框图片演示单元。
fn demo(label: &str, img: Element) -> Element {
    Element::col()
        .spacing(4)
        .child(
            img.width(96)
                .height(72)
                .bg(Color::hex(0xF6F8FA))
                .border(Color::hex(0xDDDDDD), 1),
        )
        .child(
            Element::label(label)
                .font_size(12.0)
                .fg(Color::hex(SUB))
                .height(16),
        )
}

fn main() {
    let grad = gradient(64, 48); // 4:3 源图，便于看出各 Fit 差异
    let icon = plus_icon(32);

    let fit_row = Element::row()
        .spacing(12)
        .child(demo(
            "Contain",
            Element::image_rgba(64, 48, &grad).fit(Fit::Contain),
        ))
        .child(demo(
            "Cover",
            Element::image_rgba(64, 48, &grad).fit(Fit::Cover),
        ))
        .child(demo(
            "Fill",
            Element::image_rgba(64, 48, &grad).fit(Fit::Fill),
        ))
        .child(demo(
            "None",
            Element::image_rgba(64, 48, &grad).fit(Fit::None),
        ));

    let corner_row = Element::row()
        .spacing(12)
        .cross(Align::Center)
        .child(demo(
            "圆角 12",
            Element::image_rgba(64, 48, &grad)
                .fit(Fit::Cover)
                .corner(12.0),
        ))
        .child(
            // 圆形头像：正方形 + 半边长圆角。
            Element::col()
                .spacing(4)
                .child(
                    Element::image_rgba(64, 48, &grad)
                        .fit(Fit::Cover)
                        .corner(36.0)
                        .width(72)
                        .height(72),
                )
                .child(
                    Element::label("圆形")
                        .font_size(12.0)
                        .fg(Color::hex(SUB))
                        .height(16),
                ),
        )
        .child(demo("占位(加载失败)", Element::image("不存在的文件.png")));

    // 状态：正常 / 禁用（背景与图标自动置灰）；以及单色图标着色。
    let state_row = Element::row()
        .spacing(12)
        .cross(Align::Center)
        .child(Element::button("新建").icon_rgba(32, 32, &icon))
        .child(
            Element::button("禁用")
                .icon_rgba(32, 32, &icon)
                .disabled(true),
        )
        .child(demo(
            "着色(accent)",
            // 白色加号模板 → 着成强调色（单色图标随主题/状态变色）。
            Element::image_content(
                ImageContent::from_rgba(32, 32, &icon)
                    .fit(Fit::Contain)
                    .tint(Color::hex(0x4C8BF5)),
            ),
        ));

    // 列表行图标：list_icons 让每行带前置图标（图标随选中/悬停状态调制）。
    let picked = signal(0usize);
    let icon_list = Element::list_icons(
        vec![
            (
                "收件箱",
                ImageContent::from_rgba(24, 24, &solid(24, 0x4C, 0x8B, 0xF5)),
            ),
            (
                "已发送",
                ImageContent::from_rgba(24, 24, &solid(24, 0x2E, 0xC4, 0x8B)),
            ),
            (
                "草稿箱",
                ImageContent::from_rgba(24, 24, &solid(24, 0xF5, 0xA6, 0x23)),
            ),
            (
                "垃圾箱",
                ImageContent::from_rgba(24, 24, &solid(24, 0xE5, 0x48, 0x4D)),
            ),
        ],
        picked,
    )
    .height(150)
    .width_match()
    .bg(Color::hex(0xF6F8FA))
    .corner(8.0);

    // SVG（矢量）：内联字面量含 `#` 颜色，故用 br##"..."## 定界。
    // 彩色渐变圆 + 单色对勾（对勾用于着色演示）。
    let svg_circle: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#ff6b9d"/><stop offset="1" stop-color="#4c8bf5"/></linearGradient></defs><circle cx="32" cy="32" r="28" fill="url(#g)"/></svg>"##;
    let svg_check: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path d="M9 16.2 4.8 12l-1.4 1.4L9 19 21 7l-1.4-1.4z" fill="#000000"/></svg>"##;

    let svg_row = Element::row()
        .spacing(12)
        .cross(Align::Center)
        // 固有尺寸光栅 vs 指定 2× 宽度光栅（矢量按需出清晰位图）。
        .child(demo(
            "SVG 固有",
            Element::image_svg(svg_circle, None).fit(Fit::Contain),
        ))
        .child(demo(
            "SVG 192px 光栅",
            Element::image_svg(svg_circle, Some(192)).fit(Fit::Contain),
        ))
        // 单色 SVG 模板着色（rgb 替换为强调色，保留 alpha）。
        .child(demo(
            "SVG 着色",
            Element::image_svg(svg_check, Some(64))
                .fit(Fit::Contain)
                .tint(Color::hex(0x4C8BF5)),
        ))
        .child(Element::button("SVG 图标").icon_svg(svg_check, Some(32)));

    // SVG 卡片内容；启用 svg-text feature 时追加一行内嵌文字演示（用系统字体渲染）。
    #[allow(unused_mut)]
    let mut svg_body = Element::col().spacing(10).child(svg_row);
    #[cfg(feature = "svg-text")]
    {
        // 含中文故用 str + as_bytes（字节串字面量不容非 ASCII）。
        let svg_text: &[u8] = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 220 40"><text x="6" y="29" font-size="26" font-family="Segoe UI, Arial" fill="#4c8bf5">Hello, SVG 文字</text></svg>"##.as_bytes();
        svg_body = svg_body.child(Element::divider()).child(
            Element::image_svg(svg_text, Some(440))
                .fit(Fit::Contain)
                .height(40)
                .width_match(),
        );
    }

    let body = Element::col()
        .width_match()
        .spacing(14)
        .child(card("适配模式（源图 4:3，框 96×72）", fit_row))
        .child(card("圆角裁剪 & 占位", corner_row))
        .child(card("状态：正常/禁用 + 单色图标着色", state_row))
        .child(card("SVG 矢量（resvg 光栅化 + 着色）", svg_body))
        .child(card("列表行图标（list_icons）", icon_list));

    let ui = Element::stack().fill().bg(Color::hex(BG)).child(
        Element::col()
            .fill()
            .padding(18)
            .spacing(12)
            .child(
                Element::label("图片支持")
                    .font_size(24.0)
                    .fg(Color::hex(0x1A1A2E))
                    .height(34)
                    .width_match(),
            )
            .child(Element::scroll().fill().child(body)),
    );

    App::new("windui — 图片示例", 480, 760)
        .bg(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
