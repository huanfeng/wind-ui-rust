//! Phase 0 验证：纯色窗口 + 中心绘制一个矩形（直接操作 pixmap）。
//!
//! 运行窗口：    cargo run --example phase0_window
//! 截屏验证：    cargo run --example phase0_window -- --screenshot artifacts/phase0.png

use tiny_skia::{Paint, PathBuilder, Pixmap, Rect as SkRect, Transform};
use windui::prelude::*;

fn main() {
    App::new("Phase 0 — windui", 480, 320)
        .bg(Color::hex(0x2B2B3C))
        .screenshot_from_args()
        .on_render(|pixmap: &mut Pixmap, size: Size| {
            // 在中心画一个橙色圆角块，验证 tiny-skia 绘制 + 呈现链路。
            let w = size.w as f32;
            let h = size.h as f32;
            let rw = w * 0.5;
            let rh = h * 0.4;
            let rect = SkRect::from_xywh((w - rw) / 2.0, (h - rh) / 2.0, rw, rh).unwrap();
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            let path = pb.finish().unwrap();

            let mut paint = Paint::default();
            paint.set_color_rgba8(0xFF, 0x9F, 0x43, 0xFF);
            paint.anti_alias = true;
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                None,
            );
        })
        .run();
}
