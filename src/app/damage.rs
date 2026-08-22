//! 局部重绘仲裁与后备缓冲。
//!
//! 每帧要回答一个问题：这一帧能只重画一小块，还是必须整窗重画？输入是控件上报的
//! 交互脏区、动画脏区、结构签名变化与浮层存在与否；输出是决策本身与一块持久的
//! 后备缓冲（保留上一全窗帧，供局部帧重建未变区域）。
//!
//! 只依赖 `Rect` 与 `Pixmap`，与控件树的关系仅止于"把树画进子 pixmap"。

use tiny_skia::Pixmap;

use crate::core::DamageReq;
use crate::geometry::{Color, Point, Rect, Size};
use crate::render::{Paint, RenderTarget, SkiaCanvas};

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
        // 「上一帧的画面还在不在」是局部重绘的前提，两条后端各有各的落点：软后端是宿主
        // 维护的后备 `Pixmap`（`back_ok`），GPU 后端是目标自己的常驻色纹理
        // （`supports_partial`，见 `render/gpu/surface.rs` 的 `BackBuffer`）。d2d 两者都没有，
        // 恒 false → 恒整窗，与此前的行为逐字相同。
        let partial_ok = if target.as_pixmap().is_some() {
            back_ok
        } else {
            target.supports_partial()
        };
        let do_full =
            self.damage.needs_full || !partial_ok || overlay || !scale_ok || !damage_small;
        self.damage.needs_full = false;
        #[cfg(test)]
        {
            self.damage.last_frame_full = do_full;
        }
        (do_full, damage)
    }

    /// 下一帧**预计**的脏区（逻辑坐标）；`None` = 预计整窗。
    ///
    /// 供平台收窄窗口失效区（见 `AppHandler::pending_damage`）。只是预测：真正的
    /// 局部/整窗判定在 `decide_repaint`，它还会看后备缓冲、浮层、DPI 等条件。
    pub(super) fn next_frame_damage(&self) -> Option<Rect> {
        if self.damage.needs_full || self.damage.needs_relayout {
            return None;
        }
        match (self.damage.pending, self.damage.event) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (Some(r), None) | (None, Some(r)) => Some(r),
            (None, None) => None,
        }
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
        // 续帧请求与脏区、重排请求同源同期：都是本帧绘制中控件写进 `anim` 线程全局态的
        // 东西，都必须在下一帧 `reset_request` 抹掉它们之前收进本宿主。少收这一样，
        // 多窗口下就是"另一个窗口的帧把我的动画请求清了"（见 `UiHost::wants_anim`）。
        self.wants_anim = crate::anim::animation_requested();
        // 续帧的**截止**同样是本帧的产物，与请求位一起收割。饱和到 u32：光标那点周期
        // 远够用，而真出现天文数字（静态风格误入此路）时截成上限只是多睡一会儿。
        self.next_delay = crate::anim::next_frame_delay_ms()
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;
    }

    /// 局部重绘：把脏区渲染进脏区大小的子 pixmap（tiny-skia 按 pixmap 边界自动剔除框外
    /// 图元，成本降到脏区面积），合成进后备缓冲，再整窗拷给平台 pixmap。复用上一全窗帧的
    /// 布局（当前动画均为视觉位移、不改布局）。
    /// 脏区规整（两条局部路径共用）：外扩 AA 余量、对齐到 4 逻辑像素网格、钳回窗口。
    ///
    /// 网格对齐是软路径的硬需求：Windows DPI 缩放恒为 25% 的倍数（scale = m/4），4 的
    /// 倍数 ×scale 必为整数，子 pixmap 的物理原点 `dmg.origin × scale` 于是精确无取整，
    /// 文字定位与全窗帧逐像素一致——不对齐的症状是局部帧的纵向 1px 抖动。GPU 路径画在
    /// 绝对坐标上，没有这个问题，但两条路径的脏区口径保持一致更好对账。
    fn align_damage(&self, damage: Rect) -> Rect {
        let raw = damage
            .inflate(DAMAGE_MARGIN)
            .intersect(&Rect::from_size(self.logical_size));
        const GRID: i32 = 4;
        let x0 = raw.x - raw.x.rem_euclid(GRID);
        let y0 = raw.y - raw.y.rem_euclid(GRID);
        let x1 = raw.right() + (GRID - raw.right().rem_euclid(GRID)) % GRID;
        let y1 = raw.bottom() + (GRID - raw.bottom().rem_euclid(GRID)) % GRID;
        Rect::new(x0, y0, x1 - x0, y1 - y0).intersect(&Rect::from_size(self.logical_size))
    }

    /// GPU 后端的局部重绘：画在目标的常驻色纹理上，范围由 scissor（片元）与
    /// `Canvas::cull_rect`（CPU 侧的节点剔除）两头收窄。
    ///
    /// 与软路径的结构差异只有一处：软路径要开一张脏区大小的子 pixmap 再合成回后备缓冲
    /// （因而绘制带一个原点偏移），GPU 直接画在绝对坐标的常驻纹理上，没有子目标也没有
    /// 合成。相同的那一处是**脏区铺底**：常驻纹理里留着上一帧的像素，不铺底的话半透明
    /// 图元会叠在旧内容上（对应软路径子 pixmap 的 `fill(bg)`）。
    pub(super) fn render_partial_gpu(
        &mut self,
        target: &mut dyn RenderTarget,
        size: Size,
        s: f32,
        damage: Rect,
    ) {
        let dmg = self.align_damage(damage);
        let pdmg = dmg.scaled(s).intersect(&Rect::new(0, 0, size.w, size.h));
        if pdmg.is_empty() {
            // 本帧没有实际可绘区域：常驻纹理保持上一帧内容，present 照样把它拷上屏。
            self.last_present = Some(pdmg);
            return;
        }
        target.begin_damage(Some(pdmg), self.bg);
        {
            let mut canvas = target.make_canvas(&mut self.engine, s);
            // 脏区铺底要的是**替换**，不是叠加：常驻纹理里留着上一帧的像素，而软后端那条
            // 路走的是 `sub.fill(bg)`（覆盖）、GPU 整窗帧走的是 `LoadOp::Clear`（覆盖）。
            // 这里只能经图元管线，而它恒是预乘 over 混合——`bg` 若带透明度，每个局部帧
            // 就会把底色再叠一层到旧像素上，动的那个元素在脏区里拖出残影，而窗口其余部分
            // （由整窗帧重画）看着正常。
            //
            // 故显式取不透明的那一份：`a = 255` 时 over 恰好退化成覆盖。丢掉 alpha 是安全的
            // ——窗口的合成模式是 `Opaque`（见 `render/gpu/surface.rs`），drawable 的 alpha
            // 本就被窗口系统忽略。
            let opaque_bg = Color::rgb(self.bg.r, self.bg.g, self.bg.b);
            canvas.fill_rect(
                dmg.x as f32,
                dmg.y as f32,
                dmg.w as f32,
                dmg.h as f32,
                &Paint::fill(opaque_bg),
            );
            self.tree.paint(&mut *canvas);
        }
        self.last_present = Some(pdmg);
    }

    pub(super) fn render_partial(&mut self, pixmap: &mut Pixmap, size: Size, s: f32, damage: Rect) {
        let dmg = self.align_damage(damage);
        // 物理化并钳到 pixmap 边界。
        let pdmg = dmg.scaled(s).intersect(&Rect::new(0, 0, size.w, size.h));
        if pdmg.is_empty() {
            // 本帧没有实际可绘区域：pixmap 保持上一帧内容，平台无需重新上传任何一行。
            self.last_present = Some(pdmg);
            return;
        }
        // 子 pixmap：脏区大小，按窗口背景填底（与全窗帧平台 fill 同色，重建一致）。
        let Some(mut sub) = Pixmap::new(pdmg.w as u32, pdmg.h as u32) else {
            // 分配失败：退回整窗拷贝（正确优先），并让平台整窗上传。
            self.blit_back_to(pixmap);
            self.last_present = None;
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
        // 合成进后备缓冲（脏区物理原点），再把这一块拷给平台 pixmap。
        if let Some(back) = self.damage.back.as_mut() {
            blit(&sub, back, pdmg.x, pdmg.y);
        }
        self.blit_back_rect_to(pixmap, pdmg);
        self.last_present = Some(pdmg);
    }

    /// 把后备缓冲整窗拷入 pixmap（两者同尺寸时）。
    fn blit_back_to(&self, pixmap: &mut Pixmap) {
        if let Some(back) = self.damage.back.as_ref() {
            if back.width() == pixmap.width() && back.height() == pixmap.height() {
                pixmap.data_mut().copy_from_slice(back.data());
            }
        }
    }

    /// 只把后备缓冲的 `r`（物理像素）拷入 pixmap。
    ///
    /// 局部帧此前整窗拷贝：520×700 的窗口每帧 1.4MB 内存搬运，而实际变的可能只有
    /// 光标那 4×32。pixmap 在帧间由平台复用，框外保持上一帧内容——正因如此，平台侧
    /// 的 R/B 交换与上传也必须同样只做这一块（见 `AppHandler::last_frame_damage`）。
    fn blit_back_rect_to(&self, pixmap: &mut Pixmap, r: Rect) {
        let Some(back) = self.damage.back.as_ref() else {
            return;
        };
        let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
        if back.width() as i32 != w || back.height() as i32 != h {
            return;
        }
        let r = r.intersect(&Rect::new(0, 0, w, h));
        if r.is_empty() {
            return;
        }
        let (src, dst) = (back.data(), pixmap.data_mut());
        let row_bytes = (r.w * 4) as usize;
        for y in r.y..r.bottom() {
            let off = ((y * w + r.x) * 4) as usize;
            dst[off..off + row_bytes].copy_from_slice(&src[off..off + row_bytes]);
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

    /// GPU 局部帧的脏区铺底必须是**替换**，不是把底色叠在上一帧的像素上。
    ///
    /// 软后端那条路走 `sub.fill(bg)`（覆盖）、GPU 整窗帧走 `LoadOp::Clear`（覆盖），而
    /// 局部帧只能经图元管线，它恒是预乘 over 混合。`theme.palette.bg` 是公开的 `Color`,
    /// 可以带透明度——那时"叠加"会让脏区每帧更深一层，动的那个元素在脏区里拖出残影,
    /// 而窗口其余部分（由整窗帧重画）看着完全正常，最难查的那种。
    ///
    /// 判据取**幂等**：连做两个局部帧，脏区像素必须一模一样。叠加语义下第二帧一定更深,
    /// 而替换语义下第二帧与第一帧逐字节相同。
    #[cfg(feature = "gpu")]
    #[test]
    fn gpu_partial_frame_replaces_the_dirty_rect_rather_than_blending_over_it() {
        use crate::geometry::Color;
        use crate::platform::AppHandler;
        use crate::render::gpu::OffscreenGpu;
        let Some(mut off) = OffscreenGpu::new(60, 60) else {
            println!("跳过：本机没有可用的 wgpu 适配器，GPU 局部帧铺底判据未执行");
            return;
        };
        // 半透明底色：不透明的 bg 下 over 恰好退化成覆盖，这条判据就测不到东西了。
        let mut theme = crate::theme::Theme::default();
        theme.palette.bg = Color::rgba(0, 0, 255, 128);
        let app = App::new("t", 60, 60)
            .theme(theme)
            .content(Element::col().width(60).height(60));
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);

        // 首帧：整窗，把常驻纹理铺满底色。
        {
            let mut t = off.target();
            handler.render(&mut t, Size::new(60, 60));
        }
        assert!(handler.damage.last_frame_full, "首帧应为全窗");

        let mut shot = Vec::new();
        for i in 0..2 {
            handler.damage.event = Some(Rect::new(10, 10, 12, 12));
            {
                let mut t = off.target();
                handler.render(&mut t, Size::new(60, 60));
            }
            assert!(
                !handler.damage.last_frame_full,
                "第 {i} 个局部帧被升成了整窗"
            );
            let pm = off.readback().expect("readback");
            let px = {
                let i = ((16 * 60 + 16) * 4) as usize;
                let d = pm.data();
                [d[i], d[i + 1], d[i + 2], d[i + 3]]
            };
            shot.push(px);
        }
        assert_eq!(
            shot[0], shot[1],
            "两个局部帧的脏区像素应完全相同（铺底成了叠加？{:?} → {:?}）",
            shot[0], shot[1]
        );
    }

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

    /// P2-3 回归：`fg_role_signal` 换色必须**升整窗**，走**键盘**路径。
    ///
    /// 为什么必须是键盘：`call_on_event` 对事件期内的信号写入按事件类型分级——指针
    /// Down/Up 升 `DamageReq::Layout`（`apply_damage` 直接置 needs_full，覆盖所有读者），
    /// 但 Key 只给 `DamageReq::Rect`，刻意保留局部重绘"避免打字时整窗卡顿"。于是键盘
    /// 路径上的换色是唯一漏得掉的：`on_submit` 里把回执改成绿色，那一帧只重画输入框，
    /// 回执那行字保持旧色不动——无 panic 无告警，要等下次凑巧整窗才变过来。
    ///
    /// 解法沿用 `own_enabled`（置灰同属"布局不变但像素变了"）的既有范式：把生效的前景
    /// 角色折进布局签名，重排后签名不等即自动升整窗，无需为它单开特例分支。
    #[test]
    fn fg_role_signal_change_upgrades_to_full_repaint_on_key_path() {
        use crate::app::test_support::key_ev;
        use crate::event::Key;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use crate::style::Role;
        let tone = crate::signal::signal(Role::TextMuted);
        let text = crate::signal::signal(String::new());
        let app = App::new("t", 160, 100).content(
            Element::col()
                .width(160)
                .height(100)
                .child(
                    Element::text_input(text, "查词…")
                        .height(30)
                        .autofocus()
                        // Enter 提交 → 把回执改成成功色。这是 wind-dict 那个场景的最小复刻。
                        .on_submit(move |_| tone.set(Role::Success)),
                )
                // 改色的是**另一个**节点：Key 事件的局部脏区只覆盖输入框，正是漏画的来源。
                .child(Element::label("回执").fg_role_signal(tone)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(160, 100).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(160, 100))
            };
        }
        frame!(); // 首帧兑现 autofocus 并种入后备缓冲

        // 前提一：普通打字（不改颜色信号）确实走局部——否则本测试等于什么都没验。
        let k = key_ev();
        handler.on_key(k(Key::Char('a')));
        frame!();
        assert!(
            !handler.damage.last_frame_full,
            "前提：不改颜色的按键应走局部重绘（这条不成立则下面的断言没有意义）"
        );

        // Enter → on_submit 改颜色信号。
        handler.on_key(k(Key::Enter));
        assert_eq!(tone.get(), Role::Success, "前提：Enter 应触发 on_submit");
        frame!();
        assert!(
            handler.damage.last_frame_full,
            "换色帧必须整窗——键盘路径的信号写入只给局部脏区，会把改了色的那行字漏掉"
        );
    }
}
