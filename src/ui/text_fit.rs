//! 控件文字不画出自身 bounds 的共用工具。
//!
//! `Canvas::draw_text` 按 rect 宽自动折行，装不下时顶对齐向下溢出、不自带裁剪。控件高度
//! 若是钉死的（用户 `.height(n)`，或控件自己按单行写死行高），长文本折出的行就会画到
//! 兄弟节点上：整窗帧里被后画的兄弟盖住，局部重绘只重画脏节点时又露出来——局部重绘的
//! 前提就是节点视觉不出 bounds（见 `DamageReq`）。
//!
//! 两种收口，按控件的设计意图选：
//! - [`WrapClip`]：本就允许折行的文字（Label、Link、按钮、复选/单选标签）。照常折行，
//!   只在折出 bounds 时裁回，下沿按整行对齐；单行永不裁。
//! - [`SingleLine`]：语义上是单行的文字（下拉触发器、列表行、导航行、分段控件……）。
//!   放不下就截成 `text…`，不折行。

use std::cell::{Cell, RefCell};
use std::hash::{Hash, Hasher};

use crate::geometry::{Color, Rect};
use crate::render::Canvas;
use crate::spec::Align;
use crate::text::TextStyle;
use crate::ui::Truncate;

/// 文字引擎会据以断行的字符：LF、CR、VT、FF、NEL 与 Unicode 行 / 段分隔符。
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// 换行符压成空格（CRLF 算一个）；不含换行符返回 `None`。
fn flatten_line_breaks(s: &str) -> Option<String> {
    s.contains(is_line_break).then(|| {
        s.replace("\r\n", " ")
            .chars()
            .map(|c| if is_line_break(c) { ' ' } else { c })
            .collect()
    })
}

/// 排版键：文案、排版宽度、文字属性与 DPI 缩放的散列。
///
/// 64 位散列、单条缓存：碰撞的后果只是一帧该裁没裁（或该截没截），可以接受。
fn layout_key(s: &str, w: i32, ts: &TextStyle, scale: f32) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    w.hash(&mut h);
    ts.family.hash(&mut h);
    ts.size.to_bits().hash(&mut h);
    ts.weight.hash(&mut h);
    ts.italic.hash(&mut h);
    ts.line_height.map(f32::to_bits).hash(&mut h);
    scale.to_bits().hash(&mut h);
    h.finish()
}

/// 允许折行的文字：折出 bounds 时给出裁剪矩形。缓存放在各自控件上（每个控件一份）。
#[derive(Default)]
pub(crate) struct WrapClip {
    /// `(排版键, 多行时的折行排版高度（单行为 0）, 不折行的单行高)`。
    ///
    /// paint 每帧都会来问，而文本测量是一次完整排版，不缓存就是每帧重排。
    cache: Cell<Option<(u64, i32, i32)>>,
}

impl WrapClip {
    /// `text` 是交给 `draw_text` 的矩形（折行宽 = `text.w`，溢出时从 `text.y` 起顶对齐）。
    /// 排版画出 `bounds` 下沿时返回应裁到的矩形，否则 `None`。
    ///
    /// 只认**多行**溢出：单行文本在紧凑高度（如 `.height(14)` 配 13 号字）下行盒本就
    /// 略高于分配高度，那是下伸部与字形外沿，裁掉反而切字。判多行用"折行后比不折行
    /// 高出半行以上"或含硬换行，而不是拿 `"Ay"` 的行高去比——CJK 回退字体的单行可能
    /// 比它高；留半行容差是因为两次测量在部分后端（d2d）是两套独立实现，单行也可能差
    /// 一两个像素，误判成多行就会切掉下伸部。
    ///
    /// 以 bounds 而非 `text` 为界：padding 本属自身，留给字形外沿与末行下伸部。裁剪
    /// 下沿按整行对齐——直接裁在 bounds 下沿，下一行的字头会在底部露出几个像素的碎点；
    /// 至少保留一行，故首行（含下伸部）总是完整的。
    pub(crate) fn clip_rect(
        &self,
        s: &str,
        canvas: &mut dyn Canvas,
        ts: &TextStyle,
        text: Rect,
        bounds: Rect,
    ) -> Option<Rect> {
        // 文字矩形整个落在 bounds 下沿以下（上 padding 超过了高度）：一个字也不该画。
        // 软后端与 GPU 后端对零高矩形本就跳过不画，这里是给其余后端的兜底。
        if text.y >= bounds.bottom() {
            return Some(Rect::new(bounds.x, bounds.y, bounds.w, 0));
        }
        let key = layout_key(s, text.w, ts, canvas.dpi_scale());
        let (multi_h, single_h) = match self.cache.get() {
            Some((k, m, l)) if k == key => (m, l),
            _ => {
                let wrapped = canvas.measure_text_wrapped(s, ts, text.w as f32).h;
                // 单行高量首个非空段：不折行测量也会把换行符算成多行，拿整段去量会把
                // 行高估成两倍，整行对齐时就多裁掉一行本来放得下的字。
                let probe = s
                    .split(is_line_break)
                    .find(|l| !l.is_empty())
                    .unwrap_or("Ay");
                let single = canvas.measure_text(probe, ts).h.max(1);
                let multi = s.contains(is_line_break) || wrapped * 2 > single * 3;
                let v = (if multi { wrapped } else { 0 }, single);
                self.cache.set(Some((key, v.0, v.1)));
                v
            }
        };
        // 高度不进键：它只参与下面的比较，分配高度变了不必重排。
        // 判据与裁剪同口径——都看 bounds：排版越过 `text` 而仍在 bounds 内时，裁了也是
        // 白裁（软后端每次 clip 还要分配一张整窗 mask）。装得下时 `draw_text` 在 `text`
        // 内居中，本就不出 bounds。
        if multi_h <= text.h || text.y + multi_h <= bounds.bottom() {
            return None;
        }
        // single_h 是 ceil 过的（偏大至多 1px），十几行以上时行数可能估小一行、少留一行；
        // 只在钉死高度的超长文本上出现，接受。
        let lines = (multi_h as f32 / single_h as f32).round().max(1.0);
        let line_h = multi_h as f32 / lines;
        // 可保留行数也按 bounds 算（与上面的判据同口径）：按 `text.h` 算的话，带下
        // padding 时文案多一行反而会把原本装得下的第二行也藏掉。
        let avail = bounds.bottom() - text.y;
        let keep = ((avail as f32 / line_h).floor().max(1.0) * line_h).ceil() as i32;
        let bottom = bounds.bottom().min(text.y + keep);
        Some(Rect::new(bounds.x, bounds.y, bounds.w, bottom - bounds.y))
    }

    /// `draw_text` 的就地替代：溢出时裁剪，否则原样画。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn draw_text(
        &self,
        canvas: &mut dyn Canvas,
        s: &str,
        text: Rect,
        bounds: Rect,
        color: Color,
        align: Align,
        ts: &TextStyle,
    ) {
        match self.clip_rect(s, canvas, ts, text, bounds) {
            Some(r) => {
                canvas.save();
                canvas.clip_rect(r);
                canvas.draw_text(s, text, color, align, ts);
                canvas.restore();
            }
            None => canvas.draw_text(s, text, color, align, ts),
        }
    }
}

/// 语义上单行的文字：放不下 `rect.w` 时截成 `text…`，不折行。
#[derive(Default)]
pub(crate) struct SingleLine {
    /// `(排版键, 截断串；放得下为 None)`。
    cache: RefCell<Option<(u64, Option<String>)>>,
}

impl SingleLine {
    /// `draw_text` 的就地替代：放得下原样画，放不下画截断串。
    ///
    /// 截断串按 `measure_text` 的宽度挑出 ≤ `rect.w` 的前缀，与绘制同源，故不会再折行。
    pub(crate) fn draw_text(
        &self,
        canvas: &mut dyn Canvas,
        s: &str,
        rect: Rect,
        color: Color,
        align: Align,
        ts: &TextStyle,
    ) {
        let key = layout_key(s, rect.w, ts, canvas.dpi_scale());
        let hit = match self.cache.borrow().as_ref() {
            Some((k, v)) if *k == key => Some(v.clone()),
            _ => None,
        };
        let fitted = match hit {
            Some(v) => v,
            None => {
                // 硬换行压成空格：这类控件只有一行可画，留着换行符就又折出 bounds 了。
                // 不止 `\n`——CRLF 文案（Windows 配置、剪贴板）里的 `\r` 在 DirectWrite /
                // Core Text 下同样分段。
                let flat = flatten_line_breaks(s);
                let src = flat.as_deref().unwrap_or(s);
                let (out, cut) = truncate_to_width(src, canvas, ts, rect.w.max(0), Truncate::End);
                let v = (cut || flat.is_some()).then_some(out);
                *self.cache.borrow_mut() = Some((key, v.clone()));
                v
            }
        };
        canvas.draw_text(fitted.as_deref().unwrap_or(s), rect, color, align, ts);
    }
}

/// 截断后的显示串（含省略号）及是否实际发生了截断。调用方负责缓存结果——前缀宽度表
/// 要做 O(N) 次测量，不宜每帧重算。
pub(crate) fn truncate_to_width(
    s: &str,
    canvas: &mut dyn Canvas,
    ts: &TextStyle,
    avail_w: i32,
    mode: Truncate,
) -> (String, bool) {
    let total_w = canvas.measure_text(s, ts).w;
    if total_w <= avail_w {
        return (s.to_string(), false);
    }
    let ew = canvas.measure_text("…", ts).w;
    // 连省略号都放不下：画省略号反而比矩形还宽（居中时伸出两侧），不如什么都不画。
    if avail_w < ew {
        return (String::new(), true);
    }
    let avail = avail_w - ew;
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    // 前缀累计宽度表（O(N) 次 measure，之后 partition_point 二分）。
    let mut widths = vec![0i32; n + 1];
    let mut acc = String::new();
    for (i, &c) in chars.iter().enumerate() {
        acc.push(c);
        widths[i + 1] = canvas.measure_text(&acc, ts).w;
    }
    let out = match mode {
        Truncate::End => {
            // partition_point 返回第一个 > avail 的下标，该位置的字符本身已超宽，
            // 需 -1 取最后一个能放下的字符数。
            let cut = widths
                .partition_point(|&w| w <= avail)
                .saturating_sub(1)
                .min(n);
            format!("{}…", chars[..cut].iter().collect::<String>())
        }
        Truncate::Start => {
            // partition_point(w < threshold) 返回第一个 >= threshold 的下标，
            // 即从该字符起的后缀宽度 ≤ avail，此处无 off-by-one。
            let threshold = total_w - avail;
            let cut = widths.partition_point(|&w| w < threshold).min(n);
            format!("…{}", chars[cut..].iter().collect::<String>())
        }
        Truncate::Middle => {
            let lcut = widths
                .partition_point(|&w| w <= avail / 2)
                .saturating_sub(1)
                .min(n);
            let right_avail = (avail - widths[lcut]).max(0);
            let threshold = total_w - right_avail;
            let rcut = widths.partition_point(|&w| w < threshold).min(n);
            let left: String = chars[..lcut].iter().collect();
            let right: String = chars[rcut..].iter().collect();
            format!("{left}…{right}")
        }
        Truncate::None => unreachable!(),
    };
    (out, true)
}

/// 墨量类测试的共用件：真实平台文字引擎 + 真实 build/layout/paint。
#[cfg(test)]
pub(crate) mod ink {
    use crate::core::Tree;
    use crate::geometry::{Rect, Size};
    use crate::text::TextEngine;
    use crate::ui::Element;

    /// 平台文字引擎（scale 1）。
    pub(crate) fn engine() -> crate::text::PlatformTextEngine {
        let mut eng = crate::text::PlatformTextEngine::default();
        eng.set_scale(1.0);
        eng
    }

    /// 白底上 `[y0, y1)` 行区间内的非白像素数（墨量）。
    pub(crate) fn ink_rows(pm: &tiny_skia::Pixmap, y0: i32, y1: i32) -> usize {
        let w = pm.width() as i32;
        let d = pm.data();
        let mut n = 0;
        for y in y0.max(0)..y1.min(pm.height() as i32) {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if d[i] != 255 || d[i + 1] != 255 || d[i + 2] != 255 {
                    n += 1;
                }
            }
        }
        n
    }

    /// 用平台文字引擎布局并画一帧（白底、scale 1），返回画布与被测节点的绝对矩形。
    ///
    /// `el` 的首个子节点须是被测控件（下称 label）。下方兄弟节点特意不铺底色——整窗帧里后画的
    /// 不透明兄弟会把溢出盖住，测的就不是 label 自己画没画出界了；局部重绘只重画
    /// label 时正是这个情形。
    pub(crate) fn paint_first_child(el: Element, w: i32, h: i32) -> (tiny_skia::Pixmap, Rect) {
        paint_at(el, w, h, &[0])
    }

    /// 同 [`paint_first_child`]，被测节点按子节点下标路径从根往下找。
    pub(crate) fn paint_at(
        el: Element,
        w: i32,
        h: i32,
        path: &[usize],
    ) -> (tiny_skia::Pixmap, Rect) {
        let mut eng = engine();
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(w, h), &mut eng);
        let mut node = root;
        for &i in path {
            node = tree.get(node).unwrap().children[i];
        }
        let lb = tree.abs_bounds(node);
        let mut pm = tiny_skia::Pixmap::new(w as u32, h as u32).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let mut cv = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut cv);
        drop(cv);
        (pm, lb)
    }

    pub(crate) const LONG_TITLE: &str = "方案（富内容：副标题 + 徽章 + 可点击尾随图标）";

    /// 断言：label 文字确实折成了多行（否则测不到溢出），且下沿以下没有墨。
    pub(crate) fn assert_no_ink_below(pm: &tiny_skia::Pixmap, lb: Rect, what: &str) {
        assert!(
            ink_rows(pm, lb.y, lb.bottom()) > 0,
            "{what}：正控——label 自身范围内应当有字"
        );
        assert_eq!(
            ink_rows(pm, lb.bottom(), pm.height() as i32),
            0,
            "{what}：折行后高出分配高度的文字画到了 label 下沿 {} 以下，\
             会压在下方兄弟上（局部重绘时露出）",
            lb.bottom()
        );
    }

    /// 不依赖 CJK 字体的长文案：拉丁字体每台机器都有，折行行为比中文稳定得多（CI 的
    /// ubuntu 镜像没有 CJK 字体，中文落到回退字形上、宽度与本机全然不同）。
    pub(crate) const LONG_LATIN: &str =
        "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor";

    /// 前提校验：`text` 以 `size` 号字在宽 `w` 内折行后高于 `min_h`。
    ///
    /// `w` 取控件宽即可——文字区只会比它窄、折得只会更高，前提照样成立。没有这条，某种
    /// 字体下文案恰好放得下时，"不出界"会空洞成立，去掉修复也照样绿。
    pub(crate) fn assert_wraps_taller(text: &str, size: f32, w: i32, min_h: i32) {
        let got = engine()
            .measure(text, &crate::text::TextStyle::new(size), Some(w as f32))
            .h;
        assert!(
            got > min_h,
            "前提不成立：{text:?} 以 {size} 号字在 {w} 宽内排版高 {got}，未超过 {min_h}"
        );
    }

    /// 让 `text` 排版高度超过 `min_h` 的测试宽度：从 120 往窄里试。
    ///
    /// 折成几行取决于机器上有什么字体——CI 的 ubuntu 镜像没有 CJK 字体，中文落到回退
    /// 字形上、宽度不同，120 宽只折两行，写死宽度的前提在那里不成立（本机有 Noto CJK
    /// 时成立，于是本地全绿、CI 红）。按实测选宽度，前提在任何字体下都成立。
    pub(crate) fn wrap_width_exceeding(text: &str, min_h: i32) -> i32 {
        let mut eng = engine();
        let ts = crate::text::TextStyle::new(13.0);
        (40..=120)
            .rev()
            .step_by(8)
            .find(|&w| eng.measure(text, &ts, Some(w as f32)).h > min_h)
            .unwrap_or_else(|| panic!("前提不成立：40..=120 宽内排版高都不超过 {min_h}"))
    }

    /// 前提校验：这段文字在测试宽度下确实折成多行、排版高度超过 20。
    pub(crate) fn assert_wraps_taller_than(width: i32, h: i32) {
        let mut eng = engine();
        let ts = crate::text::TextStyle::new(13.0);
        let sz = eng.measure(LONG_TITLE, &ts, Some(width as f32));
        assert!(
            sz.h > h,
            "前提不成立：{width} 宽下排版高 {} 未超过 {h}，测不到溢出",
            sz.h
        );
    }

    /// 前提校验：`text` 按 `size` 号字单行排版比 `w` 宽——否则"放不下才截断"的路径根本
    /// 没走到，换了字体的平台上测试会测个空集照样绿。
    pub(crate) fn assert_single_line_wider_than(text: &str, size: f32, w: i32) {
        let got = engine()
            .measure(text, &crate::text::TextStyle::new(size), None)
            .w;
        assert!(
            got > w,
            "前提不成立：{text:?} 单行宽 {got} 未超过 {w}，测不到截断"
        );
    }

    /// 矩形 `r` 内的墨量。
    pub(crate) fn ink_in(pm: &tiny_skia::Pixmap, r: Rect) -> usize {
        let w = pm.width() as i32;
        let d = pm.data();
        let mut n = 0;
        for y in r.y.max(0)..r.bottom().min(pm.height() as i32) {
            for x in r.x.max(0)..r.right().min(w) {
                let i = ((y * w + x) * 4) as usize;
                if d[i] != 255 || d[i + 1] != 255 || d[i + 2] != 255 {
                    n += 1;
                }
            }
        }
        n
    }

    /// 断言：`r` 内有墨（正控），`r` 外一点墨都没有。
    pub(crate) fn assert_no_ink_outside(pm: &tiny_skia::Pixmap, r: Rect, what: &str) {
        let whole = Rect::new(0, 0, pm.width() as i32, pm.height() as i32);
        let inside = ink_in(pm, r);
        assert!(inside > 0, "{what}：正控——控件范围 {r:?} 内应当有字");
        assert_eq!(
            ink_in(pm, whole) - inside,
            0,
            "{what}：有墨画到了控件 bounds {r:?} 之外（会压在兄弟节点上，局部重绘时露出）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::ink::*;
    use super::*;

    fn white(w: u32, h: u32) -> tiny_skia::Pixmap {
        let mut pm = tiny_skia::Pixmap::new(w, h).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        pm
    }

    /// 单行控件里的 CRLF / 裸 CR / Unicode 分隔符同样压成一行：DirectWrite 与 Core Text
    /// 把 `\r` 当分段，只压 `\n` 的话 CRLF 文案照样画成两行。
    #[test]
    fn single_line_flattens_every_line_break() {
        for text in ["Short\r\nSecond", "Short\rSecond", "Short\u{2028}Second"] {
            let mut eng = engine();
            let mut pm = white(200, 60);
            let mut cv = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            let fit = SingleLine::default();
            let rect = Rect::new(0, 0, 200, 20);
            fit.draw_text(
                &mut cv,
                text,
                rect,
                Color::rgb(0, 0, 0),
                Align::Start,
                &TextStyle::new(13.0),
            );
            drop(cv);
            assert!(ink_rows(&pm, 0, 20) > 0, "{text:?}：正控——应画出字");
            assert_eq!(
                ink_rows(&pm, 20, 60),
                0,
                "{text:?}：换行没压平，画出了第二行"
            );
        }
    }

    /// 压平的字符集：Linux 自带排版不把 `\r`、U+2028 当断行，上面的墨量测试在 Linux
    /// 上测不出漏压，故直接钉住字符串层面的结果。
    #[test]
    fn flatten_covers_crlf_and_unicode_separators() {
        assert_eq!(flatten_line_breaks("无换行"), None);
        assert_eq!(flatten_line_breaks("a\r\nb").as_deref(), Some("a b"));
        for sep in [
            '\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}',
        ] {
            assert_eq!(
                flatten_line_breaks(&format!("a{sep}b")).as_deref(),
                Some("a b"),
                "{sep:?} 没压平"
            );
        }
    }

    /// 宽度连省略号都放不下：截成空串，而不是画一个比矩形还宽的 `…`。
    #[test]
    fn truncate_narrower_than_ellipsis_is_empty() {
        let mut eng = engine();
        let mut pm = white(10, 10);
        let mut cv = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        let ts = TextStyle::new(13.0);
        let ew = cv.measure_text("…", &ts).w;
        assert!(ew > 1, "前提：省略号有宽度");
        let (out, cut) = truncate_to_width(LONG_TITLE, &mut cv, &ts, ew - 1, Truncate::End);
        assert_eq!((out.as_str(), cut), ("", true));
    }
}
