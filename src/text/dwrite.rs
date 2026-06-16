//! DirectWrite 文字引擎：排版 + 真背景合成，绘制进 tiny-skia pixmap。
//!
//! 渲染路径（真背景合成，gamma 由 DirectWrite 用系统校准参数自行处理）：
//! 1. `IDWriteTextLayout` 排版，`GetMetrics` 取尺寸。
//! 2. 把目标区域的**真实背景**从 pixmap 拷入离屏 GDI 位图（BGRA）。
//! 3. 自实现的 `IDWriteTextRenderer` 回调用**文字颜色**在该背景上 `DrawGlyphRun` 抗锯齿混合。
//! 4. 读回位图，仅把 RGB 被字形改动的像素（含抗锯齿边缘）写回 pixmap，背景像素跳过。
//!
//! 注：背景拷入把预乘 RGBA 当直通使用，故仅在**不透明背景**（alpha=255）下颜色精确；
//! 半透明背景区域的文字颜色为近似（当前 UI 以不透明为主）。

use std::collections::HashMap;
use std::ffi::c_void;

use tiny_skia::{Pixmap, PremultipliedColorU8};

use windows::core::{implement, IUnknown, Result, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, FALSE};
use windows::Win32::Graphics::Gdi::{GetCurrentObject, GetObjectW, DIBSECTION, OBJ_BITMAP};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteGdiInterop,
    IDWriteInlineObject, IDWritePixelSnapping_Impl, IDWriteRenderingParams, IDWriteTextFormat,
    IDWriteTextLayout, IDWriteTextRenderer, IDWriteTextRenderer_Impl, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_GLYPH_RUN, DWRITE_GLYPH_RUN_DESCRIPTION, DWRITE_MATRIX, DWRITE_MEASURING_MODE,
    DWRITE_STRIKETHROUGH, DWRITE_TEXT_METRICS, DWRITE_UNDERLINE,
};

use super::TextEngine;
use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

/// 把 &str 转为以 NUL 结尾的 UTF-16。
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
/// 把 &str 转为 UTF-16（不含 NUL）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

const DEFAULT_FAMILY: &str = "Microsoft YaHei UI"; // 中文友好的默认字体

/// DirectWrite 文字引擎。
///
/// 约束：内部 COM 对象（`IDWrite*`）非 `Send`/`Sync`，必须在创建它的
/// UI（STA）线程上使用，不可跨线程共享。
pub struct DWriteEngine {
    factory: IDWriteFactory,
    gdi_interop: IDWriteGdiInterop,
    renderer: IDWriteTextRenderer,
    /// 缓存 TextFormat，按 (family, 物理字号 bits) 复用。
    formats: HashMap<(String, u32), IDWriteTextFormat>,
    /// DPI 缩放因子（逻辑→物理）。
    scale: f32,
    /// 复用的离屏位图渲染目标（按需扩容），避免每次绘字都创建 COM 对象。
    bitmap_target: Option<IDWriteBitmapRenderTarget>,
    bitmap_w: i32,
    bitmap_h: i32,
}

impl DWriteEngine {
    pub fn new() -> Self {
        unsafe {
            let factory: IDWriteFactory =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DWriteCreateFactory 失败");
            let gdi_interop = factory.GetGdiInterop().expect("GetGdiInterop 失败");
            // 系统默认渲染参数：含用户 ClearType 校准的 gamma/对比度/渲染模式。
            // 配合"真背景合成"（draw 中把真实背景拷入位图后让 DirectWrite 直接
            // 在其上抗锯齿混合），gamma 由 DirectWrite 自己正确处理，文字不再发重。
            let params = factory.CreateRenderingParams().expect("CreateRenderingParams 失败");
            let renderer: IDWriteTextRenderer = GlyphRenderer { params: params.clone() }.into();
            Self {
                factory,
                gdi_interop,
                renderer,
                formats: HashMap::new(),
                scale: 1.0,
                bitmap_target: None,
                bitmap_w: 0,
                bitmap_h: 0,
            }
        }
    }

    fn format(&mut self, family: Option<&str>, size: f32) -> Option<IDWriteTextFormat> {
        let fam = family.unwrap_or(DEFAULT_FAMILY).to_string();
        let key = (fam.clone(), size.to_bits());
        if let Some(f) = self.formats.get(&key) {
            return Some(f.clone());
        }
        let fam_w = wide_nul(&fam);
        let locale = wide_nul("zh-cn");
        let format = unsafe {
            self.factory
                .CreateTextFormat(
                    PCWSTR(fam_w.as_ptr()),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    PCWSTR(locale.as_ptr()),
                )
                .ok()?
        };
        self.formats.insert(key, format.clone());
        Some(format)
    }

    fn layout(
        &mut self,
        text: &str,
        family: Option<&str>,
        size: f32,
        max_w: f32,
    ) -> Option<IDWriteTextLayout> {
        let format = self.format(family, size)?;
        let text_w = wide(text);
        unsafe { self.factory.CreateTextLayout(&text_w, &format, max_w, f32::MAX).ok() }
    }

    /// 返回复用的位图渲染目标，必要时按历史最大尺寸扩容（减少 COM 重建）。
    fn ensure_bitmap(&mut self, w: i32, h: i32) -> Option<IDWriteBitmapRenderTarget> {
        if self.bitmap_target.is_none() || w > self.bitmap_w || h > self.bitmap_h {
            let nw = w.max(self.bitmap_w).max(1);
            let nh = h.max(self.bitmap_h).max(1);
            let brt =
                unsafe { self.gdi_interop.CreateBitmapRenderTarget(None, nw as u32, nh as u32) }
                    .ok()?;
            unsafe { brt.SetPixelsPerDip(1.0).ok() };
            self.bitmap_target = Some(brt);
            self.bitmap_w = nw;
            self.bitmap_h = nh;
        }
        self.bitmap_target.clone()
    }
}

impl Default for DWriteEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine for DWriteEngine {
    fn set_scale(&mut self, scale: f32) {
        self.scale = scale.max(0.1);
    }

    fn measure(&mut self, text: &str, family: Option<&str>, size: f32, max_width: Option<f32>) -> Size {
        if text.is_empty() {
            return Size::new(0, size.ceil() as i32);
        }
        // 物理字号排版（与 draw 同源），结果 /scale 回逻辑供布局使用。
        let s = self.scale;
        let psize = size * s;
        let pmw = max_width.map(|w| w * s).unwrap_or(f32::MAX);
        let Some(layout) = self.layout(text, family, psize, pmw) else {
            return Size::new(0, size.ceil() as i32);
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut m).ok() };
        Size::new((m.width / s).ceil() as i32, (m.height / s).ceil() as i32)
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        family: Option<&str>,
        size: f32,
        clip: Option<Rect>,
    ) {
        if text.is_empty() || rect.is_empty() {
            return;
        }
        // 逻辑 rect/size/clip 物理化（与 measure 同源物理字号排版）。
        let s = self.scale;
        let prect = rect.scaled(s);
        let pclip = clip.map(|c| c.scaled(s));
        let psize = size * s;
        // 按物理 rect 宽度换行（与 measure 传入的物理 maxWidth 一致）。
        let Some(layout) = self.layout(text, family, psize, prect.w as f32) else {
            return;
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        if unsafe { layout.GetMetrics(&mut m) }.is_err() {
            return;
        }
        // 位图尺寸钳制在 pixmap 内，省内存并防止超大文本分配失败。
        let tw = (m.width.ceil().max(1.0) as i32).min(pixmap.width() as i32);
        let th = (m.height.ceil().max(1.0) as i32).min(pixmap.height() as i32);

        // 复用的离屏位图渲染目标（按需扩容）；失败则跳过该文字。
        let Some(brt) = self.ensure_bitmap(tw, th) else {
            return;
        };

        // 取位图内存（DIBSection，BGRA top-down）。
        let dc = unsafe { brt.GetMemoryDC() };
        let hbm = unsafe { GetCurrentObject(dc, OBJ_BITMAP) };
        let mut ds = DIBSECTION::default();
        let got = unsafe {
            GetObjectW(
                hbm,
                std::mem::size_of::<DIBSECTION>() as i32,
                Some(&mut ds as *mut _ as *mut c_void),
            )
        };
        if got == 0 || ds.dsBm.bmBits.is_null() {
            return;
        }
        let stride_px = ds.dsBm.bmWidthBytes / 4; // 每行像素数（含对齐 padding）
        let bmw = ds.dsBm.bmWidth;
        let bmh = ds.dsBm.bmHeight;
        // BitmapRenderTarget 恒为 top-down（bmHeight 正）；防御性断言固化该假设。
        debug_assert!(bmh > 0, "expected top-down bitmap render target");
        let bits = ds.dsBm.bmBits as *mut u32;
        let cw = tw.min(bmw);
        let ch = th.min(bmh);

        // 文字位图在 pixmap 中的目标位置（物理坐标）。
        let ox = match align {
            Align::Start | Align::Stretch => prect.x,
            Align::Center => prect.x + (prect.w - tw) / 2,
            Align::End => prect.x + prect.w - tw,
        };
        let oy = prect.y + (prect.h - th).max(0) / 2;
        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;

        // 1. 把真实背景从 pixmap 拷入位图（BGRA）；DirectWrite 将在其上抗锯齿混合，
        //    gamma 由 DirectWrite 自己正确处理（不再由我们反推覆盖率）。
        {
            let px = pixmap.pixels();
            for y in 0..ch {
                let sy = oy + y;
                for x in 0..cw {
                    let sx = ox + x;
                    let off = (y * stride_px + x) as usize;
                    let bgra = if sx >= 0 && sx < pw && sy >= 0 && sy < ph {
                        let p = px[(sy * pw + sx) as usize];
                        // 预乘 RGBA → BGRA（不透明像素预乘=直通；半透明近似）
                        ((p.alpha() as u32) << 24)
                            | ((p.red() as u32) << 16)
                            | ((p.green() as u32) << 8)
                            | (p.blue() as u32)
                    } else {
                        0
                    };
                    unsafe { bits.add(off).write_unaligned(bgra) };
                }
            }
        }

        // 2. 用文字色在背景上 DrawGlyphRun（layout.Draw 同步执行，ctx 在调用期间存活）。
        let colorref =
            COLORREF(((color.b as u32) << 16) | ((color.g as u32) << 8) | (color.r as u32));
        let ctx = BitmapCtx { target: brt.clone(), color: colorref };
        unsafe {
            layout
                .Draw(Some(&ctx as *const _ as *const c_void), &self.renderer, 0.0, 0.0)
                .ok()
        };

        // 3. 读回：RGB 被字形改动的像素（含抗锯齿边缘）写回 pixmap；背景像素跳过。
        {
            let px = pixmap.pixels_mut();
            for y in 0..ch {
                let dy = oy + y;
                if dy < 0 || dy >= ph {
                    continue;
                }
                if let Some(c) = pclip {
                    if dy < c.y || dy >= c.y + c.h {
                        continue;
                    }
                }
                for x in 0..cw {
                    let dx = ox + x;
                    if dx < 0 || dx >= pw {
                        continue;
                    }
                    if let Some(c) = pclip {
                        if dx < c.x || dx >= c.x + c.w {
                            continue;
                        }
                    }
                    let off = (y * stride_px + x) as usize;
                    let new = unsafe { bits.add(off).read_unaligned() };
                    let idx = (dy * pw + dx) as usize;
                    let d = px[idx];
                    let nb = (new & 0xFF) as u8;
                    let ng = ((new >> 8) & 0xFF) as u8;
                    let nr = ((new >> 16) & 0xFF) as u8;
                    // RGB 未变 = 背景未被字形覆盖，保持原预乘值。
                    if nr == d.red() && ng == d.green() && nb == d.blue() {
                        continue;
                    }
                    // 文字像素：DirectWrite 输出的直通色按背景 alpha 预乘后写回。
                    let a = d.alpha() as u32;
                    let pr = (nr as u32 * a / 255) as u8;
                    let pg = (ng as u32 * a / 255) as u8;
                    let pb = (nb as u32 * a / 255) as u8;
                    if let Some(p) = PremultipliedColorU8::from_rgba(pr, pg, pb, a as u8) {
                        px[idx] = p;
                    }
                }
            }
        }
    }
}

/// 传给 layout.Draw 的客户端上下文：目标位图 + 文字颜色。
struct BitmapCtx {
    target: IDWriteBitmapRenderTarget,
    color: COLORREF,
}

/// 自实现的文字渲染回调：把字形以文字色转发到位图渲染目标。
#[implement(IDWriteTextRenderer)]
struct GlyphRenderer {
    params: IDWriteRenderingParams,
}

#[allow(non_snake_case)]
impl IDWriteTextRenderer_Impl for GlyphRenderer_Impl {
    fn DrawGlyphRun(
        &self,
        clientdrawingcontext: *const c_void,
        baselineoriginx: f32,
        baselineoriginy: f32,
        measuringmode: DWRITE_MEASURING_MODE,
        glyphrun: *const DWRITE_GLYPH_RUN,
        _glyphrundescription: *const DWRITE_GLYPH_RUN_DESCRIPTION,
        _clientdrawingeffect: Option<&IUnknown>,
    ) -> Result<()> {
        if clientdrawingcontext.is_null() {
            return Ok(());
        }
        let ctx = unsafe { &*(clientdrawingcontext as *const BitmapCtx) };
        unsafe {
            // 用文字颜色直接在已拷入真实背景的位图上抗锯齿混合。
            let _ = ctx.target.DrawGlyphRun(
                baselineoriginx,
                baselineoriginy,
                measuringmode,
                glyphrun,
                &self.params,
                ctx.color,
                None,
            );
        }
        Ok(())
    }

    fn DrawUnderline(
        &self,
        _ctx: *const c_void,
        _x: f32,
        _y: f32,
        _underline: *const DWRITE_UNDERLINE,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        Ok(())
    }

    fn DrawStrikethrough(
        &self,
        _ctx: *const c_void,
        _x: f32,
        _y: f32,
        _strikethrough: *const DWRITE_STRIKETHROUGH,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        Ok(())
    }

    fn DrawInlineObject(
        &self,
        _ctx: *const c_void,
        _x: f32,
        _y: f32,
        _inlineobject: Option<&IDWriteInlineObject>,
        _issideways: BOOL,
        _isrtl: BOOL,
        _effect: Option<&IUnknown>,
    ) -> Result<()> {
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IDWritePixelSnapping_Impl for GlyphRenderer_Impl {
    fn IsPixelSnappingDisabled(&self, _ctx: *const c_void) -> Result<BOOL> {
        Ok(FALSE)
    }
    fn GetCurrentTransform(&self, _ctx: *const c_void, transform: *mut DWRITE_MATRIX) -> Result<()> {
        if transform.is_null() {
            return Ok(());
        }
        unsafe {
            *transform = DWRITE_MATRIX { m11: 1.0, m12: 0.0, m21: 0.0, m22: 1.0, dx: 0.0, dy: 0.0 };
        }
        Ok(())
    }
    fn GetPixelsPerDip(&self, _ctx: *const c_void) -> Result<f32> {
        Ok(1.0)
    }
}
