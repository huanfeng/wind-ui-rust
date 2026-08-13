//! 触摸惯性滑动（fling）：松手后的滑行、撞界回弹，以及触摸平移的亚像素残差。
//!
//! 只依赖 `Tree` 的滚动接口（`pan_scroll` / `scroll_range` / `scroll_target` /
//! `set_scroll_y` / `set_over_scroll`），与控件树其余部分无耦合。

use crate::core::NodeId;
use crate::geometry::Point;

use super::UiHost;

/// 每 ms 速度保留系数（指数摩擦）。0.996 ≈ 衰减常数 0.004/ms，松手后约 1s 内停下。
const FLING_FRICTION: f32 = 0.996;
/// 启动惯性的最小释放速度，比较对象是 `vy`（**物理像素/ms**）；低于此视为缓慢拖放，不滑。
const FLING_TRIGGER: f32 = 0.25;
/// 停止阈值，比较对象是 `Fling::vel`（**逻辑像素/ms**，与触发阈值差一个 scale）；
/// 速度低于此即结束（约 <0.3px/帧@60）。
const FLING_STOP: f32 = 0.02;
/// 两帧间隔超过此值（ms）视为长停滞（最小化、卡顿、后台恢复）→ 结算惯性，避免巨跳。
const FLING_STALL_MS: u64 = 100;
/// 撞界回弹冲量增益（ms）：越界偏移 ≈ 撞界速度 × 此值（逻辑像素/ms × ms = 像素）。
const BOUNCE_GAIN: f32 = 22.0;
/// 越界偏移上限（逻辑像素）：保证"轻微缓冲"而非大幅橡皮筋。
const MAX_BOUNCE: f32 = 26.0;
/// 回弹每 ms 衰减系数：0.98 ≈ 150ms 内弹回归零，短促不拖沓。
const BOUNCE_DECAY: f32 = 0.98;

/// 惯性滑动相位。
#[derive(Clone, Copy, PartialEq)]
enum FlingPhase {
    /// 滑行：按速度推进 scroll_y、摩擦衰减。
    Glide,
    /// 回弹：撞界后短暂越界偏移弹回归零。
    Bounce,
}

/// 进行中的惯性滑动状态。
struct Fling {
    /// 目标滚动容器节点。
    node: NodeId,
    /// 当前相位（滑行/回弹）。
    phase: FlingPhase,
    /// scroll_y 速度（**逻辑像素/ms**）；正=继续增大 scroll_y（内容上移）。
    vel: f32,
    /// 回弹越界偏移（逻辑像素，Bounce 相位用）；正=顶部回弹、负=底部回弹。
    over: f32,
    /// 亚像素累积，避免逐帧取整丢失。
    residual: f32,
    /// 上次步进时的帧时钟（ms）；None=尚未步进（首帧用标称帧长起步，
    /// 避免借用 fling 前可能陈旧的渲染时钟得到巨 dt）。
    last_ms: Option<u64>,
}

/// 宿主持有的滚动手势状态：进行中的惯性滑动 + 触摸平移残差。
#[derive(Default)]
pub(super) struct ScrollState {
    /// 触摸惯性滑动状态（None=无）。
    active: Option<Fling>,
    /// 触摸平移的亚像素残差（物理→逻辑取整丢失部分累积，避免高 DPI 细微平移发黏）。
    pan_residual: f32,
}

impl UiHost {
    /// 结束惯性滑动并复位目标节点的越界回弹偏移（打断/取消/重启时必经，
    /// 否则 Bounce 相位中途清除会残留 over_scroll 使内容卡偏）。返回此前是否在滑动。
    pub(super) fn clear_fling(&mut self) -> bool {
        match self.scroll.active.take() {
            Some(f) => {
                self.tree.set_over_scroll(f.node, 0);
                true
            }
            None => false,
        }
    }

    /// 步进惯性滑动一帧：Glide 按速度推进 scroll_y、摩擦衰减，撞界转 Bounce；
    /// Bounce 短暂越界偏移弹回归零。仍在进行时请求下一帧重绘。
    pub(super) fn step_fling(&mut self, now_ms: u64) {
        let Some(f) = self.scroll.active.as_ref() else {
            return;
        };
        let (node, phase, last) = (f.node, f.phase, f.last_ms);
        // 首帧用标称帧长起步；其后按真实间隔，长停滞（最小化/卡顿）直接结算防巨跳。
        let dt = match last {
            None => 16,
            Some(prev) => {
                let raw = now_ms.saturating_sub(prev);
                if raw > FLING_STALL_MS {
                    self.tree.set_over_scroll(node, 0);
                    self.scroll.active = None;
                    return;
                }
                raw.min(64)
            }
        } as f32;
        match phase {
            FlingPhase::Glide => {
                let f = self.scroll.active.as_mut().unwrap();
                f.last_ms = Some(now_ms);
                f.vel *= FLING_FRICTION.powf(dt);
                let advance = f.vel * dt + f.residual;
                let delta = advance.trunc() as i32;
                f.residual = advance - delta as f32;
                let vel = f.vel;
                // 推进并检测撞界（scroll_y 始终钳制；clamp 改变值即撞界）。
                let hit = match self.tree.scroll_range(node) {
                    Some((cur, max)) => {
                        let next = cur + delta;
                        let clamped = next.clamp(0, max);
                        self.tree.set_scroll_y(node, clamped);
                        clamped != next
                    }
                    None => {
                        self.scroll.active = None; // 节点消失（结构变更）→ 结束
                        return;
                    }
                };
                if hit {
                    // 撞界 → 按撞界速度给一个小幅越界偏移，转入回弹。
                    let impulse = (-vel * BOUNCE_GAIN).clamp(-MAX_BOUNCE, MAX_BOUNCE);
                    if impulse.abs() < 1.0 {
                        self.tree.set_over_scroll(node, 0);
                        self.scroll.active = None;
                    } else {
                        self.tree.set_over_scroll(node, impulse.round() as i32);
                        let f = self.scroll.active.as_mut().unwrap();
                        f.phase = FlingPhase::Bounce;
                        f.over = impulse;
                        crate::anim::request_repaint();
                    }
                } else if vel.abs() < FLING_STOP {
                    self.scroll.active = None;
                } else {
                    crate::anim::request_repaint();
                }
            }
            FlingPhase::Bounce => {
                let f = self.scroll.active.as_mut().unwrap();
                f.last_ms = Some(now_ms);
                f.over *= BOUNCE_DECAY.powf(dt);
                let over = f.over;
                if over.abs() < 0.5 {
                    self.tree.set_over_scroll(node, 0);
                    self.scroll.active = None;
                } else {
                    self.tree.set_over_scroll(node, over.round() as i32);
                    crate::anim::request_repaint();
                }
            }
        }
    }

    /// 触摸平移一帧（`AppHandler::on_pan` 的实现体）。
    pub(super) fn pan(&mut self, pos: Point, dy: i32) -> bool {
        self.damage.needs_full = true; // 滚动改变大片区域 → 全窗重绘。
                                       // 菜单激活时忽略平移（并清残差，避免菜单关闭后跳变）。
        if self.menu.is_open() {
            self.scroll.pan_residual = 0.0;
            return false;
        }
        // 物理 → 逻辑（命中与滚动均在逻辑空间）；亚像素残差累积，避免高 DPI 发黏。
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        let total = dy as f32 / s + self.scroll.pan_residual;
        let dyl = total.trunc() as i32;
        self.scroll.pan_residual = total - dyl as f32;
        if dyl == 0 {
            return false;
        }
        // 拖动跟手时打断残留惯性/回弹，避免方向冲突。
        self.clear_fling();
        self.tree.pan_scroll(p, dyl)
    }

    /// 松手启动惯性滑动（`AppHandler::start_fling` 的实现体）。
    pub(super) fn begin_fling(&mut self, pos: Point, vy: f32) -> bool {
        // 复位任何残留惯性/回弹偏移，再决定是否启动新的。
        self.clear_fling();
        // 菜单激活时不滑。
        if self.menu.is_open() {
            return false;
        }
        // 释放速度过低 → 视为缓慢拖放，不进入惯性。
        if vy.abs() < FLING_TRIGGER {
            return false;
        }
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        // scroll_y 速度 = −手指速度（手指上移 vy<0 → 内容上移、scroll_y 增大）；物理→逻辑。
        let vel = -vy / s;
        // 按惯性方向找能继续滚动的容器：内层到界则冒泡外层（与 pan 一致）。
        let Some(node) = self.tree.scroll_target(p, vel > 0.0) else {
            return false;
        };
        self.scroll.active = Some(Fling {
            node,
            phase: FlingPhase::Glide,
            vel,
            over: 0.0,
            residual: 0.0,
            last_ms: None,
        });
        // 触发持续动画，下一帧起由 step_fling 推进。
        crate::anim::request_repaint();
        true
    }
}
