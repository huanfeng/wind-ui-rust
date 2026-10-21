//! 动画驱动与补间。
//!
//! 两块：
//! - **帧驱动**（既有）：控件在 paint 中调 [`request_repaint`] 表示"我在动画中，需要下一帧"；
//!   宿主每帧绘制前 [`reset_request`]，绘制后读 [`animation_requested`] 决定是否继续驱动帧。
//!   平台据此选唤醒机制（Win32 带超时的消息等待），无动画时回阻塞空闲（零 CPU）。
//!   控件用 [`clock_ms`] 取当前帧单调时钟算动画相位。
//!   续帧请求还带一个**截止时间**（[`next_frame_delay_ms`]）：控件可以说"我 530ms 后才需要
//!   下一帧"，平台据此把唤醒推迟到那时——见 [`request_repaint_in_after`]。
//! - **补间**：[`Easing`] 缓动曲线 / [`Spring`] 弹簧 + [`Transition`] 补间助手 + [`MotionLevel`]
//!   全局动效档位。控件把 `Cell<Transition<T>>` 当字段持有，paint 内据状态 `retarget` 并
//!   `animate()` 取值。档位为 [`MotionLevel::Off`] 时所有补间瞬时收敛、不再续帧；
//!   [`MotionLevel::Reduced`] 时只有标了 [`Transition::movement`] 的位移类补间瞬时收敛。

use std::cell::Cell;

use serde::{Deserialize, Serialize};

use crate::geometry::{Color, Insets, Point, Rect, Size};

thread_local! {
    static REQUEST: Cell<bool> = const { Cell::new(false) };
    static CLOCK_MS: Cell<u64> = const { Cell::new(0) };
    /// 全局动效档位（尊重系统"显示动画"/"减弱动态效果"/省电/无障碍）。
    static LEVEL: Cell<MotionLevel> = const { Cell::new(MotionLevel::Full) };
    /// 补间时间放慢倍数（调试用，`WINDUI_ANIM_SCALE`）。
    static TIME_SCALE: Cell<f32> = Cell::new(env_time_scale());
    /// 钉住的帧时钟：Some 时宿主写入的时钟一律被它替换（截图取中间帧用）。
    static CLOCK_PIN: Cell<Option<u64>> = const { Cell::new(None) };
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
    /// 续帧请求的累计次数（只增不清）。见 [`wake_count`]。
    static WAKES: Cell<u64> = const { Cell::new(0) };
}

/// 续帧请求的累计次数：前后各取一次比对，即知"这段代码里有没有人要过一帧"。
///
/// 不能拿 `REQUEST` 位判：它是帧概念，上一帧有动画时本就为真，帧外读到的是残值。
/// 用于帧外的 App 级回调（`on_interval`）——主题 / 语言 / 热键 / 托盘这些运行期
/// 句柄不写信号、不经 `EventCtx`，唯一的唤帧手段就是 `request_repaint`。
pub(crate) fn wake_count() -> u64 {
    WAKES.with(|c| c.get())
}

/// 记一笔续帧截止（毫秒），与本帧已有的取较早者。
fn note_delay(delay_ms: u64) {
    // 所有续帧请求都经过这里，计数放在这一处即不漏。
    WAKES.with(|c| c.set(c.get().wrapping_add(1)));
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

/// 请求 `delay_ms` 毫秒后整窗重绘一帧（截止语义同 [`request_repaint_in_after`]）。
///
/// 给**节点之外**、等的是一个时刻而不是连续动画的绘制者：悬停提示的延时弹出——
/// 等待期画面不变，到点那一帧浮层才出现，而浮层不属于任何节点、给不出自报脏区。
pub(crate) fn request_repaint_after(delay_ms: u64) {
    REQUEST.with(|c| c.set(true));
    note_delay(delay_ms);
    DAMAGE_FULL.with(|c| c.set(true));
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

/// 全局动效档位。
///
/// 系统要求"减弱动态效果"时落到哪一档由应用决定（[`App::reduced_motion`](crate::App::reduced_motion)），
/// 默认 [`Reduced`](Self::Reduced)：位移、缩放这类"东西在动"的过渡最容易引起不适，颜色与
/// 透明度的渐变则不会，故只关前者——这也是 Apple HIG 与 WCAG 2.3.3 的建议做法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotionLevel {
    /// 全部动效。
    #[default]
    Full,
    /// 减弱：位移类补间（[`Transition::movement`]）瞬时到位，颜色/透明度过渡照常。
    Reduced,
    /// 关闭：所有补间瞬时收敛、不续帧。
    Off,
}

/// 当前全局动效档位。
pub fn motion_level() -> MotionLevel {
    LEVEL.with(|c| c.get())
}

/// 设置全局动效档位（宿主据系统设置初始化，或运行期切换）。
pub fn set_motion_level(level: MotionLevel) {
    LEVEL.with(|c| c.set(level));
}

/// 全局动画是否启用（档位不是 [`MotionLevel::Off`]）。关闭时补间瞬时收敛。
pub fn enabled() -> bool {
    motion_level() != MotionLevel::Off
}

/// 设置全局动画开关：`true` = [`MotionLevel::Full`]，`false` = [`MotionLevel::Off`]。
pub fn set_enabled(on: bool) {
    set_motion_level(if on {
        MotionLevel::Full
    } else {
        MotionLevel::Off
    });
}

/// 位移类动效是否启用（档位为 [`MotionLevel::Full`]）。
///
/// 自己算位移相位、不经 [`Transition`] 的控件（例如手搓 `clock_ms` 的滑动）据此决定要不要动。
pub fn movement_enabled() -> bool {
    motion_level() == MotionLevel::Full
}

/// 补间的时间放慢倍数（≥1 变慢）。默认取环境变量 `WINDUI_ANIM_SCALE`，未设为 1。
///
/// 只作用于 [`Transition`]——肉眼查一段 200ms 的过渡几乎不可能，放慢 5–10 倍才看得清
/// 曲线与中途改向。帧时钟本身不受影响（双击、长按等时长判定仍按真实时间）。
pub fn time_scale() -> f32 {
    TIME_SCALE.with(|c| c.get())
}

/// 设置补间的时间放慢倍数（钳到 `[0.01, 100]`）。
pub fn set_time_scale(scale: f32) {
    TIME_SCALE.with(|c| c.set(clamp_time_scale(scale)));
}

fn clamp_time_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(0.01, 100.0)
    } else {
        1.0
    }
}

fn env_time_scale() -> f32 {
    // 测试不读环境：开发者 shell 里留着这个变量会让一批断言时长的老测试无端失败。
    if cfg!(test) {
        return 1.0;
    }
    std::env::var("WINDUI_ANIM_SCALE")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map_or(1.0, clamp_time_scale)
}

/// 钉住帧时钟：`Some(ms)` 后宿主写入的时钟一律被替换为 `ms`，`None` 解除。
///
/// 给截图取中间帧（`--anim-at`）用：真实时钟下"第 60ms 那一帧"无法复现。
pub(crate) fn pin_clock(ms: Option<u64>) {
    CLOCK_PIN.with(|c| c.set(ms));
    if let Some(v) = ms {
        CLOCK_MS.with(|c| c.set(v));
    }
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
    let v = CLOCK_PIN.with(|c| c.get()).unwrap_or(v);
    CLOCK_MS.with(|c| c.set(v));
}

/// 缓动曲线：把线性进度 `t∈[0,1]` 映射为缓动进度（端点恒为 0/1）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// CSS `cubic-bezier(x1, y1, x2, y2)`。`x1`/`x2` 钳到 `[0,1]` 以保证对时间单调；
    /// `y1`/`y2` 可越出 `[0,1]`，此时输出会过冲（回弹类曲线就靠它）。
    CubicBezier(f32, f32, f32, f32),
}

impl Easing {
    /// 标准曲线：快起、长尾减速。进场、展开、状态切换的默认选择
    /// （同 Material 3 standard / Fluent 的 `cubic-bezier(0.2, 0, 0, 1)`）。
    pub const STANDARD: Easing = Easing::CubicBezier(0.2, 0.0, 0.0, 1.0);
    /// 减速：从运动中进入静止。元素进场。
    pub const DECELERATE: Easing = Easing::CubicBezier(0.0, 0.0, 0.0, 1.0);
    /// 加速：从静止离开。元素离场——离场不需要被看清，越快越好。
    pub const ACCELERATE: Easing = Easing::CubicBezier(0.3, 0.0, 1.0, 1.0);

    /// 应用缓动。输入 `t` 钳到 `[0,1]`；三次族输出也钳到 `[0,1]`（兜底浮点过冲），
    /// [`CubicBezier`](Self::CubicBezier) 的输出不钳（见其说明）。
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
            Easing::CubicBezier(x1, y1, x2, y2) => return cubic_bezier(x1, y1, x2, y2, t),
        };
        r.clamp(0.0, 1.0)
    }
}

/// 一维三次贝塞尔（端点 0 与 1）在参数 `s` 处的值。
fn bezier_axis(p1: f32, p2: f32, s: f32) -> f32 {
    let u = 1.0 - s;
    3.0 * u * u * s * p1 + 3.0 * u * s * s * p2 + s * s * s
}

fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let (x1, x2) = (x1.clamp(0.0, 1.0), x2.clamp(0.0, 1.0));
    // 先牛顿迭代求 x(s)=t 的参数 s；导数过小（曲线在该处近乎竖直）时改二分，保证收敛。
    let mut s = t;
    for _ in 0..8 {
        let err = bezier_axis(x1, x2, s) - t;
        if err.abs() < 1e-5 {
            return bezier_axis(y1, y2, s);
        }
        let u = 1.0 - s;
        let d = 3.0 * u * u * x1 + 6.0 * u * s * (x2 - x1) + 3.0 * s * s * (1.0 - x2);
        if d.abs() < 1e-6 {
            break;
        }
        s = (s - err / d).clamp(0.0, 1.0);
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    s = t;
    for _ in 0..30 {
        let x = bezier_axis(x1, x2, s);
        if (x - t).abs() < 1e-5 {
            break;
        }
        if x < t {
            lo = s;
        } else {
            hi = s;
        }
        s = (lo + hi) / 2.0;
    }
    bezier_axis(y1, y2, s)
}

/// 弹簧：按物理阻尼振动从当前值运动到目标。
///
/// 与时间曲线的根本区别在**中途改向**：[`Transition::retarget_spring`] 把当前速度带进
/// 新的运动，于是悬停快速进出、开关连点时不会出现"速度突然归零"的折角。时长不由调用方
/// 指定，而由参数决定（运动停稳即结束）。
///
/// 取值是解析解，只依赖帧时钟、不逐帧积分——同一时刻永远算出同一个值，截图可复现。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Spring {
    /// 响应时间（毫秒）：无阻尼时振动一个周期的时长，越小越"脆"。
    pub response_ms: f32,
    /// 阻尼比：`1` 为临界阻尼（最快到位且不过冲），`<1` 会回弹，`>1` 更"肉"。
    pub damping: f32,
}

impl Spring {
    /// 平滑：不过冲。状态色、滑块、hover 这类反馈的默认选择。
    pub const SMOOTH: Spring = Spring {
        response_ms: 300.0,
        damping: 1.0,
    };
    /// 干脆：略有过冲，短。
    pub const SNAPPY: Spring = Spring {
        response_ms: 220.0,
        damping: 0.85,
    };
    /// 弹跳：明显回弹。只用于强调，别用在高频反馈上。
    pub const BOUNCY: Spring = Spring {
        response_ms: 350.0,
        damping: 0.6,
    };

    /// 进度达到后视为停稳的误差（进度单位）。
    const EPS: f32 = 1e-3;
    /// 停稳判定的速度阈值（进度/毫秒）：约等于每 16ms 帧挪不到 EPS。
    const EPS_V: f32 = 6e-5;
    /// 停稳判定的扫描步长（毫秒）。
    const SCAN_STEP_MS: f32 = 4.0;
    /// 运动时长上限（毫秒）：参数再极端也在此落定，不让一个补间续帧到天荒地老。
    const SETTLE_LIMIT_MS: f32 = 30_000.0;

    /// 钳过的 `(响应毫秒, 阻尼比)`。上限保证最慢的组合（响应 3s、阻尼 2）也在
    /// [`SETTLE_LIMIT_MS`](Self::SETTLE_LIMIT_MS) 内停稳。
    fn sanitized(self) -> (f32, f32) {
        let r = if self.response_ms.is_finite() {
            self.response_ms.clamp(1.0, 3_000.0)
        } else {
            Self::SMOOTH.response_ms
        };
        let z = if self.damping.is_finite() {
            self.damping.clamp(0.05, 2.0)
        } else {
            1.0
        };
        (r, z)
    }

    /// `t` 毫秒（名义时间）时的进度：起点 0、目标 1，初速度 `v0`（进度/毫秒）。
    fn progress(self, t: f32, v0: f32) -> f32 {
        let (r, z) = self.sanitized();
        let w0 = std::f32::consts::TAU / r;
        // 以"离目标的位移" x 求解：x(0)=1、x'(0)=-v0，进度 = 1 - x。
        let (x0, dx0) = (1.0f32, -v0);
        let x = if (z - 1.0).abs() < 1e-4 {
            (-w0 * t).exp() * (x0 + (dx0 + w0 * x0) * t)
        } else if z < 1.0 {
            let wd = w0 * (1.0 - z * z).sqrt();
            (-z * w0 * t).exp() * (x0 * (wd * t).cos() + (dx0 + z * w0 * x0) / wd * (wd * t).sin())
        } else {
            let k = (z * z - 1.0).sqrt();
            let (r1, r2) = (-w0 * (z - k), -w0 * (z + k));
            let c2 = (dx0 - r1 * x0) / (r2 - r1);
            let c1 = x0 - c2;
            c1 * (r1 * t).exp() + c2 * (r2 * t).exp()
        };
        1.0 - x
    }

    /// 从初速度 `v0` 出发到停稳所需的名义毫秒数。
    ///
    /// 改向时调用（不在每帧里），但悬停快速划过一排控件时一帧能触发多次，故不做全程扫描：
    /// - 欠阻尼有闭式解。振幅包络 `A·e^(-ζω₀t)` 严格单调，而 `x` 与 `(x' + ζω₀x)/ω_d`
    ///   的平方和恰为包络的平方——包络落进 EPS 即 `x` 与速度都已落进，此后不再出来；
    /// - 临界/过阻尼不振荡，至多过零一次（过零时速度最大，速度条件挡得住），
    ///   故向前扫到第一个"位移与速度都已足够小"的时刻即可。
    fn settle_ms(self, v0: f32) -> f32 {
        let (r, z) = self.sanitized();
        let w0 = std::f32::consts::TAU / r;
        if z < 1.0 - 1e-4 {
            let a = z * w0;
            let wd = w0 * (1.0 - z * z).sqrt();
            let amp = (1.0 + ((a - v0) / wd).powi(2)).sqrt();
            let t = (amp / Self::EPS).ln() / a;
            return t.clamp(Self::SCAN_STEP_MS, Self::SETTLE_LIMIT_MS);
        }
        let mut t = 0.0f32;
        while t < Self::SETTLE_LIMIT_MS {
            let p = self.progress(t, v0);
            let v = self.progress(t + 1.0, v0) - p;
            if (1.0 - p).abs() <= Self::EPS && v.abs() <= Self::EPS_V {
                break;
            }
            t += Self::SCAN_STEP_MS;
        }
        t.max(Self::SCAN_STEP_MS)
    }
}

/// 可插值的值（补间货币）。`t=0` 返回 self、`t=1` 返回 to；`t` 可越出 `[0,1]`（弹簧与
/// 回弹曲线会过冲），整数型按四舍五入取整。
pub trait Lerp: Copy {
    fn lerp(self, to: Self, t: f32) -> Self;

    /// 速度换算：旧运动段 `old_from→old_to` 上的单位进度速度，折算到新运动段
    /// `cur→new_to` 上是多少单位进度速度。即两段位移的投影比 `⟨A,B⟩/⟨B,B⟩`。
    ///
    /// 只有弹簧改向用到（[`Transition::retarget_spring`] 借此继承速度）。返回 `None`
    /// 表示不继承、从静止起步——新段退化为零长度时也是如此。
    fn delta_ratio(old_from: Self, old_to: Self, cur: Self, new_to: Self) -> Option<f32> {
        let _ = (old_from, old_to, cur, new_to);
        None
    }
}

/// 两段位移（各以分量表示）的投影比 `⟨a,b⟩/⟨b,b⟩`；`b` 近乎零长时为 `None`。
fn projection_ratio(a: &[f32], b: &[f32]) -> Option<f32> {
    let bb: f32 = b.iter().map(|v| v * v).sum();
    if bb < 1e-6 {
        return None;
    }
    let ab: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    Some(ab / bb)
}

/// 为分量全是整数/浮点的类型实现 [`Lerp`]：逐分量插值 + 投影比速度换算。
macro_rules! lerp_by_fields {
    ($ty:ty { $($f:ident),+ }) => {
        impl Lerp for $ty {
            fn lerp(self, to: Self, t: f32) -> Self {
                Self { $($f: (self.$f as f32).lerp(to.$f as f32, t).round() as i32),+ }
            }
            fn delta_ratio(old_from: Self, old_to: Self, cur: Self, new_to: Self) -> Option<f32> {
                projection_ratio(
                    &[$((old_to.$f - old_from.$f) as f32),+],
                    &[$((new_to.$f - cur.$f) as f32),+],
                )
            }
        }
    };
}

impl Lerp for f32 {
    fn lerp(self, to: f32, t: f32) -> f32 {
        self + (to - self) * t
    }
    fn delta_ratio(old_from: f32, old_to: f32, cur: f32, new_to: f32) -> Option<f32> {
        projection_ratio(&[old_to - old_from], &[new_to - cur])
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
    fn delta_ratio(old_from: Color, old_to: Color, cur: Color, new_to: Color) -> Option<f32> {
        let d = |a: Color, b: Color| {
            [
                b.r as f32 - a.r as f32,
                b.g as f32 - a.g as f32,
                b.b as f32 - a.b as f32,
                b.a as f32 - a.a as f32,
            ]
        };
        projection_ratio(&d(old_from, old_to), &d(cur, new_to))
    }
}

lerp_by_fields!(Point { x, y });
lerp_by_fields!(Size { w, h });
lerp_by_fields!(Rect { x, y, w, h });
lerp_by_fields!(Insets {
    left,
    top,
    right,
    bottom
});

/// 补间的运动方式。
#[derive(Debug, Clone, Copy, PartialEq)]
enum Curve {
    Tween(Easing),
    /// `v0`：起步速度（进度/名义毫秒），由改向时继承而来。
    Spring {
        spring: Spring,
        v0: f32,
    },
}

/// 补间：沿时间曲线（[`retarget`](Self::retarget)）或弹簧（[`retarget_spring`](Self::retarget_spring)）
/// 从当前值过渡到目标。
///
/// 控件把它存进 `Cell`（本类型 `Copy`），paint 内据当前状态改向到新目标、再
/// [`animate`](Self::animate) 取值。读取走 [`clock_ms`]，故须在宿主已注入帧时钟的 paint 期使用。
/// 全局档位为 [`MotionLevel::Off`] 时一律返回目标值、不视为活跃；标了
/// [`movement`](Self::movement) 的补间在 [`MotionLevel::Reduced`] 下同样如此。
#[derive(Debug, Clone, Copy)]
pub struct Transition<T: Lerp> {
    from: T,
    to: T,
    start_ms: u64,
    /// 真实毫秒（已乘改向时的 [`time_scale`]）；0 = 静止。
    duration_ms: u32,
    curve: Curve,
    /// 改向时的时间放慢倍数：弹簧按名义时间求解，需要把真实流逝折回去。
    scale: f32,
    movement: bool,
}

/// 静止于 `T::default()`。
impl<T: Lerp + Default> Default for Transition<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: Lerp> Transition<T> {
    /// 构造一个静止于 `value` 的补间（无动画）。
    pub fn new(value: T) -> Self {
        Self {
            from: value,
            to: value,
            start_ms: 0,
            duration_ms: 0,
            curve: Curve::Tween(Easing::default()),
            scale: 1.0,
            movement: false,
        }
    }

    /// 标记为**位移类**补间（位置、尺寸、缩放、展开高度）：[`MotionLevel::Reduced`] 下
    /// 瞬时到位。颜色、透明度类补间不要标。
    pub fn movement(mut self) -> Self {
        self.movement = true;
        self
    }

    /// 瞬时落定到 `v`（不过渡），保留 [`movement`](Self::movement) 标记。
    ///
    /// 首帧落定、跟随布局漂移这类"不该动画"的改值用它，别用 `Transition::new(v)` 重建——
    /// 重建会丢掉标记，减弱动效档位就在那个控件上悄悄失效，编译与测试都发现不了。
    pub fn settle(&mut self, v: T) {
        self.from = v;
        self.to = v;
        self.duration_ms = 0;
    }

    /// 当前目标值。
    pub fn target(&self) -> T {
        self.to
    }

    /// 本补间在当前档位下是否应瞬时到位。
    fn snaps(&self) -> bool {
        match motion_level() {
            MotionLevel::Full => false,
            MotionLevel::Reduced => self.movement,
            MotionLevel::Off => true,
        }
    }

    /// 已流逝的真实毫秒；静止或应瞬时到位时为 `None`。
    fn live_elapsed(&self) -> Option<f32> {
        if self.duration_ms == 0 || self.snaps() {
            return None;
        }
        let elapsed = clock_ms().saturating_sub(self.start_ms);
        (elapsed < self.duration_ms as u64).then_some(elapsed as f32)
    }

    /// 真实流逝 `elapsed` 毫秒时的进度。
    fn progress(&self, elapsed: f32) -> f32 {
        match self.curve {
            Curve::Tween(e) => e.apply(elapsed / self.duration_ms as f32),
            Curve::Spring { spring, v0 } => spring.progress(elapsed / self.scale, v0),
        }
    }

    /// 改向新目标：从**当前值**起、用 `clock_ms()` 作起点，沿 `easing` 走 `duration_ms`。
    /// 当前档位下应瞬时到位或时长为 0 时直接落定到 `to`（下次取值即终值）。
    ///
    /// 时间曲线不继承速度：中途改向从静止起步。高频打断的反馈用 [`retarget_spring`](Self::retarget_spring)。
    pub fn retarget(&mut self, to: T, duration_ms: u32, easing: Easing) {
        let now = self.value();
        let scale = time_scale();
        self.from = now;
        self.to = to;
        self.start_ms = clock_ms();
        self.scale = scale;
        self.curve = Curve::Tween(easing);
        self.duration_ms = if self.snaps() {
            0
        } else {
            (duration_ms as f32 * scale).round() as u32
        };
    }

    /// 改向新目标，按弹簧运动；若正在运动，**继承当前速度**（见 [`Lerp::delta_ratio`]）。
    pub fn retarget_spring(&mut self, to: T, spring: Spring) {
        let now = self.value();
        let v0 = match self.live_elapsed() {
            Some(e) => {
                // 进度速度（每名义毫秒），再按两段位移的投影比折算到新段。
                let dp = (self.progress(e + 1.0) - self.progress(e)) * self.scale;
                T::delta_ratio(self.from, self.to, now, to).map_or(0.0, |k| dp * k)
            }
            None => 0.0,
        };
        let scale = time_scale();
        self.from = now;
        self.to = to;
        self.start_ms = clock_ms();
        self.scale = scale;
        self.curve = Curve::Spring { spring, v0 };
        self.duration_ms = if self.snaps() {
            0
        } else {
            (spring.settle_ms(v0) * scale).ceil() as u32
        };
    }

    /// 当前插值。应瞬时到位/静止/已结束 → `to`；否则按 `clock_ms()` 相位插值。
    pub fn value(&self) -> T {
        match self.live_elapsed() {
            Some(e) => self.from.lerp(self.to, self.progress(e)),
            None => self.to,
        }
    }

    /// 是否仍在过渡中（据 `clock_ms()`）。应瞬时到位时恒 false。
    pub fn is_active(&self) -> bool {
        self.live_elapsed().is_some()
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

impl Transition<Point> {
    /// 把整段运动平移 `d`（起点、终点一起挪），进度与速度不变。
    ///
    /// 参照系变了而视觉位置不该跳的场合用它：布局过渡（FLIP）里节点的布局位置又变了，
    /// 偏移量要整体加上新旧位置之差，接着 [`retarget_spring`](Self::retarget_spring)
    /// 回零就能继承原来的速度。
    pub(crate) fn translate(&mut self, d: Point) {
        self.from = Point::new(self.from.x + d.x, self.from.y + d.y);
        self.to = Point::new(self.to.x + d.x, self.to.y + d.y);
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

    #[test]
    fn cubic_bezier_matches_reference_points() {
        // CSS `ease` = cubic-bezier(0.25, 0.1, 0.25, 1)：t=0.5 处浏览器给出约 0.8024。
        let ease = Easing::CubicBezier(0.25, 0.1, 0.25, 1.0);
        assert!(
            (ease.apply(0.5) - 0.8024).abs() < 2e-3,
            "实得 {}",
            ease.apply(0.5)
        );
        // 端点恒定、单调。
        for e in [
            Easing::STANDARD,
            Easing::DECELERATE,
            Easing::ACCELERATE,
            ease,
        ] {
            assert_eq!(e.apply(0.0), 0.0);
            assert_eq!(e.apply(1.0), 1.0);
            let mut prev = 0.0;
            for i in 1..=50 {
                let v = e.apply(i as f32 / 50.0);
                assert!(v >= prev - 1e-4, "{e:?} 应单调");
                prev = v;
            }
        }
        // 线性控制点退化为匀速。
        let lin = Easing::CubicBezier(1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0);
        assert!((lin.apply(0.3) - 0.3).abs() < 1e-3);
    }

    #[test]
    fn cubic_bezier_may_overshoot() {
        // y2 > 1 的回弹曲线：中段应越过 1，而不是被钳死。
        let back = Easing::CubicBezier(0.3, 0.0, 0.3, 1.6);
        let peak = (1..50)
            .map(|i| back.apply(i as f32 / 50.0))
            .fold(0.0, f32::max);
        assert!(peak > 1.05, "回弹曲线应过冲，峰值 {peak}");
    }

    #[test]
    fn spring_settles_and_critical_does_not_overshoot() {
        set_motion_level(MotionLevel::Full);
        set_time_scale(1.0);
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget_spring(100.0, Spring::SMOOTH);
        let mut max = 0.0f32;
        let mut t = 0;
        while tr.is_active() {
            t += 8;
            set_clock(t);
            max = max.max(tr.value());
            assert!(t < 5_000, "临界阻尼弹簧应在 5s 内停稳");
        }
        assert_eq!(tr.value(), 100.0, "停稳后应精确落在目标");
        assert!(max <= 100.0 + 0.2, "临界阻尼不应过冲，峰值 {max}");
        // 欠阻尼会过冲。
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget_spring(100.0, Spring::BOUNCY);
        let peak = (1..200)
            .map(|i| {
                set_clock(i * 8);
                tr.value()
            })
            .fold(0.0, f32::max);
        assert!(peak > 105.0, "欠阻尼应过冲，峰值 {peak}");
    }

    /// 弹簧改向的价值：速度不归零。同一时刻改向，弹簧下一毫秒仍朝原方向走；
    /// 时间曲线则从静止起步、立刻掉头——后者就是"折角"。
    #[test]
    fn spring_retarget_keeps_velocity() {
        set_motion_level(MotionLevel::Full);
        set_time_scale(1.0);
        set_clock(0);
        let mut sp = Transition::new(0.0f32);
        sp.retarget_spring(100.0, Spring::SMOOTH);
        set_clock(60);
        let at = sp.value();
        sp.retarget_spring(0.0, Spring::SMOOTH); // 掉头
        assert!((sp.value() - at).abs() < 1e-3, "改向瞬间值连续");
        set_clock(76);
        assert!(sp.value() > at, "继承了向上的速度，掉头后短时间内仍应上冲");

        set_clock(0);
        let mut tw = Transition::new(0.0f32);
        tw.retarget(100.0, 200, Easing::Linear);
        set_clock(60);
        let at = tw.value();
        tw.retarget(0.0, 200, Easing::Linear);
        set_clock(76);
        assert!(tw.value() < at, "时间曲线不继承速度，立刻往回走");
    }

    #[test]
    fn delta_ratio_projects_segments() {
        assert_eq!(f32::delta_ratio(0.0, 10.0, 5.0, 0.0), Some(-2.0));
        assert_eq!(
            f32::delta_ratio(0.0, 10.0, 5.0, 5.0),
            None,
            "新段零长不继承"
        );
        let p = |x, y| Point::new(x, y);
        assert_eq!(
            Point::delta_ratio(p(0, 0), p(10, 0), p(0, 0), p(20, 0)),
            Some(0.5)
        );
        assert_eq!(
            Point::delta_ratio(p(0, 0), p(10, 0), p(0, 0), p(0, 10)),
            Some(0.0),
            "正交无分量"
        );
        let c = Color::rgb(0, 0, 0);
        let w = Color::rgb(255, 255, 255);
        assert_eq!(Color::delta_ratio(c, w, w, c), Some(-1.0));
    }

    #[test]
    fn geometry_lerp_rounds() {
        let r = Rect::new(0, 0, 10, 10).lerp(Rect::new(10, 20, 30, 40), 0.5);
        assert_eq!(r, Rect::new(5, 10, 20, 25));
        assert_eq!(
            Point::new(0, 0).lerp(Point::new(3, -3), 0.5),
            Point::new(2, -2)
        );
    }

    /// Reduced 只关位移类：标了 `movement` 的瞬时到位，颜色类照常过渡。
    #[test]
    fn reduced_snaps_only_movement() {
        set_motion_level(MotionLevel::Reduced);
        set_time_scale(1.0);
        set_clock(0);
        let mut pos = Transition::new(0.0f32).movement();
        let mut col = Transition::new(0.0f32);
        pos.retarget(100.0, 200, Easing::Linear);
        col.retarget(100.0, 200, Easing::Linear);
        pos.retarget_spring(50.0, Spring::SMOOTH);
        set_clock(100);
        assert_eq!(pos.value(), 50.0, "位移类在 Reduced 下瞬时到位");
        assert!(!pos.is_active());
        assert!((col.value() - 50.0).abs() < 1e-3, "颜色类照常过渡");
        assert!(col.is_active());
        assert!(enabled() && !movement_enabled());
        set_motion_level(MotionLevel::Full);
    }

    #[test]
    fn time_scale_stretches_transitions() {
        set_motion_level(MotionLevel::Full);
        set_time_scale(4.0);
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget(100.0, 200, Easing::Linear);
        set_clock(400); // 名义 100ms → 半程
        assert!(
            (tr.value() - 50.0).abs() < 1e-3,
            "放慢 4 倍后 400ms 应在半程"
        );
        let mut sp = Transition::new(0.0f32);
        sp.retarget_spring(100.0, Spring::SMOOTH);
        set_time_scale(1.0);
        set_clock(400);
        let mut fast = Transition::new(0.0f32);
        fast.retarget_spring(100.0, Spring::SMOOTH);
        set_clock(400 + 50);
        let v_fast = fast.value();
        set_clock(400 + 200); // 弹簧放慢 4 倍：真实 200ms = 名义 50ms
        assert!(
            (sp.value() - v_fast).abs() < 0.5,
            "放慢的弹簧真实 200ms 应等于名义 50ms"
        );
        assert_eq!(clamp_time_scale(f32::NAN), 1.0);
        assert_eq!(clamp_time_scale(0.0), 0.01);
    }

    #[test]
    fn pinned_clock_overrides_host_writes() {
        pin_clock(Some(1234));
        set_clock_ms(99);
        assert_eq!(clock_ms(), 1234, "钉住后宿主写入应被替换");
        pin_clock(None);
        set_clock_ms(99);
        assert_eq!(clock_ms(), 99);
    }

    /// 判停契约：到 `settle_ms` 时位移已落进 EPS，**且此后不再出来**（否则末帧落定会跳一下）。
    /// 期望值来自物理定义而非实现：扫阻尼三种形态 × 初速度两个方向。
    #[test]
    fn spring_settle_time_is_final() {
        for damping in [0.1, 0.6, 0.85, 1.0, 1.5, 2.0] {
            for v0 in [0.0, 0.02, -0.02, 0.2] {
                let sp = Spring {
                    response_ms: 300.0,
                    damping,
                };
                let ts = sp.settle_ms(v0);
                assert!(
                    ts < Spring::SETTLE_LIMIT_MS,
                    "ζ={damping} v0={v0} 应在上限内停稳"
                );
                let mut t = ts;
                while t < ts + 3_000.0 {
                    let off = (1.0 - sp.progress(t, v0)).abs();
                    assert!(
                        off <= Spring::EPS * 1.01,
                        "ζ={damping} v0={v0}：判停于 {ts}ms，但 {t}ms 时仍偏 {off}"
                    );
                    t += 2.0;
                }
            }
        }
    }

    /// 过阻尼：不过冲、比临界阻尼慢（更"肉"）。
    #[test]
    fn overdamped_spring_is_slower_and_monotone() {
        let crit = Spring {
            response_ms: 300.0,
            damping: 1.0,
        };
        let over = Spring {
            response_ms: 300.0,
            damping: 2.0,
        };
        let mut prev = 0.0;
        for i in 1..200 {
            let p = over.progress(i as f32 * 5.0, 0.0);
            assert!(p >= prev - 1e-6 && p <= 1.0 + 1e-6, "过阻尼应单调且不过冲");
            prev = p;
        }
        assert!(over.progress(150.0, 0.0) < crit.progress(150.0, 0.0));
        assert!(over.settle_ms(0.0) > crit.settle_ms(0.0));
    }

    /// 时间曲线中途改成弹簧也继承速度（hover 进场用曲线、被打断后用弹簧收的组合）。
    #[test]
    fn tween_to_spring_inherits_velocity() {
        set_motion_level(MotionLevel::Full);
        set_time_scale(1.0);
        set_clock(0);
        let mut tr = Transition::new(0.0f32);
        tr.retarget(100.0, 200, Easing::Linear); // 速度 0.5/ms
        set_clock(100);
        tr.retarget_spring(0.0, Spring::SMOOTH);
        set_clock(108);
        assert!(
            tr.value() > 50.0,
            "继承了向上的速度，掉头后短时间内仍应上冲，实得 {}",
            tr.value()
        );
    }

    /// 二维值的弹簧改向：值连续，且只继承与新方向共线的那部分速度。
    #[test]
    fn point_spring_retarget_is_continuous() {
        set_motion_level(MotionLevel::Full);
        set_time_scale(1.0);
        set_clock(0);
        let mut tr = Transition::new(Point::new(0, 0));
        tr.retarget_spring(Point::new(100, 0), Spring::SMOOTH);
        set_clock(80);
        let at = tr.value();
        tr.retarget_spring(Point::new(at.x, 100), Spring::SMOOTH); // 转成纯竖直
        assert_eq!(tr.value(), at, "改向瞬间值连续");
        set_clock(96);
        assert_eq!(tr.value().x, at.x, "原水平速度与新方向正交，不应继承");
        assert!(tr.value().y > 0);
    }

    /// `settle` 保留位移标记：Reduced 下随后的改向仍瞬时到位。
    #[test]
    fn settle_keeps_movement_flag() {
        set_motion_level(MotionLevel::Reduced);
        set_clock(0);
        let mut tr = Transition::new(0.0f32).movement();
        tr.settle(10.0);
        assert_eq!(tr.value(), 10.0);
        tr.retarget(50.0, 200, Easing::Linear);
        assert_eq!(tr.value(), 50.0, "settle 后标记仍在，Reduced 下应瞬时到位");
        set_motion_level(MotionLevel::Full);
    }
}
