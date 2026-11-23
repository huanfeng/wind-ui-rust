//! 拖出时跟着指针走的拖动图像：文档图标 + 首个文件名，多个文件时图标右上角带个数角标。
//!
//! 纯渲染（tiny-skia + 自带文字栈），不碰 X 连接，便于单测；上屏见 `dnd_source`。
//! 颜色取当前主题，与本程序界面一致。

use std::path::PathBuf;

use tiny_skia::Pixmap;

use crate::geometry::{Color, Rect};
use crate::render::{Canvas, Paint, SkiaCanvas};
use crate::spec::Align;
use crate::text::{LinuxTextEngine, TextEngine as _, TextStyle, WEIGHT_SEMIBOLD};
use crate::ui::text_fit::truncate_to_width;
use crate::ui::Truncate;

/// 逻辑尺寸。
const PAD: f32 = 8.0;
const ICON_W: f32 = 22.0;
const ICON_H: f32 = 28.0;
const FOLD: f32 = 7.0;
const GAP: f32 = 8.0;
const BADGE: f32 = 18.0;
const HEIGHT: f32 = 46.0;
const FONT: f32 = 13.0;
const MAX_TEXT_W: i32 = 200;
const RADIUS: f32 = 8.0;
/// 整体不透明度（只在有 ARGB 视觉时生效）：半透明能看见底下的落点。
const OPACITY: f32 = 0.88;

/// 渲染出的图像（预乘 RGBA，物理像素）。`rounded` = 能用透明像素（有 ARGB 视觉）：
/// 圆角 + 半透明；否则方角不透明——24 位窗口不认 alpha，透明处会显成黑色。
pub(super) fn render(paths: &[PathBuf], scale: f32, rounded: bool) -> Option<Pixmap> {
    let name = paths
        .first()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut engine = LinuxTextEngine::new();
    engine.set_scale(scale);
    let ts = TextStyle::new(FONT);

    // 先用 1×1 画布量出文字宽，再定图像宽。
    let mut probe = Pixmap::new(1, 1)?;
    let (text, text_w) = {
        let mut c = SkiaCanvas::with_text(&mut probe, &mut engine, scale);
        let (t, _) = truncate_to_width(&name, &mut c, &ts, MAX_TEXT_W, Truncate::Middle);
        let w = c.measure_text(&t, &ts).w;
        (t, w as f32)
    };
    let width = PAD * 2.0 + ICON_W + 4.0 + GAP + text_w;
    let pw = (width * scale).ceil() as u32;
    let ph = (HEIGHT * scale).ceil() as u32;
    let mut pm = Pixmap::new(pw, ph)?;

    let theme = crate::theme::current();
    let p = &theme.palette;
    {
        let mut c = SkiaCanvas::with_text(&mut pm, &mut engine, scale);
        let r = if rounded { RADIUS } else { 0.0 };
        c.fill_round_rect(0.0, 0.0, width, HEIGHT, r, &Paint::fill(p.surface));
        c.stroke_round_rect(
            0.5,
            0.5,
            width - 1.0,
            HEIGHT - 1.0,
            r,
            1.0,
            &Paint::fill(p.border),
        );

        let ix = PAD + 2.0;
        let iy = (HEIGHT - ICON_H) / 2.0;
        if paths.len() > 1 {
            // 身后再叠一张，表示「不止一个」。
            draw_doc(&mut c, ix + 3.0, iy - 2.0, p.border, p.surface_alt);
        }
        draw_doc(&mut c, ix, iy, p.text_muted, p.bg);

        let tx = PAD + ICON_W + 4.0 + GAP;
        c.draw_text(
            &text,
            Rect::new(tx as i32, 0, text_w.ceil() as i32 + 1, HEIGHT as i32),
            p.text,
            Align::Start,
            &ts,
        );

        if paths.len() > 1 {
            let label = if paths.len() > 99 {
                "99+".to_string()
            } else {
                paths.len().to_string()
            };
            let bts = TextStyle {
                weight: WEIGHT_SEMIBOLD,
                ..TextStyle::new(11.0)
            };
            let lw = (c.measure_text(&label, &bts).w as f32 + 8.0).max(BADGE);
            let bx = ix + ICON_W - lw / 2.0 + 2.0;
            let by = iy - 6.0;
            c.fill_round_rect(bx, by, lw, BADGE, BADGE / 2.0, &Paint::fill(p.accent));
            c.draw_text(
                &label,
                Rect::new(
                    bx.round() as i32,
                    by.round() as i32,
                    lw.round() as i32,
                    BADGE as i32,
                ),
                p.on_accent,
                Align::Center,
                &bts,
            );
        }
    }
    if rounded {
        fade(&mut pm, OPACITY);
    }
    Some(pm)
}

/// 折角文档图标：外轮廓用 `edge` 色、内里用 `fill` 色，再画折角与三道「文字行」。
fn draw_doc(c: &mut dyn Canvas, x: f32, y: f32, edge: Color, fill: Color) {
    let outline = |inset: f32| {
        [
            (x + inset, y + inset),
            (x + ICON_W - FOLD, y + inset),
            (x + ICON_W - inset, y + FOLD),
            (x + ICON_W - inset, y + ICON_H - inset),
            (x + inset, y + ICON_H - inset),
        ]
    };
    c.fill_polygon(&[&outline(0.0)], &Paint::fill(edge));
    c.fill_polygon(&[&outline(1.2)], &Paint::fill(fill));
    let fold = [
        (x + ICON_W - FOLD, y),
        (x + ICON_W - FOLD, y + FOLD),
        (x + ICON_W, y + FOLD),
    ];
    c.fill_polygon(&[&fold], &Paint::fill(edge));
    for i in 0..3 {
        let ly = y + 12.0 + i as f32 * 4.5;
        let lx1 = x + ICON_W - 5.0 - if i == 2 { 5.0 } else { 0.0 };
        c.draw_line(x + 5.0, ly, lx1, ly, 1.2, &Paint::fill(edge));
    }
}

/// 预乘像素整体乘不透明度。
fn fade(pm: &mut Pixmap, opacity: f32) {
    let k = (opacity.clamp(0.0, 1.0) * 256.0) as u32;
    for b in pm.data_mut() {
        *b = ((u32::from(*b) * k) >> 8) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one() -> Vec<PathBuf> {
        vec![PathBuf::from("/tmp/a.txt")]
    }

    #[test]
    fn empty_paths_render_nothing() {
        assert!(render(&[], 1.0, true).is_none());
    }

    #[test]
    fn size_follows_scale() {
        let a = render(&one(), 1.0, true).unwrap();
        let b = render(&one(), 2.0, true).unwrap();
        assert_eq!(a.height(), HEIGHT as u32);
        assert_eq!(b.height(), (HEIGHT * 2.0) as u32);
        assert!(b.width() >= a.width() * 2 - 2 && b.width() <= a.width() * 2 + 2);
    }

    #[test]
    fn long_names_are_capped() {
        let long = vec![PathBuf::from(format!(
            "/tmp/{}.txt",
            "很长的文件名".repeat(40)
        ))];
        let pm = render(&long, 1.0, true).unwrap();
        let max = PAD * 2.0 + ICON_W + 4.0 + GAP + MAX_TEXT_W as f32;
        assert!(
            pm.width() as f32 <= max.ceil(),
            "宽 {} 超过上限 {max}",
            pm.width()
        );
    }

    /// 角标用主题强调色：多个文件时图像里有强调色像素，单个文件时没有。
    #[test]
    fn badge_only_for_multiple_files() {
        let accent = crate::theme::current().palette.accent;
        let has_accent = |pm: &Pixmap| {
            pm.pixels().iter().any(|px| {
                let c = px.demultiply();
                c.alpha() == 255
                    && c.red() == accent.r
                    && c.green() == accent.g
                    && c.blue() == accent.b
            })
        };
        let two = vec![PathBuf::from("/tmp/a.txt"), PathBuf::from("/tmp/b.txt")];
        // 不透明版才能逐色比对（半透明版颜色被乘过）。
        assert!(has_accent(&render(&two, 1.0, false).unwrap()));
        assert!(!has_accent(&render(&one(), 1.0, false).unwrap()));
    }

    #[test]
    fn square_variant_is_fully_opaque() {
        let pm = render(&one(), 1.0, false).unwrap();
        assert!(pm.pixels().iter().all(|p| p.alpha() == 255));
    }
}
