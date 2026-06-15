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
        Self { x: 0, y: 0, w: s.w, h: s.h }
    }
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }
    pub const fn size(&self) -> Size {
        Size { w: self.w, h: self.h }
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
        Self { left: v, top: v, right: v, bottom: v }
    }
    pub const fn symmetric(h: i32, v: i32) -> Self {
        Self { left: h, top: v, right: h, bottom: v }
    }
    pub const fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self { left, top, right, bottom }
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
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255, a: 255 };
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0, a: 255 };
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

    #[test]
    fn color_hex() {
        assert_eq!(Color::hex(0x336699), Color::rgb(0x33, 0x66, 0x99));
    }
}
