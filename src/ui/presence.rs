//! 进出场过渡（presence）：节点显示时淡入/滑入，隐藏时先淡出/滑出再真正消失。
//!
//! 由 [`Element::transition`](super::Element::transition) 声明，核心在三处接管：
//! - **布局**：离场中的节点继续占位（[`PresenceState::alive`]），播完才把空间让出来——
//!   否则同级在离场的第一帧就补位，与还在淡出的残影叠在一起；
//! - **绘制**：按进度乘不透明度、叠加滑动偏移；
//! - **命中 / 焦点 / 模态**：一律按**逻辑可见**判，离场中的残影点不到、拿不到焦点。
//!
//! 进度是一个 `Transition<f32>`（0=隐、1=显），在 paint 里据"本节点及祖先是否逻辑可见"
//! 改向——与控件里的 retarget-in-paint 同一模式，时钟永远新鲜。祖先离场时子孙里声明了
//! 过渡的节点会随之离场，故对话框可以"遮罩淡出、面板同时下沉"。

use std::cell::Cell;

use crate::anim::{self, Easing, Transition};
use crate::geometry::Point;
use crate::theme::Theme;

/// 进出场过渡的规格。
///
/// ```
/// use windui::prelude::*;
/// let show = signal(false);
/// let hint = Element::label("已保存")
///     .visible_signal(show)
///     .transition(Presence::fade().slide(0, -6));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Presence {
    fade: bool,
    slide: (i32, i32),
    expand: bool,
    /// 显示时从这个倍数缩放到 1（隐藏时反向）。`None` = 不缩放。
    zoom: Option<f32>,
    enter_ms: Option<u32>,
    exit_ms: Option<u32>,
}

impl Presence {
    /// 淡入淡出。
    pub fn fade() -> Self {
        Self {
            fade: true,
            ..Self::default()
        }
    }

    /// 锚定浮层（下拉面板、弹出卡片）：淡入，同时自上方 4px 落下。
    pub fn popover() -> Self {
        Self::fade().slide(0, -4)
    }

    /// 对话框：淡入，同时从 0.96 倍放大到原大（Fluent / WinUI 对话框的进场方式）。
    pub fn dialog() -> Self {
        Self::fade().scale(0.96)
    }

    /// 折叠内容（折叠面板、手风琴的 body）：淡入，同时高度从 0 展开。
    pub fn reveal() -> Self {
        Self::fade().expand()
    }

    /// 缩放：显示时从 `from` 倍（以节点中心为支点）缩放到原大，隐藏时反向。属位移类动效，
    /// 减弱动效档位下不缩放（淡入淡出保留）。
    ///
    /// **只支持缩小进场**：`from` 钳到 `[0.5, 1]`。缩放发生在合成阶段（整棵子树先按原大画进
    /// 离屏层，再缩放合成），合成不带祖先的裁剪——放大的话会越出滚动视口盖到兄弟上、被祖先
    /// 的离屏层截断。布局、命中都按原大算。
    ///
    /// 只有软件与 GPU 后端会缩放；Windows 的 D2D 后端忽略缩放——只声明了缩放的过渡在那里
    /// 没有可见效果（对话框面板即如此，只剩遮罩淡入）。
    pub fn scale(mut self, from: f32) -> Self {
        debug_assert!(
            from <= 1.0,
            "Presence::scale 只支持缩小进场（from ≤ 1），传入 {from}"
        );
        // 钳完等于 1 的不记：没有可见效果却会让离场白白占位一段时长。
        self.zoom = from
            .is_finite()
            .then(|| from.clamp(0.5, 1.0))
            .filter(|z| 1.0 - z > crate::render::ZOOM_EPS);
        self
    }

    /// 展开：显示时占位高度从 0 长到自然高度，隐藏时收回 0；子树按自然尺寸排布、裁到
    /// 当前高度（像卷帘），同级随之平滑让位。属位移类动效，减弱动效档位下不展开。
    ///
    /// 过渡期每帧重排一次（高度在变）——用在面板、分组这类子树上，别挂在长列表的每一行。
    ///
    /// 边角：过渡期本节点自己的焦点环与投影（画在矩形外）一并被裁掉；放在设了
    /// `spacing` 的线性容器里，高度为 0 时仍计一个间距（起止各跳一个 spacing）；子树里高度随父的（`height_match` / 权重）子节点会按过渡中的高度排、
    /// 表现为被压扁而非被裁。过渡期按 Tab 可能停到被裁掉的子节点上（约 0.2 秒窗口）。
    pub fn expand(mut self) -> Self {
        self.expand = true;
        self
    }

    /// 滑动：显示时从 `(dx, dy)`（逻辑像素，相对最终位置）滑到原位，隐藏时反向滑回。
    /// 属位移类动效，减弱动效档位下不滑动（淡入淡出保留）。
    pub fn slide(mut self, dx: i32, dy: i32) -> Self {
        self.slide = (dx, dy);
        self
    }

    /// 覆盖时长（毫秒）。不设时进场取主题 `anim.normal()`、离场取 `anim.fast()`——
    /// 离场不需要被看清，越快越不挡路。
    pub fn duration(mut self, enter_ms: u32, exit_ms: u32) -> Self {
        self.enter_ms = Some(enter_ms);
        self.exit_ms = Some(exit_ms);
        self
    }

    /// 按当前动效档位，这个过渡还有没有看得见的效果。
    fn visible_effect(&self) -> bool {
        self.fade
            || ((self.slide != (0, 0) || self.expand || self.zoom.is_some())
                && anim::movement_enabled())
    }
}

/// 节点上的进出场状态（只有声明了过渡的节点才分配）。
pub(crate) struct PresenceState {
    spec: Presence,
    /// 进度：0=完全隐藏，1=完全显示。
    tr: Cell<Transition<f32>>,
    /// 上次布局时本节点**只因离场未完**才占位。离场播完那帧据此补一次重排，把空间让出来。
    pub(crate) laid_exiting: Cell<bool>,
}

impl PresenceState {
    /// `shown`：构建时本节点及祖先是否逻辑可见。据此落定初值，首次出现不回放进场。
    pub(crate) fn new(spec: Presence, shown: bool) -> Self {
        Self {
            spec,
            tr: Cell::new(Transition::new(if shown { 1.0 } else { 0.0 })),
            laid_exiting: Cell::new(false),
        }
    }

    /// 是否还需要占位与绘制：显示中、进场中、离场未完，或刚被隐藏而本帧 paint 还没
    /// 来得及改向（目标仍是 1）。
    ///
    /// 离场会瞬时完成时（动画关闭、当前档位下没有可见效果、离场时长为 0）恒为假：
    /// 否则隐藏那一帧布局还替它占着位、paint 却什么都不画，凭空多出一帧空洞——
    /// 与不声明过渡时的行为不再逐像素一致。
    pub(crate) fn alive(&self) -> bool {
        if self.exit_snaps() {
            return false;
        }
        let tr = self.tr.get();
        tr.target() > 0.0 || tr.value() > 0.0
    }

    fn exit_snaps(&self) -> bool {
        !anim::enabled() || !self.spec.visible_effect() || self.spec.exit_ms == Some(0)
    }

    /// 不经过渡直接落定到显示 / 隐藏。给"不在绘制路径上"的节点用：进度只在 paint 里
    /// 改向，没被画到的那段时间里发生的显隐，露出来时不该补播（更不该先以旧状态冒出来）。
    pub(crate) fn settle(&self, shown: bool) {
        let mut tr = self.tr.get();
        tr.settle(if shown { 1.0 } else { 0.0 });
        self.tr.set(tr);
    }

    /// paint 期调用：据本帧逻辑可见改向，返回进度 `[0,1]`。
    pub(crate) fn advance(&self, shown: bool, th: &Theme) -> f32 {
        let mut tr = self.tr.get();
        let target = if shown { 1.0 } else { 0.0 };
        if tr.target() != target {
            if self.spec.visible_effect() {
                let (ms, easing) = if shown {
                    (
                        self.spec.enter_ms.unwrap_or(th.anim.normal()),
                        th.anim.easing(),
                    )
                } else {
                    (
                        self.spec.exit_ms.unwrap_or(th.anim.fast()),
                        Easing::ACCELERATE,
                    )
                };
                tr.retarget(target, ms, easing);
            } else {
                // 当前档位下什么都看不出（纯滑动 + 减弱动效）：别让节点原样僵在那儿一段时间。
                tr = Transition::new(target);
            }
            self.tr.set(tr);
        }
        tr.value().clamp(0.0, 1.0)
    }

    /// 是否仍在过渡中（需要续帧）。
    pub(crate) fn is_active(&self) -> bool {
        self.tr.get().is_active()
    }

    /// 进度 `p` 下对子树不透明度的乘数。
    pub(crate) fn opacity(&self, p: f32) -> f32 {
        if self.spec.fade {
            p
        } else {
            1.0
        }
    }

    /// 进度 `p` 下的滑动偏移（逻辑像素）。减弱动效档位下恒为零。
    pub(crate) fn offset(&self, p: f32) -> Point {
        if !anim::movement_enabled() {
            return Point::new(0, 0);
        }
        let k = 1.0 - p;
        let (dx, dy) = self.spec.slide;
        Point::new(
            (dx as f32 * k).round() as i32,
            (dy as f32 * k).round() as i32,
        )
    }

    /// 展开类过渡在当前进度下的高度系数；不展开（或减弱动效）时为 `None`。
    pub(crate) fn height_factor(&self) -> Option<f32> {
        (self.spec.expand && anim::movement_enabled())
            .then(|| self.tr.get().value().clamp(0.0, 1.0))
    }

    /// 是否为展开类过渡且仍在进行：过渡期每帧都要重排（高度在变）、子树要裁到当前高度。
    pub(crate) fn expanding(&self) -> bool {
        self.height_factor().is_some() && self.is_active()
    }

    /// 进度 `p` 下的合成缩放倍数；不缩放（或减弱动效、已到原大）时为 `None`。
    pub(crate) fn zoom(&self, p: f32) -> Option<f32> {
        let from = self.spec.zoom.filter(|_| anim::movement_enabled())?;
        let z = from + (1.0 - from) * p;
        ((z - 1.0).abs() > crate::render::ZOOM_EPS).then_some(z)
    }

    /// 滑动的最大幅度：过渡期的脏区要按它外扩，盖住上一帧与这一帧两处位置。
    pub(crate) fn slide_extent(&self) -> i32 {
        self.spec.slide.0.abs().max(self.spec.slide.1.abs())
    }
}

/// 高度过渡状态（`Element::animate_height`）：自然高度变化时，占位高度平滑跟过去。
pub(crate) struct HeightAnim {
    tr: Cell<Transition<f32>>,
    /// 上一次测量处于"跟随子树"状态（见 [`measure`](Self::measure)）。
    following: Cell<bool>,
    /// 已见过第一次自然高度。首次测量（以及重新露出时）直接落定，不从 0 长出来。
    primed: Cell<bool>,
}

impl HeightAnim {
    pub(crate) fn new() -> Self {
        Self {
            tr: Cell::new(Transition::new(0.0).movement()),
            following: Cell::new(false),
            primed: Cell::new(false),
        }
    }

    /// 测量期调用：据自然高度改向，返回本帧的占位高度。
    ///
    /// 改向放在测量而非 paint：要的正是"布局一算出新高度就开始过渡"，而宿主在布局前
    /// 已刷新帧时钟（每帧只布局一次）。
    ///
    /// `inner_moving`：子树里有展开 / 高度过渡正在进行。这时自然高度每帧都在变，而变化
    /// 本身已经是平滑的——外层直接跟随，不再自己起一段过渡。否则外层每帧改向去追一个移动的
    /// 目标，必然远远落后，里面的内容大半时间被裁掉。内层停下那一帧剩下的零头也直接落定
    /// （`following`），不为最后一两像素再起一段 200ms 的过渡。
    pub(crate) fn measure(&self, natural: i32, inner_moving: bool, th: &Theme) -> f32 {
        let target = natural as f32;
        let mut tr = self.tr.get();
        let changed = (tr.target() - target).abs() >= 0.5;
        if !self.primed.get() {
            tr.settle(target);
            self.primed.set(true);
        } else if inner_moving || (self.following.get() && changed) {
            tr.settle(target);
            self.following.set(true);
        } else if changed {
            if tr.is_active() {
                tr.retarget_spring(target, th.anim.spring());
            } else {
                tr.retarget(target, th.anim.normal(), th.anim.easing());
            }
        }
        // 只延续一帧：内层停下那一帧剩下的零头靠上一帧留下的 `true` 落定，此后即清。
        // 不能等"某一帧没变化"再清——树一空闲就不再布局，标志会一直留到下一次无关的
        // 高度变化，让它一帧跳到位。
        self.following.set(inner_moving);
        self.tr.set(tr);
        tr.value()
    }

    /// 下次测量直接落定（节点不在绘制路径上期间内容变了，露出来时不补播）。
    pub(crate) fn unprime(&self) {
        self.primed.set(false);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.tr.get().is_active()
    }
}
