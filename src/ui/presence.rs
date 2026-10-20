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

    /// 对话框面板：淡入，同时自下方 12px 升起。
    pub fn dialog() -> Self {
        Self::fade().slide(0, 12)
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
        self.fade || (self.slide != (0, 0) && anim::movement_enabled())
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

    /// 滑动的最大幅度：过渡期的脏区要按它外扩，盖住上一帧与这一帧两处位置。
    pub(crate) fn slide_extent(&self) -> i32 {
        self.spec.slide.0.abs().max(self.spec.slide.1.abs())
    }
}
