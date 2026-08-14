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
//!
//! **DPI 感知**（SVG）：`from_svg_bytes(bytes, None)` 保留矢量源，paint 期按 `dst`
//! 的**实际物理尺寸**重新光栅化并按该尺寸缓存——写死光栅宽的做法只在恰好等于该
//! 倍率的 DPI 下是 1:1，其余倍率都要经一次双线性重采样，细描边会被摊成灰边。

use std::any::Any;
use std::cell::RefCell;
use std::path::Path;
#[cfg(feature = "svg")]
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::Event;
use crate::geometry::{Color, Rect, Size};
use crate::render::image::{Fit, Image, VisualState, PLACEHOLDER_SIZE};
use crate::render::{Canvas, Paint};
use crate::style::{Role, Style};
use crate::text::TextEngine;

/// 占位框底色。语义是**中性的"此处没有可绘制内容"**，不是"出错了"：`paint_into`
/// 走到占位分支的条件只有"没有可用的图层"，它既覆盖解码失败，也覆盖调用方本就
/// 传了 `None`（图还没来）。用 `Danger` 会把后者误报成错误，故取次级表面 +
/// 常规边框——与卡片、表头等"空态容器"同源，换主题自动跟随。
const PLACEHOLDER_BG: Role = Role::SurfaceAlt;
/// 占位框边框角色。
const PLACEHOLDER_BORDER: Role = Role::Border;

/// 一层图片：原图 + 着色结果缓存（避免每帧重着色）。
struct Layer {
    raw: Image,
    tinted: RefCell<Option<Image>>,
}

impl Layer {
    fn new(raw: Image) -> Self {
        Self {
            raw,
            tinted: RefCell::new(None),
        }
    }
    /// 返回应绘制的图：无 tint 用原图；有 tint 取缓存（首次计算）。
    fn resolve(&self, tint: Option<Color>) -> Image {
        match tint {
            None => self.raw.clone(),
            Some(c) => self
                .tinted
                .borrow_mut()
                .get_or_insert_with(|| self.raw.tinted(c))
                .clone(),
        }
    }
}

/// DPI 感知的矢量源：保留 SVG 字节，按 paint 期算出的物理宽重新光栅化。
///
/// 缓存单条 `(物理宽, 结果)`：同一控件的物理宽只在 DPI 变化或布局改尺寸时才变，
/// 单条缓存即可命中每一帧；着色结果一并存入，避免每帧重跑 `tinted`。
/// 整体挂在 `svg` feature 上：无该 feature 时 `Image::from_svg_bytes` 不存在，
/// 类型留着也只会是无法构造、`bytes` 永不被读的死代码。
#[cfg(feature = "svg")]
struct SvgSource {
    bytes: Rc<[u8]>,
    cache: RefCell<Option<(u32, Image)>>,
}

#[cfg(feature = "svg")]
impl SvgSource {
    /// 取指定物理宽的光栅（含着色）结果；缓存未命中则重新光栅化。
    fn resolve(&self, target_w: u32, tint: Option<Color>) -> Option<Image> {
        let mut cache = self.cache.borrow_mut();
        if let Some((w, img)) = cache.as_ref() {
            if *w == target_w {
                return Some(img.clone());
            }
        }
        let raw = Image::from_svg_bytes(&self.bytes, Some(target_w)).ok()?;
        let img = match tint {
            Some(c) => raw.tinted(c),
            None => raw,
        };
        *cache = Some((target_w, img.clone()));
        Some(img)
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
    /// DPI 感知矢量源（仅 `from_svg_bytes(_, None)` 持有）。`base` 保留固有尺寸
    /// 光栅作 `intrinsic_size` 的度量依据与光栅失败时的回退。
    #[cfg(feature = "svg")]
    svg: Option<SvgSource>,
}

impl ImageContent {
    /// 持有解码结果（加载失败传 `None`，paint 时画占位框）。
    pub fn new(image: Option<Image>) -> Self {
        Self {
            base: image.map(Layer::new),
            overrides: Vec::new(),
            fit: Fit::default(),
            tint: None,
            #[cfg(feature = "svg")]
            svg: None,
        }
    }

    /// 便捷构造：从嵌入字节加载（失败画占位框）。
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::new(Image::from_bytes(bytes).ok())
    }
    /// 便捷构造：从文件路径加载。
    pub fn from_file(path: impl AsRef<Path>) -> Self {
        Self::new(Image::from_file(path).ok())
    }
    /// 便捷构造：从 SVG 字节光栅化（`svg` feature）。失败画占位框。
    ///
    /// - `None`（**推荐**）：**DPI 感知**——固有尺寸即逻辑尺寸，paint 期按实际物理
    ///   尺寸光栅化，任何 DPI 下都 1:1 无重采样。
    /// - `Some(w)`：写死光栅宽（逻辑尺寸随之变为 `w` dp）。只在需要精确控制光栅
    ///   分辨率时用；它在 `物理宽 != w` 的 DPI 下必然经历一次重采样。
    #[cfg(feature = "svg")]
    pub fn from_svg_bytes(bytes: &[u8], target_width: Option<u32>) -> Self {
        let mut c = Self::new(Image::from_svg_bytes(bytes, target_width).ok());
        if target_width.is_none() {
            c.svg = Some(SvgSource {
                bytes: Rc::from(bytes),
                cache: RefCell::new(None),
            });
        }
        c
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
        #[cfg(feature = "svg")]
        if let Some(s) = &self.svg {
            *s.cache.borrow_mut() = None;
        }
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

    /// 矢量源应光栅化到的**物理宽**：按 `fit` 求出图片实际绘制的逻辑宽，再乘 DPI。
    /// 与 `Canvas::draw_image` 的 fit 语义保持一致，使那里算出的缩放因子落在 1.0。
    #[cfg(feature = "svg")]
    fn svg_target_width(&self, dst: Rect, scale: f32) -> Option<u32> {
        let base = self.base.as_ref()?;
        let (iw, ih) = (base.raw.width() as f32, base.raw.height() as f32);
        if iw <= 0.0 || ih <= 0.0 || scale <= 0.0 {
            return None;
        }
        let (dw, dh) = (dst.w as f32, dst.h as f32);
        let draw_w = match self.fit {
            Fit::Fill => dw,
            Fit::Contain => iw * (dw / iw).min(dh / ih),
            Fit::Cover => iw * (dw / iw).max(dh / ih),
            // 1 图片像素 = 1 逻辑 dp。
            Fit::None => iw,
        };
        // 上限保护：异常布局下不至于光栅出巨图拖垮 paint。
        Some((draw_w * scale).round().clamp(1.0, 8192.0) as u32)
    }

    /// 按状态把图片绘制进 `dst`；无图则画占位框。圆角取 `style.corner_radius`，
    /// 与核心层给背景/边框画圆角同源。禁用等状态按 `VisualState::opacity` 调制。
    ///
    /// 持有矢量源时按 `dst` 的物理尺寸现场光栅化（见模块头 DPI 感知），使图标在
    /// 任意 DPI 下都 1:1 落像素；状态换图命中时用该状态的位图，不走矢量路径。
    pub fn paint_into(
        &self,
        dst: Rect,
        canvas: &mut dyn Canvas,
        style: &Style,
        state: VisualState,
    ) {
        if dst.is_empty() {
            return;
        }
        let radius = style.corner_radius;

        #[cfg(feature = "svg")]
        let vector = match self.overrides.iter().any(|(s, _)| *s == state) {
            true => None,
            false => self.svg.as_ref().and_then(|s| {
                self.svg_target_width(dst, canvas.dpi_scale())
                    .and_then(|w| s.resolve(w, self.tint))
            }),
        };
        #[cfg(not(feature = "svg"))]
        let vector: Option<Image> = None;

        match vector.or_else(|| self.layer_for(state).map(|l| l.resolve(self.tint))) {
            Some(img) => {
                canvas.draw_image(&img, dst, self.fit, radius, state.opacity());
            }
            None => {
                let (x, y, w, h) = (dst.x as f32, dst.y as f32, dst.w as f32, dst.h as f32);
                let th = crate::theme::current();
                canvas.fill_round_rect(
                    x,
                    y,
                    w,
                    h,
                    radius,
                    &Paint::fill(PLACEHOLDER_BG.resolve(&th)),
                );
                canvas.stroke_round_rect(
                    x,
                    y,
                    w,
                    h,
                    radius,
                    1.0,
                    &Paint::fill(PLACEHOLDER_BORDER.resolve(&th)),
                );
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
        Self {
            content: ImageContent::new(image),
        }
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
    fn paint(
        &self,
        _bounds: Rect,
        content: Rect,
        _focused: bool,
        _enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        // 独立图片控件无交互状态，按 Normal 绘制。
        self.content
            .paint_into(content, canvas, style, VisualState::Normal);
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
        assert_eq!(
            c.intrinsic_size(),
            Size::new(PLACEHOLDER_SIZE, PLACEHOLDER_SIZE)
        );
    }

    /// 20 网格 / 2px 描边的横线：网格与逻辑尺寸 1:1 时，描边正好盖满 2 个像素行。
    #[cfg(feature = "svg")]
    const BAR_SVG: &[u8] = br##"<svg viewBox="0 0 20 20" fill="none" xmlns="http://www.w3.org/2000/svg"><path d="M4 10h12" stroke="#000000" stroke-width="2"/></svg>"##;

    /// 矢量源的光栅宽须跟随 DPI——写死光栅宽只在恰好等于该倍率的 DPI 下是 1:1，
    /// 其余倍率都要经一次双线性重采样，细描边会被摊成灰边。
    #[cfg(feature = "svg")]
    #[test]
    fn svg_target_width_follows_dpi() {
        let c = ImageContent::from_svg_bytes(BAR_SVG, None);
        let dst = Rect::new(0, 0, 20, 20);
        assert_eq!(c.svg_target_width(dst, 1.0), Some(20));
        assert_eq!(c.svg_target_width(dst, 1.25), Some(25));
        assert_eq!(c.svg_target_width(dst, 1.5), Some(30));
        assert_eq!(c.svg_target_width(dst, 2.0), Some(40));
        // 写死光栅宽的构造不持有矢量源，不参与 DPI 感知。
        assert!(ImageContent::from_svg_bytes(BAR_SVG, Some(40))
            .svg
            .is_none());
    }

    /// 各 DPI 下描边都应落满整像素（出现纯色像素）。写死 2× 光栅的老做法在
    /// 1.0/1.5 下都会被降采样，一个纯色像素都拿不到。
    #[cfg(feature = "svg")]
    #[test]
    fn svg_stays_pixel_exact_across_dpi() {
        for scale in [1.0f32, 1.25, 1.5, 2.0] {
            let side = (40.0 * scale) as u32;
            let mut pm = Pixmap::new(side, side).unwrap();
            pm.fill(tiny_skia::Color::WHITE);
            let c = ImageContent::from_svg_bytes(BAR_SVG, None);
            {
                let mut te = crate::text::NullTextEngine;
                let mut canvas = SkiaCanvas::with_text(&mut pm, &mut te, scale);
                c.paint_into(
                    Rect::new(10, 10, 20, 20),
                    &mut canvas,
                    &Style::default(),
                    VisualState::Normal,
                );
            }
            let solid = pm
                .pixels()
                .iter()
                .filter(|p| p.red() == 0 && p.green() == 0 && p.blue() == 0 && p.alpha() == 255)
                .count();
            assert!(
                solid > 0,
                "scale={scale}：2px 描边应有纯色像素（按物理尺寸 1:1 光栅），实得 {solid}"
            );
        }
    }

    /// 着色变更须让矢量缓存失效，否则换主题后仍画旧颜色。
    #[cfg(feature = "svg")]
    #[test]
    fn svg_cache_invalidated_on_tint_change() {
        let c = ImageContent::from_svg_bytes(BAR_SVG, None).tint(Color::rgb(255, 0, 0));
        let img = c.svg.as_ref().unwrap().resolve(20, c.tint).unwrap();
        assert!(img.width() == 20, "应按请求的物理宽光栅");
        assert!(c.svg.as_ref().unwrap().cache.borrow().is_some());

        let mut c = c;
        c.set_tint(Color::rgb(0, 0, 255));
        assert!(
            c.svg.as_ref().unwrap().cache.borrow().is_none(),
            "改 tint 后矢量缓存应被清空"
        );
    }

    #[test]
    fn state_override_picks_dedicated_image() {
        // base 4×4，禁用态换成 8×8 专图。
        let base = Image::from_rgba(4, 4, &[10u8; 4 * 4 * 4]).unwrap();
        let disabled = Image::from_rgba(8, 8, &[20u8; 8 * 8 * 4]).unwrap();
        let c = ImageContent::new(Some(base)).on_state(VisualState::Disabled, disabled);
        // layer_for 命中覆盖 → 8×8；其余状态回退 base 4×4。
        assert_eq!(
            c.layer_for(VisualState::Disabled).unwrap().raw.size(),
            Size::new(8, 8)
        );
        assert_eq!(
            c.layer_for(VisualState::Hover).unwrap().raw.size(),
            Size::new(4, 4)
        );
    }

    /// 占位框须走主题角色，不得是模块级硬编码色——硬编码的淡灰在暗色主题下是块
    /// 近白方块，与周围深色卡片格格不入。亮/暗两套主题各画一次，断言底色/边框
    /// 恰为该主题的 `SurfaceAlt`/`Border`。
    #[test]
    fn placeholder_follows_theme() {
        use crate::theme::Theme;
        for (name, theme) in [("light", Theme::default()), ("dark", Theme::dark())] {
            let (bg, border) = (theme.palette.surface_alt, theme.palette.border);
            let surface = theme.palette.surface;
            crate::theme::set_current(std::rc::Rc::new(theme));
            let mut pm = Pixmap::new(60, 60).unwrap();
            // 底铺该主题的卡片表面色，模拟占位框实际所处的环境。
            pm.fill(tiny_skia::Color::from_rgba8(
                surface.r, surface.g, surface.b, 255,
            ));
            let c = ImageContent::new(None);
            {
                let mut canvas = SkiaCanvas::new(&mut pm);
                c.paint_into(
                    Rect::new(10, 10, 40, 40),
                    &mut canvas,
                    &Style::default(),
                    VisualState::Normal,
                );
            }
            let fill = pm.pixel(30, 30).unwrap();
            assert_eq!(
                (fill.red(), fill.green(), fill.blue()),
                (bg.r, bg.g, bg.b),
                "{name} 主题：占位框底色应为 SurfaceAlt"
            );
            // 左边框（x=10 这一列）：描边居中于边界，取 x=10、y 居中处。
            let stroke = pm.pixel(10, 30).unwrap();
            assert_eq!(
                (stroke.red(), stroke.green(), stroke.blue()),
                (border.r, border.g, border.b),
                "{name} 主题：占位框边框应为 Border"
            );
        }
        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
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
            c.paint_into(
                Rect::new(5, 5, 30, 30),
                &mut canvas,
                &Style::default(),
                VisualState::Disabled,
            );
        }
        let p = pm.pixel(20, 20).unwrap();
        assert!(
            p.green() > 120 && p.blue() > 120,
            "禁用应置灰混白，实得 g={} b={}",
            p.green(),
            p.blue()
        );
    }

    #[test]
    fn button_icon_widens_measure() {
        use crate::text::NullTextEngine;
        use crate::ui::Button;

        let style = Style::default();
        let mut te = NullTextEngine;
        let plain = Button::new("OK");
        let w0 = plain.measure(Size::ZERO, &style, &mut te).w;

        let mut iconed = Button::new("OK");
        iconed.set_icon(ImageContent::new(Image::from_rgba(4, 4, &[0u8; 64]).ok()));
        let w1 = iconed.measure(Size::ZERO, &style, &mut te).w;

        assert!(w1 > w0, "带图标按钮应更宽：w0={w0}, w1={w1}");
    }
}
