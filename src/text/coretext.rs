//! macOS 文字引擎（Core Text）：排版 + 抗锯齿合成，绘制进 tiny-skia pixmap。
//!
//! 渲染路径（直接合成）：
//! 1. 用 `CGBitmapContextCreate` 把 pixmap 的像素缓冲（RGBA8 预乘）**原地**包成一个
//!    位图上下文——tiny-skia 的像素格式与 CG 的 `PremultipliedLast` + DeviceRGB 完全一致，
//!    故无需中转缓冲，Core Text 直接在真实背景上抗锯齿混合（gamma 由系统处理）。
//! 2. 单行用 `CTLine`（手动按 `align` 定位，支持负偏移做水平滚动）；折行用 `CTFramesetter`
//!    + `CTFrame`（段落样式带对齐），按 `rect`×`scale` 物理化定位、垂直居中。
//!
//! 坐标系：Core Graphics 原点在左下、Y 轴向上；而 pixmap 行序自上而下。把自上而下的缓冲
//! 交给 CG 后，CG 视第 0 行为**底**——于是"在 CG 空间正立绘制的字形，落到自上而下的内存里
//! 也正好正立"。故**不翻转上下文**，只把基线/矩形的 y 由"距顶"换算成"距底"（`ph - 距顶`）。
//!
//! 对照实现：`src/text/dwrite.rs`（DirectWrite 版，含 scale 物理化与裁剪合成的完整思路）。

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use tiny_skia::Pixmap;

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFAttributedString,
    CFDictionary, CFRange, CFRetained, CFString, CGAffineTransform, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColor, CGColorSpace, CGContext, CGImageAlphaInfo, CGPath,
};
use objc2_core_text::{
    kCTFontAttributeName, kCTForegroundColorAttributeName, kCTParagraphStyleAttributeName, CTFont,
    CTFramesetter, CTLine, CTParagraphStyle, CTParagraphStyleSetting, CTParagraphStyleSpecifier,
    CTTextAlignment,
};

use super::{AlphaMask, GlyphSource, RunRequest, TextEngine, TextStyle};
use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

const DEFAULT_FAMILY: &str = "PingFang SC"; // 中文友好的 macOS 系统字体

/// 单位变换矩阵（绘制文字前复位文本矩阵，避免继承翻转）。
const IDENTITY: CGAffineTransform = CGAffineTransform {
    a: 1.0,
    b: 0.0,
    c: 0.0,
    d: 1.0,
    tx: 0.0,
    ty: 0.0,
};

/// Core Text 文字引擎。
///
/// 约束：内部 Core Text/Graphics 对象须在 UI 线程上使用，不可跨线程共享。
pub struct CoreTextEngine {
    /// DPI 缩放因子（逻辑→物理）。measure/draw 据此物理化字号与排版。
    scale: f32,
    /// 缓存 CTFont，按 (family, 物理字号 bits) 复用，避免每次绘字都创建字体对象。
    fonts: HashMap<(String, u32), CFRetained<CTFont>>,
    /// 复用的 DeviceRGB 色彩空间。
    color_space: CFRetained<CGColorSpace>,
}

impl CoreTextEngine {
    pub fn new() -> Self {
        let color_space = CGColorSpace::new_device_rgb().expect("CGColorSpaceCreateDeviceRGB 失败");
        Self {
            scale: 1.0,
            fonts: HashMap::new(),
            color_space,
        }
    }

    /// 取（缓存的）指定字族与物理字号的 CTFont。
    fn font(&mut self, family: Option<&str>, psize: f32) -> CFRetained<CTFont> {
        let fam = family.unwrap_or(DEFAULT_FAMILY).to_string();
        let key = (fam.clone(), psize.to_bits());
        if let Some(f) = self.fonts.get(&key) {
            return f.clone();
        }
        let name = CFString::from_str(&fam);
        // matrix=null → 用字号本身的缩放，正立无旋转。
        let font = unsafe { CTFont::with_name(&name, psize as f64, ptr::null()) };
        self.fonts.insert(key, font.clone());
        font
    }

    /// 用 (font, color, align) 组装属性字典 → CFAttributedString。
    /// 段落样式仅折行路径用到；单行路径手动定位，故对其无影响（保留一条路径即可）。
    /// 构造带段落样式的属性串。`line_h` 为**物理**行高（None = 用字体自带行距）。
    fn attributed(
        &mut self,
        text: &str,
        font: &CTFont,
        color: &CGColor,
        align: Align,
        line_h: Option<f32>,
    ) -> CFRetained<CFAttributedString> {
        let ct_align = match align {
            Align::Start | Align::Stretch => CTTextAlignment::Natural,
            Align::Center => CTTextAlignment::Center,
            Align::End => CTTextAlignment::Right,
        };
        // 行高同时设最小与最大，等价于 DirectWrite 的 UNIFORM——只设其一时 Core Text
        // 仍会让行高随最高字形浮动，中西文混排下行距就会参差。
        let lh = line_h.unwrap_or(0.0) as f64;
        let mut settings = vec![CTParagraphStyleSetting {
            spec: CTParagraphStyleSpecifier::Alignment,
            valueSize: std::mem::size_of::<CTTextAlignment>(),
            value: NonNull::from(&ct_align).cast(),
        }];
        if line_h.is_some() {
            settings.push(CTParagraphStyleSetting {
                spec: CTParagraphStyleSpecifier::MinimumLineHeight,
                valueSize: std::mem::size_of::<f64>(),
                value: NonNull::from(&lh).cast(),
            });
            settings.push(CTParagraphStyleSetting {
                spec: CTParagraphStyleSpecifier::MaximumLineHeight,
                valueSize: std::mem::size_of::<f64>(),
                value: NonNull::from(&lh).cast(),
            });
        }
        let para = unsafe { CTParagraphStyle::new(settings.as_ptr(), settings.len()) };

        // 属性名是 Core Text 的 extern static（CFString 常量），取其指针需 unsafe。
        let mut keys: [*const c_void; 3] = unsafe {
            [
                (kCTFontAttributeName as *const CFString).cast(),
                (kCTForegroundColorAttributeName as *const CFString).cast(),
                (kCTParagraphStyleAttributeName as *const CFString).cast(),
            ]
        };
        let mut vals: [*const c_void; 3] = [
            (font as *const CTFont).cast(),
            (color as *const CGColor).cast(),
            (&*para as *const CTParagraphStyle).cast(),
        ];
        let dict = unsafe {
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                vals.as_mut_ptr(),
                3,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .expect("CFDictionaryCreate 失败");
        let cfstr = CFString::from_str(text);
        unsafe { CFAttributedString::new(None, Some(&cfstr), Some(&dict)) }
            .expect("CFAttributedStringCreate 失败")
    }
}

impl Default for CoreTextEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// 单行/折行判据：无显式换行符、且整行排版宽装得进排版宽度。
///
/// 抽成函数是因为它有**两个**调用点（直接合成的 `draw` 与光栅到位图的 `raster_run`），
/// 而两者判得不一样的话，同一段文字在软后端是一行、在 GPU 后端却折成两行——这种差异
/// 不会报错，只会让两条路径的截图对不上，且要到很后面才归因到这一行判据上。
fn is_single_line(text: &str, line_w: f64, layout_w: f64) -> bool {
    !text.contains('\n') && line_w <= layout_w
}

/// 字形出挑余量（物理像素）：位图四周各留这么多，防止音调符、斜体右出挑、
/// 某些字体越出 ascent/descent 的部件被位图边界裁掉（见 [`AlphaMask::pad`]）。
///
/// 取物理字号的 10%、下限 2 上限 16：常见 UI 字号（12~24dp）下是 2~4px，够覆盖
/// PingFang/Helvetica 的出挑，又不至于把每张位图都撑大一圈。
fn overhang_pad(psize: f32) -> u32 {
    (psize * 0.1).ceil().clamp(2.0, 16.0) as u32
}

/// 单张 mask 的物理尺寸上限。超限直接不画而不是分块：这个量级的**单段**文字在 UI 里
/// 不存在，出现即调用方传了个荒谬的 rect，画出来也没人看得到。
const MAX_MASK_DIM: u32 = 8192;

/// 取 CTLine 的排版尺寸：返回 (宽, 上行高 ascent, 下行高 descent, 行距 leading)，单位物理像素。
fn line_metrics(line: &CTLine) -> (f64, f64, f64, f64) {
    let mut ascent = 0.0f64;
    let mut descent = 0.0f64;
    let mut leading = 0.0f64;
    let width = unsafe { line.typographic_bounds(&mut ascent, &mut descent, &mut leading) };
    (width, ascent, descent, leading)
}

impl TextEngine for CoreTextEngine {
    fn set_scale(&mut self, scale: f32) {
        self.scale = scale.max(0.1);
    }

    /// 漏了这个 getter 的后果远超"少一个访问器"：`TextEngine::scale` 的默认实现恒返回
    /// 1.0，于是**测量路径**（`EngineMeasurer`）看到 1.0、**绘制路径**（`CanvasMeasurer`
    /// → `Canvas::dpi_scale`）看到真实的 2.0。富文本把 scale 计入布局缓存键，两条路径
    /// 就此逐帧互相顶掉：Retina 上每帧重排整篇文档，而重排会清空选区——表现为"划选
    /// 之后高亮立刻消失、Ctrl+C 复制到全文"。Windows 的 DirectWrite 引擎两个都实现了，
    /// 故只有 macOS 中招。
    fn scale(&self) -> f32 {
        self.scale
    }

    fn glyph_source(&mut self) -> Option<&mut dyn GlyphSource> {
        Some(self)
    }

    fn measure(&mut self, text: &str, ts: &TextStyle, max_width: Option<f32>) -> Size {
        let size = ts.size;
        if text.is_empty() {
            return Size::new(0, ts.line_height_px().unwrap_or(size).ceil() as i32);
        }
        let s = self.scale;
        let psize = size * s;
        let font = self.font(ts.family, psize);
        // 颜色与对齐不影响测量，取占位值。
        let black = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        let plh = ts.line_height_px().map(|h| h * s);
        let attr = self.attributed(text, &font, &black, Align::Start, plh);

        match max_width {
            // 折行：用 framesetter 在宽度内排版，取建议尺寸。
            Some(w) if w > 0.0 => {
                let fs = unsafe { CTFramesetter::with_attributed_string(&attr) };
                let constraints = CGSize {
                    width: (w * s) as f64,
                    height: f64::MAX,
                };
                let fit = unsafe {
                    fs.suggest_frame_size_with_constraints(
                        CFRange {
                            location: 0,
                            length: 0,
                        },
                        None,
                        constraints,
                        ptr::null_mut(),
                    )
                };
                Size::new(
                    (fit.width / s as f64).ceil() as i32,
                    (fit.height / s as f64).ceil() as i32,
                )
            }
            // 单行不换行：CTLine 排版宽 + 行高（ascent+descent+leading）。
            _ => {
                let line = unsafe { CTLine::with_attributed_string(&attr) };
                let (width, ascent, descent, leading) = line_metrics(&line);
                // 显式行高优先：CTLine 的 typographic bounds 反映的是字形度量，
                // 不含段落样式强制的行高。
                let line_h = plh.map(|h| h as f64).unwrap_or(ascent + descent + leading);
                Size::new(
                    (width / s as f64).ceil() as i32,
                    (line_h / s as f64).ceil() as i32,
                )
            }
        }
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
        let s = self.scale;
        let prect = rect.scaled(s);
        let pclip = clip.map(|c| c.scaled(s));
        let psize = size * s;

        let pw = pixmap.width() as i32;
        let ph = pixmap.height() as i32;
        let phf = ph as f64;

        // 把 pixmap 缓冲原地包成位图上下文（RGBA8 预乘，与 tiny-skia 同格式）。
        let bytes_per_row = pw as usize * 4;
        let data = pixmap.data_mut().as_mut_ptr() as *mut c_void;
        let ctx = match unsafe {
            CGBitmapContextCreate(
                data,
                pw as usize,
                ph as usize,
                8,
                bytes_per_row,
                Some(&self.color_space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        } {
            Some(c) => c,
            None => return,
        };

        let font = self.font(ts.family, psize);
        let cg_color = CGColor::new_srgb(
            color.r as f64 / 255.0,
            color.g as f64 / 255.0,
            color.b as f64 / 255.0,
            color.a as f64 / 255.0,
        );
        let plh = ts.line_height_px().map(|h| h * s);
        let attr = self.attributed(text, &font, &cg_color, align, plh);

        // 单行测量，判定是否需要折行（无换行符且整行宽 ≤ rect 宽 → 单行，支持水平滚动）。
        let probe = unsafe { CTLine::with_attributed_string(&attr) };
        let (line_w, ascent, descent, leading) = line_metrics(&probe);
        // 排版宽度用 scaled_out（外扩取整，恒 >= rect.w * s），与 measure 的 pmw = max_width * s
        // 同源；prect.w 由四边各自 round 得来，可能略窄于 rect.w * s，使 measure 判定放得下的
        // 文本在这里被提前折行（非整数 DPI 下末字掉到下一行）。定位仍用 prect。
        let playout_w = rect.scaled_out(s).w as f64;
        let single = is_single_line(text, line_w, playout_w);

        CGContext::save_g_state(Some(&ctx));
        CGContext::set_allows_antialiasing(Some(&ctx), true);
        // 裁剪到可见矩形（滚动视口等）：距顶 → 距底换算。
        if let Some(c) = pclip {
            let cg = CGRect {
                origin: CGPoint {
                    x: c.x as f64,
                    y: phf - (c.y + c.h) as f64,
                },
                size: CGSize {
                    width: c.w as f64,
                    height: c.h as f64,
                },
            };
            CGContext::clip_to_rect(Some(&ctx), cg);
        }
        CGContext::set_text_matrix(Some(&ctx), IDENTITY);

        if single {
            // 单行：按 align 手动定位 x（支持 prect.x 为负的水平滚动），垂直居中。
            let line_h = ascent + descent + leading;
            let text_x0 = match align {
                Align::Start | Align::Stretch => prect.x as f64,
                Align::Center => prect.x as f64 + (prect.w as f64 - line_w) / 2.0,
                Align::End => prect.x as f64 + prect.w as f64 - line_w,
            };
            // 同折行分支：装不下时顶对齐而非居中溢出（行高大于容器高的窄行场景）。
            let baseline_from_top =
                prect.y as f64 + (prect.h as f64 - line_h).max(0.0) / 2.0 + ascent;
            let cg_y = phf - baseline_from_top;
            CGContext::set_text_position(Some(&ctx), text_x0, cg_y);
            unsafe { probe.draw(&ctx) };
        } else {
            // 折行：framesetter 在 rect 宽内排版，段落样式负责水平对齐，整体垂直居中。
            let fs = unsafe { CTFramesetter::with_attributed_string(&attr) };
            let constraints = CGSize {
                width: playout_w,
                height: f64::MAX,
            };
            let fit = unsafe {
                fs.suggest_frame_size_with_constraints(
                    CFRange {
                        location: 0,
                        length: 0,
                    },
                    None,
                    constraints,
                    ptr::null_mut(),
                )
            };
            let text_h = fit.height;
            // 装得下→垂直居中；装不下→顶对齐。`.max(0)` 是与 Windows 两条路径
            // （dwrite `oy`、d2d `draw_text`）共有的约定，缺了它差值为负，文本会
            // 以容器中心为中心向上下**溢出**——表格单元格放不下多行时，Windows 顶
            // 对齐截断、macOS 却上下各露半行，同一份数据两个平台不一样高。
            let top_from_top = prect.y as f64 + (prect.h as f64 - text_h).max(0.0) / 2.0;
            let path_rect = CGRect {
                origin: CGPoint {
                    x: prect.x as f64,
                    y: phf - (top_from_top + text_h),
                },
                // 高度多留 1px，避免末行被边界裁掉。宽度与 constraints 同源，否则实际
                // 排版宽度回落到 prect.w，suggest_frame_size 的换行结果对不上。
                size: CGSize {
                    width: playout_w,
                    height: text_h.ceil() + 1.0,
                },
            };
            let path = unsafe { CGPath::with_rect(path_rect, ptr::null()) };
            let frame = unsafe {
                fs.frame(
                    CFRange {
                        location: 0,
                        length: 0,
                    },
                    &path,
                    None,
                )
            };
            unsafe { frame.draw(&ctx) };
        }

        CGContext::restore_g_state(Some(&ctx));
        // ctx（仅包裹 pixmap 缓冲、未持有像素所有权）在此析构，pixmap 内容已就绪。
    }
}

/// GPU 后端的整段光栅：同一套排版代码，只是把字画到**透明背景的独立位图**上，
/// 取其覆盖度交给 GPU 调色合成（见 `render/gpu/text.rs`）。
///
/// # 为什么是「白底黑字取反」而不是「透明底白字取 alpha」
///
/// 覆盖度位图的自然做法是画到透明背景上直接取 alpha 通道。**实测那样出来的字明显偏细**：
/// 同一段 14dp 英文，软后端的墨量比它高 13.5%、纯黑像素多 20%（48dp 下同样是 15%，
/// 说明差在字形本体而不是抗锯齿边缘）。
///
/// 成因是 Core Graphics 的字形加重（stem darkening / font smoothing）**按前景与背景的
/// 明暗关系取量**：深字浅底加重少，浅字深底加重多。透明背景在 CG 眼里接近黑，于是
/// 「白字画在透明底」被当成浅字深底——把 smoothing 显式关掉字就细了 13%，显式打开
/// 又粗了 22%，两头都不对，因为那是个连续量而不是开关。
///
/// 于是改成让 mask 上下文与真实绘制路径**处在同一种明暗关系**里：铺不透明白底、画
/// 黑字、取 `255 - R` 作覆盖度，smoothing 一概不碰（与 [`TextEngine::draw`] 一样用
/// 上下文默认值）。代价是缓存的这份加重量固定按「深字浅底」来——颜色进不了缓存键
/// （进了缓存就没意义了），只能选一种。浅色主题（UI 的主流）下两条路径同源；深色主题
/// 下软后端会比它再重一点点，属于已知偏差。
///
/// # 与 [`TextEngine::draw`] 的另一处差异：不带位置
///
/// `draw` 知道 rect 的 x/y，能用 `Rect::scaled_out` 把排版宽度与 `measure` 对齐到同一个
/// 物理值；这里只有逻辑宽度（位置进不了缓存键——同一个标签出现在两个 x 就会各占一条
/// 缓存），排版宽取 `ceil(max_width × scale)`。两者最多差一个物理像素列，只在非整数
/// DPI 且文本恰好卡在折行边界时显形。
impl GlyphSource for CoreTextEngine {
    fn raster_run(&mut self, req: &RunRequest) -> Option<AlphaMask> {
        let ts = &req.style;
        if req.text.is_empty() || ts.size <= 0.0 {
            return None;
        }
        let s = req.scale.max(0.1);
        let psize = ts.size * s;
        let font = self.font(ts.family, psize);
        // 黑字画在不透明白底上（理由见 impl 头注释）：覆盖度 = 255 − 任一颜色通道。
        let black = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        let plh = ts.line_height_px().map(|h| h * s);
        let attr = self.attributed(req.text, &font, &black, req.align, plh);

        let playout_w = (req.max_width.max(0.0) * s).ceil() as f64;
        let probe = unsafe { CTLine::with_attributed_string(&attr) };
        let (line_w, ascent, descent, leading) = line_metrics(&probe);
        let single = is_single_line(req.text, line_w, playout_w);

        // 文本块的物理排版尺寸。单行取自然行高（ascent+descent+leading）而非显式行高
        // ——与 `draw` 的单行分支同源：那里的垂直定位也用自然行高，基线落在块顶 + ascent。
        let (block_w, block_h, framesetter) = if single {
            (line_w, ascent + descent + leading, None)
        } else {
            let fs = unsafe { CTFramesetter::with_attributed_string(&attr) };
            let fit = unsafe {
                fs.suggest_frame_size_with_constraints(
                    CFRange {
                        location: 0,
                        length: 0,
                    },
                    None,
                    CGSize {
                        width: playout_w,
                        height: f64::MAX,
                    },
                    ptr::null_mut(),
                )
            };
            // 块宽取排版容器宽而非 fit.width：折行时各行的水平对齐是段落样式在
            // **容器**内做的，块宽若收窄到最长行，右/居中对齐的行就会整体偏移。
            (playout_w, fit.height, Some(fs))
        };
        let bw = (block_w.ceil().max(1.0)) as u32;
        let bh = (block_h.ceil().max(1.0)) as u32;
        let pad = overhang_pad(psize);
        let (w, h) = (bw + 2 * pad, bh + 2 * pad);
        if w > MAX_MASK_DIM || h > MAX_MASK_DIM {
            return None;
        }

        // RGBA8 预乘、**不透明白底**的位图：与 `draw` 走同一类上下文（DeviceRGB +
        // PremultipliedLast）、同一种明暗关系。不用 `kCGImageAlphaOnly`——那是另一类
        // 上下文，既没有背景色可言（于是回到上面那个加重量不对的问题），Core Text 在
        // 其上的行为也要单独验证。多出的三个通道在取完覆盖度后立刻丢掉，只是一次临时分配。
        let mut rgba = vec![255u8; (w as usize) * (h as usize) * 4];
        let ptr = rgba.as_mut_ptr() as *mut c_void;
        let ctx = unsafe {
            CGBitmapContextCreate(
                ptr,
                w as usize,
                h as usize,
                8,
                w as usize * 4,
                Some(&self.color_space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }?;

        // smoothing 的两个开关一概不碰——`draw` 那条路径也不碰，用同一份上下文默认值
        // 才可能得到同一份字形加重。
        CGContext::set_allows_antialiasing(Some(&ctx), true);
        CGContext::set_text_matrix(Some(&ctx), IDENTITY);

        // 坐标系同 `draw`：不翻转上下文，只把「距顶」换算成「距底」。
        let hf = h as f64;
        let padf = pad as f64;
        if single {
            // 文本块顶边在 y=pad，基线再往下 ascent。
            CGContext::set_text_position(Some(&ctx), padf, hf - (padf + ascent));
            unsafe { probe.draw(&ctx) };
        } else {
            let fs = framesetter.expect("折行分支必有 framesetter");
            let path_rect = CGRect {
                origin: CGPoint {
                    x: padf,
                    y: hf - (padf + block_h),
                },
                // 高度多留 1px 避免末行被边界裁掉、宽度与 constraints 同源——两条都与
                // `draw` 的折行分支逐字一致。
                size: CGSize {
                    width: playout_w,
                    height: block_h.ceil() + 1.0,
                },
            };
            let path = unsafe { CGPath::with_rect(path_rect, ptr::null()) };
            let frame = unsafe {
                fs.frame(
                    CFRange {
                        location: 0,
                        length: 0,
                    },
                    &path,
                    None,
                )
            };
            unsafe { frame.draw(&ctx) };
        }
        // 上下文先析构，之后 rgba 里的字节才保证写完。
        drop(ctx);

        // 白底黑字 → 覆盖度是通道的补。三个颜色通道等值（黑字灰度 AA），取红即可。
        let data = rgba.chunks_exact(4).map(|p| 255 - p[0]).collect();
        // 首行基线距块顶：单行分支就是上面手动定位用的 ascent；折行分支交给 CTFrame
        // 排版，显式行高时首行基线在「行高 − descent」处，否则同样是 ascent。
        let baseline = if single {
            ascent
        } else {
            plh.map(|h| h as f64 - descent).unwrap_or(ascent)
        };
        Some(AlphaMask {
            data,
            width: w,
            height: h,
            pad,
            block: (block_w as f32, block_h as f32),
            ascent: baseline as f32,
        })
    }
}

#[cfg(test)]
mod scale_contract_tests {
    use super::*;

    /// 引擎必须**报回**它被设定的 scale。
    ///
    /// `TextEngine::scale` 有个默认实现恒返回 1.0，漏实现不会报错、只会静默说谎。
    /// 代价是：富文本把 scale 计入布局缓存键，而测量路径读引擎、绘制路径读画布，
    /// 两者一旦不一致就逐帧互相顶掉缓存——Retina 上每帧重排整篇文档，且重排会清空
    /// 选区（表现为"划选后高亮立刻消失、Ctrl+C 复制到全文"）。这条正是那个 bug 的判据。
    #[test]
    fn engine_reports_the_scale_it_was_given() {
        let mut eng = CoreTextEngine::new();
        assert_eq!(eng.scale(), 1.0, "初值应为 1.0");
        for s in [2.0f32, 1.5, 1.25, 3.0] {
            eng.set_scale(s);
            assert_eq!(eng.scale(), s, "set_scale({s}) 之后 scale() 必须回同一个值");
        }
        // 下限钳制：0 会让物理字号退化，引擎按 0.1 兜底。
        eng.set_scale(0.0);
        assert!(eng.scale() > 0.0, "scale 不得为 0");
    }
}
