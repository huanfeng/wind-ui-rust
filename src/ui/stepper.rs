//! 数字步进 Stepper：`[−] 数值 [+]`，绑定 `Signal<f64>`，带范围/步长钳制。
//!
//! **中部是一个真正的 [`TextInput`]**，不是自绘文本。选中、拖选、双击选词、Ctrl+A/C/X/V、
//! 右键菜单、输入法这些能力全部直接继承，不必在这里重造一遍——旧实现自绘那份只支持
//! 逐字符增删与左右移光标，连"选中复制"都做不到。
//!
//! 于是本控件是**复合**的：一个行容器带三个子节点。
//!
//! ```text
//! ┌ StepperFrame（底 + 边框 + 两条分隔线；焦点态由中部报上来）
//! │ [StepperButton −] [NumberField（TextInput 包装）] [StepperButton +]
//! └
//! ```
//!
//! 三者共享同一份 [`NumSpec`]（范围/步长/小数位）与两个信号：`value: Signal<f64>` 是
//! 对外契约，`text: Signal<String>` 是输入框的正文。两者的同步收口在
//! [`NumberField::on_update`]，单向优先——`value` 变则重排文本，否则文本变则回写 `value`。
//! 反过来让两边都能随时改对方会互相追尾，打字打到一半被自己格式化过的串顶掉。

use std::cell::Cell;
use std::rc::Rc;

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::{signal, Signal};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;
use crate::ui::inputs::TextInput;
use crate::ui::Element;

/// 左右按钮区宽度。
const BTN_W: i32 = 30;
/// 控件默认宽度（沿用旧 `Stepper::measure` 的固有宽，未显式 `.width()` 的调用点不变形）。
const DEFAULT_W: i32 = 120;

/// 长按首次触发重复前的等待时间（ms）。
const REPEAT_DELAY_MS: u64 = 400;
/// 第一加速阈值：超过此时长后进入中速重复（ms）。
const REPEAT_ACCEL1_MS: u64 = 1000;
/// 第二加速阈值：超过此时长后进入高速重复（ms）。
const REPEAT_ACCEL2_MS: u64 = 2000;
/// 初速重复间隔（ms，elapsed < REPEAT_ACCEL1_MS）。
const REPEAT_INTERVAL_SLOW_MS: u64 = 80;
/// 中速重复间隔（ms，elapsed < REPEAT_ACCEL2_MS）。
const REPEAT_INTERVAL_MID_MS: u64 = 50;
/// 高速重复间隔（ms，elapsed ≥ REPEAT_ACCEL2_MS）。
const REPEAT_INTERVAL_FAST_MS: u64 = 30;

/// `press_start_ms` 的哨兵：按下事件只标记「已按下」，真实起点留给首帧 paint 写入。
///
/// `anim::clock_ms()` 是**帧时钟**，仅在 `render` 里刷新；控件空闲不出帧时它冻结在上一帧。
/// 事件分发早于本帧 render，故在 `on_event` 里读它拿到的是「上一帧几点」而非「现在几点」，
/// 两次点击之间的静默期会被整段算进长按时长，导致按下即判定为已长按数秒、直接跳进高速档。
const PRESS_START_PENDING: u64 = u64::MAX;

fn repeat_interval_ms(elapsed_ms: u64) -> u64 {
    if elapsed_ms < REPEAT_ACCEL1_MS {
        REPEAT_INTERVAL_SLOW_MS
    } else if elapsed_ms < REPEAT_ACCEL2_MS {
        REPEAT_INTERVAL_MID_MS
    } else {
        REPEAT_INTERVAL_FAST_MS
    }
}

/// 推进长按状态机，返回本帧是否应步进。
///
/// 按下后的首帧（`press_start` 为哨兵）只把起点锚到当前帧时钟、不步进；其后先过
/// `REPEAT_DELAY_MS` 等待期，再看距上次步进是否够一个（随时长加速的）间隔。
fn advance_repeat(now_ms: u64, press_start: &Cell<u64>, last_step: &Cell<u64>) -> bool {
    if press_start.get() == PRESS_START_PENDING {
        press_start.set(now_ms);
        last_step.set(now_ms);
        return false;
    }
    let elapsed = now_ms.saturating_sub(press_start.get());
    if elapsed >= REPEAT_DELAY_MS
        && now_ms.saturating_sub(last_step.get()) >= repeat_interval_ms(elapsed)
    {
        last_step.set(now_ms);
        return true;
    }
    false
}

fn hover_amt(cell: &Cell<Transition<f32>>, on: bool) -> f32 {
    let mut tr = cell.get();
    let target = if on { 1.0 } else { 0.0 };
    if tr.target() != target {
        tr.retarget(target, crate::theme::current().anim.fast(), Easing::EaseOut);
    }
    let v = tr.animate();
    cell.set(tr);
    v
}

// ---------------- 数值规格 ----------------

/// 范围 / 步长 / 显示小数位。三个部件各持一份（`Copy`），保证格式化与钳制口径一致。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NumSpec {
    min: f64,
    max: f64,
    step: f64,
    decimals: usize,
}

impl NumSpec {
    /// 规范化入参：步长取绝对值（0 视作 1），min/max 反了就换过来，小数位由步长推断。
    fn new(min: f64, max: f64, step: f64) -> Self {
        let step = if step.abs() < 1e-12 { 1.0 } else { step.abs() };
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        let mut decimals = 0;
        let mut s = step;
        while decimals < 6 && (s - s.round()).abs() > 1e-9 {
            s *= 10.0;
            decimals += 1;
        }
        Self {
            min,
            max,
            step,
            decimals,
        }
    }

    fn clamp(&self, v: f64) -> f64 {
        v.clamp(self.min, self.max)
    }

    fn format(&self, v: f64) -> String {
        format!("{:.*}", self.decimals, v)
    }

    /// 这串正文能否**留在输入框里**（不要求它已经是个完整数字）。
    ///
    /// 判据比 `parse::<f64>()` 宽：编辑途中必然经过 `""`、`"-"`、`"1."` 这些半成品，
    /// 用 parse 当门禁会让"删光重打"和"打小数点"直接卡死。真正的取值合法性交给
    /// [`Self::parse`] 与提交时的钳制。
    fn accepts(&self, s: &str) -> bool {
        let body = match s.strip_prefix('-') {
            // 负号只在允许负值时放行，且只能打头（`body` 里再出现即非法）。
            Some(rest) if self.min < 0.0 => rest,
            Some(_) => return false,
            None => s,
        };
        if body.contains('-') {
            return false;
        }
        let dots = body.chars().filter(|c| *c == '.').count();
        if dots > usize::from(self.decimals > 0) {
            return false;
        }
        body.chars().all(|c| c.is_ascii_digit() || c == '.')
    }

    /// 正文 → 数值。半成品（空串 / `-` / `.`）返回 `None`，表示"这一刻没有可用值"。
    fn parse(&self, s: &str) -> Option<f64> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        t.parse::<f64>().ok().filter(|v| v.is_finite())
    }
}

// ---------------- 共享状态 ----------------

/// 三个部件之间的窄通道。都是「一方写、另一方下一帧读」的单向标志，故用 `Cell` 不用信号：
/// 它们是控件内部装配细节，不该占用信号运行时的槽位，也不该被外部观察到。
#[derive(Default)]
struct Shared {
    /// 中部输入框是否持有焦点——外框据此把边框画成 accent 色。
    /// 由 `NumberField::paint` 写、`StepperFrame::paint` 读。
    focused: Cell<bool>,
    /// `value` / `text` 上次同步时的版本号。**放在共享区而不是 `NumberField` 里，
    /// 是因为写这两个信号的不止一个人**——± 按钮也写。
    ///
    /// 记账跟着写走：谁写完谁把版本记上，`on_update` 于是只会看见"没记过账的那次写"，
    /// 单向优先规则（`value` 变则重排文本，否则文本变则回写 `value`）才自洽。
    /// 记账留在 `NumberField` 里的话，「点一次 + 紧接着打字」会被判成两边都变、
    /// 按 `value` 优先，把用户刚打进去的字冲掉。
    seen_value: Cell<u64>,
    seen_text: Cell<u64>,
    /// Escape 要回退到的值——「本轮键入开始前」的基线。
    ///
    /// 获得焦点时置为当时的值；此后**每次步进（± 按钮或方向键）都重设它**。
    /// 步进是一个当场落地的动作，不属于"还没提交的键入"，Escape 不该把它一起吃掉：
    /// 聚焦 → 点三次 + → 打错一个字 → Escape，用户要的是撤销那个字，不是退回三次点击之前。
    /// 放在 `Shared` 里而不是 `NumberField` 内，正是因为按钮也要重设它。
    edit_origin: Cell<f64>,
    /// 整个复合控件的最近一次 bounds（逻辑坐标），由 `StepperFrame::paint` 写。
    ///
    /// 给各子节点互相报脏区用：谁改了共享的 `value`/`text`，要重绘的往往在**兄弟节点**
    /// 上（按钮改数字、中部改 ± 的置灰态），自己的 bounds 盖不住。
    ///
    /// **paint 期读到的必是同帧新鲜值**，理由是结构性的而非"布局恰好没变"：
    /// `StepperFrame` 是 row 节点**自身**的 widget，± 与中部是它的**子节点**，而
    /// `Tree::paint_node` 恒先画自身 widget 再递归子树；剪枝只跳过自绘、子树照常递归，
    /// 但外框的 bounds **包含**每个子节点的 bounds，对父的剔除判定恒不严于对子。
    /// 于是只要任一子节点的 `paint` 跑了，本帧外框的 `paint` 一定已经跑过。
    /// 首帧、resize 后的第一帧、局部帧，三种情形都不例外。
    ///
    /// 事件期（`on_event`）读到的则是上一帧的值：布局未变时精确，而会改变布局的操作
    /// 本身走整窗帧，盖得住。
    whole: Cell<Rect>,
}

impl Shared {
    /// 记账：把两个信号的当前版本记下，声明「这次改动已经处理过了」。
    ///
    /// 每一处写 `value` 或 `text` 的地方**写完都要调它**，否则那次写会被
    /// `NumberField::on_update` 当成"没处理过的外部改动"再处理一遍。
    fn mark_synced(&self, value: Signal<f64>, text: Signal<String>) {
        self.seen_value.set(value.version());
        self.seen_text.set(text.version());
    }
}

// ---------------- 外框 ----------------

/// 复合外框：底色、边框、两条分隔线。不参与交互（子节点各管各的命中）。
struct StepperFrame {
    shared: Rc<Shared>,
}

// 刻意不实现 `measure`：`Layout::Linear` 走 `Tree::measure_linear`，它只累加子节点，
// **从不调用容器自身 widget 的 `measure`**（只有 `Layout::None` 的叶子才调）。写在这里
// 的任何固有尺寸都是死代码。行的高度实际来自 `StepperButton` 与 `TextInput` 的 measure。
impl Widget for StepperFrame {
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
        // 记下整体矩形供 ± 按钮报脏区（见 `Shared::whole`）。
        self.shared.whole.set(bounds);
        let th = crate::theme::current();
        let (pal, st) = (&th.palette, &th.stepper);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let corner = th.metrics.corner_md;
        let bg = if enabled { st.bg(pal) } else { pal.surface_alt };
        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(bg));

        // 焦点在中部输入框身上，本节点自己永远拿不到焦点，故读共享标志。
        let border = if self.shared.focused.get() {
            pal.accent
        } else {
            st.border(pal)
        };
        let bw = th.metrics.border_width.to_logical(canvas.dpi_scale());
        canvas.stroke_round_rect(x, y, w, h, corner, bw, &Paint::fill(border));

        let div = Paint::fill(st.border(pal));
        for dx in [BTN_W, bounds.w - BTN_W] {
            canvas.draw_line(
                (bounds.x + dx) as f32,
                y + 4.0,
                (bounds.x + dx) as f32,
                y + h - 4.0,
                1.0,
                &div,
            );
        }
    }
}

// ---------------- ± 按钮 ----------------

/// 一侧的步进按钮。`dir` 为 -1（减）或 +1（加）。
///
/// 刻意**不可聚焦**：整个 Stepper 对 Tab 只占一个焦点位，且那一位归中部输入框
/// ——否则每个数字框都要按三次 Tab 才能跨过去。
struct StepperButton {
    dir: i8,
    value: Signal<f64>,
    text: Signal<String>,
    spec: NumSpec,
    shared: Rc<Shared>,
    hover: bool,
    hover_amt: Cell<Transition<f32>>,
    /// 长按中。
    pressed: Cell<bool>,
    /// 长按起点（ms）；`PRESS_START_PENDING` 表示待首帧 paint 用新鲜帧时钟初始化。
    press_start_ms: Cell<u64>,
    /// 上次重复步进的时钟（ms）。
    last_step_ms: Cell<u64>,
}

impl StepperButton {
    fn new(
        dir: i8,
        value: Signal<f64>,
        text: Signal<String>,
        spec: NumSpec,
        shared: Rc<Shared>,
    ) -> Self {
        Self {
            dir,
            value,
            text,
            spec,
            shared,
            hover: false,
            hover_amt: Cell::new(Transition::new(0.0)),
            pressed: Cell::new(false),
            press_start_ms: Cell::new(0),
            last_step_ms: Cell::new(0),
        }
    }

    /// 走一步：`value` 与 `text` 一起写，并**当场记账**（见 `Shared::seen_value`）。
    ///
    /// 文本不能等 `on_update` 去排——长按的重复步进跑在 `paint` 里，那条路不置
    /// `needs_relayout`，下一帧的 `layout_root` 整个会被跳过，`on_update` 自然也不跑。
    /// 只写 `value` 的话，长按期间数字会一直定在原地，松手那次点击才跳到终值。
    fn step(&self) {
        let v = self
            .spec
            .clamp(self.value.get() + self.dir as f64 * self.spec.step);
        if v != self.value.get() {
            self.value.set(v);
        }
        let s = self.spec.format(v);
        if self.text.with(|t| *t != s) {
            self.text.set(s);
        }
        self.shared.mark_synced(self.value, self.text);
        // 步进当场落地，Escape 的基线随之前移（见 `Shared::edit_origin`）。
        self.shared.edit_origin.set(v);
    }

    /// 到头了（减到 min / 加到 max）：字形置灰，与旧实现一致。
    fn at_limit(&self) -> bool {
        if self.dir < 0 {
            self.value.get() <= self.spec.min
        } else {
            self.value.get() >= self.spec.max
        }
    }
}

impl Widget for StepperButton {
    fn measure(&self, _avail: Size, style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(BTN_W, (style.font_size as i32) + 16)
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        // 长按重复步进（retarget-in-paint 驱动，与 hover 动画同一帧循环）。
        if self.pressed.get() {
            // 帧时钟此刻刚由宿主刷新，与「现在」同步；按下后的首帧据此锚定长按起点。
            let now = crate::anim::clock_ms();
            if advance_repeat(now, &self.press_start_ms, &self.last_step_ms) {
                self.step();
                // 脏区必须自报成**整个复合控件**。`step()` 改的是共享的 `value`/`text`，
                // 而数字显示在**兄弟节点**（中部输入框）上；`Signal::set` 在非事件期
                // 自动走的 `request_repaint()` 只把脏区归到当前绘制节点、也就是本按钮，
                // 局部帧于是只重画按钮——长按期间数字定在原地，直到松手那一下走事件
                // 路径（脏区由 `ctx.mark_dirty()` 正确给出）才跳到终值。
                //
                // 与 `NumberField::paint` 里焦点变色那处同因同治，只是那边脏的是外框。
                // `whole` 由外框写，而外框是本节点的父 widget、恒先于本节点绘制
                // （见 `Shared::whole`），故这里读到的是本帧新鲜值。
                let whole = self.shared.whole.get();
                if !whole.is_empty() {
                    crate::anim::request_repaint_in(whole);
                }
            }
            crate::anim::request_repaint();
        }

        let th = crate::theme::current();
        let (pal, st) = (&th.palette, &th.stepper);
        let corner = th.metrics.corner_md;

        let amt = hover_amt(&self.hover_amt, enabled && self.hover);
        if amt > 0.0 {
            // 内缩 1px 让底色不盖住外框那条描边。
            canvas.fill_round_rect(
                bounds.x as f32 + 1.0,
                bounds.y as f32 + 1.0,
                bounds.w as f32 - 2.0,
                bounds.h as f32 - 2.0,
                corner,
                &Paint::fill(st.button_hover(pal).scale_alpha(amt)),
            );
        }

        let color = if !enabled || self.at_limit() {
            pal.text_disabled
        } else {
            st.button(pal)
        };
        let glyph = if self.dir < 0 { "\u{2212}" } else { "+" };
        let ts = &crate::text::TextStyle::of(style);
        canvas.draw_text(glyph, bounds, color, Align::Center, ts);
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else {
            return false;
        };
        match p.kind {
            PointerKind::Enter | PointerKind::Move => {
                if !self.hover {
                    self.hover = true;
                    ctx.mark_dirty();
                }
                true
            }
            PointerKind::Leave => {
                if self.hover {
                    self.hover = false;
                    ctx.mark_dirty();
                }
                true
            }
            PointerKind::Down => {
                // **刻意不 `request_focus`**：调值和编辑是两件事，点 ± 只该改数字，
                // 不该把光标拽过来开始闪。想编辑就直接点中间那格。
                //
                // 「不请求」就等于「不聚焦」——宿主每次按下都重新裁决焦点，没有节点
                // 认领就清空（`apply_dispatch_effects` 的 blur 分支），本控件不必插手。
                self.step();
                ctx.capture();
                self.pressed.set(true);
                // 起点不在此处取：事件路径读到的帧时钟是陈旧的（见 PRESS_START_PENDING）。
                self.press_start_ms.set(PRESS_START_PENDING);
                ctx.mark_dirty();
                true
            }
            PointerKind::Up => {
                if self.pressed.get() {
                    self.pressed.set(false);
                    ctx.release_capture();
                    ctx.mark_dirty();
                }
                true
            }
            _ => false,
        }
    }

    fn reset_interaction(&mut self) {
        self.hover = false;
        self.pressed.set(false);
        self.hover_amt.set(Transition::new(0.0));
    }
}

// ---------------- 中部数值框 ----------------

/// [`TextInput`] 的透明包装：把每个 `Widget` 方法原样转发下去，只在四处插手。
///
/// 用「同一个节点上包一层」而不是「直接放一个 TextInput 子节点」，是因为包装层需要一样
/// 只有节点自己才有的东西：`paint` 的 `focused` 参数——外框要据此变色，而外框自己
/// 永远拿不到焦点。转发是逐方法的，命中/坐标/输入法全都还在同一个节点上，
/// 不存在坐标换算——`TextInput` 的一切内部假设照旧成立。
struct NumberField {
    inner: TextInput,
    value: Signal<f64>,
    text: Signal<String>,
    spec: NumSpec,
    shared: Rc<Shared>,
    /// 上一帧的焦点态：用来识别「刚获得焦点」与「刚失去焦点」两个瞬间。
    was_focused: Cell<bool>,
}

impl NumberField {
    fn new(value: Signal<f64>, text: Signal<String>, spec: NumSpec, shared: Rc<Shared>) -> Self {
        let mut inner = TextInput::new(text, String::new());
        {
            let cfg = inner.config_mut();
            cfg.frameless = true; // 外框由 StepperFrame 统一画，这里再画一层就是双线
            cfg.align = Align::Center;
        }
        inner.set_filter(move |s| spec.accepts(s));
        Self {
            inner,
            value,
            text,
            spec,
            shared,
            was_focused: Cell::new(false),
        }
    }

    /// 把正文规整成 `value` 的标准写法（提交/失焦时用）。
    ///
    /// 半成品正文（空串、`-`、`1.`）解析不出值——此时**保留 `value` 不动**，只把文本
    /// 排回去，等价于旧实现"非法则保留原值"。
    fn normalize(&self) {
        if let Some(v) = self.text.with(|t| self.spec.parse(t)) {
            let c = self.spec.clamp(v);
            if c != self.value.get() {
                self.value.set(c);
            }
        }
        let s = self.spec.format(self.value.get());
        if self.text.with(|t| *t != s) {
            self.text.set(s);
        }
        self.shared.mark_synced(self.value, self.text);
        // 提交即落地：Escape 的基线随之前移，否则「Enter 定了一次、再改、再 Escape」
        // 会越过那次提交、一路退回聚焦那一刻的值（与 ± 步进同理，见 `Shared::edit_origin`）。
        self.shared.edit_origin.set(self.value.get());
    }

    /// `value` → `text` 的兜底同步，每帧 paint 都走一遍。
    ///
    /// 光靠 `on_update` 覆盖不全，它有两种跑不到的帧：
    /// - **本节点被禁用**——`Tree::call_on_update` 开头就 `if !node_enabled { return }`。
    ///   置灰期间外部写 `value`，框里会一直显示禁用那一刻的旧数字（旧的自绘实现是
    ///   每帧现读 `value`，没有这个问题，故这属于回归，必须补）。
    /// - **没有触发重排的帧**——`on_update` 只在 `layout_root` 前广播。
    ///
    /// 只做 `value` → `text` 这一个方向：反向那条需要用户键入，而键入必然伴随事件分发
    /// 与重排，`on_update` 一定跑得到。有记账守卫，值没变时不写信号，不会自激重绘。
    fn sync_text_from_value(&self) {
        if self.value.version() == self.shared.seen_value.get() {
            return;
        }
        let s = self.spec.format(self.value.get());
        if self.text.with(|t| *t != s) {
            self.text.set(s);
            // 同族：数字本身画在本节点上会跟着刷新，但 ± 的置灰态在兄弟节点上。
            // 这里在 paint 内，`Signal::set` 自动走的 `request_repaint()` 只脏本节点。
            let whole = self.shared.whole.get();
            if !whole.is_empty() {
                crate::anim::request_repaint_in(whole);
            }
        }
        self.shared.mark_synced(self.value, self.text);
    }

    /// 键盘步进：改值、重排文本、全选——接着键入即整体替换，与原生数字框一致。
    fn step(&mut self, ctx: &mut EventCtx, dir: f64) {
        let v = self.spec.clamp(self.value.get() + dir * self.spec.step);
        if v != self.value.get() {
            self.value.set(v);
        }
        let s = self.spec.format(v);
        if self.text.with(|t| *t != s) {
            self.text.set(s);
        }
        self.shared.mark_synced(self.value, self.text);
        // 步进当场落地，Escape 的基线随之前移（见 `Shared::edit_origin`）。
        self.shared.edit_origin.set(v);
        self.inner.select_all();
        // 脏区要盖住**整个复合控件**：改的是共享的 `value`，而 ± 的置灰态
        // （`StepperButton::at_limit`）画在**兄弟节点**上。`ctx.mark_dirty()` 给的只是
        // 本节点的 `visual_bounds`，而键盘事件的失效升级也只到 `DamageReq::Rect(本节点)`，
        // 不像指针那样升整窗——于是按 ↑ 顶到 max 时数字到位了，「+」却一直不变灰，
        // 要等下一次整窗帧（切页/弹对话框/换主题）才碰巧补齐。
        //
        // 事件期读到的 `whole` 是上一帧 paint 写的：布局未变时精确，而会改变布局的
        // 操作（resize/DPI 切换）本身就走整窗帧，盖得住。
        let whole = self.shared.whole.get();
        if whole.is_empty() {
            ctx.mark_dirty();
        } else {
            ctx.mark_dirty_rect(whole);
        }
    }
}

impl Widget for NumberField {
    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        self.inner.measure(avail, style, text)
    }

    fn paint(
        &self,
        bounds: Rect,
        content: Rect,
        focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let was = self.was_focused.get();
        if focused && !was {
            // 记下本轮键入的基线，供 Escape 回退。
            self.shared.edit_origin.set(self.value.get());
        } else if was && !focused {
            // 失焦即提交：paint 的 focused 参数是感知焦点切换的最早时机（旧实现同此）。
            self.normalize();
        }
        self.was_focused.set(focused);

        // 外框在本节点**之前**绘制，读到的是上一帧的值；焦点刚变的那一帧要多出一帧
        // 才能把边框刷成对的颜色，故这里主动续一帧。只在真的变了时续，不留常驻帧。
        //
        // 脏区必须自报成**整个复合控件**：`request_repaint()` 把脏区归到当前绘制节点，
        // 也就是中部这一段，而要改色的是外框那一圈。局部帧以脏区为裁剪重绘，
        // 于是边框只有中段刷成 accent、± 按钮上下那两截仍是旧色，左右竖边永远不变
        // ——直到下一次整窗帧（切页/弹对话框/换主题）才碰巧补齐。
        if self.shared.focused.get() != focused {
            self.shared.focused.set(focused);
            // 读 `Shared::whole` 这个权威值，不再按「左右各一个 BTN_W」现算：那份算法
            // 把布局假设复制了一份，一旦排布改成别的样式（两个按钮都在右侧的 spinner）
            // 就会算错，而 `whole` 总是对的。为空时才回退现算。
            let whole = self.shared.whole.get();
            crate::anim::request_repaint_in(if whole.is_empty() {
                Rect::new(bounds.x - BTN_W, bounds.y, bounds.w + 2 * BTN_W, bounds.h)
            } else {
                whole
            });
        }

        self.sync_text_from_value();
        self.inner
            .paint(bounds, content, focused, enabled, canvas, style);
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Key(k) = ev {
            if k.pressed && !k.ctrl {
                match k.key {
                    Key::Up => {
                        self.step(ctx, 1.0);
                        return true;
                    }
                    Key::Down => {
                        self.step(ctx, -1.0);
                        return true;
                    }
                    Key::Enter => {
                        // 没有未提交的改动就**不消费**，让 Enter 冒泡出去触发对话框的
                        // 默认按钮。无条件吞掉的话，Tab 到数字框、什么都没改按回车，
                        // 「确定」就再也按不动了。
                        //
                        // 但基线照样要前移：正文碰巧已是标准写法（键入的值本就规范）
                        // 时走的正是这条早退，漏掉的话 Escape 会越过这次回车退到更早。
                        if self.text.with(|t| *t == self.spec.format(self.value.get())) {
                            self.shared.edit_origin.set(self.value.get());
                            return false;
                        }
                        self.normalize();
                        self.inner.select_all();
                        ctx.mark_dirty();
                        return true;
                    }
                    Key::Escape => {
                        // 撤销整轮编辑。没改过就不消费——让 Escape 继续冒泡去关对话框，
                        // 否则数字框一聚焦，Escape 就成了黑洞。
                        let origin = self.shared.edit_origin.get();
                        let s = self.spec.format(origin);
                        if self.value.get() == origin && self.text.with(|t| *t == s) {
                            return false;
                        }
                        self.value.set(origin);
                        self.text.set(s);
                        self.shared.mark_synced(self.value, self.text);
                        self.inner.select_all();
                        ctx.mark_dirty();
                        return true;
                    }
                    _ => {}
                }
            }
        }
        self.inner.on_event(ctx, ev)
    }

    /// `value` ↔ `text` 同步。单向优先：`value` 变过就以它为准重排文本（外部写入、
    /// ± 按钮），否则才把用户打的字回写成 `value`。两边同时生效会互相追尾。
    fn on_update(&mut self, _ctx: &mut EventCtx) {
        if self.value.version() != self.shared.seen_value.get() {
            let s = self.spec.format(self.value.get());
            if self.text.with(|t| *t != s) {
                self.text.set(s);
            }
            self.shared.mark_synced(self.value, self.text);
            return;
        }

        if self.text.version() != self.shared.seen_text.get() {
            // 编辑途中**只回写不重排**：此刻把 `value` 格式化回文本框，正在打的
            // "1." 会被顶成 "1"，小数点永远打不进去。规整留到 Enter / 失焦。
            if let Some(v) = self.text.with(|t| self.spec.parse(t)) {
                let c = self.spec.clamp(v);
                if c != self.value.get() {
                    self.value.set(c);
                }
            }
            self.shared.mark_synced(self.value, self.text);
        }
    }

    // ---- 以下纯转发 ----

    fn focusable(&self) -> bool {
        self.inner.focusable()
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        self.inner.as_any_mut()
    }
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        self.inner.ime_caret()
    }
    fn set_composing(&mut self, composing: bool) {
        self.inner.set_composing(composing);
    }
    fn set_preedit(&mut self, pe: &crate::event::Preedit) {
        self.inner.set_preedit(pe);
    }
    fn selection_range(&self) -> Option<(usize, usize)> {
        self.inner.selection_range()
    }
    fn ime_text(&self) -> Option<String> {
        self.inner.ime_text()
    }
    fn wants_right_click(&self) -> bool {
        self.inner.wants_right_click()
    }
    fn cursor(&self) -> CursorShape {
        self.inner.cursor()
    }
    fn take_click(&mut self, f: crate::core::ClickFn) {
        self.inner.take_click(f);
    }
    fn reset_interaction(&mut self) {
        // 一并清掉焦点记忆：隐藏期间节点不 paint，留着 `true` 会让重新显示的第一帧
        // 被判成「刚失焦」而跑一次提交。当前可自愈（值早已由 on_update 钳过），
        // 但让状态跟着复位走更省心。
        self.was_focused.set(false);
        self.inner.reset_interaction();
    }
}

// ---------------- 装配 ----------------

/// 组装 `[−] 数值 [+]`（见 [`crate::ui::Element::stepper`]）。
pub(crate) fn build(value: Signal<f64>, min: f64, max: f64, step: f64) -> Element {
    let spec = NumSpec::new(min, max, step);
    // 初值先钳进范围：越界的初值若原样显示，用户不动它就一直是个非法值。
    let v0 = spec.clamp(value.get());
    if v0 != value.get() {
        value.set(v0);
    }
    let text = signal(spec.format(v0));
    // 基线先给初值：正常路径下聚焦/步进都会重设它，但「从未聚焦过就收到 Escape」
    // 这条路上没人写过，留 Default 的 0.0 会把值一把拽到 0（或钳到 min）。
    let shared = Rc::new(Shared {
        edit_origin: Cell::new(v0),
        ..Default::default()
    });
    shared.mark_synced(value, text);

    let btn = |dir: i8| {
        Element::leaf()
            .widget(StepperButton::new(dir, value, text, spec, shared.clone()))
            .width(BTN_W)
    };

    // cross=Stretch 让三个子节点各自长到行高：**不要**改用 `.height_match()`，
    // 那是 `Dimension::Match`——父行自身高度是 Wrap，父问子、子又答"跟父一样"，
    // 这一环解不开时高度会一路撑到可用空间，整个控件竖着拉满一列。
    Element::row()
        .cross(Align::Stretch)
        .width(DEFAULT_W)
        .widget(StepperFrame {
            shared: shared.clone(),
        })
        .child(btn(-1))
        .child(
            Element::leaf()
                .widget(NumberField::new(value, text, spec, shared.clone()))
                .weight(1.0)
                .reactive(),
        )
        .child(btn(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 模拟按下：起点置哨兵，交给首帧锚定。
    fn press() -> (Cell<u64>, Cell<u64>) {
        (Cell::new(PRESS_START_PENDING), Cell::new(0))
    }

    /// 回归：长按 ± 时，脏区必须覆盖**整个复合控件**，否则中部的数字不刷新。
    ///
    /// 长按的重复步进跑在 `StepperButton::paint` 里，属**非事件期**。那条路上
    /// `Signal::set` 自动走的是 `anim::request_repaint()`，它把脏区归到「当前绘制
    /// 节点」——也就是按钮自己。而数字显示在兄弟节点（中部输入框）上，于是局部帧
    /// 只重画了按钮：长按期间数字定在原地，直到松手那一下走事件路径
    /// （`ctx.mark_dirty()` 给出正确脏区）才跳到终值。
    ///
    /// 判据取「脏区宽度是否盖过整个控件」而非「是否等于某个矩形」：盖住即可，
    /// 多盖一点只是多画，少盖一点就是这个 bug。
    #[test]
    fn long_press_damage_covers_the_whole_stepper() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::{Point, Size};

        const W: i32 = 120;
        const H: i32 = 40;

        crate::anim::reset_request();
        let value = crate::signal::signal(5.0f64);
        let mut tree = crate::core::Tree::new();
        let root = crate::ui::Element::stepper(value, 0.0, 100.0, 1.0)
            .width(W)
            .height(H)
            .build(&mut tree);
        tree.root = Some(root);
        // 可用空间**正好**给到控件尺寸：row 作为根会被拉伸到可用宽度，留富余的话
        // ± 按钮的位置就要靠猜（实测给 160 时按钮跑到了 [130,160)）。
        tree.layout_root(Size::new(W, H), &mut crate::text::NullTextEngine);

        // 按在「+」按钮正中（最右 BTN_W 宽的一格）。
        let plus_x = W - BTN_W / 2;
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Down,
                Point::new(plus_x, H / 2),
                MouseButton::Left,
            ),
            &mut None,
            &mut None,
        );

        let mut pm = tiny_skia::Pixmap::new(W as u32, H as u32).unwrap();
        let paint_frame = |ms: u64, pm: &mut tiny_skia::Pixmap| {
            crate::anim::reset_request();
            crate::anim::set_clock_ms(ms);
            let mut canvas = crate::render::SkiaCanvas::new(pm);
            tree.paint(&mut canvas);
        };

        // 首帧只锚定长按起点，不步进。
        paint_frame(1_000, &mut pm);
        let before = value.get();

        // 越过等待期后的一帧：应当步进，且脏区盖住整个控件。
        paint_frame(
            1_000 + REPEAT_DELAY_MS + REPEAT_INTERVAL_SLOW_MS + 10,
            &mut pm,
        );
        assert!(
            value.get() > before,
            "前提不成立：越过等待期后应当步进（{before} -> {}）",
            value.get()
        );

        match crate::anim::take_damage() {
            crate::anim::Damage::Full => {} // 整窗重绘自然盖得住
            crate::anim::Damage::Rect(r) => {
                assert!(
                    r.x <= 0 && r.x + r.w >= W,
                    "脏区只盖住了按钮而非整个控件：{r:?}（控件横跨 0..{W}）——\
                     中部数字将不会重绘，长按时数字定在原地"
                );
            }
            crate::anim::Damage::None => {
                panic!("长按步进后没有任何脏区，数字不可能刷新");
            }
        }
    }

    /// `Shared::whole` 必须是**同帧**写入、且写在子节点读它之前——布局变了也不例外。
    ///
    /// 上面两条测试守不住这一点：它们布局静止，矩形永不变，于是把 `whole.set` 挪到
    /// 更晚绘制的节点（比如中部的 `NumberField`）照样全绿。真实后果是 resize / DPI
    /// 切换后的第一帧拿旧矩形报脏，长按期间闪一帧局部残影。
    ///
    /// 这里按的是「−」——它是 row 的**第一个**子节点，绘制早于中部与「+」。若 `whole`
    /// 改由比它更晚的节点写，它读到的就是上一帧的旧宽度，断言立刻失败。
    #[test]
    fn whole_stays_fresh_across_relayout() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::{Point, Size};

        const W1: i32 = 120;
        const W2: i32 = 220;
        const H: i32 = 40;

        crate::anim::reset_request();
        let value = crate::signal::signal(50.0f64);
        let mut tree = crate::core::Tree::new();
        let root = crate::ui::Element::stepper(value, 0.0, 100.0, 1.0)
            .height(H)
            .build(&mut tree);
        tree.root = Some(root);

        let frame = |tree: &mut crate::core::Tree, w: i32, ms: u64| {
            crate::anim::reset_request();
            crate::anim::set_clock_ms(ms);
            let mut pm = tiny_skia::Pixmap::new(w as u32, H as u32).unwrap();
            let mut canvas = crate::render::SkiaCanvas::new(&mut pm);
            tree.paint(&mut canvas);
        };

        // 窄宽度布局，按住「−」（row 的第一个子节点，绘制早于中部与「+」），
        // 并让长按先跑过等待期——关键是把 resize 后的那一帧变成**立刻步进**的帧。
        // 若等到 resize 后的第二帧才步进，上一帧已经把 whole 刷新过了，
        // 「谁来写 whole」写错也照样绿。
        tree.layout_root(Size::new(W1, H), &mut crate::text::NullTextEngine);
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Down,
                Point::new(BTN_W / 2, H / 2),
                MouseButton::Left,
            ),
            &mut None,
            &mut None,
        );
        frame(&mut tree, W1, 1_000); // 锚定长按起点
        frame(&mut tree, W1, 1_000 + REPEAT_DELAY_MS + 10); // 过等待期, 步进一次

        // 窗口变宽后重新布局——紧接着的这一帧就会步进, whole 必须已是新宽度。
        tree.layout_root(Size::new(W2, H), &mut crate::text::NullTextEngine);
        frame(
            &mut tree,
            W2,
            1_000 + REPEAT_DELAY_MS + REPEAT_INTERVAL_SLOW_MS + 20,
        );

        match crate::anim::take_damage() {
            crate::anim::Damage::Full => {}
            crate::anim::Damage::Rect(r) => {
                assert!(
                    r.x <= 0 && r.x + r.w >= W2,
                    "脏区仍是重排前的宽度：{r:?}（控件已变为横跨 0..{W2}）——\
                     whole 不是同帧新鲜值, resize 后第一帧会用旧矩形报脏"
                );
            }
            crate::anim::Damage::None => panic!("长按步进后没有任何脏区"),
        }
    }

    /// 回归：键盘 ↑/↓ 步进时，脏区也必须覆盖**整个复合控件**。
    ///
    /// 与长按那条同因同族、方向相反：那边是按钮改中部的数字，这边是中部改**按钮**的
    /// 置灰态（`StepperButton::at_limit` 决定 ± 字形灰不灰，画在兄弟节点上）。
    /// `ctx.mark_dirty()` 给的是本节点的 `visual_bounds`，而键盘事件走的失效升级
    /// 只到 `DamageReq::Rect(本节点)`，不像指针那样升整窗。
    ///
    /// 可复现的用户可见症状：聚焦中部数字框、按住 ↑ 一路顶到 max，数字正常走到上限，
    /// 但「+」号**不会变灰**，一直停在正常色，直到下一次整窗帧（切页/弹对话框/换主题）
    /// 才碰巧补齐。按 ↓ 顶到 min 同理。
    #[test]
    fn keyboard_step_damage_covers_the_whole_stepper() {
        use crate::core::DamageReq;
        use crate::event::{Key, KeyEvent, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::{Point, Size};

        const W: i32 = 120;
        const H: i32 = 40;

        crate::anim::reset_request();
        // 起始值差一步到顶：按一次 ↑ 正好把「+」推进置灰态。
        let value = crate::signal::signal(99.0f64);
        let mut tree = crate::core::Tree::new();
        let root = crate::ui::Element::stepper(value, 0.0, 100.0, 1.0)
            .width(W)
            .height(H)
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(W, H), &mut crate::text::NullTextEngine);

        // 先绘制一帧：`Shared::whole` 由外框在 paint 里写，而真实场景中控件必然先画出来
        // 才谈得上被点击和按键。少了这一帧，读到的 `whole` 是空的、走回退分支，
        // 测出来的就不是真实通路。
        {
            let mut pm = tiny_skia::Pixmap::new(W as u32, H as u32).unwrap();
            let mut canvas = crate::render::SkiaCanvas::new(&mut pm);
            tree.paint(&mut canvas);
        }

        // 点中部数字框取得焦点（± 按钮刻意不请求焦点，故必须点中间那格）。
        let hit = tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Down,
                Point::new(W / 2, H / 2),
                MouseButton::Left,
            ),
            &mut None,
            &mut None,
        );
        let focus = hit.focus;
        assert!(focus.is_some(), "前提不成立：点中部应当取得焦点");

        let res = tree.dispatch_key(
            KeyEvent {
                key: Key::Up,
                pressed: true,
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
            },
            focus,
        );
        assert_eq!(value.get(), 100.0, "前提不成立：↑ 应把值推到上限");

        match res.damage {
            DamageReq::Full | DamageReq::Layout(_) => {} // 整窗/重排自然盖得住
            DamageReq::Rect(r) => {
                assert!(
                    r.x <= 0 && r.x + r.w >= W,
                    "脏区没盖住整个控件：{r:?}（控件横跨 0..{W}）——\
                     ± 按钮的置灰态画在兄弟节点上, 不重绘就一直停在旧色"
                );
            }
            DamageReq::None => panic!("键盘步进后没有任何脏区"),
        }
    }

    /// 回归：两次点击之间的静默期不得计入长按时长。
    ///
    /// 帧时钟空闲时冻结，按下事件读到的是上一帧时刻。旧实现在事件里取起点，静默 1.5s 后
    /// 再点会算出 elapsed=1500，一按下就越过等待期并直落中速档 → 「点一下跳两格 / 秒进快速加」。
    #[test]
    fn stale_frame_clock_between_clicks_does_not_trigger_repeat() {
        let (start, last) = press();
        // 上一帧停在 150ms，用户 1650ms 才按下，本帧时钟 1650。
        assert!(
            !advance_repeat(1650, &start, &last),
            "首帧只锚定起点，不得步进"
        );
        assert_eq!(start.get(), 1650, "起点须锚到当前帧而非上一帧的 150");
        // 紧接着的几帧仍在等待期内，一格都不许多跳。
        assert!(!advance_repeat(1666, &start, &last));
        assert!(!advance_repeat(1700, &start, &last));
        assert!(
            !advance_repeat(2000, &start, &last),
            "距按下 350ms，仍未过 400ms 等待期"
        );
    }

    /// 真长按：过等待期后按间隔步进，并随时长逐级加速。
    #[test]
    fn hold_repeats_after_delay_and_accelerates() {
        let (start, last) = press();
        assert!(!advance_repeat(1000, &start, &last));
        assert!(!advance_repeat(1399, &start, &last), "399ms 未到等待期");
        assert!(advance_repeat(1400, &start, &last), "400ms 起首次重复");
        // 慢速档 80ms。
        assert!(!advance_repeat(1479, &start, &last));
        assert!(advance_repeat(1480, &start, &last));
        // 距按下 >1000ms 进中速档 50ms。
        last.set(2400);
        assert!(!advance_repeat(2449, &start, &last));
        assert!(advance_repeat(2450, &start, &last));
        // 距按下 >2000ms 进高速档 30ms。
        last.set(3500);
        assert!(!advance_repeat(3529, &start, &last));
        assert!(advance_repeat(3530, &start, &last));
    }

    /// 每次按下都重新锚定，不受上一轮长按残留的起点影响。
    #[test]
    fn each_press_reanchors_start() {
        let (start, last) = press();
        advance_repeat(1000, &start, &last);
        assert!(
            advance_repeat(5000, &start, &last),
            "同一轮长按 4s 后应仍在重复"
        );
        // 松开后再次按下：置回哨兵。
        start.set(PRESS_START_PENDING);
        assert!(!advance_repeat(9000, &start, &last));
        assert_eq!(start.get(), 9000);
        assert_eq!(last.get(), 9000, "last_step 须一并重置，否则首帧即满足间隔");
    }

    /// 小数位由步长推断，格式化按此对齐（0.25 → 两位）。
    #[test]
    fn decimals_inferred_from_step() {
        assert_eq!(NumSpec::new(0.0, 10.0, 1.0).format(3.0), "3");
        assert_eq!(NumSpec::new(0.0, 3.0, 0.25).format(1.5), "1.50");
        assert_eq!(NumSpec::new(0.0, 1.0, 0.1).format(0.3), "0.3");
    }

    /// 反了的 min/max 与 0 步长都被规范化，不留下不可用的规格。
    #[test]
    fn spec_normalizes_bad_input() {
        let s = NumSpec::new(9.0, 1.0, 0.0);
        assert_eq!((s.min, s.max, s.step), (1.0, 9.0, 1.0));
        assert_eq!(s.clamp(100.0), 9.0);
        assert_eq!(s.clamp(-100.0), 1.0);
    }

    /// 准入过滤放行**编辑途中的半成品**，只挡真正打不成数字的字符。
    ///
    /// 这条正是自绘版做不到的地方：它是逐字符判定的，而 `.` 只能有一个、`-` 只能打头
    /// 这类规则本就是整串的性质。这里同一把尺子同时管住键入与粘贴。
    #[test]
    fn filter_accepts_partial_edits_but_rejects_junk() {
        let s = NumSpec::new(0.0, 10.0, 0.1);
        for ok in ["", "1", "1.", "1.5", ".5", "10"] {
            assert!(s.accepts(ok), "{ok:?} 应放行（编辑途中的合法中间态）");
        }
        for bad in ["1.2.3", "abc", "1a", "1-2", "-1", " 1"] {
            assert!(!s.accepts(bad), "{bad:?} 应拒绝");
        }
    }

    /// 允许负值时负号只能打头；整数步长时小数点一律不放行。
    #[test]
    fn filter_respects_sign_and_decimals() {
        let neg = NumSpec::new(-5.0, 5.0, 1.0);
        assert!(neg.accepts("-"), "负范围里单独的负号是合法中间态");
        assert!(neg.accepts("-3"));
        assert!(!neg.accepts("3-"), "负号不在打头位");
        assert!(!neg.accepts("-1.5"), "整数步长不接受小数点");

        let pos = NumSpec::new(0.0, 5.0, 1.0);
        assert!(!pos.accepts("-1"), "非负范围不接受负号");
    }

    /// 半成品解析不出值（保留原值），完整数字才给值。
    #[test]
    fn parse_rejects_incomplete_text() {
        let s = NumSpec::new(0.0, 10.0, 0.1);
        assert_eq!(s.parse(""), None);
        assert_eq!(s.parse("-"), None);
        assert_eq!(s.parse("."), None);
        assert_eq!(s.parse("2.5"), Some(2.5));
        assert_eq!(s.parse(" 2 "), Some(2.0));
    }
}
