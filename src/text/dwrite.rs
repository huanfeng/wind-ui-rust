//! DirectWrite 文字引擎：排版 + 真背景合成，绘制进 tiny-skia pixmap。
//!
//! 渲染路径（真背景合成，gamma 由 DirectWrite 用系统校准参数自行处理）：
//! 1. `IDWriteTextLayout` 排版，`GetMetrics` 取尺寸。
//! 2. 把目标区域的**真实背景**从 pixmap 拷入离屏 GDI 位图（BGRA）。
//! 3. 自实现的 `IDWriteTextRenderer` 回调用**文字颜色**在该背景上 `DrawGlyphRun` 抗锯齿混合。
//! 4. 读回位图，仅把 RGB 相对拷入值发生改变的像素（含抗锯齿边缘）写回 pixmap，其余跳过。
//!
//! 背景**不透明**（alpha=255）时，预乘 RGB 即直通 RGB，拷入的就是真背景，DirectWrite
//! 混出的 RGB 直接就是最终颜色，输出恒不透明——这是绝大多数文字走的路径。
//!
//! 背景**非不透明**（alpha<255，`Element::opacity` 的离屏层是极端情形 alpha=0）时没有
//! "真背景"可给：预乘 RGB 不是颜色，而离屏层的真实底色要到 `pop_layer` 才知道。此时改
//! 拷入**探针色**（每通道取与文字色相距最远的黑或白），从混合结果反解每通道覆盖率，
//! 再由我们自己做标准预乘 source-over——见 [`glyph_backdrop`] 与
//! [`composite_glyph_over_translucent`]。

use std::collections::HashMap;
use std::ffi::c_void;

use tiny_skia::{Pixmap, PremultipliedColorU8};

use windows::core::{implement, IUnknown, Interface, Ref, Result, BOOL, PCWSTR};
use windows::Win32::Foundation::{COLORREF, DWRITE_E_NOCOLOR, FALSE};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteBitmapRenderTarget, IDWriteFactory, IDWriteFactory2,
    IDWriteFactory3, IDWriteFontCollection1, IDWriteFontSetBuilder1, IDWriteGdiInterop,
    IDWriteInlineObject, IDWritePixelSnapping_Impl, IDWriteRenderingParams, IDWriteTextFormat,
    IDWriteTextLayout, IDWriteTextRenderer, IDWriteTextRenderer_Impl, DWRITE_COLOR_F,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_GLYPH_RUN, DWRITE_GLYPH_RUN_DESCRIPTION,
    DWRITE_LINE_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM, DWRITE_MATRIX, DWRITE_MEASURING_MODE,
    DWRITE_STRIKETHROUGH, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_UNDERLINE,
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

/// 扫描 UTF-16 序列，返回私用区（PUA）字符的连续段 `[(起始下标, 码元长度)]`。
/// 下标/长度均以 **UTF-16 码元** 计，可直接用作 `DWRITE_TEXT_RANGE`。
///
/// 三段私用区缺一不可——图标字体用哪一段并不统一：多数图标集（Font Awesome、
/// Material Icons 等）落在 BMP 私用区，而自制字体常用补充私用区以避开冲突。
/// 只判 BMP 一段的话，后者会静默落回主字体、渲染成方框。
///
/// - BMP 私用区 `U+E000..=U+F8FF`：单码元，`u16` 值即码位。
/// - 补充私用区 A/B `U+F0000..=U+10FFFD`：UTF-16 下是代理对。高位代理恰好占满
///   `0xDB80..=0xDBFF`（`0xDB80..=0xDBBF` → 第 15 平面，`0xDBC0..=0xDBFF` → 第 16
///   平面），不多不少，故判「高位代理落在该段 + 后随合法低位代理」即可，无需还原码位。
///
/// 相邻的 BMP 与补充私用区字符合并进同一段——它们目标字体族相同，合并只减少
/// `SetFontFamilyName` 调用次数，不改变渲染结果。
fn pua_runs(wide: &[u16]) -> Vec<(usize, usize)> {
    /// 单码元即为私用区码位（BMP PUA）。
    fn is_bmp_pua(u: u16) -> bool {
        (0xE000..=0xF8FF).contains(&u)
    }
    /// 补充私用区 A/B 的高位代理段。
    fn is_spua_lead(u: u16) -> bool {
        (0xDB80..=0xDBFF).contains(&u)
    }
    /// 任意低位代理（配对合法性；具体码位无需还原）。
    fn is_trail(u: u16) -> bool {
        (0xDC00..=0xDFFF).contains(&u)
    }

    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut i = 0usize;
    while i < wide.len() {
        // 命中长度：1=BMP 私用区，2=补充私用区代理对，0=非私用区。
        let step = if is_bmp_pua(wide[i]) {
            1
        } else if is_spua_lead(wide[i]) && wide.get(i + 1).is_some_and(|&t| is_trail(t)) {
            2
        } else {
            0
        };
        if step == 0 {
            if let Some(s) = start.take() {
                runs.push((s, i - s));
            }
            i += 1;
        } else {
            start.get_or_insert(i);
            i += step;
        }
    }
    if let Some(s) = start {
        runs.push((s, wide.len() - s));
    }
    runs
}

/// 私用区回退字体：自建字体集 + 其家族名。
///
/// 用**自建字体集**而非系统字体，是为了让应用能直接带一个 `.ttf` 随包分发，
/// 无需安装到系统（安装需要管理员权限，且会污染用户字体列表）。
///
/// `Clone` 是 COM 的 AddRef，廉价；引擎创建时从注册表克隆一份持有。
#[derive(Clone)]
struct PrivateUseFont {
    collection: IDWriteFontCollection1,
    family: Vec<u16>,
}

thread_local! {
    /// 进程内已注册的私用区回退字体。照搬 `render::image` 解码器注册表的
    /// thread-local 模式（UI 单线程，免加锁）。
    static PRIVATE_USE_FONT: std::cell::RefCell<Option<PrivateUseFont>> =
        const { std::cell::RefCell::new(None) };
}

/// 加载字体文件为自建字体集。不安装到系统。
fn build_private_use_font(path: &str, family: &str) -> Result<PrivateUseFont> {
    let factory: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
    let f3: IDWriteFactory3 = factory.cast()?;
    let path_w = wide_nul(path);
    let file = unsafe { f3.CreateFontFileReference(PCWSTR(path_w.as_ptr()), None)? };
    let builder: IDWriteFontSetBuilder1 = unsafe { f3.CreateFontSetBuilder()? }.cast()?;
    unsafe { builder.AddFontFile(&file)? };
    let set = unsafe { builder.CreateFontSet()? };
    let collection = unsafe { f3.CreateFontCollectionFromFontSet(&set)? };
    Ok(PrivateUseFont {
        collection,
        family: wide_nul(family),
    })
}

/// 注册**私用区回退字体**：此后文本里落在私用区的码位改用该字体渲染，其余字符不受影响。
///
/// 典型用途是图标字体（Font Awesome、Material Icons 等，其字形全部落在私用区）：
/// 注册后就能把图标码位当普通文字放进任何 `label`/`button`，与文本同流布局、随字号缩放。
///
/// ```no_run
/// windui::text::register_private_use_font("assets/fa-solid-900.ttf", "Font Awesome 6 Free")?;
/// # Ok::<(), windows::core::Error>(())
/// ```
///
/// - `path`：字体文件路径。**不需要安装到系统**——用 DirectWrite 自建字体集加载，
///   应用可以直接把 `.ttf` 随包分发（安装需要管理员权限，还会污染用户字体列表）。
/// - `family`：字体文件**内部的家族名**（不是文件名）——双击字体文件即可在预览窗口顶部
///   看到。它无法从路径推断，必须显式给出；写错的表现是图标仍为方框。
///
/// **须在 [`crate::app::App::run`] 之前调用**：文字引擎在窗口创建时读取本注册表。
/// 运行期改字体请拿 [`DWriteEngine::set_private_use_font`]（它会一并清测量缓存）。
///
/// 失败返回 `Err`（文件不存在、格式不受支持等），此时保持未注册状态——图标渲染为方框，
/// 其余文本不受影响。重复调用以最后一次为准。
pub fn register_private_use_font(path: &str, family: &str) -> Result<()> {
    let puf = build_private_use_font(path, family)?;
    PRIVATE_USE_FONT.with(|f| *f.borrow_mut() = Some(puf));
    Ok(())
}

/// 文本测量缓存容量上限；满则整体清空（周期性重测，命中率仍极高）。
const MEASURE_CACHE_CAP: usize = 4096;

/// 交给 DirectWrite 当"背景"用的 RGB。
///
/// - 目标像素**不透明**：预乘 RGB 就是直通 RGB，直接给真背景——DirectWrite 用系统
///   校准的 gamma 在真色上抗锯齿，混出的结果即最终颜色，无需我们反推任何东西。
/// - 目标像素**非不透明**：给探针色。每通道取 0 或 255 中离文字色更远的那个，使
///   `|c − p| ≥ 128` 恒成立，覆盖率总能被稳定反解（见
///   [`composite_glyph_over_translucent`]）。
///
/// 探针必须逐通道选而不是整体选黑或白：ClearType 的覆盖率本就是逐通道的，且逐通道
/// 选才能保证**每个**通道都有足够大的分母。
///
/// 顺带解决一条老边界：判"该像素是否被字形覆盖"用的是"RGB 相对拷入值是否变化"。
/// 拷入真背景时，文字色恰好等于背景色就无从分辨——纯黑文字画在（预乘 RGB 恒为 0 的）
/// 透明层上正是这种情况，整段文字会被判成背景而**整体跳过**。探针与文字色永远相距
/// ≥128，该判据于是重新可靠。
fn glyph_backdrop(color: Color, dst: PremultipliedColorU8) -> (u8, u8, u8) {
    if dst.alpha() == 255 {
        (dst.red(), dst.green(), dst.blue())
    } else {
        let probe = |c: u8| if c >= 128 { 0 } else { 255 };
        (probe(color.r), probe(color.g), probe(color.b))
    }
}

/// 非不透明背景上的文字合成：从探针背景上的混合结果反解覆盖率，做预乘 source-over。
///
/// `dst` 为目标像素（预乘）、`color` 文字色（直通）、`backdrop` 拷给 DirectWrite 的探针色、
/// `new` 为 DirectWrite 混出的 RGB。
///
/// DirectWrite 在探针色 `p` 上混出 `n = c·α + p·(1−α)`，故 `α = (n − p) / (c − p)`，
/// 分母由 [`glyph_backdrop`] 保证 `|c − p| ≥ 128`。三通道各解一个 α（ClearType 的覆盖率
/// 本就逐通道不同）后**取平均**得单一覆盖率 `a`，再乘进文字色自身的 alpha，做标准
/// 预乘 source-over：
///
/// ```text
/// out.rgb = c·a + dst.rgb·(1 − a)   （dst.rgb 已预乘，故这就是预乘输出）
/// out.a   = a + dst.a·(1 − a)
/// ```
///
/// ## 为什么必须先把三个覆盖率并成一个
///
/// 次像素抗锯齿要求每个通道有各自的 alpha，而 RGBA 只有一个——**离屏层上留不住
/// ClearType**。曾试过"RGB 逐通道、alpha 取三者最大"，结果是低覆盖通道被那个大 alpha
/// 遮掉了背景、却只补回自己那点文字色：某白底浅灰字的 R 通道实测比正确值暗了 109。
/// 并成一个覆盖率即退化为灰度抗锯齿——浏览器给 `opacity` 子树的也是这个取舍，且取
/// **平均**能让总墨量守恒（取 max 会整体加粗）。
///
/// 单一 `a` 同时保住预乘不变式 `rgb ≤ a`：`c ≤ 255` 且 `t ↦ 255t + dst.a·(1−t)` 单调递增。
fn composite_glyph_over_translucent(
    dst: PremultipliedColorU8,
    color: Color,
    backdrop: (u8, u8, u8),
    new: (u8, u8, u8),
) -> PremultipliedColorU8 {
    let fa = color.a as u32;
    // 单通道有效覆盖率（0..=255）：反解 α 后乘进文字色 alpha。
    let coverage = |c: u8, p: u8, n: u8| -> u32 {
        let den = c as i32 - p as i32;
        if den == 0 {
            return 0; // 探针保证不会发生；防御性兜底，按"未覆盖"处理。
        }
        let a = ((n as i32 - p as i32) * 255 / den).clamp(0, 255) as u32;
        a * fa / 255
    };
    let a = (coverage(color.r, backdrop.0, new.0)
        + coverage(color.g, backdrop.1, new.1)
        + coverage(color.b, backdrop.2, new.2))
        / 3;
    let over = |c: u8, d: u8| ((c as u32 * a + d as u32 * (255 - a)) / 255) as u8;
    let out_a = (a + dst.alpha() as u32 * (255 - a) / 255).min(255) as u8;
    PremultipliedColorU8::from_rgba(
        over(color.r, dst.red()),
        over(color.g, dst.green()),
        over(color.b, dst.blue()),
        out_a,
    )
    // 上面的不变式论证保证 from_rgba 恒为 Some；真越界时保留目标像素而非 panic。
    .unwrap_or(dst)
}

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
    /// 私用区回退字体（可选）：设置后，文本里的私用区码位改用它渲染。
    private_use: Option<PrivateUseFont>,
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
                // 取启动时注册的回退字体（见 `register_private_use_font`）。此刻缓存尚空，
                // 故无需失效处理；运行期改字体走 `set_private_use_font`，它自己清缓存。
                private_use: PRIVATE_USE_FONT.with(|f| f.borrow().clone()),
            }
        }
    }

    /// 运行期替换本引擎的私用区回退字体。参数语义同
    /// [`register_private_use_font`]，多数应用用那个自由函数在启动时注册即可。
    ///
    /// **会清空测量与基线缓存**——私用区字符换了字形来源，宽度随之改变，留着旧值会让
    /// 图标按上一套字形的宽度布局（未注册时即主字体的方框宽度）。
    pub fn set_private_use_font(&mut self, path: &str, family: &str) -> Result<()> {
        self.private_use = Some(build_private_use_font(path, family)?);
        self.measure_cache.clear();
        self.metrics_cache.clear();
        Ok(())
    }

    /// 把 `layout` 中的私用区段切到回退字体。无回退字体或无私用区字符时不做任何事。
    ///
    /// 走**字体集 + 家族名**而非 DirectWrite 的 fallback 机制：后者按脚本/语言匹配，
    /// 私用区没有脚本归属，匹配不到。
    fn apply_private_use(&self, layout: &IDWriteTextLayout, text_w: &[u16]) {
        let Some(puf) = &self.private_use else {
            return;
        };
        for (start, len) in pua_runs(text_w) {
            let range = DWRITE_TEXT_RANGE {
                startPosition: start as u32,
                length: len as u32,
            };
            unsafe {
                let _ = layout.SetFontCollection(&puf.collection, range);
                let _ = layout.SetFontFamilyName(PCWSTR(puf.family.as_ptr()), range);
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
        let layout = unsafe {
            self.factory
                .CreateTextLayout(&text_w, &format, max_w, f32::MAX)
                .ok()?
        };
        // 私用区改字体必须在这里做——measure 与 draw 共用本函数，两处若不同源，
        // 布局会按主字体的方框宽度排、绘制却出图标字形，宽度对不上。
        self.apply_private_use(&layout, &text_w);
        Some(layout)
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
        // 换行宽度用 scaled_out（外扩取整，恒 >= rect.w * s），与 measure 传入的
        // 物理 maxWidth（pmw = max_width * s）同源。用 prect.w 会因四边各自 round
        // 而略窄于 rect.w * s，把本应单行的文字最后一字挤到下一行（非整数 DPI 典型）。
        let layout_max_w = rect.scaled_out(s).w as f32;
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

        // 1. 把背景从 pixmap 拷入位图（BGRA），DirectWrite 将在其上抗锯齿混合。
        //    不透明处拷真背景（gamma 由 DirectWrite 自己正确处理）；非不透明处拷探针色，
        //    留待读回时反解覆盖率（见 `glyph_backdrop`）。
        {
            let px = pixmap.pixels();
            for y in 0..ch {
                let sy = oy + y;
                for x in 0..cw {
                    let sx = ox + x;
                    let off = (y * stride_px + x) as usize;
                    let bgra = if sx >= 0 && sx < pw && sy >= 0 && sy < ph {
                        let p = px[(sy * pw + sx) as usize];
                        let (br, bg, bb) = glyph_backdrop(color, p);
                        // GDI 的 DrawGlyphRun 只看 RGB，alpha 通道原样带着即可。
                        ((p.alpha() as u32) << 24)
                            | ((br as u32) << 16)
                            | ((bg as u32) << 8)
                            | (bb as u32)
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
                    let bd = glyph_backdrop(color, d);
                    // RGB 相对拷入值未变 = 该像素未被字形覆盖，保持原预乘值。
                    if (nr, ng, nb) == bd {
                        continue;
                    }
                    if d.alpha() == 255 {
                        // 不透明背景：DirectWrite 已在真背景上混出不透明文字色 (nr,ng,nb)；
                        // 再按 fg.alpha 与原背景二次混合，使 fg.alpha 乘进有效覆盖率
                        // （半透明文字色）。alpha=255 处预乘即直通，输出恒不透明。
                        let fa = color.a as u32;
                        let mix =
                            |n: u8, b: u8| ((n as u32 * fa + b as u32 * (255 - fa)) / 255) as u8;
                        if let Some(p) = PremultipliedColorU8::from_rgba(
                            mix(nr, d.red()),
                            mix(ng, d.green()),
                            mix(nb, d.blue()),
                            255,
                        ) {
                            px[idx] = p;
                        }
                    } else {
                        // 非不透明背景（离屏层等）：从探针背景反解覆盖率后自行 source-over。
                        px[idx] = composite_glyph_over_translucent(d, color, bd, (nr, ng, nb));
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

    /// issue #6 回归：measure 判定为单行的文本，在非整数 DPI 下经 draw 的排版宽度
    /// 换算后必须仍是单行。measure 把物理宽按 `ceil(物理宽 / scale)` 换回逻辑宽度，
    /// 布局原样发回给 draw；draw 若用 `rect.scaled(s).w`（四边各自 round）当排版宽度，
    /// 会比 `rect.w × scale` 略窄，DirectWrite 就把最后一个字挤到第二行。
    #[test]
    fn non_integer_dpi_keeps_measured_text_on_one_line() {
        let mut eng = DWriteEngine::new();
        let ts = crate::text::TextStyle::new(14.0);
        // measure 的 ceil 会给出最多 1 逻辑像素的余量，多数文本宽度下正好吸收掉取整截短；
        // 只有 `物理宽 / scale` 逼近整数时余量趋零才暴露。故扫一批长度找临界点，
        // 而不是赌某一个字符串。
        let full = "非整数DPI下不应折行的一行字abcdefghij0123456789";
        let texts: Vec<String> = (4..=full.chars().count())
            .map(|n| full.chars().take(n).collect())
            .collect();
        for s in [1.25f32, 1.5, 1.75, 2.25, 3.0] {
            eng.set_scale(s);
            for text in &texts {
                let logical = eng.measure(text, &ts, None);
                // x 取多个值：物理化取整的分歧只在 x*s 的小数部分落到特定区间时才暴露。
                for x in 0..8 {
                    let rect = Rect::new(x, 0, logical.w, logical.h);
                    // 与 draw() 中排版宽度的算法保持同源。
                    let layout_max_w = rect.scaled_out(s).w as f32;
                    let layout = eng
                        .layout(text, &ts, ts.size * s, layout_max_w)
                        .expect("排版应成功");
                    let mut m = DWRITE_TEXT_METRICS::default();
                    unsafe { layout.GetMetrics(&mut m) }.expect("取度量应成功");
                    assert_eq!(
                        m.lineCount, 1,
                        "s={s} x={x} {:?}: 逻辑宽 {} → 排版宽 {layout_max_w} 装不下 measure 出的单行文本",
                        text, logical.w
                    );
                }
            }
        }
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

/// 私用区分段的测试。`pua_runs` 是纯函数（不碰 DirectWrite），故不需要图形环境。
///
/// 覆盖三段私用区各自的识别、非私用区代理对的排除、以及段边界的合并规则——
/// 这几条正是「图标字体渲染成方框」类问题的根源所在：漏判一段，那一段就静默
/// 落回主字体。
#[cfg(test)]
mod pua_runs_tests {
    use super::pua_runs;

    fn runs(s: &str) -> Vec<(usize, usize)> {
        pua_runs(&s.encode_utf16().collect::<Vec<u16>>())
    }

    /// BMP 私用区（U+E000..=U+F8FF）：多数图标集所在段，单码元。
    #[test]
    fn bmp_pua_run_is_detected() {
        assert_eq!(runs("\u{E0E1}\u{E124}\u{E147}\u{E13D}"), vec![(0, 4)]);
    }

    /// 补充私用区 A（第 15 平面）：各占 2 个码元，故长度是字符数的两倍。
    #[test]
    fn spua_a_run_is_detected() {
        assert_eq!(runs("\u{F00FD}\u{F00F7}\u{F013C}"), vec![(0, 6)]);
    }

    /// 补充私用区 B（第 16 平面）同样纳入。
    #[test]
    fn spua_b_run_is_detected() {
        assert_eq!(runs("\u{100000}\u{10FFFD}"), vec![(0, 4)]);
    }

    /// 非私用区的**代理对不得命中**——CJK 扩展 B（U+20000）等生僻字若被误切到
    /// 图标字体，反而会变成方框。这是判据不能只看「是不是代理对」的原因。
    #[test]
    fn non_pua_supplementary_chars_are_excluded() {
        assert!(runs("\u{20000}\u{2A6DF}").is_empty());
    }

    /// 混排时下标以 UTF-16 码元计，可直接用作 DWRITE_TEXT_RANGE。
    #[test]
    fn mixed_text_run_offsets_are_utf16_units() {
        // "ab" + PUA + "c" → 段起点 2、长度 1
        assert_eq!(runs("ab\u{E001}c"), vec![(2, 1)]);
    }

    /// 被普通文本隔开的两段不合并。
    #[test]
    fn separate_runs_are_not_merged_across_plain_text() {
        assert_eq!(runs("\u{E001}x\u{E002}"), vec![(0, 1), (2, 1)]);
    }

    /// 相邻的 BMP 与补充私用区字符合并为一段——目标字体族相同，合并只减少
    /// SetFontFamilyName 调用次数，不改变渲染结果。
    #[test]
    fn adjacent_bmp_and_supplementary_pua_merge() {
        assert_eq!(runs("\u{E001}\u{F00FD}"), vec![(0, 3)]);
    }

    /// 孤立高位代理（非法 UTF-16 片段）不得命中，否则 range 会越界。
    #[test]
    fn lone_lead_surrogate_is_ignored() {
        assert!(pua_runs(&[0xDB80]).is_empty());
    }

    /// 空串与纯普通文本不产生段。
    #[test]
    fn empty_and_plain_text_yield_no_runs() {
        assert!(runs("").is_empty());
        assert!(runs("hello 世界").is_empty());
    }
}

/// 私用区字体注册的 COM 链路与接线测试。需要真实 DirectWrite，故随 Windows 测试跑。
#[cfg(test)]
mod private_use_font_tests {
    use super::{build_private_use_font, register_private_use_font, DWriteEngine};

    /// 系统自带、各 Windows 版本均存在，用来验证「加载文件为自建字体集」这条 COM 链路。
    /// 它不是图标字体，但注册流程与字体内容无关。
    const SYSTEM_FONT: &str = r"C:\Windows\Fonts\segoeui.ttf";

    /// COM 链路可用：文件 → FontFileReference → FontSet → FontCollection1 全程不报错。
    #[test]
    fn builds_collection_from_font_file() {
        assert!(
            std::path::Path::new(SYSTEM_FONT).exists(),
            "测试前提：{SYSTEM_FONT} 应存在于任何 Windows 上"
        );
        assert!(build_private_use_font(SYSTEM_FONT, "Segoe UI").is_ok());
    }

    /// 路径不存在时返回 Err 而非 panic，且**不改动注册表**——注册失败应保持原状态，
    /// 让图标退回方框，而不是把引擎推进半初始化的状态。
    #[test]
    fn missing_file_returns_err_and_leaves_registry_intact() {
        let before = super::PRIVATE_USE_FONT.with(|f| f.borrow().is_some());
        assert!(register_private_use_font(r"Z:\no\such\font.ttf", "Nope").is_err());
        let after = super::PRIVATE_USE_FONT.with(|f| f.borrow().is_some());
        assert_eq!(before, after, "失败的注册不得改动注册表");
    }

    /// 接线：注册后新建的引擎应当拿到该字体。
    ///
    /// 没有这条，前两条测试在「注册表写了但引擎从不读」时也会全绿——而引擎读不到
    /// 就等于功能没接上，图标照样是方框。
    ///
    /// 注册表是 thread_local，Rust 测试各跑各的线程，故本用例的写入不影响其它用例。
    #[test]
    fn registered_font_reaches_new_engine() {
        assert!(
            DWriteEngine::new().private_use.is_none(),
            "未注册时引擎不应持有回退字体"
        );
        register_private_use_font(SYSTEM_FONT, "Segoe UI").expect("注册系统字体");
        assert!(
            DWriteEngine::new().private_use.is_some(),
            "注册后新建的引擎应拿到回退字体"
        );
    }

    /// 运行期替换会清掉测量缓存——私用区字符换了字形来源，宽度随之改变。
    #[test]
    fn set_private_use_font_clears_caches() {
        use crate::text::{TextEngine, TextStyle};
        let mut eng = DWriteEngine::new();
        eng.set_scale(1.0);
        eng.measure("测量填充缓存", &TextStyle::new(14.0), None);
        assert!(!eng.measure_cache.is_empty(), "前提：缓存应已有内容");
        eng.set_private_use_font(SYSTEM_FONT, "Segoe UI")
            .expect("注册系统字体");
        assert!(eng.measure_cache.is_empty(), "换字体须清空测量缓存");
        assert!(eng.metrics_cache.is_empty(), "换字体须清空基线缓存");
    }
}

/// 合成算术的纯函数测试：不碰 DirectWrite，也不需要图形环境。
///
/// 端到端用例（`layer_backdrop_tests`）证明"文字出现了"，这一组证明"数值是对的"——
/// 覆盖率反解、预乘不变式、以及**不透明背景恒等**这条零回归的硬约束。
#[cfg(test)]
mod glyph_composite_tests {
    use super::{composite_glyph_over_translucent, glyph_backdrop, Color};
    use tiny_skia::PremultipliedColorU8;

    fn premul(r: u8, g: u8, b: u8, a: u8) -> PremultipliedColorU8 {
        PremultipliedColorU8::from_rgba(r, g, b, a).unwrap()
    }
    fn rgba(c: PremultipliedColorU8) -> (u8, u8, u8, u8) {
        (c.red(), c.green(), c.blue(), c.alpha())
    }

    /// 不透明目标像素照旧拿到真背景——这条保证不透明路径的输入没变。
    #[test]
    fn opaque_pixel_gets_the_real_background() {
        let d = premul(30, 60, 90, 255);
        assert_eq!(glyph_backdrop(Color::hex(0xFFFFFF), d), (30, 60, 90));
    }

    /// 非不透明目标像素拿探针色：每通道与文字色相距 ≥128。
    #[test]
    fn translucent_pixel_gets_a_far_probe() {
        let transparent = premul(0, 0, 0, 0);
        for hex in [0x000000, 0xFFFFFF, 0x7F80A0, 0x3366CC] {
            let c = Color::hex(hex);
            let (pr, pg, pb) = glyph_backdrop(c, transparent);
            for (ch, p) in [(c.r, pr), (c.g, pg), (c.b, pb)] {
                assert!(p == 0 || p == 255, "探针只能取黑或白，实得 {p}");
                assert!(
                    (ch as i32 - p as i32).abs() >= 128,
                    "文字色 {ch} 与探针 {p} 相距不足 128，覆盖率会失精"
                );
            }
        }
    }

    /// 全覆盖 → 目标像素变成完全不透明的文字色，与原背景无关。
    #[test]
    fn full_coverage_yields_opaque_text_color() {
        let transparent = premul(0, 0, 0, 0);
        for hex in [0x000000, 0xFFFFFF, 0x3366CC] {
            let c = Color::hex(hex);
            let bd = glyph_backdrop(c, transparent);
            // 全覆盖即 DirectWrite 混出文字色本身。
            let out = composite_glyph_over_translucent(transparent, c, bd, (c.r, c.g, c.b));
            assert_eq!(
                rgba(out),
                (c.r, c.g, c.b, 255),
                "全覆盖应得不透明文字色 {hex:#08X}"
            );
        }
    }

    /// 半覆盖 → alpha 落在中段，预乘 RGB 随之减半。
    #[test]
    fn half_coverage_yields_half_alpha() {
        let transparent = premul(0, 0, 0, 0);
        let c = Color::hex(0xFFFFFF);
        let bd = glyph_backdrop(c, transparent); // (0,0,0)
        let out = composite_glyph_over_translucent(transparent, c, bd, (128, 128, 128));
        let (r, g, b, a) = rgba(out);
        assert!((120..=136).contains(&a), "半覆盖 alpha 应在中段，实得 {a}");
        assert_eq!((r, g, b), (a, a, a), "白字预乘后 RGB 应等于 alpha");
    }

    /// 纯黑文字在透明层上：预乘 RGB 恒为 0，可见性全靠 alpha——探针把它救了回来。
    #[test]
    fn pure_black_text_survives_on_a_transparent_layer() {
        let transparent = premul(0, 0, 0, 0);
        let c = Color::hex(0x000000);
        let bd = glyph_backdrop(c, transparent);
        assert_eq!(bd, (255, 255, 255), "黑字的探针必须是白");
        assert_eq!(
            rgba(composite_glyph_over_translucent(
                transparent,
                c,
                bd,
                (0, 0, 0)
            )),
            (0, 0, 0, 255),
            "全覆盖的黑字应为不透明黑"
        );
        let half = composite_glyph_over_translucent(transparent, c, bd, (128, 128, 128));
        assert!(
            (120..=136).contains(&half.alpha()),
            "半覆盖的黑字 alpha 应在中段，实得 {}",
            half.alpha()
        );
    }

    /// 文字色自身半透明时，有效覆盖率随之打折。
    #[test]
    fn foreground_alpha_scales_the_coverage() {
        let transparent = premul(0, 0, 0, 0);
        let opaque_fg = Color::rgba(255, 255, 255, 255);
        let half_fg = Color::rgba(255, 255, 255, 128);
        let bd = glyph_backdrop(opaque_fg, transparent);
        let full = composite_glyph_over_translucent(transparent, opaque_fg, bd, (255, 255, 255));
        let faded = composite_glyph_over_translucent(transparent, half_fg, bd, (255, 255, 255));
        assert_eq!(full.alpha(), 255);
        assert!(
            (124..=132).contains(&faded.alpha()),
            "半透明文字色应把覆盖率折半，实得 {}",
            faded.alpha()
        );
    }

    /// 已有半透明内容之上叠字：alpha 只增不减（source-over 的基本性质）。
    #[test]
    fn compositing_never_lowers_the_destination_alpha() {
        let c = Color::hex(0xFFFFFF);
        for bg_a in [0u8, 1, 64, 128, 254] {
            let d = premul(0, 0, 0, bg_a);
            let bd = glyph_backdrop(c, d);
            for n in [0u8, 40, 128, 200, 255] {
                let out = composite_glyph_over_translucent(d, c, bd, (n, n, n));
                assert!(
                    out.alpha() >= bg_a,
                    "alpha 不应下降：bg_a={bg_a} n={n} 实得 {}",
                    out.alpha()
                );
            }
        }
    }

    /// 预乘不变式在整个输入空间上成立——越界会让 `from_rgba` 返回 None，
    /// 进而静默丢弃该像素（表现为字形上的洞）。
    #[test]
    fn output_is_always_valid_premultiplied() {
        let c = Color::rgba(200, 30, 90, 170);
        for bg_a in [0u8, 1, 77, 128, 200, 254] {
            let d = premul(bg_a / 3, bg_a / 2, bg_a, bg_a);
            let bd = glyph_backdrop(c, d);
            for n in 0..=255u8 {
                let out = composite_glyph_over_translucent(d, c, bd, (n, n / 2, 255 - n));
                let a = out.alpha();
                assert!(
                    out.red() <= a && out.green() <= a && out.blue() <= a,
                    "预乘越界 bg_a={bg_a} n={n}: {:?}",
                    rgba(out)
                );
            }
        }
    }
}

/// 端到端：真实 DirectWrite 画进**透明离屏层**（`Element::opacity` 的载体）。
///
/// ★ 回归：读回阶段曾把输出 alpha 直接取自目标像素原有 alpha（"文字混入不透明背景
/// 仍不透明"）。该前提在透明层上不成立——`bg_a == 0` 时每个文字像素都被写成
/// `(0,0,0,0)`，整段文字被抹平，于是任何 `.opacity()` 子树里的文字**完全消失**，
/// 且与 alpha 取值无关（0.99 与 0.65 一样全没）。
#[cfg(test)]
mod layer_backdrop_tests {
    use super::{Color, DWriteEngine, Rect};
    use crate::spec::Align;
    use crate::text::{TextEngine, TextStyle};
    use tiny_skia::Pixmap;

    const W: u32 = 140;
    const H: u32 = 30;

    /// 在指定背景上画一行字，返回结果 pixmap。`fill=None` 即全透明层。
    fn draw(fill: Option<tiny_skia::Color>, color: Color) -> Pixmap {
        let mut pm = Pixmap::new(W, H).unwrap();
        if let Some(c) = fill {
            pm.fill(c);
        }
        let mut eng = DWriteEngine::new();
        eng.set_scale(1.0);
        eng.draw(
            &mut pm,
            "Wg吗",
            Rect::new(0, 0, W as i32, H as i32),
            color,
            Align::Start,
            &TextStyle::new(18.0),
            None,
        );
        pm
    }

    /// 与初始背景不同的像素数（"画上去了多少墨"）。
    fn ink(pm: &Pixmap, bg: tiny_skia::PremultipliedColorU8) -> usize {
        pm.pixels()
            .iter()
            .filter(|p| {
                p.red() != bg.red()
                    || p.green() != bg.green()
                    || p.blue() != bg.blue()
                    || p.alpha() != bg.alpha()
            })
            .count()
    }

    /// 全透明层的初始像素值。
    fn transparent() -> tiny_skia::PremultipliedColorU8 {
        tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, 0).unwrap()
    }

    /// 浅色文字画在全透明层上必须留下不透明度可见的墨。
    #[test]
    fn light_text_on_transparent_layer_is_visible() {
        let pm = draw(None, Color::hex(0xE6E6E6));
        let n = ink(&pm, transparent());
        assert!(n > 20, "透明层上的文字不应被抹平，实得着墨像素 {n}");
        assert!(
            pm.pixels().iter().any(|p| p.alpha() > 200),
            "字心应接近不透明"
        );
    }

    /// 纯黑文字是最狠的边界：它与"透明黑"背景的 RGB 完全相同，若判据是
    /// 「RGB 未变即未被字形覆盖」，整段文字会被判成背景而整体跳过。
    #[test]
    fn pure_black_text_on_transparent_layer_is_visible() {
        let pm = draw(None, Color::hex(0x000000));
        let n = ink(&pm, transparent());
        assert!(n > 20, "纯黑文字在透明层上不应消失，实得着墨像素 {n}");
        // 预乘形态：RGB 恒为 0，可见性完全体现在 alpha 上。
        assert!(
            pm.pixels().iter().any(|p| p.alpha() > 200),
            "黑字字心 alpha 应接近 255"
        );
    }

    /// 预乘不变式：任何通道都不得超过 alpha，否则合成到父层会溢出成亮边。
    #[test]
    fn transparent_layer_output_stays_premultiplied() {
        for c in [0x000000, 0xE6E6E6, 0x3366CC] {
            let pm = draw(None, Color::hex(c));
            for p in pm.pixels() {
                let a = p.alpha();
                assert!(
                    p.red() <= a && p.green() <= a && p.blue() <= a,
                    "预乘越界：rgba=({},{},{},{}) 文字色 {c:#08X}",
                    p.red(),
                    p.green(),
                    p.blue(),
                    a
                );
            }
        }
    }

    /// 不透明背景仍走原路径：输出恒不透明，且确实画上了字。
    #[test]
    fn text_on_opaque_background_stays_opaque() {
        let white = tiny_skia::PremultipliedColorU8::from_rgba(255, 255, 255, 255).unwrap();
        let pm = draw(Some(tiny_skia::Color::WHITE), Color::hex(0x000000));
        assert!(ink(&pm, white) > 20, "不透明白底上应画出黑字");
        assert!(
            pm.pixels().iter().all(|p| p.alpha() == 255),
            "不透明背景上的输出必须逐像素保持 alpha=255"
        );
    }

    /// 半透明背景（0 < a < 255）：文字应把该处推向更不透明，而非维持原 alpha。
    #[test]
    fn text_on_translucent_background_raises_alpha() {
        let half = tiny_skia::Color::from_rgba8(0, 0, 0, 128);
        let pm = draw(Some(half), Color::hex(0xFFFFFF));
        let max_a = pm.pixels().iter().map(|p| p.alpha()).max().unwrap();
        assert!(
            max_a > 200,
            "字心应把半透明背景推向不透明，实得最大 alpha {max_a}"
        );
    }
}
