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
    CFDictionary, CFIndex, CFRange, CFRetained, CFString, CGAffineTransform, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColor, CGColorSpace, CGContext, CGImageAlphaInfo, CGPath,
};
use objc2_core_text::{
    kCTFontAttributeName, kCTForegroundColorAttributeName, kCTParagraphStyleAttributeName, CTFont,
    CTFontOrientation, CTFramesetter, CTLine, CTParagraphStyle, CTParagraphStyleSetting,
    CTParagraphStyleSpecifier, CTRun, CTTextAlignment,
};

use super::{
    AlphaMask, GlyphBitmap, GlyphKey, GlyphSource, PlacedGlyph, RunRequest, ShapedRun, TextEngine,
    TextStyle, SUBPIXEL_PHASES,
};
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
    /// `shape_run` 见过的**物理**字体，按 (PostScript 名, 物理字号 bits) 索引。
    ///
    /// `raster_glyph` 必须用这一张表里的对象，**不能**拿名字重新 `CTFontCreateWithName`：
    /// 系统字体的 PostScript 名以点开头（`.SFNS-Regular` 之类），拿它去创建会得到另一个
    /// 字体，而字形索引是**字体内部**的编号——索引没变、字体变了，画出来就是别的字。
    /// 症状极隐蔽：字形的位置、步进宽度全对，只有字形长得不对（实测 `l` 画成了 15px
    /// 宽的方块，逗号画成了 21px 高的字形，而墨迹外接框只差几个像素）。
    ///
    /// 字号进键是因为 `CTFont` 自带字号：同名不同号是两个对象，字形边界也不同。
    ///
    /// **不淘汰**，靠"键的取值集合本来就有限"兜底：字号来自主题里那几档 × 有限的几个
    /// DPI 缩放，字体名来自系统装了的那些。真正会让它无界的是**字号补间动画**（每帧一个
    /// 新字号）——那种界面同样会把 glyph atlas 撑爆，届时两处要一起加淘汰，不能只淘汰
    /// 这一张：`raster_glyph` 找不到条目会退回按名字创建，而那条退路画出来是**别的字**
    /// （见本字段上方那段）。故宁可让它涨，也不能让它半淘汰。
    run_fonts: HashMap<(String, u32), CFRetained<CTFont>>,
    /// 复用的 DeviceRGB 色彩空间。
    color_space: CFRetained<CGColorSpace>,
}

impl CoreTextEngine {
    pub fn new() -> Self {
        let color_space = CGColorSpace::new_device_rgb().expect("CGColorSpaceCreateDeviceRGB 失败");
        Self {
            scale: 1.0,
            fonts: HashMap::new(),
            run_fonts: HashMap::new(),
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
        let data = rgba.as_chunks::<4>().0.iter().map(|p| 255 - p[0]).collect();
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

    /// 排版成字形序列。**排版仍然整段做**——kerning、连字、字体回退都依赖上下文，
    /// 这里交出来的是 `CTLine` 排版的结果，不是绕开它。
    ///
    /// 折行的段落交回 [`Self::raster_run`]：`CTFrame` 的行原点与段落对齐是另一套定位，
    /// 而折行的是段落文本，本来就不属于「每帧都在变」的那一类，先不摊这份复杂度。
    fn shape_run(&mut self, req: &RunRequest) -> Option<ShapedRun> {
        let ts = &req.style;
        if req.text.is_empty() || ts.size <= 0.0 {
            return None;
        }
        let s = req.scale.max(0.1);
        let psize = ts.size * s;
        let font = self.font(ts.family, psize);
        let black = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        let plh = ts.line_height_px().map(|h| h * s);
        let attr = self.attributed(req.text, &font, &black, req.align, plh);
        let playout_w = (req.max_width.max(0.0) * s).ceil() as f64;
        let line = unsafe { CTLine::with_attributed_string(&attr) };
        let (line_w, ascent, descent, leading) = line_metrics(&line);
        if !is_single_line(req.text, line_w, playout_w) {
            return None;
        }

        let runs = unsafe { line.glyph_runs() };
        let count = runs.count() as usize;
        let mut glyphs = Vec::new();
        for i in 0..count {
            let ptr = unsafe { runs.value_at_index(i as CFIndex) };
            if ptr.is_null() {
                continue;
            }
            // CTLine 的 glyph runs 数组元素恒为 CTRun（Core Text 的契约）。
            let run: &CTRun = unsafe { &*ptr.cast() };
            let n = unsafe { run.glyph_count() } as usize;
            if n == 0 {
                continue;
            }
            // 每个 run 的字体可能不同：字体回退对每个缺字的字符都会换一个物理字体，
            // 拿整段那个 `font` 当身份会把回退字形认成主字体的同号字形——那是另一个字。
            let Some((name, rf)) = run_font(run) else {
                continue;
            };
            // 登记这个物理字体，`raster_glyph` 稍后要用**它本人**去光栅。
            self.run_fonts
                .entry((name.clone(), psize.to_bits()))
                .or_insert(rf);
            let font_id: std::sync::Arc<str> = std::sync::Arc::from(name.as_str());
            let mut gs = vec![0u16; n];
            let mut ps = vec![CGPoint { x: 0.0, y: 0.0 }; n];
            let range = CFRange {
                location: 0,
                length: n as CFIndex,
            };
            unsafe {
                run.glyphs(range, NonNull::new(gs.as_mut_ptr())?);
                run.positions(range, NonNull::new(ps.as_mut_ptr())?);
            }
            for k in 0..n {
                let (x, phase) = split_phase(ps[k].x);
                glyphs.push(PlacedGlyph {
                    key: GlyphKey {
                        font: font_id.clone(),
                        size: psize.to_bits(),
                        glyph: gs[k],
                        phase,
                    },
                    x,
                    // CG 的 y 向上，屏幕行向下。
                    dy: (-ps[k].y).round() as i32,
                });
            }
        }
        if glyphs.is_empty() {
            return None;
        }
        Some(ShapedRun {
            glyphs,
            block: (line_w as f32, (ascent + descent + leading) as f32),
            ascent: ascent as f32,
        })
    }

    /// 光栅单个字形。上下文与明暗关系**逐条照抄 [`Self::raster_run`]**：白底黑字、
    /// 不碰 smoothing 开关、取 `255 − 红通道`。差一条都会让同一个字在两条路径下有
    /// 不同的字重（透明底会让 CG 的字形加重量取错档，实测墨量差 13~17%）。
    fn raster_glyph(&mut self, key: &GlyphKey) -> Option<GlyphBitmap> {
        let psize = f32::from_bits(key.size);
        if psize <= 0.0 || !psize.is_finite() {
            return None;
        }
        // 先查 `shape_run` 登记过的物理字体；查不到才退回按名字创建（那条路只在
        // 「atlas 里留着上一个引擎实例的键」这类边角情形下走到，见 `run_fonts`）。
        let font = match self.run_fonts.get(&(key.font.to_string(), key.size)) {
            Some(f) => f.clone(),
            None => self.font(Some(&key.font), psize),
        };
        let glyph = key.glyph;
        let mut rect = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 0.0,
                height: 0.0,
            },
        };
        unsafe {
            font.bounding_rects_for_glyphs(
                CTFontOrientation::Default,
                NonNull::from(&glyph),
                &mut rect,
                1,
            )
        };
        // 亚像素相位：位图按「原点右移 frac」光栅，贴到整数列上仍保住原本的字间距。
        let frac = key.phase as f64 / SUBPIXEL_PHASES as f64;
        // 边界各留 1px：字形边界盒是排版意义上的，AA 的边缘可以越出它半个像素。
        const PAD: i32 = 1;
        let left = (rect.origin.x + frac).floor() as i32 - PAD;
        let right = (rect.origin.x + rect.size.width + frac).ceil() as i32 + PAD;
        // CG 的 y 向上：`up` 后缀的量都是「基线以上为正」。
        let top_up = (rect.origin.y + rect.size.height).ceil() as i32 + PAD;
        let bot_up = rect.origin.y.floor() as i32 - PAD;
        let w = (right - left).max(1) as u32;
        let h = (top_up - bot_up).max(1) as u32;
        if w > MAX_MASK_DIM || h > MAX_MASK_DIM {
            return None;
        }

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
        CGContext::set_allows_antialiasing(Some(&ctx), true);
        CGContext::set_text_matrix(Some(&ctx), IDENTITY);
        // 字形色显式设黑：这条路径没有属性串带来的前景色，靠上下文默认值就等于把
        // 「黑字白底」这个前提交给了实现细节。
        CGContext::set_rgb_fill_color(Some(&ctx), 0.0, 0.0, 0.0, 1.0);
        // `CTFontDrawGlyphs` 的位置是用户空间的**绝对**坐标（不叠加 text position）。
        let pos = CGPoint {
            x: -left as f64 + frac,
            y: -bot_up as f64,
        };
        unsafe { font.draw_glyphs(NonNull::from(&glyph), NonNull::from(&pos), 1, &ctx) };
        drop(ctx);

        let data = rgba.as_chunks::<4>().0.iter().map(|p| 255 - p[0]).collect();
        Some(GlyphBitmap {
            data,
            width: w,
            height: h,
            left,
            top: -top_up,
        })
    }
}

/// run 的**物理**字体：PostScript 名（当身份）+ 字体对象本身（当光栅器）。
///
/// 两样都要：名字用来做缓存键（指针值会随对象释放被复用，拿它当键会误命中到另一个
/// 字体），对象用来光栅（名字重建不出同一个字体，见 `CoreTextEngine::run_fonts`）。
///
/// 取不到就跳过这一段——宁可少画一段，也不要拿整段那个字体顶上：字体回退产生的字形
/// 索引在主字体里指向的是另一个字。
fn run_font(run: &CTRun) -> Option<(String, CFRetained<CTFont>)> {
    let attrs = unsafe { run.attributes() };
    let v = unsafe { attrs.value((kCTFontAttributeName as *const CFString).cast::<c_void>()) };
    let p = NonNull::new(v as *mut CTFont)?;
    // 字典只借出引用，而这份字体要跨出 `attrs` 的生命周期活到光栅那一步。
    let font = unsafe { CFRetained::retain(p) };
    let name = unsafe { font.post_script_name() }.to_string();
    Some((name, font))
}

/// 把浮点列拆成「整数列 + 亚像素相位」，相位在 `0..SUBPIXEL_PHASES`。
///
/// `round` 与 `floor` 都试过，用重组判据的 10 个用例量了两遍（含 scale 1.0/2.0、
/// kerning 对、窄字形、标点、中文回退）：
///
/// | | 最坏墨量差 | 逐字节相等的用例 |
/// | --- | --- | --- |
/// | `floor` | 0.66% | 7/10 |
/// | `round` | **0.09%** | 1/10 |
///
/// `floor` 在多数用例上与 CG 完全吻合，但最坏情况差了七倍。取 `round` 是按**最坏值**
/// 选的——观感问题是局部的，一段文字里有一个字画歪就够显眼，而"平均很准"救不了它。
///
/// 进位由 `q / PHASES` 的 floor 吸收，故相位恒在范围内。
fn split_phase(x: f64) -> (i32, u8) {
    let p = SUBPIXEL_PHASES as f64;
    let q = (x * p).round();
    let ix = (q / p).floor();
    (ix as i32, (q - ix * p) as u8)
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

#[cfg(test)]
mod atlas_shape_tests {
    use super::*;

    fn req<'a>(text: &'a str, size: f32, scale: f32) -> RunRequest<'a> {
        RunRequest {
            text,
            style: TextStyle::new(size),
            align: Align::Start,
            max_width: 10_000.0,
            scale,
        }
    }

    /// 把 `shape_run` + `raster_glyph` 的结果重组成一张与 `raster_run` 同尺寸的 mask。
    ///
    /// 定位逐条照抄 `render/gpu/canvas.rs::draw_text`：基线吸附一次
    /// （`floor(块顶 + ascent)`，这里块顶就是 `pad`），字形贴在整数列上，亚像素的
    /// 那一份已经烘在位图里。
    fn compose(e: &mut CoreTextEngine, r: &RunRequest, m: &AlphaMask) -> Vec<u8> {
        let shaped = e.shape_run(r).expect("shape_run 应支持单行");
        let (w, h) = (m.width, m.height);
        let mut out = vec![0u8; (w * h) as usize];
        // 基线吸附是 **ceil(块顶 + ascent)**，不是 floor：CG 的 `set_text_position`
        // 收的是「距底」，它把距底 floor 到整数设备行，而距顶 = 位图高 − 距底，于是
        // 距顶那一侧恰好是 ceil。用 floor 的话整段字会稳定高一行（本判据实测：逐行
        // 墨量逐值相同，只整体错开一行）。
        let base = (m.pad as f32 + m.ascent).ceil() as i32;
        for g in &shaped.glyphs {
            let bmp = e
                .raster_glyph(&g.key)
                .expect("shape_run 交出过的键必须能光栅");
            let x0 = m.pad as i32 + g.x + bmp.left;
            let y0 = base + g.dy + bmp.top;
            for yy in 0..bmp.height as i32 {
                let y = y0 + yy;
                if y < 0 || y >= h as i32 {
                    continue;
                }
                for xx in 0..bmp.width as i32 {
                    let x = x0 + xx;
                    if x < 0 || x >= w as i32 {
                        continue;
                    }
                    let src = bmp.data[(yy as u32 * bmp.width + xx as u32) as usize] as u32;
                    let d = &mut out[(y as u32 * w + x as u32) as usize];
                    // 覆盖度并集（over）：相邻字形的 AA 边缘会重叠一两列。
                    *d = (*d as u32 + src * (255 - *d as u32) / 255).min(255) as u8;
                }
            }
        }
        out
    }

    fn ink(d: &[u8]) -> f64 {
        d.iter().map(|&v| v as f64).sum()
    }

    fn bounds(d: &[u8], w: u32, h: u32) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                if d[(y * w + x) as usize] > 8 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        (x0 != u32::MAX).then_some((x0, y0, x1, y1))
    }

    /// 拆成单字形再拼回去，必须复现整段光栅的那张图。
    ///
    /// 这是 glyph atlas 能不能做的**前提判据**：atlas 的全部风险不在性能而在观感——
    /// 「文字与系统一致」是这个项目的卖点，而拆开重拼最容易丢的就是水平亚像素相位
    /// （字间距会肉眼可见地变化）与字形加重量（上下文换了，CG 的取档也会变）。
    ///
    /// 判据取墨量与墨迹范围，不逐像素比：两条路径的相位量化差半档以内是设计允许的，
    /// 逐像素比会永远红；而墨量能抓住「整体偏胖/偏瘦」，范围能抓住「整体位移」。
    #[test]
    fn shaped_glyphs_recompose_into_the_whole_run_raster() {
        let mut e = CoreTextEngine::new();
        let mut worst = 0.0f64;
        let cases = [
            ("Hello, world", 13.0f32, 2.0f32),
            ("Hello, world", 13.0, 1.0),
            ("控件 042", 12.0, 2.0),
            ("控件 042", 12.0, 1.0),
            ("Wave AVA To. jgpq", 16.0, 1.0),
            ("Wave AVA To. jgpq", 16.0, 2.0),
            ("中英混排 Mixed 123", 14.0, 2.0),
            ("iiillljjj fi fl", 11.0, 1.0),
            ("WWWMMM@@@", 20.0, 1.0),
            ("1234567890", 11.0, 2.0),
        ];
        for (text, size, scale) in cases {
            let r = req(text, size, scale);
            let m = e.raster_run(&r).expect("raster_run");
            let b = compose(&mut e, &r, &m);
            let (ia, ib) = (ink(&m.data), ink(&b));
            let rel = (ia - ib).abs() / ia.max(1.0);
            let (ba, bb) = (
                bounds(&m.data, m.width, m.height),
                bounds(&b, m.width, m.height),
            );
            println!(
                "[{text}] 墨量 整段={ia:.0} 重组={ib:.0} 相对差={:.2}%  范围 {ba:?} vs {bb:?}",
                rel * 100.0
            );
            worst = worst.max(rel);
            let (ba, bb) = (ba.expect("整段有墨"), bb.expect("重组有墨"));
            for (i, (a, b)) in [(ba.0, bb.0), (ba.1, bb.1), (ba.2, bb.2), (ba.3, bb.3)]
                .iter()
                .enumerate()
            {
                assert!(
                    (*a as i64 - *b as i64).abs() <= 1,
                    "[{text}] 墨迹范围第 {i} 项差超 1 像素：{ba:?} vs {bb:?}"
                );
            }
        }
        // 阈值贴着实测定（最坏 0.09%，留一倍余量）：松到几个百分点就等于放过
        // 「字重整体变了」这一类，而那正是拆开重拼最容易出的事。
        assert!(
            worst < 0.002,
            "最差墨量相对差 {:.3}% 超阈——字重或相位对不上",
            worst * 100.0
        );
    }
}
