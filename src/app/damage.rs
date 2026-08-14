//! 局部重绘仲裁与后备缓冲。
//!
//! 每帧要回答一个问题：这一帧能只重画一小块，还是必须整窗重画？输入是控件上报的
//! 交互脏区、动画脏区、结构签名变化与浮层存在与否；输出是决策本身与一块持久的
//! 后备缓冲（保留上一全窗帧，供局部帧重建未变区域）。
//!
//! 只依赖 `Rect` 与 `Pixmap`，与控件树的关系仅止于"把树画进子 pixmap"。

use tiny_skia::Pixmap;

use crate::core::DamageReq;
use crate::geometry::{Point, Rect, Size};
use crate::render::{RenderTarget, SkiaCanvas};

use super::UiHost;

/// 脏区四周外扩的抗锯齿余量（逻辑像素）：覆盖滑块边缘 AA 与子像素取整，杜绝残影。
const DAMAGE_MARGIN: i32 = 2;

/// 宿主持有的重绘仲裁状态。
pub(super) struct DamageState {
    /// 持久后备缓冲（物理像素，整窗）：保留上一全窗帧，供局部帧重建未变区域。
    back: Option<Pixmap>,
    /// 上一帧累积的动画脏区（逻辑坐标）：下一动画帧据此局部重绘；None=下一帧需全窗。
    pending: Option<Rect>,
    /// 交互事件累积的失效区域（逻辑坐标）：下一帧与动画脏区并集后决定局部/整窗。
    pub(super) event: Option<Rect>,
    /// 本帧需重排（点击/按键后置位）：render 先 layout_root，再以结构签名判定是否升级整窗。
    pub(super) needs_relayout: bool,
    /// 上一帧的结构签名（可见性+布局）；与重排后签名比对，变则升级整窗。
    pub(super) last_layout_sig: u64,
    /// `last_layout_sig` 是否已就绪（首帧布局后置真）。
    pub(super) sig_valid: bool,
    /// 强制本帧全窗重绘（输入/结构/尺寸变更触发）。
    pub(super) needs_full: bool,
    /// 测试钩子：上一帧是否走了整窗路径（验证交互是否成功局部重绘）。
    #[cfg(test)]
    pub(super) last_frame_full: bool,
}

impl Default for DamageState {
    fn default() -> Self {
        Self {
            back: None,
            pending: None,
            event: None,
            needs_relayout: false,
            last_layout_sig: 0,
            sig_valid: false,
            // 首帧无后备缓冲可复用，必须整窗。
            needs_full: true,
            #[cfg(test)]
            last_frame_full: false,
        }
    }
}

impl UiHost {
    /// 消费一次分发的失效请求：`Rect` 累积为局部脏区，`Layout`/`Full` 升级为整窗。
    /// （Layer 1：`Layout` 暂等价整窗，精确子树重排留待 Layer 2。）
    pub(super) fn apply_damage(&mut self, d: DamageReq) {
        match d {
            DamageReq::Rect(r) => {
                self.damage.event = Some(match self.damage.event {
                    Some(e) => e.union(&r),
                    None => r,
                });
            }
            DamageReq::Layout(_) | DamageReq::Full => self.damage.needs_full = true,
            DamageReq::None => {}
        }
    }

    /// 全窗 vs 局部重绘决策，返回 `(是否整窗, 本帧脏区)`：
    /// - `needs_full`（输入/结构/尺寸变更）、后备缓冲缺失/尺寸不符、有浮层、无脏区 → 全窗。
    /// - 否则用上一帧动画脏区做局部重绘（仅重画动的那一小块，高 DPI 也稳 60fps）。
    pub(super) fn decide_repaint(
        &mut self,
        target: &mut dyn RenderTarget,
        size: Size,
    ) -> (bool, Option<Rect>) {
        let back_ok = self
            .damage
            .back
            .as_ref()
            .map(|b| b.width() == size.w as u32 && b.height() == size.h as u32)
            .unwrap_or(false);
        let overlay = self.menu.is_open()
            || self.toast.is_active()
            || self.tooltip.will_show(&self.tree, self.hover);
        // 下一帧脏区 = 动画脏区（上帧遗留）∪ 交互脏区（事件累积）。
        let damage = match (self.damage.pending.take(), self.damage.event.take()) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        };
        // 局部重绘前提：scale 为 0.25 的倍数——4 逻辑像素 ×scale 才为整数，子 pixmap 与全窗帧才
        // 逐像素对齐（否则文字纵向 1px 抖动）。非 25% 倍数缩放（罕见的分数缩放）一律退全窗，
        // 这也使「平台层零改动、各平台始终拿到完整 pixmap」的不变量在任何 scale 下都安全。
        let scale_ok = {
            let q = self.scale * 4.0;
            (q - q.round()).abs() < 1e-3
        };
        // 脏区超过窗口一半 → 退全窗：多控件并集过大时，局部重绘的子 pixmap 分配+合成反而净亏损。
        let damage_small = damage
            .map(|d| {
                let win = self.logical_size.w as i64 * self.logical_size.h as i64;
                win > 0 && (d.w as i64 * d.h as i64) * 2 <= win
            })
            .unwrap_or(false);
        let do_full = self.damage.needs_full
            || !back_ok
            || overlay
            || !scale_ok
            || !damage_small
            || target.as_pixmap().is_none();
        self.damage.needs_full = false;
        #[cfg(test)]
        {
            self.damage.last_frame_full = do_full;
        }
        (do_full, damage)
    }

    /// 帧末收尾（两条路径共用）：把本帧累积的动画脏区映射为下一帧的局部脏区，
    /// 并把布局动画的重排请求送进 `needs_relayout` 正规门。
    pub(super) fn finish_frame_damage(&mut self) {
        self.damage.pending = next_damage(&mut self.damage.needs_full);
        // 布局动画（高度补间等）请求下一帧重排：走 needs_relayout 正规门，
        // 重排后按结构签名升级整窗并执行 hover 重同步。
        if crate::anim::take_relayout() {
            self.damage.needs_relayout = true;
        }
    }

    /// 局部重绘：把脏区渲染进脏区大小的子 pixmap（tiny-skia 按 pixmap 边界自动剔除框外
    /// 图元，成本降到脏区面积），合成进后备缓冲，再整窗拷给平台 pixmap。复用上一全窗帧的
    /// 布局（当前动画均为视觉位移、不改布局）。
    pub(super) fn render_partial(&mut self, pixmap: &mut Pixmap, size: Size, s: f32, damage: Rect) {
        // 脏区外扩 AA 余量并钳到窗口逻辑范围。
        let raw = damage
            .inflate(DAMAGE_MARGIN)
            .intersect(&Rect::from_size(self.logical_size));
        // 原点对齐到 4 逻辑像素网格：Windows DPI 缩放恒为 25% 的倍数（scale=m/4），故 4 的倍数 ×scale
        // 必为整数，子 pixmap 物理原点 dmg.origin×scale 精确无取整 → 文字定位与全窗帧逐像素一致，
        // 消除局部帧的纵向 1px 抖动。
        const GRID: i32 = 4;
        let x0 = raw.x - raw.x.rem_euclid(GRID);
        let y0 = raw.y - raw.y.rem_euclid(GRID);
        let x1 = raw.right() + (GRID - raw.right().rem_euclid(GRID)) % GRID;
        let y1 = raw.bottom() + (GRID - raw.bottom().rem_euclid(GRID)) % GRID;
        let dmg =
            Rect::new(x0, y0, x1 - x0, y1 - y0).intersect(&Rect::from_size(self.logical_size));
        // 物理化并钳到 pixmap 边界。
        let pdmg = dmg.scaled(s).intersect(&Rect::new(0, 0, size.w, size.h));
        if pdmg.is_empty() {
            self.blit_back_to(pixmap);
            return;
        }
        // 子 pixmap：脏区大小，按窗口背景填底（与全窗帧平台 fill 同色，重建一致）。
        let Some(mut sub) = Pixmap::new(pdmg.w as u32, pdmg.h as u32) else {
            self.blit_back_to(pixmap);
            return;
        };
        sub.fill(tiny_skia::Color::from_rgba8(
            self.bg.r, self.bg.g, self.bg.b, self.bg.a,
        ));
        // 以脏区左上角（逻辑）为偏移绘制整树：框外图元由 tiny-skia 廉价剔除。
        {
            let mut canvas = SkiaCanvas::with_text_offset(
                &mut sub,
                &mut self.engine,
                s,
                Point::new(dmg.x, dmg.y),
            );
            self.tree.paint(&mut canvas);
        }
        // 合成进后备缓冲（脏区物理原点），再整窗拷给平台 pixmap。
        if let Some(back) = self.damage.back.as_mut() {
            blit(&sub, back, pdmg.x, pdmg.y);
        }
        self.blit_back_to(pixmap);
    }

    /// 把后备缓冲整窗拷入 pixmap（两者同尺寸时）。
    fn blit_back_to(&self, pixmap: &mut Pixmap) {
        if let Some(back) = self.damage.back.as_ref() {
            if back.width() == pixmap.width() && back.height() == pixmap.height() {
                pixmap.data_mut().copy_from_slice(back.data());
            }
        }
    }

    /// 全窗帧结束：把刚绘好的 pixmap 整窗种入后备缓冲，供后续局部帧复用（按需重建尺寸）。
    pub(super) fn seed_back(&mut self, pixmap: &Pixmap, size: Size) {
        let need_new = self
            .damage
            .back
            .as_ref()
            .map(|b| b.width() != size.w as u32 || b.height() != size.h as u32)
            .unwrap_or(true);
        if need_new {
            self.damage.back = Pixmap::new(size.w as u32, size.h as u32);
        }
        if let Some(back) = self.damage.back.as_mut() {
            back.data_mut().copy_from_slice(pixmap.data());
        }
    }
}

/// 取本帧累积的动画脏区，映射为下一帧的局部脏区；Full（浮层/fling 等节点外请求）→
/// 标记下一帧全窗、返回 None。
fn next_damage(needs_full: &mut bool) -> Option<Rect> {
    match crate::anim::take_damage() {
        crate::anim::Damage::Rect(r) => Some(r),
        crate::anim::Damage::Full => {
            *needs_full = true;
            None
        }
        crate::anim::Damage::None => None,
    }
}

/// 把 src（RGBA8）整块覆盖拷入 dst 的 (x,y)（src 不超出 dst；不做 alpha 混合）。
fn blit(src: &Pixmap, dst: &mut Pixmap, x: i32, y: i32) {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (dw, dh) = (dst.width() as usize, dst.height() as usize);
    let (x, y) = (x.max(0) as usize, y.max(0) as usize);
    // 契约：src 必须完整落在 dst 内（调用方已把脏区钳到 pixmap 边界）。越界即逻辑错误。
    debug_assert!(
        x + sw <= dw && y + sh <= dh,
        "blit 越界：({x},{y})+{sw}x{sh} 超出 {dw}x{dh}"
    );
    let sd = src.data();
    let dd = dst.data_mut();
    for row in 0..sh {
        let s0 = row * sw * 4;
        let d0 = ((y + row) * dw + x) * 4;
        dd[d0..d0 + sw * 4].copy_from_slice(&sd[s0..s0 + sw * 4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::geometry::Point;
    use crate::ui::Element;

    #[test]
    fn interaction_takes_partial_path() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        let app = App::new("t", 60, 60).content(Element::col().width(60).height(60));
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(60, 60).unwrap();
        // 首帧：全窗，种入后备缓冲。
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        assert!(handler.damage.last_frame_full, "首帧应为全窗");
        // 模拟交互产生的小脏区：下一帧应走局部重绘，不重排整树。
        handler.damage.event = Some(Rect::new(10, 10, 12, 12));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        assert!(
            !handler.damage.last_frame_full,
            "带小脏区的交互帧应走局部重绘"
        );
    }

    #[test]
    fn structural_click_repaints_full() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        // 按钮点击切换 visible_when 面板显隐（结构变化）→ 重排后签名变 → 必须整窗。
        let flag = std::rc::Rc::new(std::cell::Cell::new(false));
        let f2 = flag.clone();
        let app = App::new("t", 80, 80).content(
            Element::col()
                .width(80)
                .height(80)
                .child(Element::button("X").on_click(move |_| f2.set(true)))
                .child(
                    Element::col()
                        .width(80)
                        .height(30)
                        .visible_when(move || flag.get()),
                ),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(80, 80).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(80, 80)); // 首帧全窗 + 建立结构签名
        let at = Point::new(15, 12);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(80, 80));
        assert!(
            handler.damage.last_frame_full,
            "切换 visible_when 面板应整窗刷新"
        );
    }

    #[test]
    fn local_click_stays_partial() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        // 无结构副作用的按钮点击：重排后签名不变 → 走局部重绘（不整窗）。
        let app = App::new("t", 120, 120).content(
            Element::col()
                .width(120)
                .height(120)
                .child(Element::button("X")),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(120, 120).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 120)); // 首帧全窗
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(15, 12),
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 120));
        assert!(
            !handler.damage.last_frame_full,
            "无结构变化的点击应走局部重绘"
        );
    }

    #[test]
    fn closing_menu_repaints_full() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        // 回归：关闭浮层的那一帧必须整窗——菜单画在控件树之上，局部重绘只擦交互脏区，
        // 面板像素会残留在屏上。overlay 判定读的是"本帧有没有浮层"，而关闭帧已经没有了，
        // 恰好此时补间还在跑（打开时 hover 清零触发边框补间）就会带着小脏区走局部路径。
        let app = App::new("t", 200, 200).content(Element::col().width(200).height(200).child(
            Element::dropdown(vec!["甲", "乙", "丙"], crate::signal::signal(0usize)).width(120),
        ));
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));

        // 点控件展开菜单。
        let on_ctl = Point::new(40, 12);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            on_ctl,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            on_ctl,
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));
        assert!(handler.menu.is_open(), "应已展开菜单");
        assert!(handler.damage.last_frame_full, "有浮层的帧本就整窗");

        // 点面板外关闭：这一帧浮层已消失，必须整窗把面板像素擦掉。
        let outside = Point::new(190, 190);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            outside,
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));
        assert!(!handler.menu.is_open(), "点面板外应关闭菜单");
        assert!(
            handler.damage.last_frame_full,
            "关闭浮层的那一帧必须整窗，否则面板像素残留"
        );
    }
    /// 鼠标在两个文本框之间点击 → 整窗刷新，否则旧框的光标竖条会残留。
    ///
    /// 旧焦点收不到本次事件，脏区里只有被点中的那个控件；若走局部重绘，新框画出光标、
    /// 旧框的光标仍留在后备缓冲里，要等下一次全窗刷新才消失。macOS 实测发现，但成因与
    /// 平台无关——三条焦点路径里只有"鼠标点到另一个可聚焦控件"漏了这一步（Tab 与点空白
    /// 清焦点都已置 needs_full）。
    #[test]
    fn pointer_focus_transfer_repaints_full() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        let a = crate::signal::signal(String::new());
        let b = crate::signal::signal(String::new());
        let app = App::new("t", 200, 120).content(
            Element::col()
                .width(200)
                .height(120)
                .child(Element::text_input(a, "甲").width(180).height(32))
                .child(Element::text_input(b, "乙").width(180).height(32)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 120).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 120));

        let click = |h: &mut crate::app::UiHost, at: Point| {
            h.on_pointer(PointerEvent::single(
                PointerKind::Down,
                at,
                MouseButton::Left,
            ));
            h.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        };
        // 先点第一个框拿到焦点，把这帧的全窗消化掉。
        click(&mut handler, Point::new(40, 16));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 120));
        // 再点第二个框：焦点从甲转到乙，甲的光标必须被擦掉。
        click(&mut handler, Point::new(40, 48));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 120));
        assert!(
            handler.damage.last_frame_full,
            "焦点在两个文本框之间转移应整窗刷新，否则旧框光标残留"
        );
    }
}
