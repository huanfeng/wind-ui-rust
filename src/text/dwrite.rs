//! DirectWrite 文字引擎：排版 + 灰度 AA 字形位图，合成进 tiny-skia pixmap。
//!
//! 渲染路径（方案 A）：
//! 1. `IDWriteTextLayout` 排版，`GetMetrics` 取尺寸。
//! 2. `IDWriteGdiInterop::CreateBitmapRenderTarget` 建离屏 GDI 位图（黑底）。
//! 3. 自实现的 `IDWriteTextRenderer` 回调把字形以**纯白**、**灰度 AA**画到位图。
//! 4. 读回位图，灰度值即覆盖率（alpha），用真正文字色 over-blend 进 pixmap。

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
    DWRITE_PIXEL_GEOMETRY_FLAT, DWRITE_RENDERING_MODE_NATURAL, DWRITE_STRIKETHROUGH,
    DWRITE_TEXT_METRICS, DWRITE_UNDERLINE,
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
    /// 缓存 TextFormat，按 (family,size) 复用。
    formats: HashMap<(String, u32), IDWriteTextFormat>,
}

impl DWriteEngine {
    pub fn new() -> Self {
        unsafe {
            let factory: IDWriteFactory =
                DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).expect("DWriteCreateFactory 失败");
            let gdi_interop = factory.GetGdiInterop().expect("GetGdiInterop 失败");
            // gamma=1.0, 对比度=0, ClearType level=0(纯灰度), FLAT, NATURAL
            let params = factory
                .CreateCustomRenderingParams(
                    1.0,
                    0.0,
                    0.0,
                    DWRITE_PIXEL_GEOMETRY_FLAT,
                    DWRITE_RENDERING_MODE_NATURAL,
                )
                .expect("CreateCustomRenderingParams 失败");
            let renderer: IDWriteTextRenderer = GlyphRenderer { params: params.clone() }.into();
            Self {
                factory,
                gdi_interop,
                renderer,
                formats: HashMap::new(),
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
}

impl Default for DWriteEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine for DWriteEngine {
    fn measure(&mut self, text: &str, family: Option<&str>, size: f32) -> Size {
        if text.is_empty() {
            return Size::new(0, size.ceil() as i32);
        }
        let Some(layout) = self.layout(text, family, size, f32::MAX) else {
            return Size::new(0, size.ceil() as i32);
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut m).ok() };
        Size::new(m.width.ceil() as i32, m.height.ceil() as i32)
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
    ) {
        if text.is_empty() || rect.is_empty() {
            return;
        }
        // Label 是单行控件：draw 与 measure 同用 f32::MAX 不换行，避免垂直裁剪。
        let Some(layout) = self.layout(text, family, size, f32::MAX) else {
            return;
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut m).ok() };
        // 位图尺寸钳制在 pixmap 内，省内存并防止超大文本分配失败。
        let tw = (m.width.ceil().max(1.0) as i32).min(pixmap.width() as i32);
        let th = (m.height.ceil().max(1.0) as i32).min(pixmap.height() as i32);

        // 离屏位图渲染字形（白字黑底，灰度 AA）；失败则跳过该文字而非 panic。
        let brt = match unsafe {
            self.gdi_interop.CreateBitmapRenderTarget(None, tw as u32, th as u32)
        } {
            Ok(b) => b,
            Err(_) => return,
        };
        unsafe { brt.SetPixelsPerDip(1.0).ok() };

        // 把目标位图传给回调，layout.Draw 同步触发 DrawGlyphRun（ctx 在调用期间存活）。
        let ctx = BitmapCtx { target: brt.clone() };
        unsafe {
            layout
                .Draw(Some(&ctx as *const _ as *const c_void), &self.renderer, 0.0, 0.0)
                .ok()
        };

        // 读回位图像素
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
        let stride = ds.dsBm.bmWidthBytes;
        let bmw = ds.dsBm.bmWidth; // 实际分配宽（像素）
        let bmh = ds.dsBm.bmHeight; // 实际分配高（像素，恒正）
        let bits = ds.dsBm.bmBits as *const u8;
        // 用实际位图尺寸钳制遍历上界，杜绝越界读。
        let cw = tw.min(bmw);
        let ch = th.min(bmh);

        // 对齐：水平按 align，垂直居中
        let ox = match align {
            Align::Start | Align::Stretch => rect.x,
            Align::Center => rect.x + (rect.w - tw) / 2,
            Align::End => rect.x + rect.w - tw,
        };
        let oy = rect.y + (rect.h - th) / 2;

        composite_coverage(pixmap, bits, cw, ch, stride, ox, oy, color);
    }
}

/// 把灰度覆盖率位图（白字黑底）按 `color` over-blend 进 pixmap（预乘）。
fn composite_coverage(
    pixmap: &mut Pixmap,
    bits: *const u8,
    bw: i32,
    bh: i32,
    stride: i32,
    dst_x: i32,
    dst_y: i32,
    color: Color,
) {
    let pw = pixmap.width() as i32;
    let ph = pixmap.height() as i32;
    let px = pixmap.pixels_mut();
    let ca = color.a as f32 / 255.0;
    for ry in 0..bh {
        let dy = dst_y + ry;
        if dy < 0 || dy >= ph {
            continue;
        }
        // IDWriteBitmapRenderTarget 的像素存储固定为 top-down（buffer 首行=图像顶行），
        // 与 GetObjectW 报告的 biHeight 符号无关——故行序直接对应，不按 biHeight 翻转。
        let sy = ry;
        for rx in 0..bw {
            let dx = dst_x + rx;
            if dx < 0 || dx >= pw {
                continue;
            }
            // BGRA：取 R 通道作灰度覆盖率（灰度 AA 下 R=G=B）
            let cov = unsafe { *bits.add((sy * stride + rx * 4 + 2) as usize) };
            if cov == 0 {
                continue;
            }
            let a = (cov as f32 / 255.0) * ca;
            if a <= 0.0 {
                continue;
            }
            let idx = (dy * pw + dx) as usize;
            let d = px[idx];
            let inv = 1.0 - a;
            // 预乘 over：out = src_premul + dst*(1-a)
            let na = (a * 255.0 + d.alpha() as f32 * inv).round();
            let nr = (color.r as f32 * a + d.red() as f32 * inv).round().min(na);
            let ng = (color.g as f32 * a + d.green() as f32 * inv).round().min(na);
            let nb = (color.b as f32 * a + d.blue() as f32 * inv).round().min(na);
            if let Some(p) =
                PremultipliedColorU8::from_rgba(nr as u8, ng as u8, nb as u8, na as u8)
            {
                px[idx] = p;
            }
        }
    }
}

/// 传给 layout.Draw 的客户端上下文：目标位图。
struct BitmapCtx {
    target: IDWriteBitmapRenderTarget,
}

/// 自实现的文字渲染回调：把字形转发到位图渲染目标（纯白）。
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
            let _ = ctx.target.DrawGlyphRun(
                baselineoriginx,
                baselineoriginy,
                measuringmode,
                glyphrun,
                &self.params,
                COLORREF(0x00FF_FFFF), // 纯白
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
