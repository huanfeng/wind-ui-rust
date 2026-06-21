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
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFAttributedString, CFDictionary,
    CFRange, CFRetained, CFString, CGAffineTransform, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColor, CGColorSpace, CGContext, CGImageAlphaInfo, CGPath,
};
use objc2_core_text::{
    kCTFontAttributeName, kCTForegroundColorAttributeName, kCTParagraphStyleAttributeName, CTFont,
    CTFramesetter, CTLine, CTParagraphStyle, CTParagraphStyleSetting, CTParagraphStyleSpecifier,
    CTTextAlignment,
};

use super::TextEngine;
use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

const DEFAULT_FAMILY: &str = "PingFang SC"; // 中文友好的 macOS 系统字体

/// 单位变换矩阵（绘制文字前复位文本矩阵，避免继承翻转）。
const IDENTITY: CGAffineTransform =
    CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 };

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
        Self { scale: 1.0, fonts: HashMap::new(), color_space }
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
    fn attributed(&mut self, text: &str, font: &CTFont, color: &CGColor, align: Align) -> CFRetained<CFAttributedString> {
        let ct_align = match align {
            Align::Start | Align::Stretch => CTTextAlignment::Natural,
            Align::Center => CTTextAlignment::Center,
            Align::End => CTTextAlignment::Right,
        };
        let setting = CTParagraphStyleSetting {
            spec: CTParagraphStyleSpecifier::Alignment,
            valueSize: std::mem::size_of::<CTTextAlignment>(),
            value: NonNull::from(&ct_align).cast(),
        };
        let para = unsafe { CTParagraphStyle::new(&setting, 1) };

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

    fn measure(&mut self, text: &str, family: Option<&str>, size: f32, max_width: Option<f32>) -> Size {
        if text.is_empty() {
            return Size::new(0, size.ceil() as i32);
        }
        let s = self.scale;
        let psize = size * s;
        let font = self.font(family, psize);
        // 颜色与对齐不影响测量，取占位值。
        let black = CGColor::new_srgb(0.0, 0.0, 0.0, 1.0);
        let attr = self.attributed(text, &font, &black, Align::Start);

        match max_width {
            // 折行：用 framesetter 在宽度内排版，取建议尺寸。
            Some(w) if w > 0.0 => {
                let fs = unsafe { CTFramesetter::with_attributed_string(&attr) };
                let constraints = CGSize { width: (w * s) as f64, height: f64::MAX };
                let fit = unsafe {
                    fs.suggest_frame_size_with_constraints(
                        CFRange { location: 0, length: 0 },
                        None,
                        constraints,
                        ptr::null_mut(),
                    )
                };
                Size::new((fit.width / s as f64).ceil() as i32, (fit.height / s as f64).ceil() as i32)
            }
            // 单行不换行：CTLine 排版宽 + 行高（ascent+descent+leading）。
            _ => {
                let line = unsafe { CTLine::with_attributed_string(&attr) };
                let (width, ascent, descent, leading) = line_metrics(&line);
                let line_h = ascent + descent + leading;
                Size::new((width / s as f64).ceil() as i32, (line_h / s as f64).ceil() as i32)
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
        family: Option<&str>,
        size: f32,
        clip: Option<Rect>,
    ) {
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

        let font = self.font(family, psize);
        let cg_color = CGColor::new_srgb(
            color.r as f64 / 255.0,
            color.g as f64 / 255.0,
            color.b as f64 / 255.0,
            color.a as f64 / 255.0,
        );
        let attr = self.attributed(text, &font, &cg_color, align);

        // 单行测量，判定是否需要折行（无换行符且整行宽 ≤ rect 宽 → 单行，支持水平滚动）。
        let probe = unsafe { CTLine::with_attributed_string(&attr) };
        let (line_w, ascent, descent, leading) = line_metrics(&probe);
        let single = !text.contains('\n') && line_w <= prect.w as f64;

        CGContext::save_g_state(Some(&ctx));
        CGContext::set_allows_antialiasing(Some(&ctx), true);
        // 裁剪到可见矩形（滚动视口等）：距顶 → 距底换算。
        if let Some(c) = pclip {
            let cg = CGRect {
                origin: CGPoint { x: c.x as f64, y: phf - (c.y + c.h) as f64 },
                size: CGSize { width: c.w as f64, height: c.h as f64 },
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
            let baseline_from_top = prect.y as f64 + (prect.h as f64 - line_h) / 2.0 + ascent;
            let cg_y = phf - baseline_from_top;
            CGContext::set_text_position(Some(&ctx), text_x0, cg_y);
            unsafe { probe.draw(&ctx) };
        } else {
            // 折行：framesetter 在 rect 宽内排版，段落样式负责水平对齐，整体垂直居中。
            let fs = unsafe { CTFramesetter::with_attributed_string(&attr) };
            let constraints = CGSize { width: prect.w as f64, height: f64::MAX };
            let fit = unsafe {
                fs.suggest_frame_size_with_constraints(
                    CFRange { location: 0, length: 0 },
                    None,
                    constraints,
                    ptr::null_mut(),
                )
            };
            let text_h = fit.height;
            let top_from_top = prect.y as f64 + (prect.h as f64 - text_h) / 2.0;
            let path_rect = CGRect {
                origin: CGPoint { x: prect.x as f64, y: phf - (top_from_top + text_h) },
                // 高度多留 1px，避免末行被边界裁掉。
                size: CGSize { width: prect.w as f64, height: text_h.ceil() + 1.0 },
            };
            let path = unsafe { CGPath::with_rect(path_rect, ptr::null()) };
            let frame =
                unsafe { fs.frame(CFRange { location: 0, length: 0 }, &path, None) };
            unsafe { frame.draw(&ctx) };
        }

        CGContext::restore_g_state(Some(&ctx));
        // ctx（仅包裹 pixmap 缓冲、未持有像素所有权）在此析构，pixmap 内容已就绪。
    }
}
