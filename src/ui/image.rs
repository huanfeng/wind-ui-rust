//! 图片内容原语与独立图片控件。
//!
//! `ImageContent` 是可被任意控件嵌入的纯内容原语（不碰树）：封装"固有尺寸 +
//! 绘制 + 失败占位"。`ImageView` 是它的薄包装控件；其它控件（如 Button 图标）
//! 把 `ImageContent` 当字段持有、在自己的 paint 里调 `paint_into` 即可长出图片能力。

use std::any::Any;

use crate::core::EventCtx;
use crate::event::Event;
use crate::geometry::{Color, Rect, Size};
use crate::render::image::{Fit, Image, PLACEHOLDER_SIZE};
use crate::render::{Canvas, Paint};
use crate::style::Style;
use crate::text::TextEngine;
use crate::core::Widget;

/// 占位框背景色（淡灰）。
const PLACEHOLDER_BG: Color = Color::rgb(0xEE, 0xEE, 0xEE);
/// 占位框边框色。
const PLACEHOLDER_BORDER: Color = Color::rgb(0xCC, 0xCC, 0xCC);

/// 可复用图片内容原语：解码结果 + 适配模式。圆角由消费方传入的 `Style.corner_radius` 决定。
pub struct ImageContent {
    image: Option<Image>,
    fit: Fit,
}

impl ImageContent {
    /// 持有解码结果（加载失败传 `None`，paint 时画占位框）。
    pub fn new(image: Option<Image>) -> Self {
        Self { image, fit: Fit::default() }
    }

    /// 设置适配缩放模式。
    pub fn fit(mut self, fit: Fit) -> Self {
        self.fit = fit;
        self
    }

    /// 当前适配模式（消费方按需读取）。
    pub fn fit_mode(&self) -> Fit {
        self.fit
    }

    /// 是否成功持有图片。
    pub fn is_loaded(&self) -> bool {
        self.image.is_some()
    }

    /// 固有逻辑尺寸：有图返回像素尺寸；无图返回占位默认尺寸（防布局塌陷）。
    pub fn intrinsic_size(&self) -> Size {
        match &self.image {
            Some(img) => img.size(),
            None => Size::new(PLACEHOLDER_SIZE, PLACEHOLDER_SIZE),
        }
    }

    /// 把图片绘制进 `dst`；无图则画占位框。圆角取 `style.corner_radius`，
    /// 与核心层给背景/边框画圆角同源。
    pub fn paint_into(&self, dst: Rect, canvas: &mut dyn Canvas, style: &Style) {
        if dst.is_empty() {
            return;
        }
        let radius = style.corner_radius;
        match &self.image {
            Some(img) => canvas.draw_image(img, dst, self.fit, radius),
            None => {
                let (x, y, w, h) = (dst.x as f32, dst.y as f32, dst.w as f32, dst.h as f32);
                canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(PLACEHOLDER_BG));
                canvas.stroke_round_rect(x, y, w, h, radius, 1.0, &Paint::fill(PLACEHOLDER_BORDER));
            }
        }
    }
}

/// 独立图片控件：`ImageContent` 的薄包装。
pub struct ImageView {
    content: ImageContent,
}

impl ImageView {
    /// 由解码结果构造（失败传 `None`）。
    pub fn new(image: Option<Image>) -> Self {
        Self { content: ImageContent::new(image) }
    }

    /// 设置适配模式（供 Builder 的 `.fit()` 调用）。
    pub fn set_fit(&mut self, fit: Fit) {
        self.content.fit = fit;
    }
}

impl Widget for ImageView {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        self.content.intrinsic_size()
    }
    fn paint(&self, _bounds: Rect, content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        self.content.paint_into(content, canvas, style);
    }
    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::SkiaCanvas;
    use tiny_skia::Pixmap;

    #[test]
    fn loaded_content_reports_pixel_size() {
        let img = Image::from_rgba(6, 8, &[0u8; 6 * 8 * 4]).unwrap();
        let c = ImageContent::new(Some(img));
        assert!(c.is_loaded());
        assert_eq!(c.intrinsic_size(), Size::new(6, 8));
    }

    #[test]
    fn missing_content_uses_placeholder_size() {
        let c = ImageContent::new(None);
        assert!(!c.is_loaded());
        assert_eq!(c.intrinsic_size(), Size::new(PLACEHOLDER_SIZE, PLACEHOLDER_SIZE));
    }

    #[test]
    fn placeholder_paints_visible_box() {
        let mut pm = Pixmap::new(60, 60).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let c = ImageContent::new(None);
        {
            let mut canvas = SkiaCanvas::new(&mut pm);
            c.paint_into(Rect::new(10, 10, 40, 40), &mut canvas, &Style::default());
        }
        // 占位框内部应为淡灰（非纯白），证明确有绘制。
        let p = pm.pixel(30, 30).unwrap();
        assert!(
            p.red() < 250 && p.green() < 250 && p.blue() < 250,
            "占位框应可见，实得 ({},{},{})",
            p.red(),
            p.green(),
            p.blue()
        );
    }

    #[test]
    fn button_icon_widens_measure() {
        use crate::ui::Button;
        use crate::text::NullTextEngine;

        let style = Style::default();
        let mut te = NullTextEngine;
        let plain = Button::new("OK".into());
        let w0 = plain.measure(Size::ZERO, &style, &mut te).w;

        let mut iconed = Button::new("OK".into());
        iconed.set_icon(ImageContent::new(Image::from_rgba(4, 4, &[0u8; 64]).ok()));
        let w1 = iconed.measure(Size::ZERO, &style, &mut te).w;

        assert!(w1 > w0, "带图标按钮应更宽：w0={w0}, w1={w1}");
    }
}
