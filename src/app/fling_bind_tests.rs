//! 滚动绑定与触摸惯性的共存契约（设计 docs/roadmap-three-designs.md §2.5 一期验收项）：
//! 惯性滑动的每帧步进经整窗帧的 `layout_root` 进入同步相位写回信号；惯性中途外部写入
//! 信号，惯性从新位置继续（`step_fling` 对 `scroll_y` 做增量积分，不重置轨迹）。
//!
//! 帧时钟钉住取确定帧；外部写信号后手动置 `needs_relayout`（事件外裸写信号不会自己
//! 触发重排，同 `switch_tests` 的 `set` 辅助）。

use crate::anim;
use crate::app::{App, UiHost};
use crate::geometry::{Point, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::spec::Axis;
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 200;
const H: i32 = 100;

struct Clock;
impl Clock {
    fn new() -> Self {
        anim::pin_clock(Some(1_000));
        Clock
    }
    fn at(&self, ms: u64) {
        anim::pin_clock(Some(ms));
    }
}
impl Drop for Clock {
    fn drop(&mut self) {
        anim::pin_clock(None);
    }
}

fn host(pos: Signal<i32>) -> UiHost {
    let mut sc = Element::scroll().width(W).height(H).scroll_position(pos);
    for _ in 0..10 {
        sc = sc.child(Element::leaf().width_match().height(30));
    }
    let app = App::new("t", W, H).content(sc);
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn render(h: &mut UiHost) {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
}

fn scroll_y(h: &UiHost) -> i32 {
    let root = h.tree.root.unwrap();
    h.tree.get(root).unwrap().scroll_y
}

/// 惯性中外部写入：惯性从新位置继续；结束后信号收敛到终值。
#[test]
fn fling_continues_from_external_write() {
    let pos = signal(0i32);
    let mut h = host(pos);
    let clock = Clock::new();
    render(&mut h); // 首帧布局：初始对齐
    assert_eq!(scroll_y(&h), 0);

    // 上滑（v<0 → 偏移增大）启动惯性。
    assert!(h.begin_fling(Point::new(50, 50), Axis::Vertical, -1.5));
    clock.at(1_100);
    render(&mut h);
    let y1 = scroll_y(&h);
    assert!(y1 > 0, "惯性应已推进（实际 {y1}）");

    // 中途外部写入 50：消费赢同帧（设计 §2.4「外部写入赢一帧」——step 先跑、同帧
    // layout 的同步相位随后覆盖），下一帧惯性从 50 继续推进。
    pos.set(50);
    h.damage.needs_relayout = true;
    clock.at(1_200);
    render(&mut h);
    let y2 = scroll_y(&h);
    assert_eq!(y2, 50, "外部写入消费（同帧赢过惯性步进）");
    clock.at(1_210);
    render(&mut h);
    let y3 = scroll_y(&h);
    assert!(y3 > 50, "惯性从新位置继续（实际 {y3}）");

    // 推进到自然结束（钳到最大滚动量 200 或速度衰减完），信号收敛到终值。
    for i in 0..60 {
        clock.at(1_300 + i * 16);
        render(&mut h);
    }
    let y_end = scroll_y(&h);
    assert_eq!(pos.get(), y_end, "结束后信号 == 容器终值");
}
