//! 文本插入光标（caret）：静态外观、闪烁相位与平滑移动。
//!
//! 由 [`TextInput`](crate::ui::TextInput) 单/多行（数字步进 `Element::stepper` 内嵌的也是它）
//! 编辑态共用，保证同一个应用里所有可编辑处的光标观感一致。
//!
//! 三件事：
//! - **相位**：[`CaretStyle`] 把「时间」映射成 alpha 曲线。打字/移动光标后调
//!   [`CaretState::bump`] 重置相位并保持实心 [`TYPING_PAUSE_MS`]——正在输入时光标不该
//!   闪，闪烁只是"我在这儿等你"的空闲提示。
//! - **移动**：同一视觉行内的位置变化用 [`MOVE_MS`] 缓出补间滑过去；换行、改行高、
//!   首次出现则瞬移（跨行滑行会让光标斜着飞越文字，观感是拖沓而非流畅）。
//! - **续帧与脏区**：paint 内自报脏区续帧（[`crate::anim::request_repaint_in_after`]），
//!   脏区取「上一帧 ∪ 本帧」光标矩形，故闪烁每帧只重绘光标附近十几像素宽的一条，
//!   而不是整个输入框。续帧还带**截止时间**（[`next_change_ms`]）：方波风格报出"下次翻面
//!   在 530ms 后"，帧驱动就一直睡到那时，一个半周期只出两帧而不是三十几帧。
//!
//! **闪烁与「客户区动画」是两个开关**。系统的减弱动态效果（Windows
//! `SPI_GETCLIENTAREAANIMATION`、macOS 减弱动态效果）管的是窗口/控件过渡，插入符闪烁另有
//! 其设置（Windows `GetCaretBlinkTime`、macOS `NSTextInsertionPointBlinkPeriod`）——系统
//! 自带的输入框在关掉客户区动画后插入符照样闪。故：
//! - **闪烁**看 [`blink_period_ms`]（由宿主启动时从平台注入；`None` = 系统要求不闪），
//!   周期也跟着它走，与 [`crate::anim::enabled`] 无关。
//! - **平滑移动**是过渡动画，归 [`crate::anim::enabled`] 管，关掉即瞬移。
//! - [`set_animated`] 是总闸（截图/视觉回归用）：关掉则恒实心、不滑行、不续帧。
//! - **窗口失活**（[`set_window_active`]）同样转静态：后台窗口没有理由按刷新率出帧。

use std::cell::Cell;

use serde::{Deserialize, Serialize};

use crate::anim::{self, Easing, Transition};
use crate::geometry::{Color, Rect};
use crate::render::{Canvas, Paint};
use crate::theme::{InputTheme, Len};

/// 交互后保持实心的时长（ms）：其间不闪，之后才起相位。
pub const TYPING_PAUSE_MS: u64 = 500;
/// 默认闪烁半周期（ms）：平台查不到时的回退值，取 Windows `GetCaretBlinkTime` 的默认 530。
pub const BLINK_HALF_MS: u64 = 530;
/// 平滑呼吸里「端点保持」占半周期的比例（千分数）。其余是单向淡变。
/// 530ms 半周期下即 130ms 保持 + 400ms 淡变——与手调那版一致。
const SMOOTH_HOLD_PERMILLE: u64 = 245;

/// 平滑呼吸的分段：`(端点保持, 单向淡变)`，随半周期等比缩放。
fn smooth_segments(half_ms: u64) -> (u64, u64) {
    let hold = half_ms * SMOOTH_HOLD_PERMILLE / 1000;
    (hold, half_ms.saturating_sub(hold))
}
/// 半明半暗风格的暗端 alpha：不熄灭，保证任意时刻都能看见光标。
const PHASE_MIN_ALPHA: f32 = 0.35;
/// 平滑移动时长（ms）。够短，滑行读起来是"跟手"而不是"追不上"。
pub const MOVE_MS: u32 = 90;
/// 默认光标宽度（逻辑 px，绘制时向下吸附到整数物理像素）。
pub const DEFAULT_WIDTH: f32 = 2.0;
/// alpha 低于此值视作不可见，跳过绘制（同时省掉反色重绘的一次排版）。
const ALPHA_EPS: f32 = 0.004;

thread_local! {
    /// 光标动画总开关：false 时恒实心、不滑行、不续帧。截图路径关掉它取确定性画面。
    static ANIMATED: Cell<bool> = const { Cell::new(true) };
    /// 闪烁半周期（ms）；`None` = 系统/应用要求不闪。宿主启动时从平台注入。
    static BLINK_HALF: Cell<Option<u32>> = const { Cell::new(Some(BLINK_HALF_MS as u32)) };
    /// 所属窗口是否处于激活（前台）态。宿主每帧绘制前注入。
    static WINDOW_ACTIVE: Cell<bool> = const { Cell::new(true) };
}

/// 当前窗口是否激活（前台）。
pub fn window_active() -> bool {
    WINDOW_ACTIVE.with(|c| c.get())
}

/// 宿主每帧绘制前注入所属窗口的激活态。
///
/// 失活窗口的光标转为静态：既是系统观感（Windows 失活时插入符本就不显示，macOS 也不闪），
/// 也是省电——后台窗口没有任何理由按刷新率出帧。多窗口下靠"每帧注入"取得正确值，
/// 与帧时钟、主题快照同一套路数。
pub fn set_window_active(active: bool) {
    WINDOW_ACTIVE.with(|c| c.set(active));
}

/// 当前闪烁半周期（ms）。`None` 表示不闪（用户在系统里关了插入符闪烁，或应用显式关闭）。
pub fn blink_period_ms() -> Option<u32> {
    BLINK_HALF.with(|c| c.get())
}

/// 设置闪烁半周期。宿主启动时调用一次，值取自平台的**插入符**设置
/// （`GetCaretBlinkTime` / `NSTextInsertionPointBlinkPeriod`），不是客户区动画开关。
///
/// 传 `None` 表示不闪：光标恒实心且不请求续帧。
pub fn set_blink_period_ms(half_ms: Option<u32>) {
    BLINK_HALF.with(|c| c.set(half_ms.filter(|v| *v > 0)));
}

/// 光标动画是否启用（默认 true）。
pub fn animated() -> bool {
    ANIMATED.with(|c| c.get())
}

/// 设置光标动画总开关。
///
/// 截图/视觉回归路径关掉它：闪烁相位取决于真实时钟，开着会让同一界面每次截出的
/// 光标忽有忽无，比对哈希永远不稳。
pub fn set_animated(on: bool) {
    ANIMATED.with(|c| c.set(on));
}

/// 光标闪烁风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaretStyle {
    /// 静态实心，不闪（零续帧开销）。
    Solid,
    /// 经典方波（默认）：亮/灭各半周期硬切换，与系统插入符一致。
    ///
    /// 之所以是默认而非 [`Smooth`](Self::Smooth)：它只有两个状态，半周期内画面恒定，
    /// 于是能报出"下次翻面在什么时候"（[`next_change_ms`]）让帧驱动睡过去——实测同一
    /// 界面 0.3% 单核，而渐变风格每帧 alpha 都在变、只能按刷新率出帧，要 2.0%。
    /// 观感上它也是各系统文本框几十年的样子。要更柔和的过渡就切 `Smooth`。
    #[default]
    Blink,
    /// 平滑呼吸：缓入缓出地淡入淡出，两端各驻留一小段。每帧 alpha 都在变，代价最高。
    Smooth,
    /// 半明半暗：alpha 在 1.0 与 [`PHASE_MIN_ALPHA`] 间正弦起伏，从不熄灭，识别度最高。
    Phase,
}

impl CaretStyle {
    /// 是否是随时间变化的风格（决定要不要续帧）。
    pub fn animated(self) -> bool {
        !matches!(self, CaretStyle::Solid)
    }
}

/// 光标绘制参数。由 [`CaretOpts::from_theme`] 从主题取，或手工构造。
#[derive(Debug, Clone, Copy)]
pub struct CaretOpts {
    pub style: CaretStyle,
    /// 宽度。`Dp` 随 DPI 等比放大、`Physical { px }` 恒为固定物理像素；
    /// 两者最终都会**向下吸附到整数物理像素**（见 [`CaretState::paint`]）。
    pub width: Len,
    /// 两端半圆（radius = width/2）。
    pub rounded: bool,
    /// 同行内位置变化时滑行过去。
    pub smooth_move: bool,
}

impl Default for CaretOpts {
    fn default() -> Self {
        Self {
            style: CaretStyle::default(),
            width: Len::Dp(DEFAULT_WIDTH),
            rounded: true,
            smooth_move: true,
        }
    }
}

impl CaretOpts {
    /// 从输入框主题取（Stepper 编辑态同样读 `theme.input`：光标是全局一致的体验，
    /// 不该因为它长在哪个控件里而变样）。
    pub fn from_theme(inp: &InputTheme) -> Self {
        Self {
            style: inp.caret_style(),
            width: inp.caret_width(),
            rounded: inp.caret_rounded(),
            smooth_move: inp.caret_smooth_move(),
        }
    }
}

/// 给定风格、相位（自暂停结束起算的毫秒数）与半周期，求光标 alpha ∈ [0,1]。
///
/// 纯函数：不读时钟、不碰全局开关，便于直接验曲线。`half_ms` 来自系统插入符设置，
/// 三种动态风格的时序都按它等比缩放——用户把系统闪烁调快，应用里的光标就跟着快。
pub fn alpha_at(style: CaretStyle, phase_ms: u64, half_ms: u64) -> f32 {
    let half = half_ms.max(1);
    match style {
        CaretStyle::Solid => 1.0,
        CaretStyle::Blink => {
            if (phase_ms / half).is_multiple_of(2) {
                1.0
            } else {
                0.0
            }
        }
        CaretStyle::Smooth => {
            let (hold, fade) = smooth_segments(half);
            let t = phase_ms % (2 * half);
            let fade_out_end = hold + fade;
            let dark_end = fade_out_end + hold;
            if t < hold {
                1.0
            } else if t < fade_out_end {
                let u = (t - hold) as f32 / fade.max(1) as f32;
                1.0 - Easing::EaseInOut.apply(u)
            } else if t < dark_end {
                0.0
            } else {
                let u = (t - dark_end) as f32 / fade.max(1) as f32;
                Easing::EaseInOut.apply(u)
            }
        }
        CaretStyle::Phase => {
            let t = (phase_ms % (2 * half)) as f32 / (2 * half) as f32;
            // cos 从 1 起：相位 0 处最亮，与"刚停手时是实心"衔接。
            let wave = 0.5 + 0.5 * (t * std::f32::consts::TAU).cos();
            PHASE_MIN_ALPHA + (1.0 - PHASE_MIN_ALPHA) * wave
        }
    }
}

/// 给定风格与相位，求 [`alpha_at`] 的值**下次变化**还有多少毫秒。
///
/// 与 `alpha_at` 严格配对的另一半：一个答"现在多亮"，一个答"下次变亮/变暗是什么时候"。
/// 有了后者，帧驱动才能从"按刷新率轮询"改成"睡到该变的那一刻"（见
/// [`crate::anim::request_repaint_in_after`]）——方波光标一个周期只需两帧，而不是 60 帧。
///
/// 返回 `0` 表示**连续变化**（每帧都不一样，只能按刷新率出帧）；[`u64::MAX`] 表示永不变化。
/// 两个端点都必须诚实：报晚了光标会卡在旧画面上，报早了只是白付几帧。
pub fn next_change_ms(style: CaretStyle, phase_ms: u64, half_ms: u64) -> u64 {
    let half = half_ms.max(1);
    match style {
        CaretStyle::Solid => u64::MAX,
        // 方波：亮/灭各占半周期，下次翻面在本半周期结束时。
        CaretStyle::Blink => half - phase_ms % half,
        // 平滑呼吸：两端各有一段恒定的"保持"，中间是连续淡变。分支结构与 `alpha_at`
        // 逐条对应——两处若各写各的，迟早在某个边界上一个说"还是 1.0"另一个说"该变了"。
        CaretStyle::Smooth => {
            let (hold, fade) = smooth_segments(half);
            let t = phase_ms % (2 * half);
            let fade_out_end = hold + fade;
            let dark_end = fade_out_end + hold;
            if t < hold {
                hold - t
            } else if t < fade_out_end {
                0
            } else {
                // 暗端保持段 → 睡到保持结束；再往后是淡入段（连续变化）→ 饱和到 0。
                dark_end.saturating_sub(t)
            }
        }
        // 正弦起伏：任意两帧都不同值。
        CaretStyle::Phase => 0,
    }
}

/// 单个光标的运行时状态。控件按 `Cell` 内部可变持有（paint 取 `&self`）。
#[derive(Debug, Default)]
pub struct CaretState {
    /// 最近一次交互的帧时钟（ms）。其后 [`TYPING_PAUSE_MS`] 内保持实心。
    last_activity: Cell<u64>,
    /// x（逻辑绝对坐标）的平滑移动补间。
    x: Cell<Option<Transition<f32>>>,
    /// 上一帧的目标位置 `(x, y_top, height)`：变了即视为一次交互（重置相位），
    /// 其中 `(y_top, height)` 变了还意味着换行 → 瞬移不滑行。
    target: Cell<Option<(i32, i32, i32)>>,
    /// 上一帧实际绘制的矩形：与本帧并集作脏区，滑行/换行不留残影。
    last_rect: Cell<Option<Rect>>,
}

impl CaretState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记一次交互（输入、移动光标、点击定位、获得焦点）：重置闪烁相位，
    /// 之后 [`TYPING_PAUSE_MS`] 内光标保持实心。
    ///
    /// 事件路径传 [`EventCtx::now_ms`](crate::core::EventCtx::now_ms)。
    pub fn bump(&self, now_ms: u64) {
        self.last_activity.set(now_ms);
    }

    /// 丢弃残留状态（失焦/退出编辑态时调）：下次出现按"首次"处理，不从旧位置滑过来。
    pub fn reset(&self) {
        self.x.set(None);
        self.target.set(None);
        self.last_rect.set(None);
    }

    /// 本帧是否该闪（决定 alpha 与要不要续帧）。
    ///
    /// 刻意**不看** [`crate::anim::enabled`]：那是「客户区动画」开关（窗口/控件过渡），
    /// 插入符闪烁在两个系统上都是独立设置。把两者绑在一起的后果是——用户在 Windows
    /// 性能选项里关掉动画后，系统自带输入框照闪，我们的却成了一根死杠。
    fn blink_half(&self, style: CaretStyle) -> Option<u64> {
        if !style.animated() || !animated() || !window_active() {
            return None;
        }
        blink_period_ms().map(|v| v as u64)
    }

    /// 本帧 alpha。不闪（系统要求/总闸关闭/静态风格）→ 恒 1.0。
    fn alpha(&self, style: CaretStyle, now: u64) -> f32 {
        let Some(half) = self.blink_half(style) else {
            return 1.0;
        };
        let since = now.saturating_sub(self.last_activity.get());
        match since.checked_sub(TYPING_PAUSE_MS) {
            // 暂停期内：实心。
            None => 1.0,
            Some(phase) => alpha_at(style, phase, half),
        }
    }

    /// 本帧画面**下次变化**还有多少毫秒（0 = 连续变化，须按刷新率出帧）。
    ///
    /// 比 [`next_change_ms`] 多算一层打字暂停：暂停期内画面恒定，先睡到暂停结束，再加上
    /// 相位 0 处到首次变化的距离。不闪（静态风格/系统关闭/窗口失活）时返回 [`u64::MAX`]，
    /// 不过那种情况下 paint 根本不会续帧。
    ///
    /// 末尾 `+1`：睡到"刚好那一刻"会被时钟粒度磨成差 1ms 醒来，于是画出一模一样的一帧、
    /// 再报 1ms——白付一帧还把翻面拖到下一个帧间隔。多睡 1ms 落在边界之后，稳。
    fn next_change(&self, style: CaretStyle, now: u64) -> u64 {
        let Some(half) = self.blink_half(style) else {
            return u64::MAX;
        };
        let since = now.saturating_sub(self.last_activity.get());
        let d = match since.checked_sub(TYPING_PAUSE_MS) {
            None => (TYPING_PAUSE_MS - since).saturating_add(next_change_ms(style, 0, half)),
            Some(phase) => next_change_ms(style, phase, half),
        };
        // 连续变化的风格保持 0：加了 1 就成了"1ms 后再来"，语义仍是每帧，但白绕一圈。
        if d == 0 {
            0
        } else {
            d.saturating_add(1)
        }
    }

    /// 解析本帧光标 x（含平滑移动），返回 `(x, 是否仍在滑行)`。
    ///
    /// `same_line` 为假（换行/首次出现）时不滑行：跨行滑行会让光标斜着飞越文字。
    fn resolve_x(&self, target_x: i32, same_line: bool, smooth: bool) -> (f32, bool) {
        let smooth = smooth && animated() && anim::enabled();
        let target = target_x as f32;
        let mut tr = match self.x.get() {
            // 换行/首次出现：不滑行，直接落到新位置。
            Some(tr) if smooth && same_line => tr,
            _ => Transition::new(target),
        };
        if (tr.target() - target).abs() > 0.5 {
            tr.retarget(target, MOVE_MS, Easing::EaseOut);
        }
        let x = tr.value();
        let moving = tr.is_active();
        self.x.set(Some(tr));
        (x, moving)
    }

    /// 画光标条并自报脏区续帧。
    ///
    /// `(x, y, h)` 是光标目标位置：`x` 为字符边界（光标向右延伸 `opts.width`），
    /// `y`/`h` 为所在行的竖直范围，均为**逻辑绝对坐标**。
    ///
    /// 返回本帧的 `(整数矩形, alpha)`，供调用方做反色重绘（把压在光标下的那段字形
    /// 用底色重画一遍）；alpha 已不可见时返回 `None`，此时反色重绘也该跳过。
    pub fn paint(
        &self,
        canvas: &mut dyn Canvas,
        x: i32,
        y: i32,
        h: i32,
        color: Color,
        opts: &CaretOpts,
    ) -> Option<(Rect, f32)> {
        let now = anim::clock_ms();
        let target = (x, y, h);
        let prev = self.target.get();
        // 位置变了即算一次交互：重置相位、保持实心。把"打字/移动光标/点击定位/滚动"
        // 统一归到这里判定，胜过在控件的十几个事件分支里逐个插桩——那样迟早漏掉一条
        // （尤其是程序化改文本这种不经过按键的路径），表现为打着字光标却在闪。
        if prev != Some(target) {
            self.last_activity.set(now);
        }
        let same_line = prev.map(|(_, py, ph)| (py, ph) == (y, h)).unwrap_or(false);
        let (fx, moving) = self.resolve_x(x, same_line, opts.smooth_move);
        self.target.set(Some(target));
        let alpha = self.alpha(opts.style, now);

        // **物理像素网格吸附**：光标是一条 1~4 像素宽的实心细条，是整个界面里对亚像素
        // 最敏感的图元。不吸附的话，125% DPI 下 2 逻辑 px 落成 2.5 物理 px，抗锯齿把它
        // 铺成「中间 2 列实 + 两侧各半列淡」的 4 列——又糊又比实际更肿；1.75 倍这类
        // 刻度更明显。系统插入符从不这样画。
        //
        // 宽度**向下**取整（至少 1 物理像素）：向上取整会让 125% 的 2.5 变成 3 列，比
        // 100% 下更粗，"高 DPI 才该更粗"的直觉反而失效。高度与左上角就近取整即可——
        // 它们只需落格，不涉及粗细观感。
        let s = canvas.dpi_scale().max(0.01);
        let w_px = (opts.width.to_logical(s) * s).floor().max(1.0);
        let h_px = ((h as f32) * s).round().max(1.0);
        let (fx, fy) = ((fx * s).round() / s, ((y as f32) * s).round() / s);
        let (w, hh) = (w_px / s, h_px / s);

        // 整数矩形：向外取整覆盖亚像素位置，供 clip 与脏区用。
        let x0 = fx.floor() as i32;
        let x1 = (fx + w).ceil() as i32;
        let y0 = fy.floor() as i32;
        let y1 = (fy + hh).ceil() as i32;
        let rect = Rect::new(x0, y0, (x1 - x0).max(1), (y1 - y0).max(1));

        // 脏区：本帧 ∪ 上一帧，外扩 1px 收住抗锯齿边。报小了会留残影。
        let dirty = match self.last_rect.get() {
            Some(prev) => rect.union(&prev),
            None => rect,
        }
        .inflate(1);
        self.last_rect.set(Some(rect));
        if self.blink_half(opts.style).is_some() || moving {
            // 滑行中每帧都在动；否则睡到 alpha 下次变化那一刻。方波光标由此从"每帧都要"
            // 变成"一个半周期两帧"，静止界面的续帧开销塌回接近静态光标。
            let delay = if moving {
                0
            } else {
                self.next_change(opts.style, now)
            };
            anim::request_repaint_in_after(dirty, delay);
        }

        if alpha <= ALPHA_EPS {
            return None;
        }
        let paint = Paint::fill(color.scale_alpha(alpha));
        // 圆角只在够宽时才有形状收益。1~2 物理像素宽的条画半圆端，圆弧没有像素可落，
        // 只会把两端的角像素削成半透明——看着是"光标短了一截还发虚"，而不是圆润。
        // 这条降级正是低 DPI 下光标显糊的第二个来源（第一个是上面的网格吸附）。
        if opts.rounded && w_px >= 3.0 {
            canvas.fill_round_rect(fx, fy, w, hh, w / 2.0, &paint);
        } else {
            canvas.fill_rect(fx, fy, w, hh, &paint);
        }
        Some((rect, alpha))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HALF: u64 = BLINK_HALF_MS;

    #[test]
    fn solid_is_always_opaque() {
        for t in [0, 100, 530, 1060, 99_999] {
            assert_eq!(alpha_at(CaretStyle::Solid, t, HALF), 1.0);
        }
    }

    #[test]
    fn blink_is_square_wave() {
        assert_eq!(alpha_at(CaretStyle::Blink, 0, HALF), 1.0);
        assert_eq!(alpha_at(CaretStyle::Blink, HALF - 1, HALF), 1.0);
        assert_eq!(alpha_at(CaretStyle::Blink, HALF, HALF), 0.0);
        assert_eq!(alpha_at(CaretStyle::Blink, 2 * HALF - 1, HALF), 0.0);
        assert_eq!(alpha_at(CaretStyle::Blink, 2 * HALF, HALF), 1.0, "应周期");
        // 周期跟随系统设置：半周期减半，翻转时刻也减半。
        assert_eq!(alpha_at(CaretStyle::Blink, 200, 250), 1.0);
        assert_eq!(alpha_at(CaretStyle::Blink, 300, 250), 0.0);
    }

    #[test]
    fn smooth_fades_both_ways_and_is_periodic() {
        let (hold, fade) = smooth_segments(HALF);
        assert_eq!((hold, fade), (129, 401), "530ms 半周期下的分段");
        // 相位 0 全亮、半周期附近全灭。
        assert_eq!(alpha_at(CaretStyle::Smooth, 0, HALF), 1.0);
        assert_eq!(
            alpha_at(CaretStyle::Smooth, hold + fade + hold / 2, HALF),
            0.0
        );
        // 淡出段单调下降，淡入段单调上升。
        let fade_out: Vec<f32> = (0..=8)
            .map(|i| alpha_at(CaretStyle::Smooth, hold + i * fade / 8, HALF))
            .collect();
        for w in fade_out.windows(2) {
            assert!(w[1] <= w[0] + 1e-6, "淡出段应单调非增：{fade_out:?}");
        }
        let base = 2 * hold + fade;
        let fade_in: Vec<f32> = (0..=8)
            .map(|i| alpha_at(CaretStyle::Smooth, base + i * fade / 8, HALF))
            .collect();
        for w in fade_in.windows(2) {
            assert!(w[1] >= w[0] - 1e-6, "淡入段应单调非减：{fade_in:?}");
        }
        // 周期性。
        for t in [0, 200, 700, 1000] {
            let a = alpha_at(CaretStyle::Smooth, t, HALF);
            let b = alpha_at(CaretStyle::Smooth, t + 2 * HALF, HALF);
            assert!((a - b).abs() < 1e-6, "相位 {t} 应周期等价");
        }
        // 分段随半周期等比缩放：系统调快闪烁，呼吸也整体变快。
        let (h2, f2) = smooth_segments(HALF / 2);
        assert!(
            (h2 as f32 - hold as f32 / 2.0).abs() <= 1.0
                && (f2 as f32 - fade as f32 / 2.0).abs() <= 1.0,
            "分段应随半周期等比：{h2}/{f2}"
        );
    }

    #[test]
    fn phase_never_goes_dark() {
        for i in 0..64 {
            let a = alpha_at(CaretStyle::Phase, i * 2 * HALF / 64, HALF);
            assert!(
                (PHASE_MIN_ALPHA - 1e-6..=1.0 + 1e-6).contains(&a),
                "半明半暗风格不得熄灭，实得 {a}"
            );
        }
        assert!(
            (alpha_at(CaretStyle::Phase, 0, HALF) - 1.0).abs() < 1e-6,
            "相位 0 应最亮"
        );
    }

    /// `next_change_ms` 与 `alpha_at` 必须说同一件事：睡到它报的那一刻，alpha 一定
    /// 已经变了；在那之前的任何一刻，alpha 一定还没变。这条对拍把两个函数钉在一起——
    /// 它们各写各的分支，靠人眼对齐边界迟早出错，而错法是"光标卡在旧画面上"这种
    /// 只有真跑窗口才看得出来的症状。
    #[test]
    fn next_change_agrees_with_alpha_curve() {
        for style in [CaretStyle::Blink, CaretStyle::Smooth, CaretStyle::Phase] {
            for half in [250u64, HALF, 1200] {
                for phase in (0..(2 * half)).step_by(7) {
                    let now = alpha_at(style, phase, half);
                    let d = next_change_ms(style, phase, half);
                    if d == 0 {
                        continue; // 连续变化，无边界可验
                    }
                    // 截止**之前**（半开区间 [0, d)）恒定：报晚了会让光标卡在旧画面上。
                    for t in [0, d / 3, d / 2, d - 1] {
                        assert_eq!(
                            alpha_at(style, phase + t, half),
                            now,
                            "{style:?} half={half} phase={phase}：距变化还有 {d}ms，\
                             但 +{t}ms 就变了（报晚了 → 光标会卡住）"
                        );
                    }
                    // 截止**当刻**：要么值已经变了（方波翻面），要么进入了逐帧变化的区间
                    // （呼吸的淡变段起点，值还等于保持段的端点，但从此每帧都在动）。
                    // 只断言前者会误伤后者，只断言后者则放过"报早了"。
                    let changed = alpha_at(style, phase + d, half) != now;
                    let continuous = next_change_ms(style, phase + d, half) == 0;
                    assert!(
                        changed || continuous,
                        "{style:?} half={half} phase={phase}：报的 {d}ms 到了，\
                         既没变值也不是连续段（白醒一帧）"
                    );
                }
            }
        }
    }

    #[test]
    fn next_change_endpoints() {
        assert_eq!(
            next_change_ms(CaretStyle::Solid, 0, HALF),
            u64::MAX,
            "永不变"
        );
        assert_eq!(next_change_ms(CaretStyle::Blink, 0, HALF), HALF);
        assert_eq!(next_change_ms(CaretStyle::Blink, HALF - 1, HALF), 1);
        assert_eq!(
            next_change_ms(CaretStyle::Blink, HALF, HALF),
            HALF,
            "翻面即刻"
        );
        assert_eq!(next_change_ms(CaretStyle::Phase, 123, HALF), 0, "连续变化");
        // 平滑呼吸：亮端保持段内可以睡到保持结束，淡变段内每帧都要。
        let (hold, fade) = smooth_segments(HALF);
        assert_eq!(next_change_ms(CaretStyle::Smooth, 0, HALF), hold);
        assert_eq!(next_change_ms(CaretStyle::Smooth, hold, HALF), 0);
        assert_eq!(next_change_ms(CaretStyle::Smooth, hold + fade / 2, HALF), 0);
    }

    /// 打字暂停期内画面恒定，可以一直睡到暂停结束（再加上相位 0 到首次变化的距离）。
    /// 少算这一段，打字时光标就会按刷新率白白出帧——正是最该省的时候在最费。
    #[test]
    fn next_change_covers_typing_pause() {
        set_animated(true);
        set_window_active(true);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        let c = CaretState::new();
        c.bump(10_000);
        // 刚敲完：睡到暂停结束 + 半周期（暂停结束时相位 0 是亮的，还要亮满半周期）。
        assert_eq!(
            c.next_change(CaretStyle::Blink, 10_000),
            TYPING_PAUSE_MS + BLINK_HALF_MS + 1
        );
        // 暂停中途。
        assert_eq!(
            c.next_change(CaretStyle::Blink, 10_000 + 200),
            TYPING_PAUSE_MS - 200 + BLINK_HALF_MS + 1
        );
        // 暂停结束后按相位算。
        assert_eq!(
            c.next_change(CaretStyle::Blink, 10_000 + TYPING_PAUSE_MS + 100),
            BLINK_HALF_MS - 100 + 1
        );
        // 连续变化的风格不加余量，仍是 0（= 每帧）。
        assert_eq!(c.next_change(CaretStyle::Phase, 10_000 + 9_000), 0);
    }

    /// 不闪的三种情形都不需要下一帧：静态风格、系统关闭闪烁、窗口失活。
    #[test]
    fn static_caret_never_needs_a_frame() {
        set_animated(true);
        set_window_active(true);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        let c = CaretState::new();
        c.bump(0);
        assert_eq!(c.next_change(CaretStyle::Solid, 5_000), u64::MAX);
        set_blink_period_ms(None);
        assert_eq!(c.next_change(CaretStyle::Blink, 5_000), u64::MAX);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        set_window_active(false);
        assert_eq!(c.next_change(CaretStyle::Blink, 5_000), u64::MAX);
        set_window_active(true);
    }

    /// 渲染一次光标，返回 (非零列数, 非零行数, 是否存在半透明像素)。
    fn probe(scale: f32, width: Len, rounded: bool) -> (usize, usize, bool) {
        use crate::render::SkiaCanvas;
        use tiny_skia::Pixmap;
        let mut pm = Pixmap::new(64, 64).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let c = CaretState::new();
        let opts = CaretOpts {
            style: CaretStyle::Solid,
            width,
            rounded,
            smooth_move: false,
        };
        {
            let mut cv = SkiaCanvas::with_text(&mut pm, &mut eng, scale);
            c.paint(&mut cv, 10, 5, 20, Color::rgb(0, 0, 0), &opts);
        }
        let (w, h) = (pm.width() as usize, pm.height() as usize);
        let d = pm.data();
        let a = |x: usize, y: usize| d[(y * w + x) * 4 + 3];
        let cols = (0..w).filter(|&x| (0..h).any(|y| a(x, y) > 0)).count();
        let rows = (0..h).filter(|&y| (0..w).any(|x| a(x, y) > 0)).count();
        let partial = (0..w).any(|x| (0..h).any(|y| matches!(a(x, y), 1..=254)));
        (cols, rows, partial)
    }

    /// 非整数 DPI（125% / 150% / 175%）下光标必须落在整数物理像素上：
    /// 不吸附的话 2 逻辑 px 会映射成 2.5 物理 px，抗锯齿铺成「两侧各半列淡边」的 4 列，
    /// 既模糊又比实际更肿。判据是**不存在半透明像素**——只数"画了几列"会漏掉淡边。
    #[test]
    fn caret_snaps_to_physical_pixel_grid() {
        crate::ui::caret::set_animated(false);
        for s in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let (cols, rows, partial) = probe(s, Len::Dp(2.0), false);
            assert!(!partial, "scale {s} 下不应有半透明边（模糊）");
            // 宽度向下取整到整数物理像素：125% 下仍是 2 列，不因取整变得更粗。
            assert_eq!(cols, (2.0 * s).floor() as usize, "scale {s} 的物理宽度");
            assert_eq!(rows, (20.0 * s).round() as usize, "scale {s} 的物理高度");
        }
    }

    /// 圆角开着时，窄到画不出圆弧的条要自动退回直角——否则端部角像素被削成半透明，
    /// 观感是"又糊又短"。够宽（≥3 物理像素）时才允许出现圆弧带来的抗锯齿。
    #[test]
    fn rounded_caret_degrades_to_square_when_too_narrow() {
        crate::ui::caret::set_animated(false);
        for (scale, width) in [(1.0, 2.0), (1.25, 2.0)] {
            let (_, _, partial) = probe(scale, Len::Dp(width), true);
            assert!(
                !partial,
                "scale {scale} 下 {width}dp 宽的圆角光标应退回直角，不留半透明角"
            );
        }
        // 够宽时圆角生效：端部出现抗锯齿弧（这时它是想要的形状，不是糊）。
        let (_, _, partial) = probe(2.0, Len::Dp(2.0), true);
        assert!(partial, "4 物理像素宽应真的画出圆角端");
    }

    /// `{ px = N }` 写法在任意 DPI 下恒为 N 个物理像素。
    #[test]
    fn physical_width_is_dpi_invariant() {
        crate::ui::caret::set_animated(false);
        for s in [1.0, 1.25, 1.5, 2.0] {
            let (cols, _, partial) = probe(s, Len::Physical { px: 2.0 }, false);
            assert!(!partial, "scale {s} 下 px 宽不应有半透明边");
            assert_eq!(cols, 2, "{{px=2}} 在 scale {s} 下应恒为 2 物理像素");
        }
        // 亚像素宽度不会消失：至少 1 个物理像素。
        let (cols, _, _) = probe(1.0, Len::Physical { px: 0.2 }, false);
        assert_eq!(cols, 1, "过细的配置也要保底 1 物理像素");
    }

    #[test]
    fn typing_pause_keeps_caret_solid() {
        anim::set_enabled(true);
        set_animated(true);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        let c = CaretState::new();
        c.bump(10_000);
        // 暂停期内：即便相位落在方波的灭区也保持实心。
        for dt in [0, 100, TYPING_PAUSE_MS - 1] {
            assert_eq!(c.alpha(CaretStyle::Blink, 10_000 + dt), 1.0, "打字期间不闪");
        }
        // 暂停结束后相位从 0 起算 → 仍是亮半周期的开头。
        assert_eq!(c.alpha(CaretStyle::Blink, 10_000 + TYPING_PAUSE_MS), 1.0);
        assert_eq!(
            c.alpha(CaretStyle::Blink, 10_000 + TYPING_PAUSE_MS + BLINK_HALF_MS),
            0.0,
            "暂停结束后应开始闪"
        );
    }

    /// 闪烁的三个闸门各自的语义。**关键**：`anim::enabled()`（客户区动画）不在其中——
    /// 用户在 Windows 性能选项里关掉动画后，系统自带输入框的插入符照样闪，我们也必须闪。
    #[test]
    fn blink_gates_are_independent_of_client_area_animation() {
        let c = CaretState::new();
        c.bump(0);
        let dark = TYPING_PAUSE_MS + BLINK_HALF_MS;

        set_animated(true);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        anim::set_enabled(false); // 客户区动画关闭
        assert_eq!(
            c.alpha(CaretStyle::Blink, dark),
            0.0,
            "客户区动画关闭不应影响光标闪烁"
        );
        anim::set_enabled(true);

        // 系统要求插入符不闪 → 恒实心。
        set_blink_period_ms(None);
        assert_eq!(
            c.alpha(CaretStyle::Blink, dark),
            1.0,
            "系统关闭闪烁应恒实心"
        );

        // 总闸（截图路径）→ 恒实心。
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        set_animated(false);
        assert_eq!(c.alpha(CaretStyle::Blink, dark), 1.0, "总闸关闭应恒实心");
        set_animated(true);
        assert_eq!(c.alpha(CaretStyle::Blink, dark), 0.0, "恢复后应重新闪");
    }

    /// 窗口失活 → 光标静态。后台窗口的插入符在两个系统上本就不闪，更没理由按刷新率出帧。
    #[test]
    fn inactive_window_freezes_caret() {
        set_animated(true);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
        let c = CaretState::new();
        c.bump(0);
        let dark = TYPING_PAUSE_MS + BLINK_HALF_MS;
        set_window_active(false);
        assert_eq!(c.alpha(CaretStyle::Blink, dark), 1.0, "失活窗口光标应静态");
        assert!(
            c.blink_half(CaretStyle::Smooth).is_none(),
            "失活不应视作在闪"
        );
        set_window_active(true);
        assert_eq!(c.alpha(CaretStyle::Blink, dark), 0.0, "重新激活应恢复闪烁");
    }

    /// 半周期跟随系统设置：`GetCaretBlinkTime` 调快，应用里的光标同步变快。
    #[test]
    fn blink_period_follows_system_setting() {
        set_animated(true);
        let c = CaretState::new();
        c.bump(0);
        set_blink_period_ms(Some(250));
        assert_eq!(c.alpha(CaretStyle::Blink, TYPING_PAUSE_MS + 200), 1.0);
        assert_eq!(
            c.alpha(CaretStyle::Blink, TYPING_PAUSE_MS + 300),
            0.0,
            "250ms 半周期下 300ms 应已翻到灭"
        );
        // 0 视同未设置：过滤掉，避免除零与"周期为 0 的闪烁"。
        set_blink_period_ms(Some(0));
        assert_eq!(blink_period_ms(), None);
        set_blink_period_ms(Some(BLINK_HALF_MS as u32));
    }

    #[test]
    fn smooth_move_slides_within_line_and_jumps_across_lines() {
        anim::set_enabled(true);
        set_animated(true);
        crate::anim::set_clock_ms(0);
        let c = CaretState::new();
        // 首帧：直接落位，不从 0 滑过来。
        let (x, moving) = c.resolve_x(100, false, true);
        assert_eq!(x, 100.0);
        assert!(!moving);
        // 同行内移动：起手仍在旧位置附近，且报告"滑行中"。
        let (x, moving) = c.resolve_x(200, true, true);
        assert!((x - 100.0).abs() < 1e-3, "改向瞬间应还在原位，实得 {x}");
        assert!(moving, "同行移动应滑行");
        crate::anim::set_clock_ms(MOVE_MS as u64);
        let (x, moving) = c.resolve_x(200, true, true);
        assert_eq!(x, 200.0, "补间结束应到位");
        assert!(!moving);
        // 换行：瞬移，不滑。
        let (x, moving) = c.resolve_x(10, false, true);
        assert_eq!(x, 10.0, "换行应瞬移");
        assert!(!moving);
    }

    #[test]
    fn smooth_move_off_jumps_instantly() {
        anim::set_enabled(true);
        set_animated(true);
        crate::anim::set_clock_ms(0);
        let c = CaretState::new();
        c.resolve_x(100, false, false);
        let (x, moving) = c.resolve_x(400, true, false);
        assert_eq!(x, 400.0, "关闭平滑移动应瞬移");
        assert!(!moving);
    }

    #[test]
    fn reset_clears_slide_origin() {
        anim::set_enabled(true);
        set_animated(true);
        crate::anim::set_clock_ms(0);
        let c = CaretState::new();
        c.resolve_x(100, false, true);
        c.reset();
        let (x, moving) = c.resolve_x(300, true, true);
        assert_eq!(x, 300.0, "reset 后应按首次出现处理");
        assert!(!moving);
    }
}
