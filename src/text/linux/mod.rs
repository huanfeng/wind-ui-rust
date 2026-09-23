//! Linux 文字引擎：fontconfig 选字 + ttf-parser 读字形 + ab_glyph_rasterizer 光栅，
//! 自己做排版（逐字前进宽度 + `kern` 表字距 + 简化折行）与合成。
//!
//! # 取舍
//!
//! 走「小依赖」路线：不引 HarfBuzz/FreeType（C 库，需 -dev 包）也不引 cosmic-text
//! （依赖树大、启动要扫全部系统字体）。代价写在明处：
//! - **不做复杂文字整形**：阿拉伯文连写、印度系文字的字形重排、连字都没有；从左到右
//!   逐字排，对中文 / 西文 / 日文 / 韩文 UI 文本足够。
//! - **无 hinting**：灰度抗锯齿、亚像素定位（4 档相位，与 Core Text 同粒度），观感接近
//!   macOS 而非 Windows ClearType。
//! - 字距只读旧式 `kern` 表；只放在 GPOS 里的字距不生效。
//! - 字体缺粗体 / 斜体字面时合成（水平加粗 / 0.2 斜切）。
//!
//! 测量与绘制走同一条排版路径、同一物理字号（`size × scale`），与其它平台的约定一致。

mod fontconfig;
mod layout;
mod store;

use std::collections::HashMap;

use ab_glyph_rasterizer::{point, Point as RPoint, Rasterizer};
use tiny_skia::Pixmap;

use super::{LineMetrics, TextEngine, TextStyle, SUBPIXEL_PHASES};
use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;
use store::{ChainId, FaceId};

/// 字形缓存每一代的条目上限（两代轮换，见 `LinuxTextEngine::glyph`）。
///
/// 一个字形按 4 档相位各存一份，一千来个不同汉字就是四千多条——若到上限就整表清空，
/// 常用字较多的界面会在同一帧里一边清表一边重光栅，每帧循环。两代轮换让最近用过的
/// 字形在换代时被提升回新一代，只有两代都没被碰过的才真正淘汰。
const GLYPH_GEN_MAX: usize = 4096;
/// 测量缓存上限：满则清空（`TextInput` / 富文本按前缀逐个测量，不缓存就是 O(n²) 排版）。
const MEASURE_CACHE_MAX: usize = 2048;
/// 单个字形位图的边长上限（物理像素）。超过这个量级的字不是 UI 文本，直接不画。
const MAX_GLYPH_DIM: i32 = 2048;
/// 斜体合成的斜切系数（x += y × 系数），约 11°，与常见浏览器的合成斜体一致。
const SYNTH_ITALIC_SHEAR: f32 = 0.2;

pub struct LinuxTextEngine {
    scale: f32,
    glyphs: HashMap<GlyphKey, Option<GlyphBmp>>,
    /// 上一代字形缓存：命中即提升回 `glyphs`。
    glyphs_old: HashMap<GlyphKey, Option<GlyphBmp>>,
    measures: HashMap<MeasureKey, Size>,
}

/// 测量缓存键。`scale` 不进键：它变了整表清空（见 `set_scale`）。
#[derive(PartialEq, Eq, Hash)]
struct MeasureKey {
    text: String,
    family: Option<String>,
    size: u32,
    weight: u16,
    italic: bool,
    line_height: Option<u32>,
    max_width: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: FaceId,
    gid: u16,
    /// 物理字号 × 64（26.6 定点），避免拿浮点做键。
    ppem64: u32,
    phase: u8,
    bold: bool,
    italic: bool,
}

/// 一个字形的 A8 覆盖度位图。`left`/`top` 是位图左上角相对「笔位整数列 / 基线行」的偏移。
struct GlyphBmp {
    left: i32,
    top: i32,
    w: usize,
    h: usize,
    data: Vec<u8>,
}

/// 排好的一个字符。
struct Shaped {
    ch: char,
    /// `None` = 不绘制（控制字符、制表符）。
    glyph: Option<(FaceId, u16)>,
    adv: f32,
    /// 是否需要合成（粗体, 斜体）：按字形**所属字体**判——回退字体可能有真粗体而主字体没有。
    synth: (bool, bool),
}

struct Line {
    range: std::ops::Range<usize>,
    /// 可见宽度：不计行尾空白（折行判定与对齐用）。
    width: f32,
    /// 含行尾空白的宽度——**段落末行**才计入（折行处悬挂的空白不算，否则测量会超出
    /// 折行宽度）。测量与单行判定用它：与 DirectWrite `widthIncludingTrailingWhitespace`、
    /// Core Text `CTLine` 宽度同口径；不计的话 `measure("a ") == measure("a")`，输入框里
    /// 敲空格光标不前进、富文本按词碎片排版时词与词粘在一起。
    full: f32,
}

/// 一段文本的排版结果（物理像素）。
struct Layout {
    glyphs: Vec<Shaped>,
    lines: Vec<Line>,
    /// 行盒高度。
    line_h: f32,
    /// 行盒顶到基线的距离。
    baseline: f32,
    /// 字体自然上行 / 下行（`line_metrics` 用）。
    ascent: f32,
    descent: f32,
    ppem: f32,
}

impl LinuxTextEngine {
    pub fn new() -> Self {
        Self {
            scale: 1.0,
            glyphs: HashMap::new(),
            glyphs_old: HashMap::new(),
            measures: HashMap::new(),
        }
    }

    /// 排版。`max_w` 为物理宽度上限（`None` = 只按 `\n` 硬换行）。
    fn layout(&mut self, text: &str, ts: &TextStyle, max_w: Option<f32>) -> Layout {
        let ppem = (ts.size * self.scale).max(1.0);
        store::with(|st| {
            let chain = st.chain(ts.family, ts.weight, ts.italic);
            let (ascent, descent, gap) = match st.primary(chain) {
                Some(id) => {
                    let f = st.face(id);
                    let k = ppem / f.upem;
                    (f.ascent * k, f.descent * k, f.line_gap * k)
                }
                // 系统上一款字体都没有：按常见比例给度量，文字画不出来但布局不塌。
                None => (ppem * 0.8, ppem * 0.2, 0.0),
            };
            let natural = ascent + descent + gap;
            let explicit = ts.line_height_px().map(|h| h * self.scale);
            let line_h = explicit.unwrap_or(natural);
            // 显式行高：字形盒在行盒内居中（与 DirectWrite UNIFORM / Core Text 段落行高一致）；
            // 自然行高：行距（gap）留在行底。
            let baseline = match explicit {
                Some(h) => (h - (ascent + descent)) / 2.0 + ascent,
                None => ascent,
            };

            let mut glyphs = Vec::with_capacity(text.len());
            let mut lines = Vec::new();
            for para in text.split('\n') {
                let para = para.strip_suffix('\r').unwrap_or(para);
                let base = glyphs.len();
                shape_into(st, chain, para, ppem, (ts.weight, ts.italic), &mut glyphs);
                let chars: Vec<char> = glyphs[base..].iter().map(|g| g.ch).collect();
                let adv: Vec<f32> = glyphs[base..].iter().map(|g| g.adv).collect();
                let ranges = match max_w {
                    Some(w) if w > 0.0 => layout::wrap(&chars, &adv, w),
                    _ => std::iter::once(0..chars.len()).collect(),
                };
                let n = ranges.len();
                for (k, r) in ranges.into_iter().enumerate() {
                    let width = layout::line_width(&chars, &adv, r.clone());
                    let full = if k + 1 == n {
                        adv[r.clone()].iter().sum()
                    } else {
                        width
                    };
                    lines.push(Line {
                        range: base + r.start..base + r.end,
                        width,
                        full,
                    });
                }
            }
            Layout {
                glyphs,
                lines,
                line_h,
                baseline,
                ascent,
                descent: descent + gap,
                ppem,
            }
        })
    }

    fn glyph(&mut self, key: GlyphKey) -> Option<&GlyphBmp> {
        if !self.glyphs.contains_key(&key) {
            if self.glyphs.len() >= GLYPH_GEN_MAX {
                // 换代：当前代降为上一代，更老的那一代整体淘汰。
                self.glyphs_old = std::mem::take(&mut self.glyphs);
            }
            let bmp = match self.glyphs_old.remove(&key) {
                Some(b) => b,
                None => store::with(|st| rasterize(st.face(key.face), key)),
            };
            self.glyphs.insert(key, bmp);
        }
        self.glyphs.get(&key).and_then(|b| b.as_ref())
    }
}

impl Default for LinuxTextEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// 逐字符选字、取前进宽度，追加到 `out`。
fn shape_into(
    st: &mut store::Store,
    chain: ChainId,
    text: &str,
    ppem: f32,
    (weight, italic): (u16, bool),
    out: &mut Vec<Shaped>,
) {
    let start = out.len();
    for ch in text.chars() {
        if ch == '\t' {
            // 制表符按 4 个空格宽处理：UI 文本里它几乎只用于对齐，没有制表位概念可言。
            let w = st
                .glyph_for(chain, ' ')
                .map(|(f, g)| advance(st.face(f), g, ppem))
                .unwrap_or(ppem * 0.25);
            out.push(Shaped {
                ch,
                glyph: None,
                adv: w * 4.0,
                synth: (false, false),
            });
            continue;
        }
        if ch.is_control() || is_invisible_format(ch) {
            out.push(Shaped {
                ch,
                glyph: None,
                adv: 0.0,
                synth: (false, false),
            });
            continue;
        }
        let glyph = st.glyph_for(chain, ch);
        let (adv, synth) = match glyph {
            Some((f, g)) => {
                let fd = st.face(f);
                (
                    advance(fd, g, ppem),
                    (needs_synth_bold(weight, fd.weight), italic && !fd.italic),
                )
            }
            None => (0.0, (false, false)),
        };
        out.push(Shaped {
            ch,
            glyph,
            adv,
            synth,
        });
    }
    // 字距：同一字体里相邻两字查 `kern` 表，调整量并进前一个字的前进宽度。
    for i in start + 1..out.len() {
        let (Some((fa, ga)), Some((fb, gb))) = (out[i - 1].glyph, out[i].glyph) else {
            continue;
        };
        if fa != fb {
            continue;
        }
        let f = st.face(fa);
        if let Some(k) = kerning(f, ga, gb) {
            out[i - 1].adv += k as f32 * ppem / f.upem;
        }
    }
}

/// 零宽格式字符（ZWJ、变体选择符等）：不画、不占宽。字体通常没有它们的字形，
/// 不拦下来就会各自画出一个 `.notdef` 方框。
fn is_invisible_format(c: char) -> bool {
    matches!(c as u32, 0x200B..=0x200F | 0x2060..=0x2064 | 0xFE00..=0xFE0F | 0xFEFF)
}

fn advance(f: &store::FaceData, gid: u16, ppem: f32) -> f32 {
    f.face
        .glyph_hor_advance(ttf_parser::GlyphId(gid))
        .map(|a| a as f32 * ppem / f.upem)
        .unwrap_or(0.0)
}

fn kerning(f: &store::FaceData, a: u16, b: u16) -> Option<i16> {
    let kern = f.face.tables().kern?;
    kern.subtables
        .into_iter()
        .filter(|s| s.horizontal && !s.variable)
        .find_map(|s| s.glyphs_kerning(ttf_parser::GlyphId(a), ttf_parser::GlyphId(b)))
}

/// 合成粗体：请求 ≥600 而字体本身 ≤500（没有真粗体字面）。
fn needs_synth_bold(requested: u16, face_weight: u16) -> bool {
    requested >= 600 && face_weight <= 500
}

/// 光栅一个字形（未命中缓存时调用）。
fn rasterize(f: &store::FaceData, key: GlyphKey) -> Option<GlyphBmp> {
    let gid = ttf_parser::GlyphId(key.gid);
    let bb = f.face.glyph_bounding_box(gid)?;
    let ppem = key.ppem64 as f32 / 64.0;
    let k = ppem / f.upem;
    let shear = if key.italic { SYNTH_ITALIC_SHEAR } else { 0.0 };
    let dx = key.phase as f32 / SUBPIXEL_PHASES as f32;
    // 合成粗体的加宽量：字号的 1/24，限制在 [0.5, 2] 像素。
    let embolden = if key.bold {
        (ppem / 24.0).clamp(0.5, 2.0)
    } else {
        0.0
    };

    let tx = |x: f32, y: f32| (x * k + shear * y * k + dx, -y * k);
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for (x, y) in [
        (bb.x_min, bb.y_min),
        (bb.x_min, bb.y_max),
        (bb.x_max, bb.y_min),
        (bb.x_max, bb.y_max),
    ] {
        let (px, py) = tx(x as f32, y as f32);
        x0 = x0.min(px);
        y0 = y0.min(py);
        x1 = x1.max(px);
        y1 = y1.max(py);
    }
    let left = x0.floor() as i32 - 1;
    let top = y0.floor() as i32 - 1;
    let w = x1.ceil() as i32 + 1 + embolden.ceil() as i32 - left;
    let h = y1.ceil() as i32 + 1 - top;
    if w <= 0 || h <= 0 || w > MAX_GLYPH_DIM || h > MAX_GLYPH_DIM {
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    let mut b = OutlineToRaster {
        r: Rasterizer::new(w, h),
        k,
        shear,
        dx,
        ox: left as f32,
        oy: top as f32,
        start: point(0.0, 0.0),
        cur: point(0.0, 0.0),
    };
    f.face.outline_glyph(gid, &mut b)?;
    let mut data = vec![0u8; w * h];
    b.r.for_each_pixel_2d(|x, y, a| {
        data[y as usize * w + x as usize] = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    });
    if embolden > 0.0 {
        embolden_rows(&mut data, w, embolden);
    }
    Some(GlyphBmp {
        left,
        top,
        w,
        h,
        data,
    })
}

/// 水平加粗：每一行把覆盖度向右「涂抹」`amount` 像素（取最大值，不累加以免发糊）。
fn embolden_rows(data: &mut [u8], w: usize, amount: f32) {
    let whole = amount.floor() as usize;
    let frac = amount - whole as f32;
    let mut row = vec![0u8; w];
    for line in data.chunks_mut(w) {
        row.copy_from_slice(line);
        for x in 0..w {
            let mut v = row[x];
            for j in 1..=whole {
                if x >= j {
                    v = v.max(row[x - j]);
                }
            }
            if frac > 0.0 && x > whole {
                v = v.max((row[x - whole - 1] as f32 * frac) as u8);
            }
            line[x] = v;
        }
    }
}

/// ttf-parser 轮廓回调 → 光栅器。字体坐标 y 向上，位图 y 向下。
struct OutlineToRaster {
    r: Rasterizer,
    k: f32,
    shear: f32,
    dx: f32,
    ox: f32,
    oy: f32,
    start: RPoint,
    cur: RPoint,
}

impl OutlineToRaster {
    fn p(&self, x: f32, y: f32) -> RPoint {
        point(
            x * self.k + self.shear * y * self.k + self.dx - self.ox,
            -y * self.k - self.oy,
        )
    }
}

impl ttf_parser::OutlineBuilder for OutlineToRaster {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.p(x, y);
        self.start = p;
        self.cur = p;
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.p(x, y);
        self.r.draw_line(self.cur, p);
        self.cur = p;
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let c = self.p(x1, y1);
        let p = self.p(x, y);
        self.r.draw_quad(self.cur, c, p);
        self.cur = p;
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let c1 = self.p(x1, y1);
        let c2 = self.p(x2, y2);
        let p = self.p(x, y);
        self.r.draw_cubic(self.cur, c1, c2, p);
        self.cur = p;
    }
    fn close(&mut self) {
        if self.cur != self.start {
            self.r.draw_line(self.cur, self.start);
        }
        self.cur = self.start;
    }
}

/// 把覆盖度位图按颜色合成进 RGBA 预乘缓冲，只写 `clip` 内的像素。
fn blit(pm: &mut Pixmap, g: &GlyphBmp, x0: i32, y0: i32, color: Color, clip: Rect) {
    let pw = pm.width() as i32;
    let gx0 = x0.max(clip.x);
    let gy0 = y0.max(clip.y);
    let gx1 = (x0 + g.w as i32).min(clip.right());
    let gy1 = (y0 + g.h as i32).min(clip.bottom());
    if gx0 >= gx1 || gy0 >= gy1 {
        return;
    }
    let (r, gg, b, a) = (
        color.r as u32,
        color.g as u32,
        color.b as u32,
        color.a as u32,
    );
    let data = pm.data_mut();
    for y in gy0..gy1 {
        let src_row = (y - y0) as usize * g.w;
        let dst_row = (y * pw) as usize * 4;
        for x in gx0..gx1 {
            let cov = g.data[src_row + (x - x0) as usize] as u32;
            if cov == 0 {
                continue;
            }
            let sa = cov * a / 255;
            if sa == 0 {
                continue;
            }
            let inv = 255 - sa;
            let i = dst_row + x as usize * 4;
            let d = &mut data[i..i + 4];
            d[0] = ((r * sa + d[0] as u32 * inv + 127) / 255) as u8;
            d[1] = ((gg * sa + d[1] as u32 * inv + 127) / 255) as u8;
            d[2] = ((b * sa + d[2] as u32 * inv + 127) / 255) as u8;
            d[3] = ((sa * 255 + d[3] as u32 * inv + 127) / 255) as u8;
        }
    }
}

impl TextEngine for LinuxTextEngine {
    fn set_scale(&mut self, scale: f32) {
        let s = scale.max(0.1);
        if (s - self.scale).abs() > f32::EPSILON {
            self.scale = s;
            self.measures.clear();
        }
    }

    fn scale(&self) -> f32 {
        self.scale
    }

    fn measure(&mut self, text: &str, ts: &TextStyle, max_width: Option<f32>) -> Size {
        let key = MeasureKey {
            text: text.to_string(),
            family: ts.family.map(str::to_string),
            size: ts.size.to_bits(),
            weight: ts.weight,
            italic: ts.italic,
            line_height: ts.line_height.map(f32::to_bits),
            max_width: max_width.map(f32::to_bits),
        };
        if let Some(&sz) = self.measures.get(&key) {
            return sz;
        }
        let s = self.scale;
        let lay = self.layout(text, ts, max_width.map(|w| w * s));
        let sz = if text.is_empty() {
            Size::new(0, (lay.line_h / s).ceil() as i32)
        } else {
            let w = lay.lines.iter().map(|l| l.full).fold(0.0f32, f32::max);
            let h = lay.lines.len() as f32 * lay.line_h;
            Size::new((w / s).ceil() as i32, (h / s).ceil() as i32)
        };
        if self.measures.len() >= MEASURE_CACHE_MAX {
            self.measures.clear();
        }
        self.measures.insert(key, sz);
        sz
    }

    fn line_metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
        let s = self.scale;
        let lay = self.layout(text, ts, None);
        let asc = match ts.line_height_px() {
            Some(_) => lay.baseline,
            None => lay.ascent,
        };
        let desc = match ts.line_height_px() {
            Some(_) => lay.line_h - lay.baseline,
            None => lay.descent,
        };
        LineMetrics {
            ascent: asc / s,
            descent: desc / s,
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
        if text.is_empty() || rect.is_empty() || color.a == 0 {
            return;
        }
        let s = self.scale;
        let prect = rect.scaled(s);
        // 排版宽用外扩取整，与 measure 的 `max_width × scale` 同源（见 Rect::scaled_out）。
        let playout_w = rect.scaled_out(s).w as f32;
        let mut bounds = Rect::new(0, 0, pixmap.width() as i32, pixmap.height() as i32);
        if let Some(c) = clip {
            bounds = bounds.intersect(&c.scaled(s));
        }
        if bounds.is_empty() {
            return;
        }

        // 单行判据与 Core Text 一致：无显式换行且整行放得下 → 单行（支持 x 为负的水平滚动）；
        // 否则在排版宽内折行。
        let mut lay = self.layout(text, ts, None);
        // 判据用含尾随空白的宽度，与 measure 同口径：测量时装得下的，这里必须也判单行。
        let single = !text.contains('\n') && lay.lines.first().is_some_and(|l| l.full <= playout_w);
        if !single {
            lay = self.layout(text, ts, Some(playout_w));
        }
        let text_h = lay.lines.len() as f32 * lay.line_h;
        // 纵向契约：装得下居中，装不下顶对齐（见 TextEngine::draw）。
        let top = prect.y as f32 + (prect.h as f32 - text_h).max(0.0) / 2.0;
        let box_w = if single { prect.w as f32 } else { playout_w };

        for (li, line) in lay.lines.iter().enumerate() {
            let line_top = top + li as f32 * lay.line_h;
            // 整行都在可见区之外的直接跳过（长文本滚动时省掉大半字形合成）。
            if line_top > bounds.bottom() as f32 || line_top + lay.line_h < bounds.y as f32 {
                continue;
            }
            let baseline = (line_top + lay.baseline).round() as i32;
            // 单行按含尾随空白的宽度对齐（与 Core Text 的 CTLine 一致，也与 measure 给宿主
            // 的宽度一致）；折行各行按可见宽度对齐（段落样式的惯例）。
            let lw = if single { line.full } else { line.width };
            let mut pen = prect.x as f32
                + match align {
                    Align::Start | Align::Stretch => 0.0,
                    Align::Center => (box_w - lw) / 2.0,
                    Align::End => box_w - lw,
                };
            for gi in line.range.clone() {
                let (glyph, adv) = (lay.glyphs[gi].glyph, lay.glyphs[gi].adv);
                if let Some((face, gid)) = glyph {
                    let fx = pen.floor();
                    let phase =
                        (((pen - fx) * SUBPIXEL_PHASES as f32) as u8).min(SUBPIXEL_PHASES - 1);
                    let (bold, italic) = lay.glyphs[gi].synth;
                    let key = GlyphKey {
                        face,
                        gid,
                        ppem64: (lay.ppem * 64.0).round() as u32,
                        phase,
                        bold,
                        italic,
                    };
                    if let Some(g) = self.glyph(key) {
                        blit(
                            pixmap,
                            g,
                            fx as i32 + g.left,
                            baseline + g.top,
                            color,
                            bounds,
                        );
                    }
                }
                pen += adv;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eng() -> LinuxTextEngine {
        let mut e = LinuxTextEngine::new();
        e.set_scale(1.0);
        e
    }

    #[test]
    fn wider_text_measures_wider() {
        let mut e = eng();
        let ts = TextStyle::new(14.0);
        let a = e.measure("abc", &ts, None);
        let b = e.measure("abcdef", &ts, None);
        assert!(a.h > 0);
        assert!(b.w > a.w, "{a:?} vs {b:?}");
    }

    #[test]
    fn wrapping_increases_height() {
        let mut e = eng();
        let ts = TextStyle::new(14.0);
        let one = e.measure("一二三四五六七八九十", &ts, None);
        let wrapped = e.measure("一二三四五六七八九十", &ts, Some(one.w as f32 / 2.0));
        // 行高非整数：单行向上取整后 ×2 可能比两行合计多 1。
        assert!(wrapped.h >= one.h * 2 - 1, "{one:?} → {wrapped:?}");
        assert!(wrapped.w <= one.w / 2 + 1);
    }

    #[test]
    fn measure_scales_with_dpi_in_logical_units() {
        let mut e = eng();
        let ts = TextStyle::new(14.0);
        let a = e.measure("Hello 世界", &ts, None);
        e.set_scale(2.0);
        let b = e.measure("Hello 世界", &ts, None);
        assert!(
            (a.w - b.w).abs() <= 2,
            "逻辑宽不应随 DPI 成倍变化：{a:?} vs {b:?}"
        );
    }

    /// 尾随空白计入测量（DirectWrite `widthIncludingTrailingWhitespace` 同口径）：
    /// 输入框按前缀测量定光标，空格不计宽的话敲空格光标不动。
    #[test]
    fn trailing_whitespace_counts_in_measure() {
        let mut e = eng();
        let ts = TextStyle::new(14.0);
        assert!(e.measure("a ", &ts, None).w > e.measure("a", &ts, None).w);
        assert!(e.measure("a  ", &ts, None).w > e.measure("a ", &ts, None).w);
        assert!(e.measure("   ", &ts, None).w > 0);
    }

    /// 折行处悬挂的空白不计入：测量结果不能超过折行宽度。
    #[test]
    fn hanging_space_at_wrap_does_not_widen_measure() {
        let mut e = eng();
        let ts = TextStyle::new(14.0);
        let one = e.measure("aaaa", &ts, None).w as f32;
        let sz = e.measure("aaaa bbbb", &ts, Some(one + 2.0));
        assert!(
            sz.w as f32 <= one + 2.0 + 1.0,
            "{sz:?} vs max {}",
            one + 2.0
        );
    }

    #[test]
    fn draw_puts_ink_inside_rect() {
        let mut e = eng();
        let mut pm = Pixmap::new(100, 40).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        e.draw(
            &mut pm,
            "Ag中",
            Rect::new(0, 0, 100, 40),
            Color::rgb(0, 0, 0),
            Align::Start,
            &TextStyle::new(16.0),
            None,
        );
        let dark = pm.data().chunks(4).filter(|p| p[0] < 128).count();
        assert!(dark > 20, "应画出可见的字：{dark}");
    }

    #[test]
    fn clip_excludes_ink_outside() {
        let mut e = eng();
        let mut pm = Pixmap::new(100, 40).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        e.draw(
            &mut pm,
            "WWWWWWWW",
            Rect::new(0, 0, 100, 40),
            Color::rgb(0, 0, 0),
            Align::Start,
            &TextStyle::new(16.0),
            Some(Rect::new(0, 0, 20, 40)),
        );
        for y in 0..40 {
            for x in 20..100 {
                let i = (y * 100 + x) * 4;
                assert_eq!(pm.data()[i], 255, "裁剪区外 ({x},{y}) 不应有墨");
            }
        }
    }

    #[test]
    fn embolden_spreads_coverage_rightwards() {
        let mut d = vec![0, 255, 0, 0];
        embolden_rows(&mut d, 4, 1.0);
        assert_eq!(d, vec![0, 255, 255, 0]);
    }

    #[test]
    fn synth_bold_only_when_face_lacks_weight() {
        assert!(needs_synth_bold(700, 400));
        assert!(!needs_synth_bold(700, 700));
        assert!(!needs_synth_bold(400, 400));
    }
}
