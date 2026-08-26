//! 窗口/应用图标：像素载体 [`WindowIcon`] 与内置品牌图标 [`brand_icon`]。
//!
//! 图标不走 [`ImageContent`](crate::ui::ImageContent)：那一层是面向**绘制**的
//! （tint / fit / 状态换图），而窗口图标最终要交给 Win32 `CreateIconIndirect` 与
//! macOS `NSImage`，两边要的都是原始像素。让平台层直接收 RGBA，省一层剥壳，
//! 也不必把 UI 概念泄漏到平台边界。
//!
//! **尺寸不是调用方能定死的**：系统在不同场合要不同像素数（Windows 150% 缩放要 24/48，
//! 200% 要 32/64），所以 [`IconSource`] 允许把光栅化推迟到平台层知道要多大之后。
//! 内置的 [`brand_icon`] 走这条；只有一张 PNG 时退回 [`IconSource::Bitmap`]，由系统缩放。
//!
//! **平台差异**（两边都只需 `App::icon`，但落点不同）：
//! - Windows：`WM_SETICON` 设到窗口上，标题栏、Alt-Tab、任务栏各取所需尺寸。
//!   窗口类本身还会从 exe 资源加载图标（见 `platform/win32` 的 `register_window_class`），
//!   `App::icon` 覆盖它。
//! - macOS：NSWindow 没有窗口图标这个概念，落到**应用级** Dock 图标上。
//!   多窗口时最后设置的那个生效。

use std::rc::Rc;

use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// W 轮廓顶点（归一化到图标边长，y 向下，闭合多边形）。
///
/// 由 `scripts/gen_icon.py` 生成——那里是造型参数的唯一真相源。改造型要跑脚本重出，
/// 不要手改这里的数值：顶点是折线偏移求交 + 两次半平面裁剪的结果，手改必然破坏对称。
pub(crate) const W_OUTLINE: [(f32, f32); 13] = [
    (0.282615, 0.324857),
    (0.366258, 0.622255),
    (0.449901, 0.324857),
    (0.550099, 0.324857),
    (0.633742, 0.622255),
    (0.717385, 0.324857),
    (0.850000, 0.324857),
    (0.731795, 0.745143),
    (0.568106, 0.745143),
    (0.500000, 0.502987),
    (0.431894, 0.745143),
    (0.268205, 0.745143),
    (0.150000, 0.324857),
];

/// 底板圆角半径（占边长）。与 `scripts/gen_icon.py` 的 `CORNER` 同值。
const CORNER: f32 = 0.22;
/// 底板天蓝 `#1E90FF`。与 `scripts/gen_icon.py` 的 `BG` 同值。
const BRAND_RGB: (u8, u8, u8) = (0x1E, 0x90, 0xFF);

/// 圆弧的三次贝塞尔近似系数（1/4 圆的控制点偏移比例）。
const KAPPA: f32 = 0.552_284_8;

/// 窗口/应用图标的像素载体：**非预乘** RGBA8。
///
/// 非预乘是平台 API 的要求，不是随手选的：Win32 的 `CreateIconIndirect` 走 32bpp
/// DIB + alpha 通道，PNG 编码同样以非预乘为准。而 tiny-skia 的 `Pixmap` 是预乘的，
/// 所以 [`brand_icon`] 在收尾处反预乘一次。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowIcon {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl WindowIcon {
    /// 从非预乘 RGBA8 构造。长度必须是 `width * height * 4`，否则返回 `None`。
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let need = (width as usize)
            .checked_mul(height as usize)?
            .checked_mul(4)?;
        if width == 0 || height == 0 || rgba.len() != need {
            return None;
        }
        Some(Self {
            width,
            height,
            rgba,
        })
    }

    /// 从已解码的图片构造（PNG/SVG 等经 [`Image`](crate::render::image::Image) 解码后转出）。
    pub fn from_image(img: &crate::render::image::Image) -> Option<Self> {
        Self::from_rgba(img.width(), img.height(), img.to_rgba())
    }

    /// 像素宽。
    pub fn width(&self) -> u32 {
        self.width
    }
    /// 像素高。
    pub fn height(&self) -> u32 {
        self.height
    }
    /// 非预乘 RGBA8 像素。
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// 编码成 PNG 字节。macOS 侧靠它把图标喂给 `NSImage`——那边从 `NSData` 建图
    /// 只要 `NSImage` 一个 feature，比走 `NSBitmapImageRep` 少拉一整套绑定。
    ///
    /// Windows 侧走 `CreateIconIndirect` 直吃 RGBA，用不到它；留着而不是加 cfg 分叉，
    /// 是为了让 PNG 往返测试在两个平台上都跑得到——那条测试守的是 macOS 的图标路径。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn to_png(&self) -> Option<Vec<u8>> {
        let mut pm = Pixmap::new(self.width, self.height)?;
        for (dst, src) in pm.pixels_mut().iter_mut().zip(self.rgba.as_chunks::<4>().0) {
            *dst = tiny_skia::ColorU8::from_rgba(src[0], src[1], src[2], src[3]).premultiply();
        }
        pm.encode_png().ok()
    }
}

/// 图标来源：一张定死的位图，或一个"要多大就画多大"的生成器。
///
/// 这个区分是整个模块的重点。系统在**不同场合要不同尺寸**的图标：Windows 的
/// `GetSystemMetricsForDpi` 在 150% 缩放下要 24（小）与 48（大），200% 下要 32 与 64，
/// 而任务栏取的是**大**的那档。给一张固定位图，平台层就只能交给系统缩放——
/// 没有哪个固定尺寸能同时对上 40/48/56/64，选哪个都只是把模糊从一个 DPI 挪到另一个。
///
/// [`Sized`](Self::Sized) 把光栅化推迟到平台层**知道要多少物理像素之后**，每一档都是
/// 1:1 画出来的。内置的 [`brand_icon`] 就是这种。手上只有一张 PNG 时用
/// [`Bitmap`](Self::Bitmap)（`From<WindowIcon>` 自动转），行为退回"系统缩放"。
#[derive(Clone)]
pub enum IconSource {
    /// 固定位图。缩放交给系统——高 DPI 下会糊。
    Bitmap(WindowIcon),
    /// 按需求尺寸现画。平台层拿到实际要的物理像素数再光栅化，每档 1:1。
    Sized(Rc<dyn Fn(u32) -> WindowIcon>),
}

impl IconSource {
    /// 用一个"给尺寸出图标"的闭包建源。矢量或程序化图标走这条。
    pub fn sized(f: impl Fn(u32) -> WindowIcon + 'static) -> Self {
        Self::Sized(Rc::new(f))
    }

    /// 取 `size` 像素见方的图标。[`Bitmap`](Self::Bitmap) 分支忽略 `size` 直接返回那张
    /// ——调用方据此把缩放交给系统，本方法不做重采样。
    pub fn at(&self, size: u32) -> WindowIcon {
        match self {
            Self::Bitmap(icon) => icon.clone(),
            Self::Sized(f) => f(size.max(1)),
        }
    }

    /// 是否能按尺寸现画。平台层据此决定 DPI 变化时要不要重建图标——
    /// 固定位图重建也还是那一张，白费一次转换。
    pub fn is_sized(&self) -> bool {
        matches!(self, Self::Sized(_))
    }
}

impl From<WindowIcon> for IconSource {
    fn from(icon: WindowIcon) -> Self {
        Self::Bitmap(icon)
    }
}

impl std::fmt::Debug for IconSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bitmap(i) => f
                .debug_struct("IconSource::Bitmap")
                .field("width", &i.width())
                .field("height", &i.height())
                .finish(),
            Self::Sized(_) => f.write_str("IconSource::Sized(..)"),
        }
    }
}

/// 内置品牌图标（天蓝底 + 白色对称 W），**DPI 自适应**：平台按实际需要的物理像素
/// 现画，标题栏 / 任务栏 / Alt-Tab 每一档都是 1:1，换到别的缩放比的显示器也会重画。
///
/// 要一张具体尺寸的位图（比如喂给托盘的 `icon_rgba`）用 [`brand_icon_at`]。
///
/// ```no_run
/// use windui::prelude::*;
/// App::new("demo", 400, 300)
///     .icon(brand_icon())
///     .content(Element::col())
///     .run();
/// ```
pub fn brand_icon() -> IconSource {
    IconSource::sized(brand_icon_at)
}

/// 内置品牌图标的指定尺寸位图。
///
/// 每次调用都重画而不是缓存一张大图缩放——一次调用就是几万像素的填充，
/// 比缩放带来的模糊划算得多。
///
/// W 是几何构造的原创路径，**不取自任何字体**：字形轮廓受字体版权保护，随本 crate
/// 的 MIT/Apache 双许可分发有风险。造型参数与生成器见 `scripts/gen_icon.py`。
pub fn brand_icon_at(size: u32) -> WindowIcon {
    let size = size.max(1);
    let mut pm = Pixmap::new(size, size).expect("图标尺寸溢出");
    let s = size as f32;

    let mut paint = Paint {
        anti_alias: true,
        ..Default::default()
    };

    let (r, g, b) = BRAND_RGB;
    paint.set_color_rgba8(r, g, b, 255);
    if let Some(path) = rounded_rect(s, s * CORNER) {
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    paint.set_color_rgba8(255, 255, 255, 255);
    if let Some(path) = w_path(s) {
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    // Pixmap 是预乘的，反预乘还原成平台要的非预乘 RGBA8。
    let mut rgba = Vec::with_capacity((size as usize) * (size as usize) * 4);
    for px in pm.pixels() {
        let c = px.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    WindowIcon {
        width: size,
        height: size,
        rgba,
    }
}

/// 边长 `s`、圆角 `r` 的圆角矩形路径（四角用三次贝塞尔近似 1/4 圆）。
fn rounded_rect(s: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(s / 2.0);
    let k = r * KAPPA;
    let mut pb = PathBuilder::new();
    pb.move_to(r, 0.0);
    pb.line_to(s - r, 0.0);
    pb.cubic_to(s - r + k, 0.0, s, r - k, s, r);
    pb.line_to(s, s - r);
    pb.cubic_to(s, s - r + k, s - r + k, s, s - r, s);
    pb.line_to(r, s);
    pb.cubic_to(r - k, s, 0.0, s - r + k, 0.0, s - r);
    pb.line_to(0.0, r);
    pb.cubic_to(0.0, r - k, r - k, 0.0, r, 0.0);
    pb.close();
    pb.finish()
}

/// W 轮廓路径，按图标边长 `s` 缩放。
fn w_path(s: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    for (i, (x, y)) in W_OUTLINE.iter().enumerate() {
        if i == 0 {
            pb.move_to(x * s, y * s);
        } else {
            pb.line_to(x * s, y * s);
        }
    }
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W 轮廓必须左右严格对称：所有 x 关于 0.5 镜像后应与原集合逐一配对。
    ///
    /// 这条不是形式主义。生成器早期版本的末端外延是从**段起点**算的，走完外延量还没
    /// 出这一段，右上笔画就伸不出顶边、少切了一刀——渲染出来不报错，只是"看着有点怪"，
    /// 16px 下肉眼根本发现不了。顶点固化进代码后，这条断言是唯一还能抓住它的关卡。
    #[test]
    fn w_outline_is_mirror_symmetric() {
        let sorted = |f: fn(f32) -> f32| {
            let mut v: Vec<f32> = W_OUTLINE.iter().map(|(x, _)| f(*x)).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v
        };
        // 容差 1e-4 归一化单位 = 256px 图标上的 0.026px，比任何肉眼可见的偏斜都严格；
        // 用整数相等会被 f32 的末位截断误判（0.85 镜像后是 0.149999…）。
        for (a, b) in sorted(|x| x).iter().zip(sorted(|x| 1.0 - x).iter()) {
            assert!((a - b).abs() < 1e-4, "W 轮廓左右不对称：{a} vs {b}");
        }
    }

    /// 轮廓必须落在 0..1 内，否则会被底板裁掉或溢出图标。
    #[test]
    fn w_outline_within_unit_box() {
        for (x, y) in W_OUTLINE {
            assert!((0.0..=1.0).contains(&x), "x 越界: {x}");
            assert!((0.0..=1.0).contains(&y), "y 越界: {y}");
        }
    }

    #[test]
    fn brand_icon_has_expected_shape() {
        let icon = brand_icon_at(64);
        assert_eq!(icon.width(), 64);
        assert_eq!(icon.height(), 64);
        assert_eq!(icon.rgba().len(), 64 * 64 * 4);
    }

    /// 图标中心必须是白色的 W 笔画，四角必须透明（圆角外），边缘中点是品牌蓝。
    /// 只断言尺寸的话，一个全透明的空 Pixmap 也能过。
    #[test]
    fn brand_icon_paints_blue_plate_and_white_w() {
        let icon = brand_icon_at(64);
        let at = |x: usize, y: usize| {
            let i = (y * 64 + x) * 4;
            (
                icon.rgba()[i],
                icon.rgba()[i + 1],
                icon.rgba()[i + 2],
                icon.rgba()[i + 3],
            )
        };
        let is_white = |p: (u8, u8, u8, u8)| p.0 > 240 && p.1 > 240 && p.2 > 240 && p.3 == 255;
        let is_blue = |p: (u8, u8, u8, u8)| p == (0x1E, 0x90, 0xFF, 255);

        // 左上角在圆角之外 -> 完全透明
        assert_eq!(at(0, 0).3, 0, "圆角外应透明");
        // 上边中点落在底板上 -> 品牌蓝、不透明
        assert!(is_blue(at(32, 2)), "底板应为品牌蓝，实测 {:?}", at(32, 2));
        // 左外笔画内部（y=0.41 处该笔画横跨 x≈0.17..0.31，取其中点）
        assert!(is_white(at(15, 26)), "左笔画应为白，实测 {:?}", at(15, 26));
        // 中峰：x=0.5 上半段必须是白的 —— 这条断言守的是「中峰到顶」这个造型决定，
        // 中峰一旦下沉，这里立刻变成背景蓝。
        assert!(is_white(at(32, 25)), "中峰应到顶，实测 {:?}", at(32, 25));
        // 中峰下方是两谷之间的空隙，必须是底板蓝；若这里也白了说明 W 糊成了实心块。
        assert!(is_blue(at(32, 41)), "谷间应露底板，实测 {:?}", at(32, 41));
    }

    /// 任意尺寸都要能画出来，且小尺寸下 W 不能整个消失。
    #[test]
    fn brand_icon_survives_small_sizes() {
        for size in [1u32, 8, 16, 32, 256] {
            let icon = brand_icon_at(size);
            assert_eq!(icon.rgba().len(), (size as usize).pow(2) * 4);
            if size >= 16 {
                let white = icon
                    .rgba()
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200 && p[3] > 200)
                    .count();
                assert!(white > 0, "{size}px 下 W 消失了");
            }
        }
    }

    /// `Sized` 源必须**按请求尺寸出图**——这是整个 DPI 自适应的立身之本。
    /// 退化成"忽略尺寸永远返回同一张"的话，任务栏在 150%/200% 下照样是拉伸的，
    /// 而这个退化不会有任何编译错误或崩溃。
    #[test]
    fn sized_source_rasterizes_at_requested_size() {
        let src = brand_icon();
        assert!(src.is_sized());
        for px in [16u32, 24, 32, 48, 64] {
            let icon = src.at(px);
            assert_eq!(
                (icon.width(), icon.height()),
                (px, px),
                "Sized 源没按请求尺寸出图"
            );
        }
    }

    /// `Bitmap` 源**不重采样**：`at()` 忽略请求尺寸原样返回，缩放留给系统。
    /// 若它偷偷缩放，调用方就再也拿不到原始像素了。
    #[test]
    fn bitmap_source_ignores_requested_size() {
        let src: IconSource = brand_icon_at(40).into();
        assert!(!src.is_sized());
        for px in [16u32, 64] {
            let icon = src.at(px);
            assert_eq!(
                (icon.width(), icon.height()),
                (40, 40),
                "Bitmap 源被重采样了"
            );
        }
    }

    /// Windows 各缩放比实际索取的尺寸都要画得出来，且互不相同。
    /// 尺寸表：100%→16/32，125%→20/40，150%→24/48，175%→28/56，200%→32/64。
    #[test]
    fn sized_source_covers_every_dpi_step() {
        let src = brand_icon();
        let mut seen = std::collections::HashSet::new();
        for px in [16u32, 20, 24, 28, 32, 40, 48, 56, 64, 80] {
            let icon = src.at(px);
            assert_eq!(icon.width(), px);
            // 同尺寸下像素完全一致是应该的，不同尺寸之间必须真的不同。
            assert!(
                seen.insert(icon.rgba().to_vec()),
                "{px}px 与其它档画出了同一张图"
            );
        }
    }

    #[test]
    fn from_rgba_rejects_wrong_length() {
        assert!(WindowIcon::from_rgba(2, 2, vec![0; 15]).is_none());
        assert!(WindowIcon::from_rgba(2, 2, vec![0; 16]).is_some());
        assert!(WindowIcon::from_rgba(0, 4, vec![]).is_none());
    }

    /// PNG 往返：编码出的字节要能被解回同尺寸、同像素（macOS Dock 图标依赖这条路径）。
    #[test]
    fn to_png_round_trips() {
        let icon = brand_icon_at(32);
        let png = icon.to_png().expect("编码 PNG");
        let back = Pixmap::decode_png(&png).expect("解码 PNG");
        assert_eq!((back.width(), back.height()), (32, 32));
        let restored: Vec<u8> = back
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        assert_eq!(restored, icon.rgba(), "PNG 往返后像素不一致");
    }
}
