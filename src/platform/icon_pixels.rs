//! 系统图标位图 → 非预乘 RGBA8（纯函数，`file_icon` 的 Windows / macOS 实现共用）。
//!
//! 两个平台交出来的图标位图都是**预乘** alpha（Windows 的 Shell 位图供 `AlphaBlend` 用，
//! macOS 是按 `PremultipliedLast` 建的位图上下文），而 [`Image::from_rgba`] 要非预乘。
//!
//! Windows 侧有两种例外要兜住：
//! - 个别 Shell 扩展交出的是**非预乘**位图——表现为半透明像素的颜色分量大于 alpha（预乘
//!   数据里不会出现）。这时照预乘去反算会把半透明边缘提亮成一圈白边，故整张不做反预乘。
//!   判定要留余量：真正的预乘图经缩放 / 合成会有 ±1 的舍入，个别像素越线是常事；alpha 为 0
//!   的像素在预乘图里也可能带颜色（加色发光）。所以只看半透明像素、容许 2 级误差，越线的
//!   超过一成才认定是非预乘。
//! - 24 位风格的位图 alpha 全为 0——直接用会整张透明，按不透明处理。
//!
//! [`Image::from_rgba`]: crate::render::image::Image::from_rgba

/// 通道顺序。
#[derive(Clone, Copy)]
pub(crate) enum Order {
    /// Windows DIB 的 B、G、R、A。
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    Bgra,
    /// macOS 位图上下文的 R、G、B、A。
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Rgba,
}

/// 预乘位图（按 `order` 排列）→ 非预乘 RGBA8，原地转换。
pub(crate) fn to_straight_rgba(px: &mut [u8], order: Order) {
    let chunks = px.as_chunks_mut::<4>().0;
    if matches!(order, Order::Bgra) {
        for p in chunks.iter_mut() {
            p.swap(0, 2);
        }
    }
    if chunks.iter().all(|p| p[3] == 0) {
        for p in chunks.iter_mut() {
            p[3] = 255;
        }
        return;
    }
    if !looks_premultiplied(chunks) {
        return;
    }
    for p in chunks.iter_mut() {
        let a = u32::from(p[3]);
        if a == 0 || a == 255 {
            continue;
        }
        for c in &mut p[..3] {
            *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

/// 判断是否预乘，规则见模块注释。没有半透明像素时（全不透明 / 全透明）两种解释结果相同，
/// 按预乘处理即可。
fn looks_premultiplied(px: &[[u8; 4]]) -> bool {
    const ROUNDING: u8 = 2;
    let (mut semi, mut over) = (0usize, 0usize);
    for p in px {
        let a = p[3];
        if a == 0 || a == 255 {
            continue;
        }
        semi += 1;
        let limit = a.saturating_add(ROUNDING);
        if p[0] > limit || p[1] > limit || p[2] > limit {
            over += 1;
        }
    }
    over * 10 <= semi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bgra_is_swapped_and_unpremultiplied() {
        // 预乘的半透明红：R=128 A=128 → 非预乘 R=255。
        let mut px = vec![0, 0, 128, 128, 255, 0, 0, 255];
        to_straight_rgba(&mut px, Order::Bgra);
        assert_eq!(px, vec![255, 0, 0, 128, 0, 0, 255, 255]);
    }

    #[test]
    fn rgba_order_is_kept() {
        let mut px = vec![64, 0, 0, 128];
        to_straight_rgba(&mut px, Order::Rgba);
        assert_eq!(px, vec![128, 0, 0, 128]);
    }

    #[test]
    fn all_zero_alpha_is_treated_as_opaque() {
        let mut px = vec![10, 20, 30, 0, 40, 50, 60, 0];
        to_straight_rgba(&mut px, Order::Rgba);
        assert_eq!(px, vec![10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn straight_alpha_input_is_not_brightened() {
        // 颜色分量 200 > alpha 100：不可能是预乘数据，保持原样。
        let mut px = vec![200, 0, 0, 100, 50, 0, 0, 100];
        to_straight_rgba(&mut px, Order::Rgba);
        assert_eq!(px, vec![200, 0, 0, 100, 50, 0, 0, 100]);
    }

    /// 预乘图里个别像素越线（低于一成）、外加 alpha=0 的加色像素：仍应整体反预乘。
    #[test]
    fn rounding_noise_and_additive_pixels_do_not_flip_the_verdict() {
        let mut px = Vec::new();
        for _ in 0..19 {
            px.extend_from_slice(&[64, 0, 0, 128]); // 合规的预乘半透明红
        }
        px.extend_from_slice(&[6, 0, 0, 3]); // 越线超出容差（20 个半透明像素里仅此 1 个）
        px.extend_from_slice(&[30, 30, 30, 0]); // 加色像素
        to_straight_rgba(&mut px, Order::Rgba);
        assert_eq!(&px[..4], &[128, 0, 0, 128], "主体应被反预乘");
    }

    /// 非预乘图：半透明边缘普遍越线，即使混着少量合规像素也应判为非预乘、原样保留。
    #[test]
    fn mostly_over_limit_semi_pixels_mean_straight_alpha() {
        let mut px = Vec::new();
        for _ in 0..8 {
            px.extend_from_slice(&[255, 255, 255, 40]); // 白色柔边（非预乘）
        }
        px.extend_from_slice(&[20, 20, 20, 40]);
        to_straight_rgba(&mut px, Order::Rgba);
        assert_eq!(&px[..4], &[255, 255, 255, 40]);
        assert_eq!(&px[32..36], &[20, 20, 20, 40]);
    }

    #[test]
    fn fully_transparent_pixels_stay_zero() {
        let mut px = vec![0, 0, 0, 0, 0, 0, 255, 255];
        to_straight_rgba(&mut px, Order::Rgba);
        assert_eq!(&px[..4], &[0, 0, 0, 0]);
    }
}
