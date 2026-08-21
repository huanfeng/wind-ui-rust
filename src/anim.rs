//! 动画驱动与补间。
//!
//! 两块：
//! - **帧驱动**（既有）：控件在 paint 中调 [`request_repaint`] 表示"我在动画中，需要下一帧"；
//!   宿主每帧绘制前 [`reset_request`]，绘制后读 [`animation_requested`] 决定是否继续驱动帧。
//!   平台据此选唤醒机制（Win32 带超时的消息等待），无动画时回阻塞空闲（零 CPU）。
//!   控件用 [`clock_ms`] 取当前帧单调时钟算动画相位。
//!   续帧请求还带一个**截止时间**（[`next_frame_delay_ms`]）：控件可以说"我 530ms 后才需要
//!   下一帧"，平台据此把唤醒推迟到那时——见 [`request_repaint_in_after`]。
//! - **补间**（新增）：[`Easing`] 缓动曲线 + [`Transition`] 时间补间助手 + [`enabled`] 全局开关。
//!   控件把 `Cell<Transition<T>>` 当字段持有，paint 内据状态 `retarget` 并 `animate()` 取值
//!   （详见模块顶部"接入模式"）。全局关闭时所有补间瞬时收敛到目标值、不再续帧。

use std::cell::Cell;

use crate::geometry::{Color, Rect};

thread_local! {
    static REQUEST: Cell<bool> = const { Cell::new(false) };
    static CLOCK_MS: Cell<u64> = const { Cell::new(0) };
    /// 全局动画开关：false 时所有补间瞬时收敛（尊重系统"显示动画"/省电/无障碍）。
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    /// 当前正在绘制的节点矩形（逻辑坐标），由 paint 遍历每节点设置。
    /// `request_repaint` 据此把脏区归到该节点；为 None 时（节点外调用）标记整窗脏。
    static PAINT_RECT: Cell<Option<Rect>> = const { Cell::new(None) };
    /// 本帧累积的脏区矩形（逻辑坐标，各动画节点并集）。
    static DAMAGE: Cell<Option<Rect>> = const { Cell::new(None) };
    /// 本帧是否需整窗重绘（节点外的 request_repaint，或无法局部化的情况）。
    static DAMAGE_FULL: Cell<bool> = const { Cell::new(false) };
    /// 本帧是否有控件请求「下一帧重排」（布局动画：高度补间等每帧改变几何）。
    static RELAYOUT: Cell<bool> = const { Cell::new(false) };
    /// 本帧续帧请求里**最早**的那个截止（距本帧的毫秒数）。`None` = 本帧没有续帧请求。
    /// 取最小值：任何一个控件说"我马上要"，整窗就得按刷新率走。
    static NEXT_DELAY: Cell<Option<u64>> = const { Cell::new(None) };
}

/// 记一笔续帧截止（毫秒），与本帧已有的取较早者。
fn note_delay(delay_ms: u64) {
    NEXT_DELAY.with(|c| {
        c.set(Some(match c.get() {
            Some(cur) => cur.min(delay_ms),
            None => delay_ms,
        }))
    });
}

/// 本帧动画脏区。`Full`=需整窗重绘；`Rect`=仅该区域；`None`=无动画脏区。
pub(crate) enum Damage {
    None,
    Rect(Rect),
    Full,
}

/// 控件请求持续动画（在 paint 内调用）。宿主据此驱动下一帧，并把脏区归到当前绘制节点。
pub fn request_repaint() {
    REQUEST.with(|c| c.set(true));
    note_delay(0);
    match PAINT_RECT.with(|c| c.get()) {
        Some(r) => DAMAGE.with(|d| {
            let merged = match d.get() {
                Some(cur) => cur.union(&r),
                None => r,
            };
            d.set(Some(merged));
        }),
        None => DAMAGE_FULL.with(|c| c.set(true)),
    }
}

/// 控件请求持续动画，但**脏区自报**（paint 内调用）：用于视觉变化远小于节点的动画，
/// 典型是文本光标闪烁——按节点算脏区会把整个输入框每帧重绘一遍，按光标条算只重绘
/// 十几像素宽的一条。
///
/// `r` 是**逻辑绝对坐标**，且必须覆盖本帧与上一帧视觉的并集：局部帧以脏区为裁剪重绘
/// 整棵树（见 `app::damage::render_partial`），报小了不会画错内容，只会把没进脏区的
/// 那部分留成上一帧的残影。
pub fn request_repaint_in(r: Rect) {
    request_repaint_in_after(r, 0);
}

/// 同 [`request_repaint_in`]，但附带**截止时间**：`delay_ms` 毫秒之内画面不会变，平台可以
/// 一直睡到那时再唤醒。
///
/// 这是「定时动画」与「连续动画」的分水岭。方波闪烁的光标每 530ms 才翻一次面，此前 31 帧
/// 画出来的像素与上一帧完全相同——却照样把整条流水线（唤醒、遍历、光栅、上传）跑一遍。
/// 报出截止后，它一个周期只出两帧，代价从"按刷新率续帧"塌回接近静态光标
/// （[`CaretStyle::Solid`](crate::ui::CaretStyle::Solid)）。渐变类动画每帧 alpha 都在变，
/// 仍传 0。
///
/// **报早了只是白付几帧；报晚了会让动画卡在旧画面上**，所以拿不准就传 0。宿主取本帧所有
/// 请求里最早的那个（见 [`next_frame_delay_ms`]）：同窗口里只要还有别的控件在连续动画，
/// 这条截止就被它压回 0，不存在"某个控件把整窗睡过去"的可能。
pub fn request_repaint_in_after(r: Rect, delay_ms: u64) {
    REQUEST.with(|c| c.set(true));
    note_delay(delay_ms);
    DAMAGE.with(|d| {
        let merged = match d.get() {
            Some(cur) => cur.union(&r),
            None => r,
        };
        d.set(Some(merged));
    });
}

/// 控件请求「下一帧重排 + 重绘」（paint 内调用）。供**布局动画**（高度补间等每帧
/// 改变几何的动画）使用：与 `request_repaint` 不同，它让宿主下一帧走
/// `needs_relayout` 正规门——重排后按结构签名升级整窗、并执行 hover 重同步/
/// 隐藏交互复位。若绕过此门直接打整窗脏（measure 期调 `request_repaint` 的效果），
/// 动画期间光标静止悬停的控件不会随内容位移重新判定 hover。
pub fn request_relayout() {
    RELAYOUT.with(|c| c.set(true));
    request_repaint();
}

/// 宿主：取走「下一帧重排」请求（帧收尾时消费）。
pub(crate) fn take_relayout() -> bool {
    RELAYOUT.with(|c| c.replace(false))
}

/// 绘制遍历：进入某节点绘制前设置其矩形（逻辑坐标），使该节点内的 `request_repaint`
/// 把脏区归到此处。传 None 清除（节点外）。
pub(crate) fn set_paint_rect(r: Option<Rect>) {
    PAINT_RECT.with(|c| {
        // 契约：Some 必与后续 None 成对（core::paint_node 每节点 set(Some)→widget.paint→set(None)）。
        // 嵌套未清除会让脏区错归到外层节点；此断言锁死该时序。
        debug_assert!(
            r.is_none() || c.get().is_none(),
            "set_paint_rect 嵌套泄漏：上一节点未清除"
        );
        c.set(r);
    });
}

/// 宿主：取出本帧累积的动画脏区。
pub(crate) fn take_damage() -> Damage {
    if DAMAGE_FULL.with(|c| c.get()) {
        return Damage::Full;
    }
    match DAMAGE.with(|c| c.get()) {
        Some(r) => Damage::Rect(r),
        None => Damage::None,
    }
}

/// 当前单调时钟（毫秒）。控件据此计算动画相位（与挂钟无关，仅用差值）。
///
/// 宿主在**每帧 render 前**与**每次事件分发前**刷新它；两次刷新之间它是冻结的，空闲不出帧
/// 时可停留任意长时间。事件路径里请用 [`crate::core::EventCtx::now_ms`]（同一值，但名字保证
/// 它已被刷新），别把冻结期误当作用户的操作时长。
pub fn clock_ms() -> u64 {
    CLOCK_MS.with(|c| c.get())
}

/// 全局动画是否启用。关闭时补间瞬时收敛。
pub fn enabled() -> bool {
    ENABLED.with(|c| c.get())
}

/// 设置全局动画开关（宿主据系统设置初始化，或运行期切换）。
pub fn set_enabled(on: bool) {
    ENABLED.with(|c| c.set(on));
}

/// 宿主：每帧绘制前清除动画请求与脏区累积。
pub(crate) fn reset_request() {
    REQUEST.with(|c| c.set(false));
    DAMAGE.with(|c| c.set(None));
    DAMAGE_FULL.with(|c| c.set(false));
    PAINT_RECT.with(|c| c.set(None));
    RELAYOUT.with(|c| c.set(false));
    NEXT_DELAY.with(|c| c.set(None));
}

/// 宿主/平台：本帧是否有控件请求了动画。
pub(crate) fn animation_requested() -> bool {
    REQUEST.with(|c| c.get())
}

/// 宿主：本帧续帧请求里最早的截止（距本帧的毫秒数）。`None` = 本帧无续帧请求。
///
/// `Some(0)` 与 `None` 是**两回事**：前者是"下一帧就要"，后者是"不需要下一帧"。
/// 平台先看 [`animation_requested`] 决定要不要驱动帧，再看这里决定隔多久驱动。
pub(crate) fn next_frame_delay_ms() -> Option<u64> {
    NEXT_DELAY.with(|c| c.get())
}

/// 宿主：设置当前帧时钟。
pub(crate) fn set_clock_ms(v: u64) {
    CLOCK_MS.with(|c| c.set(v));
}

/// 缓动曲线：把线性进度 `t∈[0,1]` 映射为缓动进度 `∈[0,1]`（端点恒为 0/1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Easing {
    /// 匀速。
    Linear,
    /// 三次缓入（慢起快收）。
    EaseIn,
    /// 三次缓出（快起慢收）。
    EaseOut,
    /// 三次缓入缓出（两端慢中间快），UI 默认。
    #[default]
    EaseInOut,
}

impl Easing {
    /// 应用缓动。输入 `t` 与输出均钳到 `[0,1]`（输出钳制兜底浮点过冲，保 Lerp 不越界）。
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        let r = match self {
            Easing::Linear => t,
            Easing::EaseIn => t * t * t,
            Easing::EaseOut => {
                let u = 1.0 - t;
                1.0 - u * u * u
            }
            Easing::EaseInOut => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    let u = -2.0 * t + 2.0;
                    1.0 - u * u * u / 2.0
                }
            }
        };
        r.clamp(0.0, 1.0)
    }
}

/// 可线性插值的值（补间货币）。`t∈[0,1]`，`t=0` 返回 self、`t=1` 返回 to。
pub trait Lerp: Copy {
    fn lerp(self, to: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(self, to: f32, t: f32) -> f32 {
        self + (to - self) * t
    }
}

impl Lerp for Color {
    fn lerp(self, to: Color, t: f32) -> Color {
        let mix = |a: u8, b: u8| (a as f32).lerp(b as f32, t).round().clamp(0.0, 255.0) as u8;
        Color::rgba(
            mix(self.r, to.r),
            mix(self.g, to.g),
            mix(self.b, to.b),
            mix(self.a, to.a),
        )
    }
}

/// 时间补间：在 `duration_ms` 内沿 `easing` 从 `from` 过渡到 `to`。
///
/// 控件把它存进 `Cell`（本类型 `Copy`），paint 内据当前状态 [`retarget`](Self::retarget)
/// 到新目标、再 [`animate`](Self::animate) 取值。读取走 [`clock_ms`]，故须在宿主已注入帧
/// 时钟的 paint 期使用。全局动画关闭（[`enabled`] 为 false）时一律返回目标值、不视为活跃。
#[derive(Debug, Clone, Copy)]
pub struct Transition<T: Lerp> {
    from: T,
    to: T,
    start_ms: u64,
    duration_ms: u32,
    easing: Easing,
}

impl<T: Lerp> Transition<T> {
    /// 构造一个静止于 `value` 的补间（无动画）。
    pub fn new(value: T) -> Self {
        Self {
            from: value,
            to: value,
            start_ms: 0,
            duration_ms: 0,
            easing: Easing::default(),
        }
    }

    /// 当前目标值。
    pub fn target(&self) -> T {
        self.to
    }

    /// 改向新目标：从**当前值**起、用 `clock_ms()` 作起点开始 `duration_ms` 的过渡。
    /// 全局动画关闭或时长为 0 时直接落定到 `to`（下次取值即终值）。
    pub fn retarget(&mut self, to: T, duration_ms: u32, easing: Easing) {
        let now = self.value();
        self.from = now;
        self.to = to;
        self.start_ms = clock_ms();
        self.duration_ms = if enabled() { duration_ms } else { 0 };
        self.easing = easing;
    }

    /// 当前插值。全局关闭/时长 0/已结束 → `to`；否则按 `clock_ms()` 相位插值。
    pub fn value(&self) -> T {
        if !enabled() || self.duration_ms == 0 {
            return self.to;
        }
        let elapsed = clock_ms().saturating_sub(self.start_ms);
        if elapsed >= self.duration_ms as u64 {
            return self.to;
        }
        let t = elapsed as f32 / self.duration_ms as f32;
        self.from.lerp(self.to, self.easing.apply(t))
    }

    /// 是否仍在过渡中（据 `clock_ms()`）。全局关闭恒 false。
    pub fn is_active(&self) -> bool {
        if !enabled() || self.duration_ms == 0 {
            return false;
        }
        clock_ms().saturating_sub(self.start_ms) < self.duration_ms as u64
    }

    /// 取当前值，并在仍活跃时请求下一帧（控件 paint 内的常用入口）。
    pub fn animate(&self) -> T {
        let v = self.value();
        if self.is_active() {
            request_repaint();
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试夹具：设定全局开关 + 帧时钟到确定值。
    fn set_clock(ms: u64) {
        set_clock_ms(ms);
    }

    #[test]
    fn easing_endpoints_are_fixed() {
        for e in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            assert!((e.apply(0.0) - 0.0).abs() < 1e-6, "{e:?} 在 0 应为 0");
            assert!((e.apply(1.0) - 1.0).abs() < 1e-6, "{e:?} 在 1 应为 1");
            // 越界钳制。
            assert_eq!(e.apply(-1.0), e.apply(0.0));
            assert_eq!(e.apply(2.0), e.apply(1.0));
        }
    }

    #[test]
    fn easing_is_monotonic_increasing() {
        for e in [
            Easing::Linear,
            Easing::EaseIn,
            Easing::EaseOut,
            Easing::EaseInOut,
        ] {
            let mut prev = e.apply(0.0);
            for i in 1..=20 {
                let v = e.apply(i as f32 / 20.0);
                assert!(v >= prev - 1e-6, "{e:?} 应单调非减");
                prev = v;
            }
        }
    }

    #[test]
    fn f32_and_color_lerp() {
        assert_eq!(0.0f32.lerp(10.0, 0.5), 5.0);
        let c = Color::rgb(0, 0, 0).lerp(Color::rgb(255, 100, 50), 0.5);
        assert_eq!((c.r, c.g, c.b), (128, 50, 25));
    }

    #[test]
    fn transition_interpolates_and_settles() {
        set_enabled(true);
        set_clock(1000);
        let mut tr = Transition::new(0.0f32);
        tr.retarget(100.0, 200, Easing::Linear);
        set_clock(1000); // 起点
        assert_eq!(tr.value(), 0.0);
        set_clock(1100); // 半程，线性 → 50
        assert!(
            (tr.value() - 50.0).abs() < 1e-3,
            "半程应约 50，实得 {}",
            tr.value()
        );
        assert!(tr.is_active());
        set_clock(1200); // 终点
        assert_eq!(tr.value(), 100.0);
        assert!(!tr.is_active(), "结束后不应活跃");
        set_clock(5000); // 远超
        assert_eq!(tr.value(), 100.0);
    }

    #[test]
    fn retarget_midflight_starts_from_current() {
        set_enabled(true);
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget(100.0, 200, Easing::Linear);
        set_clock(100); // 半程 = 50
        assert!((tr.value() - 50.0).abs() < 1e-3);
        tr.retarget(0.0, 200, Easing::Linear); // 从 50 改回 0
        assert!((tr.value() - 50.0).abs() < 1e-3, "改向瞬间应保持当前值 50");
        set_clock(200); // 新过渡半程：50 → 0 的一半 = 25
        assert!(
            (tr.value() - 25.0).abs() < 1e-3,
            "改向半程应约 25，实得 {}",
            tr.value()
        );
    }

    /// 截止时间取本帧所有请求里**最早**的那个：一个控件说"我可以睡 530ms"，另一个说
    /// "我下一帧就要"，整窗必须按后者走。反过来才是 bug——那会让连续动画掉到 2fps。
    #[test]
    fn next_frame_delay_takes_the_earliest() {
        let r = Rect::new(0, 0, 10, 10);
        reset_request();
        assert_eq!(next_frame_delay_ms(), None, "没人请求时应为 None");
        assert!(!animation_requested());

        request_repaint_in_after(r, 530);
        assert_eq!(next_frame_delay_ms(), Some(530));
        request_repaint_in_after(r, 90);
        assert_eq!(next_frame_delay_ms(), Some(90), "应取较早者");
        request_repaint_in_after(r, 800);
        assert_eq!(next_frame_delay_ms(), Some(90), "较晚的请求不得推迟截止");

        // 任何一个"马上要"都把整窗压回满帧。
        request_repaint_in(r);
        assert_eq!(next_frame_delay_ms(), Some(0));

        // Some(0) 与 None 语义不同：前者要下一帧，后者不需要帧。
        reset_request();
        assert_eq!(next_frame_delay_ms(), None);
        request_repaint_in_after(r, 0);
        assert!(animation_requested());
        assert_eq!(next_frame_delay_ms(), Some(0));
        reset_request();
    }

    /// 节点级 `request_repaint`（补间 `animate()` 走这条）恒为"下一帧就要"：它不知道
    /// 自己的曲线什么时候才变，只能按最保守的报。
    #[test]
    fn node_level_repaint_is_immediate() {
        reset_request();
        set_enabled(true);
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget(100.0, 200, Easing::Linear);
        set_clock(100);
        tr.animate();
        assert!(animation_requested());
        assert_eq!(next_frame_delay_ms(), Some(0), "补间进行中应每帧都要");
        reset_request();
    }

    #[test]
    fn disabled_snaps_instantly() {
        set_enabled(false);
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget(100.0, 200, Easing::EaseInOut);
        set_clock(0);
        assert_eq!(tr.value(), 100.0, "全局关闭应瞬时到目标");
        assert!(!tr.is_active());
        set_enabled(true); // 复位本线程开关（thread-local，本不会污染他测，复位求稳）
    }
}
