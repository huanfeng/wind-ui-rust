//! 交互式提示（[`Element::hint`](crate::ui::Element::hint)）的开合状态机。
//!
//! 面板是锚点的 `popup` 子节点，摆放、绘制、命中、选择复制都由核心现成能力承担；宿主
//! 只管**什么时候开、什么时候关**。状态转移写成不依赖窗口的纯函数 [`HintState::step`]，
//! 可直接单测。
//!
//! 「指针在面板上」与「指针在锚点上」在悬停链里是同一件事（面板是锚点的子孙），所以
//! 输入只有一个：当前悬停节点所属的锚点（`Tree::hint_of`）。

use crate::core::NodeId;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// 悬停计时中，满 `open_delay` 开。
    Pending {
        anchor: NodeId,
        since: u64,
    },
    /// 已开。`pinned`：点击固定，移开不收。
    Open {
        anchor: NodeId,
        pinned: bool,
    },
    /// 指针已离开锚点与面板，满 `close_delay` 收。
    Grace {
        anchor: NodeId,
        since: u64,
    },
}

/// 状态转移要宿主做的事。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Act {
    Open(NodeId),
    Close(NodeId),
}

pub(super) struct Step {
    pub act: Option<Act>,
    /// 还在等时间：到这个时刻（ms）需要再来一帧。
    pub wake_at: Option<u64>,
}

pub(super) struct HintState {
    phase: Phase,
    /// 点击收起后，指针仍停在该锚点上时不再重新计时弹出，直到指针移到别处。
    suppress: Option<NodeId>,
}

impl Default for HintState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            suppress: None,
        }
    }
}

impl HintState {
    /// 当前打开（含待收）的锚点。
    #[cfg(test)]
    pub fn open_anchor(&self) -> Option<NodeId> {
        match self.phase {
            Phase::Open { anchor, .. } | Phase::Grace { anchor, .. } => Some(anchor),
            _ => None,
        }
    }

    /// 是否有未了的计时或打开的面板（宿主据此决定要不要继续出帧、禁用局部重绘）。
    pub fn active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// 推进一步。`hover`：悬停节点所属锚点；`is_open(a)`：面板信号当前是否为开
    /// （外部点击 / ESC 会直接把它置假，状态机据此回到空闲）。
    pub fn step(
        &mut self,
        hover: Option<NodeId>,
        is_open: impl Fn(NodeId) -> bool,
        now: u64,
        open_delay: u64,
        close_delay: u64,
    ) -> Step {
        if self.suppress.is_some() && hover != self.suppress {
            self.suppress = None;
        }
        let hover = hover.filter(|h| Some(*h) != self.suppress);
        let mut act = None;
        let mut wake_at = None;
        match self.phase {
            Phase::Idle => {
                if let Some(a) = hover {
                    self.phase = Phase::Pending {
                        anchor: a,
                        since: now,
                    };
                    wake_at = Some(now + open_delay);
                }
            }
            Phase::Pending { anchor, since } => match hover {
                Some(a) if a == anchor => {
                    if now.saturating_sub(since) >= open_delay {
                        self.phase = Phase::Open {
                            anchor,
                            pinned: false,
                        };
                        act = Some(Act::Open(anchor));
                    } else {
                        wake_at = Some(since + open_delay);
                    }
                }
                Some(a) => {
                    self.phase = Phase::Pending {
                        anchor: a,
                        since: now,
                    };
                    wake_at = Some(now + open_delay);
                }
                None => self.phase = Phase::Idle,
            },
            Phase::Open { anchor, pinned } => {
                if !is_open(anchor) {
                    // 被外部点击 / ESC 收起了。
                    self.phase = Phase::Idle;
                } else if pinned {
                    // 固定态：移开不收。
                } else if hover == Some(anchor) {
                    // 仍在锚点或面板上。
                } else if let Some(b) = hover {
                    // 直接移到另一个锚点：旧的立刻收，新的重新计时。
                    act = Some(Act::Close(anchor));
                    self.phase = Phase::Pending {
                        anchor: b,
                        since: now,
                    };
                    wake_at = Some(now + open_delay);
                } else {
                    self.phase = Phase::Grace { anchor, since: now };
                    wake_at = Some(now + close_delay);
                }
            }
            Phase::Grace { anchor, since } => {
                if !is_open(anchor) {
                    self.phase = Phase::Idle;
                } else if hover == Some(anchor) {
                    self.phase = Phase::Open {
                        anchor,
                        pinned: false,
                    };
                } else if now.saturating_sub(since) >= close_delay {
                    act = Some(Act::Close(anchor));
                    self.phase = Phase::Idle;
                    if let Some(b) = hover {
                        self.phase = Phase::Pending {
                            anchor: b,
                            since: now,
                        };
                        wake_at = Some(now + open_delay);
                    }
                } else {
                    wake_at = Some(since + close_delay);
                }
            }
        }
        Step { act, wake_at }
    }

    /// 指针按下。`hover`：悬停所属 `(锚点, 是否在面板内)`。
    ///
    /// - 点锚点自身：未开 → 立即开并固定；已开未固定 → 固定；已固定 → 收起（并抑制，
    ///   免得指针还停在上面又弹出来）。
    /// - 点面板内：固定（用户在选文字，不能因为指针划出去就收掉）。
    pub fn on_down(&mut self, hover: Option<(NodeId, bool)>) -> Option<Act> {
        let (a, in_panel) = hover?;
        let on_this = match self.phase {
            Phase::Open { anchor, .. } | Phase::Grace { anchor, .. } => anchor == a,
            _ => false,
        };
        if on_this {
            // Grace 不会是固定态（固定态不进宽限）。
            let pinned = matches!(self.phase, Phase::Open { pinned: true, .. });
            if in_panel {
                self.phase = Phase::Open {
                    anchor: a,
                    pinned: true,
                };
                None
            } else if pinned {
                self.phase = Phase::Idle;
                self.suppress = Some(a);
                Some(Act::Close(a))
            } else {
                self.phase = Phase::Open {
                    anchor: a,
                    pinned: true,
                };
                None
            }
        } else if in_panel {
            None
        } else {
            self.phase = Phase::Open {
                anchor: a,
                pinned: true,
            };
            Some(Act::Open(a))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OD: u64 = 120;
    const CD: u64 = 180;

    fn a() -> NodeId {
        NodeId::from_raw(1, 0)
    }
    fn b() -> NodeId {
        NodeId::from_raw(2, 0)
    }
    fn step(s: &mut HintState, hover: Option<NodeId>, open: bool, now: u64) -> Option<Act> {
        s.step(hover, |_| open, now, OD, CD).act
    }

    #[test]
    fn opens_after_open_delay_not_before() {
        let mut s = HintState::default();
        assert_eq!(step(&mut s, Some(a()), false, 0), None);
        assert_eq!(step(&mut s, Some(a()), false, OD - 1), None);
        assert_eq!(step(&mut s, Some(a()), false, OD), Some(Act::Open(a())));
    }

    #[test]
    fn leaving_before_delay_cancels() {
        let mut s = HintState::default();
        step(&mut s, Some(a()), false, 0);
        step(&mut s, None, false, 50);
        assert!(!s.active());
        assert_eq!(step(&mut s, None, false, 1000), None);
    }

    #[test]
    fn closes_after_grace_and_returning_cancels_it() {
        let mut s = HintState::default();
        step(&mut s, Some(a()), false, 0);
        step(&mut s, Some(a()), false, OD);
        // 离开 → 宽限
        assert_eq!(step(&mut s, None, true, 200), None);
        // 宽限内回来（鼠标跨过锚点与面板之间的缝）
        assert_eq!(step(&mut s, Some(a()), true, 200 + CD - 1), None);
        // 再离开，满宽限才收
        assert_eq!(step(&mut s, None, true, 500), None);
        assert_eq!(step(&mut s, None, true, 500 + CD - 1), None);
        assert_eq!(step(&mut s, None, true, 500 + CD), Some(Act::Close(a())));
        assert!(!s.active());
    }

    #[test]
    fn click_pins_and_pinned_survives_leaving() {
        let mut s = HintState::default();
        step(&mut s, Some(a()), false, 0);
        step(&mut s, Some(a()), false, OD);
        assert_eq!(s.on_down(Some((a(), false))), None);
        assert_eq!(step(&mut s, None, true, 5000), None);
        assert!(s.active(), "固定后移开不收");
    }

    #[test]
    fn click_on_closed_anchor_opens_pinned_at_once() {
        let mut s = HintState::default();
        assert_eq!(s.on_down(Some((a(), false))), Some(Act::Open(a())));
        assert_eq!(step(&mut s, None, true, 9999), None);
    }

    #[test]
    fn click_again_on_pinned_closes_and_is_not_reopened_while_hovering() {
        let mut s = HintState::default();
        s.on_down(Some((a(), false)));
        assert_eq!(s.on_down(Some((a(), false))), Some(Act::Close(a())));
        // 指针仍停在锚点上：不重新计时弹出
        assert_eq!(step(&mut s, Some(a()), false, 10), None);
        assert_eq!(step(&mut s, Some(a()), false, 10 + OD * 3), None);
        // 移走再回来才算新的悬停
        step(&mut s, None, false, 2000);
        step(&mut s, Some(a()), false, 2100);
        assert_eq!(
            step(&mut s, Some(a()), false, 2100 + OD),
            Some(Act::Open(a()))
        );
    }

    #[test]
    fn press_inside_panel_pins_so_selection_survives_pointer_leaving() {
        let mut s = HintState::default();
        step(&mut s, Some(a()), false, 0);
        step(&mut s, Some(a()), false, OD);
        assert_eq!(s.on_down(Some((a(), true))), None);
        assert_eq!(step(&mut s, None, true, 9000), None);
        assert!(s.active());
    }

    #[test]
    fn external_dismiss_returns_to_idle() {
        let mut s = HintState::default();
        s.on_down(Some((a(), false)));
        // 面板信号被外部点击 / ESC 置假
        assert_eq!(step(&mut s, None, false, 100), None);
        assert!(!s.active());
    }

    #[test]
    fn moving_to_another_anchor_closes_old_and_times_new() {
        let mut s = HintState::default();
        step(&mut s, Some(a()), false, 0);
        step(&mut s, Some(a()), false, OD);
        assert_eq!(step(&mut s, Some(b()), true, 300), Some(Act::Close(a())));
        assert_eq!(
            step(&mut s, Some(b()), false, 300 + OD),
            Some(Act::Open(b()))
        );
    }

    #[test]
    fn pinned_ignores_hovering_other_anchor() {
        let mut s = HintState::default();
        s.on_down(Some((a(), false)));
        assert_eq!(step(&mut s, Some(b()), true, 100), None);
        assert_eq!(s.open_anchor(), Some(a()));
    }
}
