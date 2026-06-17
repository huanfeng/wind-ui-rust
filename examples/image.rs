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

fn card(title: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .bg(Color::hex(CARD))
        .corner(10.0)
        .padding(16)
        .spacing(10)
        .child(Element::label(title).font_size(16.0).fg(Color::hex(FG)).height(24).width_match())
        .child(Element::divider())
        .child(body)
}

/// 一个带标题的固定框图片演示单元。
fn demo(label: &str, img: Element) -> Element {
    Element::col()
        .spacing(4)
        .child(img.width(96).height(72).bg(Color::hex(0xF6F8FA)).border(Color::hex(0xDDDDDD), 1))
        .child(Element::label(label).font_size(12.0).fg(Color::hex(SUB)).height(16))
}

fn main() {
    let grad = gradient(64, 48); // 4:3 源图，便于看出各 Fit 差异
    let icon = plus_icon(32);

    let fit_row = Element::row()
        .spacing(12)
        .child(demo("Contain", Element::image_rgba(64, 48, &grad).fit(Fit::Contain)))
        .child(demo("Cover", Element::image_rgba(64, 48, &grad).fit(Fit::Cover)))
        .child(demo("Fill", Element::image_rgba(64, 48, &grad).fit(Fit::Fill)))
        .child(demo("None", Element::image_rgba(64, 48, &grad).fit(Fit::None)));

    let corner_row = Element::row()
        .spacing(12)
        .cross(Align::Center)
        .child(demo("圆角 12", Element::image_rgba(64, 48, &grad).fit(Fit::Cover).corner(12.0)))
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
                .child(Element::label("圆形").font_size(12.0).fg(Color::hex(SUB)).height(16)),
        )
        .child(demo("占位(加载失败)", Element::image("不存在的文件.png")));

    let button_row = Element::row()
        .spacing(12)
        .cross(Align::Center)
        .child(Element::button("新建").icon_rgba(32, 32, &icon))
        .child(Element::button("无图标按钮"));

    let body = Element::col()
        .width_match()
        .spacing(14)
        .child(card("适配模式（源图 4:3，框 96×72）", fit_row))
        .child(card("圆角裁剪 & 占位", corner_row))
        .child(card("控件内嵌图标（Button）", button_row));

    let ui = Element::stack().fill().bg(Color::hex(BG)).child(
        Element::col()
            .fill()
            .padding(18)
            .spacing(12)
            .child(Element::label("图片支持").font_size(24.0).fg(Color::hex(0x1A1A2E)).height(34).width_match())
            .child(Element::scroll().fill().child(body)),
    );

    App::new("windui — 图片示例", 480, 520)
        .bg(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
