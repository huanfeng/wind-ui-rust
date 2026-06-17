//! 图片内容原语与独立图片控件。
//!
//! `ImageContent` 是可被任意控件嵌入的纯内容原语（不碰树）：封装"固有尺寸 +
//! 状态感知绘制 + 失败占位"。`ImageView` 是它的薄包装控件；其它控件（如 Button
//! 图标）把 `ImageContent` 当字段持有、在自己的 paint 里调 `paint_into` 即可长出
//! 图片能力。
//!
//! 状态处理（与控件解耦）：原语不认识控件状态枚举，只接受通用 `VisualState`：
//! - **调制**：按状态调不透明度（禁用置灰，见 `VisualState::opacity`）。
//! - **着色**：可选 `tint`，把单色图标按颜色重着色（随主题/状态变色），结果按层缓存。
//! - **换图**：可选 `on_state` 覆盖表，特定状态用专图，否则回退基图。

use std::any::Any;
use std::cell::RefCell;
use std::path::Path;

use crate::core::{EventCtx, Widget};
use crate::event::Event;
use crate::geometry::{Color, Rect, Size};
use crate::render::image::{Fit, Image, VisualState, PLACEHOLDER_SIZE};
use crate::render::{Canvas, Paint};
use crate::style::Style;
use crate::text::TextEngine;

/// 占位框背景色（淡灰）。
const PLACEHOLDER_BG: Color = Color::rgb(0xEE, 0xEE, 0xEE);
/// 占位框边框色。
const PLACEHOLDER_BORDER: Color = Color::rgb(0xCC, 0xCC, 0xCC);

/// 一层图片：原图 + 着色结果缓存（避免每帧重着色）。
struct Layer {
    raw: Image,
    tinted: RefCell<Option<Image>>,
}

impl Layer {
    fn new(raw: Image) -> Self {
        Self { raw, tinted: RefCell::new(None) }
    }
    /// 返回应绘制的图：无 tint 用原图；有 tint 取缓存（首次计算）。
    fn resolve(&self, tint: Option<Color>) -> Image {
        match tint {
            None => self.raw.clone(),
            Some(c) => self.tinted.borrow_mut().get_or_insert_with(|| self.raw.tinted(c)).clone(),
        }
    }
}

/// 可复用图片内容原语：解码结果 + 适配模式 + 状态调制（着色/换图）。
/// 圆角由消费方传入的 `Style.corner_radius` 决定。
pub struct ImageContent {
    base: Option<Layer>,
    /// 状态换图覆盖（稀疏；命中则用专图，否则回退 base）。
    overrides: Vec<(VisualState, Layer)>,
    fit: Fit,
    /// 模板着色（单色图标随主题/状态变色）；None=按原色绘制。
    tint: Option<Color>,
}

impl ImageContent {
    /// 持有解码结果（加载失败传 `None`，paint 时画占位框）。
    pub fn new(image: Option<Image>) -> Self {
        Self { base: image.map(Layer::new), overrides: Vec::new(), fit: Fit::default(), tint: None }
    }

    /// 便捷构造：从嵌入字节加载（失败画占位框）。
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::new(Image::from_bytes(bytes).ok())
    }
    /// 便捷构造：从文件路径加载。
    pub fn from_file(path: impl AsRef<Path>) -> Self {
        Self::new(Image::from_file(path).ok())
    }
    /// 便捷构造：从原始 RGBA8。
    pub fn from_rgba(w: u32, h: u32, rgba: &[u8]) -> Self {
        Self::new(Image::from_rgba(w, h, rgba).ok())
    }

    /// 设置适配缩放模式。
    pub fn fit(mut self, fit: Fit) -> Self {
        self.fit = fit;
        self
    }
    /// 模板着色（单色图标随颜色变色）。彩色图请勿用，会丢失原色。
    pub fn tint(mut self, color: Color) -> Self {
        self.set_tint(color);
        self
    }
    /// 为某状态注册专用图片（状态换图）。
    pub fn on_state(mut self, state: VisualState, image: Image) -> Self {
        self.overrides.retain(|(s, _)| *s != state);
        self.overrides.push((state, Layer::new(image)));
        self
    }

    /// `&mut` 版着色设置（供 Builder 的 `.tint()` 调用）。着色色变更时清缓存。
    pub fn set_tint(&mut self, color: Color) {
        self.tint = Some(color);
        if let Some(l) = &self.base {
            *l.tinted.borrow_mut() = None;
        }
        for (_, l) in &self.overrides {
            *l.tinted.borrow_mut() = None;
        }
    }
    /// `&mut` 版适配模式设置。
    pub fn set_fit(&mut self, fit: Fit) {
        self.fit = fit;
    }

    /// 当前适配模式。
    pub fn fit_mode(&self) -> Fit {
        self.fit
    }
    /// 是否成功持有（基）图片。
    pub fn is_loaded(&self) -> bool {
        self.base.is_some()
    }

    /// 选取某状态应绘制的层：命中覆盖则用之，否则回退 base。
    fn layer_for(&self, state: VisualState) -> Option<&Layer> {
        self.overrides
            .iter()
            .find(|(s, _)| *s == state)
            .map(|(_, l)| l)
            .or(self.base.as_ref())
    }

    /// 固有逻辑尺寸：有图返回基图像素尺寸；无图返回占位默认尺寸（防布局塌陷）。
    pub fn intrinsic_size(&self) -> Size {
        match &self.base {
            Some(l) => l.raw.size(),
            None => Size::new(PLACEHOLDER_SIZE, PLACEHOLDER_SIZE),
        }
    }

    /// 按状态把图片绘制进 `dst`；无图则画占位框。圆角取 `style.corner_radius`，
    /// 与核心层给背景/边框画圆角同源。禁用等状态按 `VisualState::opacity` 调制。
    pub fn paint_into(&self, dst: Rect, canvas: &mut dyn Canvas, style: &Style, state: VisualState) {
        if dst.is_empty() {
            return;
        }
        let radius = style.corner_radius;
        match self.layer_for(state) {
            Some(layer) => {
                let img = layer.resolve(self.tint);
                canvas.draw_image(&img, dst, self.fit, radius, state.opacity());
            }
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
    /// 由预先组装好的内容原语构造（用于状态换图等高级用法）。
    pub fn from_content(content: ImageContent) -> Self {
        Self { content }
    }

    /// 设置适配模式（供 Builder 的 `.fit()` 调用）。
    pub fn set_fit(&mut self, fit: Fit) {
        self.content.set_fit(fit);
    }
    /// 设置模板着色（供 Builder 的 `.tint()` 调用）。
    pub fn set_tint(&mut self, color: Color) {
        self.content.set_tint(color);
    }
}

impl Widget for ImageView {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        self.content.intrinsic_size()
    }
    fn paint(&self, _bounds: Rect, content: Rect, _focused: bool, _enabled: bool, canvas: &mut dyn Canvas, style: &Style) {
        // 独立图片控件无交互状态，按 Normal 绘制。
        self.content.paint_into(content, canvas, style, VisualState::Normal);
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
    fn state_override_picks_dedicated_image() {
        // base 4×4，禁用态换成 8×8 专图。
        let base = Image::from_rgba(4, 4, &[10u8; 4 * 4 * 4]).unwrap();
        let disabled = Image::from_rgba(8, 8, &[20u8; 8 * 8 * 4]).unwrap();
        let c = ImageContent::new(Some(base)).on_state(VisualState::Disabled, disabled);
        // layer_for 命中覆盖 → 8×8；其余状态回退 base 4×4。
        assert_eq!(c.layer_for(VisualState::Disabled).unwrap().raw.size(), Size::new(8, 8));
        assert_eq!(c.layer_for(VisualState::Hover).unwrap().raw.size(), Size::new(4, 4));
    }

    #[test]
    fn placeholder_paints_visible_box() {
        let mut pm = Pixmap::new(60, 60).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let c = ImageContent::new(None);
        {
            let mut canvas = SkiaCanvas::new(&mut pm);
            c.paint_into(Rect::new(10, 10, 40, 40), &mut canvas, &Style::default(), VisualState::Normal);
        }
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
    fn disabled_state_dims_image() {
        // 红图在禁用态应被调淡（混入白底）。
        let mut pm = Pixmap::new(40, 40).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let img = Image::from_rgba(4, 4, &[255u8, 0, 0, 255].repeat(4 * 4)).unwrap();
        let c = ImageContent::new(Some(img)).fit(Fit::Fill);
        {
            let mut canvas = SkiaCanvas::new(&mut pm);
            c.paint_into(Rect::new(5, 5, 30, 30), &mut canvas, &Style::default(), VisualState::Disabled);
        }
        let p = pm.pixel(20, 20).unwrap();
        assert!(p.green() > 120 && p.blue() > 120, "禁用应置灰混白，实得 g={} b={}", p.green(), p.blue());
    }

    #[test]
    fn button_icon_widens_measure() {
        use crate::text::NullTextEngine;
        use crate::ui::Button;

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
