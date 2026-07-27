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

use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, DWRITE_E_NOCOLOR, FALSE};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFactory2,
    IDWriteGdiInterop, IDWriteInlineObject, IDWritePixelSnapping_Impl, IDWriteRenderingParams,
    IDWriteTextFormat, IDWriteTextLayout, IDWriteTextRenderer, IDWriteTextRenderer_Impl,
    DWRITE_COLOR_F, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_RUN,
    DWRITE_GLYPH_RUN_DESCRIPTION, DWRITE_LINE_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM,
    DWRITE_MATRIX, DWRITE_MEASURING_MODE, DWRITE_STRIKETHROUGH, DWRITE_TEXT_METRICS,
    DWRITE_UNDERLINE,
};
use windows::Win32::Graphics::Gdi::{GetCurrentObject, GetObjectW, DIBSECTION, OBJ_BITMAP};

use super::{LineMetrics, TextEngine, TextStyle};
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

/// 文本测量缓存容量上限；满则整体清空（周期性重测，命中率仍极高）。
const MEASURE_CACHE_CAP: usize = 4096;

/// DirectWrite 文字引擎。
///
/// 约束：内部 COM 对象（`IDWrite*`）非 `Send`/`Sync`，必须在创建它的
/// UI（STA）线程上使用，不可跨线程共享。
pub struct DWriteEngine {
    factory: IDWriteFactory,
    gdi_interop: IDWriteGdiInterop,
    renderer: IDWriteTextRenderer,
    /// 缓存 TextFormat，按 (family, 物理字号 bits, 字重, 行高 bits) 复用。
    formats: HashMap<(String, u32, u16, Option<u32>), IDWriteTextFormat>,
    /// 文本测量缓存：键为 (文本+字体+字号+换行宽+字重+行高+scale) 的 64 位哈希，值为逻辑尺寸。
    /// 避免每帧对稳定文本重复 CreateTextLayout/GetMetrics（DirectWrite COM 往返昂贵）。
    /// 用哈希键省去每次查表的字符串分配；64 位空间碰撞概率可忽略。
    measure_cache: HashMap<u64, Size>,
    /// 基线度量缓存：键与 measure 同要素（无 max_width），值为逻辑 LineMetrics。
    /// 富文本每个碎片都要问基线，缓存后 COM 往返只发生在首次。
    metrics_cache: HashMap<u64, LineMetrics>,
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
            let params = factory
                .CreateRenderingParams()
                .expect("CreateRenderingParams 失败");
            // IDWriteFactory2（Win8.1+）提供彩色字形拆层；取不到则 renderer 退化为单色绘制。
            let factory2: Option<IDWriteFactory2> = factory.cast().ok();
            let renderer: IDWriteTextRenderer = GlyphRenderer {
                params: params.clone(),
                factory2,
            }
            .into();
            Self {
                factory,
                gdi_interop,
                renderer,
                formats: HashMap::new(),
                measure_cache: HashMap::new(),
                metrics_cache: HashMap::new(),
                scale: 1.0,
                bitmap_target: None,
                bitmap_w: 0,
                bitmap_h: 0,
            }
        }
    }

    /// 构造（并缓存）文字格式。`psize` 是**物理**字号，与 measure/draw 同源。
    fn format(&mut self, ts: &TextStyle, psize: f32) -> Option<IDWriteTextFormat> {
        let fam = ts.family.unwrap_or(DEFAULT_FAMILY).to_string();
        let weight = ts.weight;
        // 行距进缓存键：同字族同字号但行距不同，是两套格式。漏掉它会让先构造的那套
        // 被后者复用，表现为行高时灵时不灵——取决于谁先进缓存。
        let lh_key = ts.line_height.map(f32::to_bits);
        let key = (fam.clone(), psize.to_bits(), weight, lh_key);
        if let Some(f) = self.formats.get(&key) {
            return Some(f.clone());
        }
        let dw_weight = if weight == crate::text::WEIGHT_NORMAL {
            DWRITE_FONT_WEIGHT_NORMAL
        } else {
            DWRITE_FONT_WEIGHT(weight as i32)
        };
        let fam_w = wide_nul(&fam);
        let locale = wide_nul("zh-cn");
        let format = unsafe {
            self.factory
                .CreateTextFormat(
                    PCWSTR(fam_w.as_ptr()),
                    None,
                    dw_weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    psize,
                    PCWSTR(locale.as_ptr()),
                )
                .ok()?
        };
        // 行高：UNIFORM 方法强制每行占固定高度，不再随字形起伏。
        //
        // 基线取行高的 0.8——DirectWrite 要求显式给出基线位置，而它没有「按比例
        // 自动分配」的模式。0.8 是西文与 CJK 都能接受的经验值：低于此值行内文字
        // 贴上沿，高于则贴下沿。
        if let Some(mult) = ts.line_height {
            let line = psize * mult;
            unsafe {
                let _ = format.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line, line * 0.8);
            }
        }
        self.formats.insert(key, format.clone());
        Some(format)
    }

    fn layout(
        &mut self,
        text: &str,
        ts: &TextStyle,
        psize: f32,
        max_w: f32,
    ) -> Option<IDWriteTextLayout> {
        let format = self.format(ts, psize)?;
        let text_w = wide(text);
        unsafe {
            self.factory
                .CreateTextLayout(&text_w, &format, max_w, f32::MAX)
                .ok()
        }
    }

    /// 返回复用的位图渲染目标，必要时按历史最大尺寸扩容（减少 COM 重建）。
    fn ensure_bitmap(&mut self, w: i32, h: i32) -> Option<IDWriteBitmapRenderTarget> {
        if self.bitmap_target.is_none() || w > self.bitmap_w || h > self.bitmap_h {
            let nw = w.max(self.bitmap_w).max(1);
            let nh = h.max(self.bitmap_h).max(1);
            let brt = unsafe {
                self.gdi_interop
                    .CreateBitmapRenderTarget(None, nw as u32, nh as u32)
            }
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
        let new = scale.max(0.1);
        if new != self.scale {
            // scale 变更使所有缓存尺寸失效（物理字号变了）。
            self.measure_cache.clear();
            self.metrics_cache.clear();
        }
        self.scale = new;
    }

    fn scale(&self) -> f32 {
        self.scale
    }

    fn measure(&mut self, text: &str, ts: &TextStyle, max_width: Option<f32>) -> Size {
        let size = ts.size;
        if text.is_empty() {
            // 空串也要占一行的高度，否则空标签会把周围布局吸扁。
            return Size::new(0, ts.line_height_px().unwrap_or(size).ceil() as i32);
        }
        // 缓存键：把所有影响排版的输入哈希成 64 位（含线程局部字重与当前 scale）。
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            ts.family.hash(&mut h);
            size.to_bits().hash(&mut h);
            max_width.map(f32::to_bits).hash(&mut h);
            ts.weight.hash(&mut h);
            ts.line_height.map(f32::to_bits).hash(&mut h);
            self.scale.to_bits().hash(&mut h);
            h.finish()
        };
        if let Some(sz) = self.measure_cache.get(&key) {
            return *sz;
        }
        // 物理字号排版（与 draw 同源），结果 /scale 回逻辑供布局使用。
        let s = self.scale;
        let psize = size * s;
        let pmw = max_width.map(|w| w * s).unwrap_or(f32::MAX);
        let Some(layout) = self.layout(text, ts, psize, pmw) else {
            return Size::new(0, size.ceil() as i32);
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut m).ok() };
        // width 不含尾随空白宽度，连续空格会被折叠为同一测量值，导致光标定位到
        // 尾随空格处时 x 坐标不再前进；改用 widthIncludingTrailingWhitespace。
        let sz = Size::new(
            (m.widthIncludingTrailingWhitespace / s).ceil() as i32,
            (m.height / s).ceil() as i32,
        );
        // 容量上限：满则清空（稳定 UI 下命中率仍极高）。
        if self.measure_cache.len() >= MEASURE_CACHE_CAP {
            self.measure_cache.clear();
        }
        self.measure_cache.insert(key, sz);
        sz
    }

    fn line_metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
        // 近似回退：空文本 / 排版失败时按行高 0.8 处取基线（与 UNIFORM 行距约定一致）。
        let approx = || {
            let h = ts.line_height_px().unwrap_or(ts.size);
            LineMetrics {
                ascent: h * 0.8,
                descent: h * 0.2,
            }
        };
        if text.is_empty() {
            return approx();
        }
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            ts.family.hash(&mut h);
            ts.size.to_bits().hash(&mut h);
            ts.weight.hash(&mut h);
            ts.line_height.map(f32::to_bits).hash(&mut h);
            self.scale.to_bits().hash(&mut h);
            h.finish()
        };
        if let Some(m) = self.metrics_cache.get(&key) {
            return *m;
        }
        // 物理字号排版（与 measure/draw 同源），首行 GetLineMetrics 取基线后 /scale 回逻辑。
        // 不限宽——询问的是碎片级文本的固有基线，不涉及换行。
        let s = self.scale;
        let Some(layout) = self.layout(text, ts, ts.size * s, f32::MAX) else {
            return approx();
        };
        let mut lm = [DWRITE_LINE_METRICS::default(); 1];
        let mut n = 0u32;
        // 本方法面向单行文本（富文本碎片不含 \n）。文本超一行时缓冲区不足、调用返回
        // E_NOT_SUFFICIENT_BUFFER 且**不保证写入数据**——lm[0] 保持零初始化，由下方
        // `height <= 0.0` 兜底回退近似值；勿删该防御判断。
        let _ = unsafe { layout.GetLineMetrics(Some(&mut lm), &mut n) };
        if n == 0 || lm[0].height <= 0.0 {
            return approx();
        }
        let m = LineMetrics {
            ascent: lm[0].baseline / s,
            descent: (lm[0].height - lm[0].baseline) / s,
        };
        if self.metrics_cache.len() >= MEASURE_CACHE_CAP {
            self.metrics_cache.clear();
        }
        self.metrics_cache.insert(key, m);
        m
    }

    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        ts: &TextStyle,
        clip: Option<Rect>,
    ) {
        let size = ts.size;
        if text.is_empty() || rect.is_empty() {
            return;
        }
        // 逻辑 rect/size/clip 物理化（与 measure 同源物理字号排版）。
        let s = self.scale;
        let prect = rect.scaled(s);
        let pclip = clip.map(|c| c.scaled(s));
        let psize = size * s;
        // 用 rect.w * s（精确乘法，与 measure 里 pmw = max_width * s 同源）作为
        // 排版最大宽度。prect.w 是经 scaled() 取整后的物理宽度，可能因取整略小于
        // rect.w * s，导致本应单行的文字被截断换行（125%/175% 等非整数 DPI 典型）。
        let layout_max_w = (rect.w as f32 * s).max(prect.w as f32);
        let Some(layout) = self.layout(text, ts, psize, layout_max_w) else {
            return;
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        if unsafe { layout.GetMetrics(&mut m) }.is_err() {
            return;
        }
        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;
        // 文本完整物理宽度——单行横向超长时可远超 pixmap 宽。
        let mw = m.width.ceil().max(1.0) as i32;
        let th = (m.height.ceil().max(1.0) as i32).min(ph);

        // 文本原点 X（pixmap 物理坐标）：按对齐用**完整**文本宽度推算，
        // 故横向滚动后 prect.x 为负、或文本宽超窗口时定位仍正确。
        let text_x0 = match align {
            Align::Start | Align::Stretch => prect.x,
            Align::Center => prect.x + (prect.w - mw) / 2,
            Align::End => prect.x + prect.w - mw,
        };

        // 只为**可见切片**分配位图：与 pixmap 边界及裁剪矩形求交。横向超长文本
        // 被滚到右侧时，靠把字形整体左移（glyph_dx）让可见部分（含行尾）落入位图，
        // 不再因位图锚定文本起点而把右侧字形丢在位图之外。
        let mut vis0 = text_x0.max(0);
        let mut vis1 = (text_x0 + mw).min(pw);
        if let Some(c) = pclip {
            vis0 = vis0.max(c.x);
            vis1 = vis1.min(c.x + c.w);
        }
        if vis1 <= vis0 {
            return;
        }
        let tw = vis1 - vis0; // 可见宽度（恒 <= pixmap 宽）
        let glyph_dx = (text_x0 - vis0) as f32; // 字形横向偏移：滚动右移时为负

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

        // 文字位图在 pixmap 中的目标位置（物理坐标）：可见切片起点。
        let ox = vis0;
        let oy = prect.y + (prect.h - th).max(0) / 2;

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
        let ctx = BitmapCtx {
            target: brt.clone(),
            color: colorref,
        };
        unsafe {
            layout
                .Draw(
                    Some(&ctx as *const _ as *const c_void),
                    &self.renderer,
                    glyph_dx,
                    0.0,
                )
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
                    // 文字像素：DirectWrite 已在背景上混出不透明文字色 (nr,ng,nb)；
                    // 再按 fg.alpha 与原背景二次混合，使 fg.alpha 乘进有效覆盖率（半透明文字色）。
                    // fg.alpha=255 时下式退化为 new，逐像素等同旧逻辑（不透明文字零回归）。
                    let fa = color.a as u32;
                    let bg_a = d.alpha() as u32;
                    let mix = |n: u8, b: u8| ((n as u32 * fa + b as u32 * (255 - fa)) / 255) as u8;
                    let fr = mix(nr, d.red());
                    let fgc = mix(ng, d.green());
                    let fb = mix(nb, d.blue());
                    // 输出 alpha 取背景 alpha（文字混入不透明背景仍不透明），按其预乘写回。
                    let pr = (fr as u32 * bg_a / 255) as u8;
                    let pg = (fgc as u32 * bg_a / 255) as u8;
                    let pb = (fb as u32 * bg_a / 255) as u8;
                    if let Some(p) = PremultipliedColorU8::from_rgba(pr, pg, pb, bg_a as u8) {
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

/// DWRITE_COLOR_F（直通 0..1 各通道）→ GDI COLORREF（0x00BBGGRR）。
/// BitmapRenderTarget.DrawGlyphRun 只接受不含 alpha 的 COLORREF，半透明层 alpha 在此被丢弃
/// （彩色 emoji 层通常 a=1.0，可接受）。
fn color_f_to_colorref(c: DWRITE_COLOR_F) -> COLORREF {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    COLORREF((q(c.b) << 16) | (q(c.g) << 8) | q(c.r))
}

/// 自实现的文字渲染回调：优先把字形拆成彩色层逐层着色（emoji），否则以文字色单色绘制。
#[implement(IDWriteTextRenderer)]
struct GlyphRenderer {
    params: IDWriteRenderingParams,
    /// 彩色字形拆层接口（IDWriteFactory2，Win8.1+）；None 时仅单色绘制。
    factory2: Option<IDWriteFactory2>,
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
        glyphrundescription: *const DWRITE_GLYPH_RUN_DESCRIPTION,
        _clientdrawingeffect: Ref<'_, IUnknown>,
    ) -> Result<()> {
        if clientdrawingcontext.is_null() {
            return Ok(());
        }
        let ctx = unsafe { &*(clientdrawingcontext as *const BitmapCtx) };

        // 优先：把字形拆成彩色层（COLR/CPAL，如 emoji）逐层着色叠加。
        // 字体无彩色数据时 TranslateColorGlyphRun 返回 DWRITE_E_NOCOLOR，落到下方单色路径。
        if let Some(f2) = &self.factory2 {
            let desc = if glyphrundescription.is_null() {
                None
            } else {
                Some(glyphrundescription)
            };
            let enumr = unsafe {
                f2.TranslateColorGlyphRun(
                    baselineoriginx,
                    baselineoriginy,
                    glyphrun,
                    desc,
                    measuringmode,
                    None, // 无世界变换（位图已按物理像素 1:1）
                    0,    // 默认调色板
                )
            };
            match enumr {
                Ok(en) => {
                    unsafe {
                        // 逐层绘制；枚举出错则中止彩色路径（已绘层保留）。
                        while let Ok(more) = en.MoveNext() {
                            if !more.as_bool() {
                                break;
                            }
                            let Ok(run_ptr) = en.GetCurrentRun() else {
                                break;
                            };
                            if run_ptr.is_null() {
                                break;
                            }
                            let run = &*run_ptr;
                            // paletteIndex == 0xFFFF 为规范哨兵：该层用文字前景色，runColor 未定义。
                            let color = if run.paletteIndex == 0xFFFF {
                                ctx.color
                            } else {
                                color_f_to_colorref(run.runColor)
                            };
                            let _ = ctx.target.DrawGlyphRun(
                                run.baselineOriginX,
                                run.baselineOriginY,
                                measuringmode,
                                &run.glyphRun,
                                &self.params,
                                color,
                                None,
                            );
                        }
                    }
                    return Ok(());
                }
                Err(e) if e.code() == DWRITE_E_NOCOLOR => {} // 无彩色数据：走单色
                Err(_) => {}                                 // 其它失败：保守走单色
            }
        }

        // 单色：用文字颜色直接在已拷入真实背景的位图上抗锯齿混合。
        unsafe {
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
        _effect: Ref<'_, IUnknown>,
    ) -> Result<()> {
        Ok(())
    }

    fn DrawStrikethrough(
        &self,
        _ctx: *const c_void,
        _x: f32,
        _y: f32,
        _strikethrough: *const DWRITE_STRIKETHROUGH,
        _effect: Ref<'_, IUnknown>,
    ) -> Result<()> {
        Ok(())
    }

    fn DrawInlineObject(
        &self,
        _ctx: *const c_void,
        _x: f32,
        _y: f32,
        _inlineobject: Ref<'_, IDWriteInlineObject>,
        _issideways: BOOL,
        _isrtl: BOOL,
        _effect: Ref<'_, IUnknown>,
    ) -> Result<()> {
        Ok(())
    }
}

#[allow(non_snake_case)]
impl IDWritePixelSnapping_Impl for GlyphRenderer_Impl {
    fn IsPixelSnappingDisabled(&self, _ctx: *const c_void) -> Result<BOOL> {
        Ok(FALSE)
    }
    fn GetCurrentTransform(
        &self,
        _ctx: *const c_void,
        transform: *mut DWRITE_MATRIX,
    ) -> Result<()> {
        if transform.is_null() {
            return Ok(());
        }
        unsafe {
            *transform = DWRITE_MATRIX {
                m11: 1.0,
                m12: 0.0,
                m21: 0.0,
                m22: 1.0,
                dx: 0.0,
                dy: 0.0,
            };
        }
        Ok(())
    }
    fn GetPixelsPerDip(&self, _ctx: *const c_void) -> Result<f32> {
        Ok(1.0)
    }
}

#[cfg(all(test, windows))]
mod alpha_text_tests {
    use super::*;
    use crate::geometry::{Color, Rect};
    use crate::spec::Align;
    use crate::text::TextEngine;
    use tiny_skia::Pixmap;

    /// 扫描块体覆盖区最暗红通道（笔画中心 coverage≈1）。
    fn darkest_red(pm: &Pixmap, x0: u32, x1: u32, y0: u32, y1: u32) -> u8 {
        let mut d = 255u8;
        for y in y0..y1 {
            for x in x0..x1 {
                d = d.min(pm.pixel(x, y).unwrap().red());
            }
        }
        d
    }

    /// 50% alpha 纯黑全块字符（█ U+2588，coverage=1）画在白底，块体中心应约中灰（96..160），
    /// 而非纯黑（旧逻辑丢弃 fg.alpha 会得近黑）。
    #[test]
    fn half_alpha_text_blends_to_midtone() {
        let mut eng = DWriteEngine::new();
        eng.set_scale(1.0);
        let mut pm = Pixmap::new(64, 48).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        eng.draw(
            &mut pm,
            "\u{2588}\u{2588}",
            Rect::new(4, 4, 56, 40),
            Color::rgba(0, 0, 0, 128),
            Align::Start,
            &crate::text::TextStyle::new(32.0),
            None,
        );
        let d = darkest_red(&pm, 6, 40, 8, 40);
        assert!((96..=170).contains(&d), "50% 黑字块中心应为中灰，实得 {d}");
    }

    /// 测量缓存：相同输入命中不新增条目，不同字号/文本为不同键；结果稳定一致。
    #[test]
    fn measure_cache_dedups_and_keys() {
        let mut eng = DWriteEngine::new();
        eng.set_scale(1.0);
        let _ = eng.measure("hello", &crate::text::TextStyle::new(14.0), None);
        let _ = eng.measure("world", &crate::text::TextStyle::new(14.0), None);
        assert_eq!(eng.measure_cache.len(), 2);
        let a = eng.measure("hello", &crate::text::TextStyle::new(14.0), None);
        let b = eng.measure("hello", &crate::text::TextStyle::new(14.0), None);
        assert_eq!(a, b, "相同输入测量结果应一致");
        assert_eq!(eng.measure_cache.len(), 2, "重复测量不应新增缓存条目");
        let _ = eng.measure("hello", &crate::text::TextStyle::new(18.0), None); // 不同字号 → 新键
        assert_eq!(eng.measure_cache.len(), 3);
        eng.set_scale(2.0); // scale 变更应清空缓存
        assert_eq!(eng.measure_cache.len(), 0, "scale 变更应清空测量缓存");
    }

    /// fg.alpha=255 时与不透明渲染一致：纯黑全块中心应近黑（无回归）。
    #[test]
    fn opaque_text_unchanged() {
        let mut eng = DWriteEngine::new();
        eng.set_scale(1.0);
        let mut pm = Pixmap::new(64, 48).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        eng.draw(
            &mut pm,
            "\u{2588}\u{2588}",
            Rect::new(4, 4, 56, 40),
            Color::rgba(0, 0, 0, 255),
            Align::Start,
            &crate::text::TextStyle::new(32.0),
            None,
        );
        let d = darkest_red(&pm, 6, 40, 8, 40);
        assert!(d < 40, "不透明黑字块中心应近黑(<40)，实得 {d}");
    }
}
