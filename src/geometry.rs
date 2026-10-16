//! 基础几何与颜色类型。坐标单位默认物理像素（i32）或浮点（f32，用于绘制）。

/// 点（整数像素，用于布局/事件命中）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// 尺寸（整数像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

impl Size {
    pub const fn new(w: i32, h: i32) -> Self {
        Self { w, h }
    }
    pub const ZERO: Size = Size { w: 0, h: 0 };
}

/// 矩形：左上角 + 宽高（整数像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub const fn from_size(s: Size) -> Self {
        Self {
            x: 0,
            y: 0,
            w: s.w,
            h: s.h,
        }
    }
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub const fn size(&self) -> Size {
        Size {
            w: self.w,
            h: self.h,
        }
    }
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }
    /// 两矩形交集；无交集时返回零宽高矩形。
    pub fn intersect(&self, o: &Rect) -> Rect {
        let x = self.x.max(o.x);
        let y = self.y.max(o.y);
        let r = self.right().min(o.right());
        let b = self.bottom().min(o.bottom());
        Rect::new(x, y, (r - x).max(0), (b - y).max(0))
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    /// 按缩放因子转为物理像素矩形（**就近**取整）。按边界（右/下）取整，避免四分量独立 round 漂移。
    ///
    /// 语义是「不得超出」+「相邻无缝」：相邻逻辑矩形物理化后严丝合缝、既不留缝也不重叠，
    /// 空矩形恒为空。裁剪 mask、图片 dst、脏区等定位/限制类用途都用它。
    ///
    /// 需要「必须容纳得下」（物理宽高不小于 `size × scale`）的场景用 [`Rect::scaled_out`]。
    ///
    /// 取整用 [`round_half_up`] 而非 `f32::round`：局部重绘把世界坐标减去脏区原点再物理化，
    /// 脏区左/上方的节点坐标是负数，`round` 的「半数远离零」在零两侧方向相反，同一条边在
    /// 局部帧与整窗帧里会差 1 物理像素（分数缩放下光标旁的字一淡一深）。
    pub fn scaled(&self, s: f32) -> Rect {
        let x0 = round_half_up(self.x as f32 * s);
        let y0 = round_half_up(self.y as f32 * s);
        let x1 = round_half_up(self.right() as f32 * s);
        let y1 = round_half_up(self.bottom() as f32 * s);
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    /// 按缩放因子转为物理像素矩形（**外扩**取整）：左/上 `floor`、右/下 `ceil`。
    ///
    /// 契约是物理宽高**不小于** `size × scale`——[`Rect::scaled`] 四条边各自 round，
    /// 取整方向不一致时 `x1 - x0` 可能略小于 `w × scale`；文字测量阶段用
    /// `ceil(物理宽 / scale)` 得到逻辑宽度，绘制阶段若反向换算出更窄的物理宽度，
    /// DirectWrite/CoreText 就会把本应单行的文字最后一个字挤到下一行
    /// （125%/175%/225% 等非整数 DPI 下尤其明显）。
    ///
    /// 代价是相邻矩形会重叠 1 物理像素、左上角可能比 `scaled()` 小 1 像素，
    /// 故**只用于「容纳」语义**（排版最大宽度等），不要拿来做裁剪或定位。
    ///
    /// 空矩形恒返回空矩形：否则 `w == 0` 在非整数缩放下会外扩成 1 像素，
    /// 调用方的 `is_empty()` 短路失效，完全滚出视野的内容会漏出 1 像素列。
    pub fn scaled_out(&self, s: f32) -> Rect {
        let x0 = (self.x as f32 * s).floor() as i32;
        let y0 = (self.y as f32 * s).floor() as i32;
        if self.is_empty() {
            return Rect::new(x0, y0, 0, 0);
        }
        let x1 = (self.right() as f32 * s).ceil() as i32;
        let y1 = (self.bottom() as f32 * s).ceil() as i32;
        Rect::new(x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
    }

    /// 包含两矩形的最小外接矩形（空矩形被忽略）。
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        let x = self.x.min(o.x);
        let y = self.y.min(o.y);
        let r = self.right().max(o.right());
        let b = self.bottom().max(o.bottom());
        Rect::new(x, y, r - x, b - y)
    }

    /// 平移。
    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// 四边各外扩 m 像素（脏区留抗锯齿余量用）。
    pub fn inflate(&self, m: i32) -> Rect {
        Rect::new(self.x - m, self.y - m, self.w + 2 * m, self.h + 2 * m)
    }

    /// 向内收缩四边（用于 padding）。
    pub fn inset(&self, i: Insets) -> Rect {
        Rect::new(
            self.x + i.left,
            self.y + i.top,
            (self.w - i.left - i.right).max(0),
            (self.h - i.top - i.bottom).max(0),
        )
    }
}

/// 就近取整到整数物理像素，恰在半数时**一律往 +∞**（`f32::round` 是远离零）。
///
/// 非负值上与 `round` 逐值相同，所以整窗渲染一个像素都不变；差别只在负的半数上，
/// 而那正是局部重绘需要的性质：对整数平移不变，`round_half_up(v - n) == round_half_up(v) - n`。
/// 不写成 `(v + 0.5).floor()`：`0.49999997 + 0.5` 会被 f32 进位成 1.0，与 `round` 不一致。
pub(crate) fn round_half_up(v: f32) -> i32 {
    let r = v.round();
    if r - v == -0.5 {
        (r + 1.0) as i32
    } else {
        r as i32
    }
}

/// 四边内边距/外边距。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Insets {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Insets {
    pub const fn all(v: i32) -> Self {
        Self {
            left: v,
            top: v,
            right: v,
            bottom: v,
        }
    }
    pub const fn symmetric(h: i32, v: i32) -> Self {
        Self {
            left: h,
            top: v,
            right: h,
            bottom: v,
        }
    }
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }
    pub const fn horizontal(&self) -> i32 {
        self.left + self.right
    }
    pub const fn vertical(&self) -> i32 {
        self.top + self.bottom
    }
}

/// 非预乘 sRGB 颜色（u8 通道）。绘制时再转 tiny-skia 的预乘格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    /// 从 0xRRGGBB 构造（不含 alpha）。
    pub const fn hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xff) as u8,
            g: ((v >> 8) & 0xff) as u8,
            b: (v & 0xff) as u8,
            a: 255,
        }
    }
    pub const TRANSPARENT: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };
    pub const WHITE: Color = Color {
        r: 255,
        g: 255,
        b: 255,
        a: 255,
    };
    pub const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };

    /// 按系数缩放 alpha（保 RGB），用于淡入淡出。`f` 钳到 `[0,1]`。
    pub fn scale_alpha(self, f: f32) -> Color {
        Color {
            a: (self.a as f32 * f.clamp(0.0, 1.0)).round() as u8,
            ..self
        }
    }

    /// 解析 `#RGB` / `#RRGGBB` / `#RRGGBBAA`（# 可省）。失败返回 None。
    pub fn from_hex_str(s: &str) -> Option<Self> {
        let h = s.trim().trim_start_matches('#');
        // 必须为 ASCII：否则按字节切片可能落在多字节字符内部 panic（不可信 TOML 输入）。
        if !h.is_ascii() {
            return None;
        }
        let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        match h.len() {
            3 => {
                let d = |c: char| c.to_digit(16).map(|v| (v * 17) as u8);
                let mut it = h.chars();
                Some(Self::rgb(d(it.next()?)?, d(it.next()?)?, d(it.next()?)?))
            }
            6 => Some(Self::rgba(p(0)?, p(2)?, p(4)?, 255)),
            8 => Some(Self::rgba(p(0)?, p(2)?, p(4)?, p(6)?)),
            _ => None,
        }
    }
    /// 序列化为 `#RRGGBB`（alpha=255）或 `#RRGGBBAA`。
    pub fn to_hex_string(&self) -> String {
        if self.a == 255 {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }

    /// 向白色混合 f（0..=1）：变亮。保留 alpha。
    pub fn lighten(self, f: f32) -> Color {
        self.mix(Color::WHITE, f)
    }
    /// 向黑色混合 f（0..=1）：变暗。保留 alpha。
    pub fn darken(self, f: f32) -> Color {
        self.mix(Color::BLACK, f)
    }
    /// 与 other 按比例 t 混合 RGB（保留 self 的 alpha）。t 钳到 [0,1]。
    fn mix(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Color {
            r: m(self.r, other.r),
            g: m(self.g, other.g),
            b: m(self.b, other.b),
            a: self.a,
        }
    }
    /// self 作背景时挑选可读前景：感知亮度 > 阈值返回 dark，否则 light。
    pub fn pick_fg(self, dark: Color, light: Color) -> Color {
        let luma = 0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32;
        if luma > 153.0 {
            dark
        } else {
            light
        }
    }
}

impl serde::Serialize for Color {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex_string())
    }
}

impl<'de> serde::Deserialize<'de> for Color {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Color::from_hex_str(&s).ok_or_else(|| serde::de::Error::custom(format!("无效颜色: {s}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_contains_and_intersect() {
        let r = Rect::new(10, 10, 100, 50);
        assert!(r.contains(Point::new(10, 10)));
        assert!(r.contains(Point::new(109, 59)));
        assert!(!r.contains(Point::new(110, 10)));
        let i = r.intersect(&Rect::new(50, 0, 100, 100));
        assert_eq!(i, Rect::new(50, 10, 60, 50));
    }

    #[test]
    fn rect_inset() {
        let r = Rect::new(0, 0, 100, 100).inset(Insets::all(10));
        assert_eq!(r, Rect::new(10, 10, 80, 80));
    }

    /// 常见 DPI 档位。125%/175%/225% 是 `x * s` 落在 .5 边界、四边取整方向最易分歧的档位。
    const SCALES: [f32; 8] = [1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 3.0];

    #[test]
    fn rect_scaled_tiles_without_gap_or_overlap() {
        // scaled() 的契约之一：相邻逻辑矩形物理化后严丝合缝。
        // 留缝 → 元素之间出现 1px 亮线；重叠 → 后画的元素吃掉前一个 1 像素列。
        // 裁剪 mask / 图片 dst / 脏区都依赖这个性质，勿改成外扩取整（那是 scaled_out 的活）。
        for &s in &SCALES {
            for x in -40..40 {
                for w in 1..30 {
                    let a = Rect::new(x, 0, w, 10).scaled(s);
                    let b = Rect::new(x + w, 0, w, 10).scaled(s);
                    assert_eq!(
                        a.right(),
                        b.x,
                        "s={s} x={x} w={w}: 相邻矩形物理化后必须首尾相接，实得 {a:?} / {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn rect_scaled_and_scaled_out_keep_empty() {
        // 空矩形必须恒为空——clip_rect() 与 draw_image() 都靠 is_empty() 短路。
        // 一旦零宽矩形被取整放大成 1 物理像素，完全滚出视野 / 尺寸为 0 的内容
        // 会漏出 1 像素列，且白跑一遍 Mask 分配与合成。
        for &s in &SCALES {
            for v in -40..40 {
                for r in [
                    Rect::new(v, 3, 0, 20),  // 零宽
                    Rect::new(3, v, 20, 0),  // 零高
                    Rect::new(v, v, 0, 0),   // 全零
                    Rect::new(v, v, -5, -5), // 负尺寸（intersect 之外的脏数据兜底）
                ] {
                    assert!(r.scaled(s).is_empty(), "s={s} {r:?}: scaled 后应仍为空");
                    assert!(
                        r.scaled_out(s).is_empty(),
                        "s={s} {r:?}: scaled_out 后应仍为空"
                    );
                }
            }
        }
    }

    #[test]
    fn rect_scaled_out_never_shrinks() {
        // scaled_out() 的核心契约：物理宽高恒 >= 逻辑尺寸 × scale，一个像素都不能少。
        for &s in &SCALES {
            for x in -40..40 {
                for w in 1..80 {
                    let p = Rect::new(x, x + 7, w, w).scaled_out(s);
                    let want = w as f32 * s;
                    assert!(
                        p.w as f32 >= want,
                        "s={s} x={x} w={w}: 物理宽 {} < {want}",
                        p.w
                    );
                    assert!(
                        p.h as f32 >= want,
                        "s={s} y={} h={w}: 物理高 {} < {want}",
                        x + 7,
                        p.h
                    );
                }
            }
        }
    }

    #[test]
    fn rect_scaled_out_contains_scaled() {
        // scaled_out 恒为 scaled 的超集：把某条链路从 scaled 切到 scaled_out 只会放宽、
        // 不会把已经画得下的内容截短。
        for &s in &SCALES {
            for x in -20..20 {
                for w in 1..40 {
                    let r = Rect::new(x, x + 5, w, w + 3);
                    let (a, b) = (r.scaled(s), r.scaled_out(s));
                    assert!(
                        b.x <= a.x
                            && b.y <= a.y
                            && b.right() >= a.right()
                            && b.bottom() >= a.bottom(),
                        "s={s} {r:?}: scaled_out {b:?} 未包含 scaled {a:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn text_measure_roundtrip_never_clips_last_glyph() {
        // issue #6 回归锚点：文字 measure 把物理宽度按 ceil(物理宽 / scale) 换回逻辑宽度，
        // 布局据此给出 rect.w；绘制阶段算出的排版最大宽度必须仍装得下原始物理宽度，
        // 否则 DirectWrite/CoreText 会把本应单行的文字最后一个字挤到下一行。
        for &s in &SCALES {
            for pw in 1..400 {
                let logical_w = (pw as f32 / s).ceil() as i32; // measure 的回逻辑换算
                for x in -20..20 {
                    let layout_w = Rect::new(x, 0, logical_w, 20).scaled_out(s).w;
                    assert!(
                        layout_w >= pw,
                        "s={s} x={x} 物理宽={pw} → 逻辑宽={logical_w} → 排版宽={layout_w}，装不下"
                    );
                }
            }
        }
    }

    #[test]
    fn rect_scaled_may_shrink_hence_text_uses_scaled_out() {
        // 反向锚定：证明上面那条不是空转——scaled() 确实会算出比 w×scale 更窄的物理宽度，
        // 所以文字排版宽度必须走 scaled_out()，不能图省事用 rect.scaled(s).w。
        let r = Rect::new(3, 0, 10, 10);
        assert!(
            (r.scaled(1.25).w as f32) < 10.0 * 1.25,
            "125% 下 scaled() 应当截短"
        );
        assert!(r.scaled_out(1.25).w as f32 >= 10.0 * 1.25);
    }

    #[test]
    fn color_hex() {
        assert_eq!(Color::hex(0x336699), Color::rgb(0x33, 0x66, 0x99));
    }

    #[test]
    fn color_lighten_darken_bounds() {
        let c = Color::rgb(100, 100, 100);
        assert_eq!(c.lighten(0.0), c, "f=0 不变");
        assert_eq!(c.darken(0.0), c, "f=0 不变");
        assert_eq!(c.lighten(1.0), Color::WHITE, "f=1 趋白");
        assert_eq!(c.darken(1.0), Color::BLACK, "f=1 趋黑");
        assert!(
            c.lighten(0.5).r > c.r && c.darken(0.5).r < c.r,
            "中间值单调"
        );
        assert_eq!(c.lighten(0.5).a, c.a, "保留 alpha");
    }

    #[test]
    fn color_pick_fg_by_luminance() {
        // self 作背景：偏亮选 dark 前景、偏暗选 light 前景。
        assert_eq!(
            Color::rgb(240, 240, 240).pick_fg(Color::BLACK, Color::WHITE),
            Color::BLACK
        );
        assert_eq!(
            Color::rgb(20, 20, 20).pick_fg(Color::BLACK, Color::WHITE),
            Color::WHITE
        );
    }

    #[test]
    fn color_from_hex_str_forms() {
        assert_eq!(
            Color::from_hex_str("#336699"),
            Some(Color::rgb(0x33, 0x66, 0x99))
        );
        assert_eq!(
            Color::from_hex_str("336699"),
            Some(Color::rgb(0x33, 0x66, 0x99))
        );
        assert_eq!(
            Color::from_hex_str("#369"),
            Some(Color::rgb(0x33, 0x66, 0x99))
        );
        assert_eq!(
            Color::from_hex_str("#11223344"),
            Some(Color::rgba(0x11, 0x22, 0x33, 0x44))
        );
    }

    #[test]
    fn color_from_hex_str_rejects_bad_input() {
        // 多字节 UTF-8（字节长 6）不得 panic，返回 None。
        assert_eq!(Color::from_hex_str("€abc"), None);
        assert_eq!(Color::from_hex_str("aébcd"), None);
        assert_eq!(Color::from_hex_str("xyz"), None);
        assert_eq!(Color::from_hex_str("#12"), None);
        assert_eq!(Color::from_hex_str(""), None);
    }

    #[test]
    fn color_hex_string_omits_opaque_alpha() {
        assert_eq!(Color::rgb(0x33, 0x66, 0x99).to_hex_string(), "#336699");
        assert_eq!(
            Color::rgba(0x11, 0x22, 0x33, 0x44).to_hex_string(),
            "#11223344"
        );
    }

    /// 局部重绘把节点坐标减去脏区原点（逻辑，且 `原点 × scale` 为整数）再物理化；
    /// 结果必须与整窗帧物理化后再平移同一个整数完全相同，否则脏区边界处差 1 物理像素。
    #[test]
    fn rect_scaled_is_invariant_under_integer_physical_shift() {
        for s in [1.25f32, 1.5, 1.75, 2.25, 2.5] {
            // 局部帧原点按 4 逻辑像素网格对齐（见 app/damage.rs 的 align_damage），4 × 这几档缩放都是整数。
            for ox in [-40, -4, 4, 8, 40, 400] {
                let shift = (ox as f32 * s) as i32;
                assert_eq!(shift as f32, ox as f32 * s, "前提：原点物理化为整数");
                for x in -400..=400 {
                    for w in [1, 3, 7] {
                        let world = Rect::new(x, x, w, w).scaled(s);
                        let local = Rect::new(x - ox, x - ox, w, w).scaled(s);
                        assert_eq!(
                            (local.x, local.y, local.right(), local.bottom()),
                            (
                                world.x - shift,
                                world.y - shift,
                                world.right() - shift,
                                world.bottom() - shift
                            ),
                            "s={s} ox={ox} x={x} w={w}"
                        );
                    }
                }
            }
        }
    }

    /// 反向锚定：证明上一条不是空转——`f32::round` 在这类输入上确实不满足平移不变。
    #[test]
    fn plain_round_is_not_shift_invariant() {
        // 1.25 × -2 = -2.5：round → -3；而 1.25 × 2 = 2.5 → 3，平移 4 物理像素后应为 -1。
        let (s, x, ox) = (1.25f32, 2, 4);
        let shift = (ox as f32 * s) as i32;
        assert_ne!(
            ((x - ox) as f32 * s).round() as i32,
            (x as f32 * s).round() as i32 - shift
        );
        assert_eq!(
            round_half_up((x - ox) as f32 * s),
            round_half_up(x as f32 * s) - shift
        );
    }

    #[test]
    fn round_half_up_matches_round_for_nonnegative_values() {
        for i in 0..=100_000 {
            let v = i as f32 * 0.0125;
            assert_eq!(round_half_up(v), v.round() as i32, "v={v}");
        }
        assert_eq!(round_half_up(0.49999997), 0);
        assert_eq!(round_half_up(-2.5), -2);
        assert_eq!(round_half_up(-2.6), -3);
    }
}
