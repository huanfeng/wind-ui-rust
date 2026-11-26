//! 交互式提示走真实宿主路径的契约：悬停弹出、指针可移进面板、点击固定、外部点击收起。
//!
//! 帧时钟钉住，逐帧推进；面板是否打开直接读锚点的 `hint_panel` 信号。

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::core::NodeId;
use crate::event::{Key, MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 320;
const H: i32 = 240;
const OPEN: u64 = 120;
const CLOSE: u64 = 180;

struct Clock(u64);

impl Clock {
    fn new() -> Self {
        anim::set_motion_level(MotionLevel::Off);
        anim::pin_clock(Some(1_000));
        Clock(1_000)
    }
    fn set(&mut self, ms: u64) {
        self.0 = ms;
        anim::pin_clock(Some(ms));
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        anim::pin_clock(None);
    }
}

fn host() -> UiHost {
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .padding(10)
            .spacing(120)
            .child(
                Element::button("说明")
                    .width(60)
                    .hint("第一行说明\n第二行说明"),
            )
            .child(Element::button("另一个").width(60).hint("别的说明")),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn render(h: &mut UiHost) {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
}

fn mv(h: &mut UiHost, p: Point) {
    h.on_pointer(PointerEvent::single(
        PointerKind::Move,
        p,
        MouseButton::Left,
    ));
}

fn down(h: &mut UiHost, p: Point) {
    h.on_pointer(PointerEvent::single(
        PointerKind::Down,
        p,
        MouseButton::Left,
    ));
    h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
}

/// 在 `ms` 时刻出一帧。
fn frame(h: &mut UiHost, clk: &mut Clock, ms: u64) {
    clk.set(ms);
    render(h);
}

fn first_anchor(h: &UiHost) -> NodeId {
    h.tree
        .hit_test(Point::new(20, 20))
        .and_then(|n| h.tree.hint_of(n))
        .unwrap()
        .0
}

fn is_open(h: &UiHost, a: NodeId) -> bool {
    h.tree.hint_panel(a).is_some_and(|(_, s)| s.get())
}

/// 面板中心点（绝对坐标）。
fn panel_center(h: &UiHost, a: NodeId) -> Point {
    let (panel, _) = h.tree.hint_panel(a).unwrap();
    let r = h.tree.abs_bounds(panel);
    Point::new(r.x + r.w / 2, r.y + r.h / 2)
}

#[test]
fn hover_opens_after_delay_and_panel_lies_right_of_anchor() {
    let mut clk = Clock::new();
    let mut h = host();
    frame(&mut h, &mut clk, 1_000);
    let a = first_anchor(&h);
    mv(&mut h, Point::new(20, 20));
    frame(&mut h, &mut clk, 1_000);
    assert!(!is_open(&h, a), "刚悬停不应立即弹出");
    frame(&mut h, &mut clk, 1_000 + OPEN - 1);
    assert!(!is_open(&h, a));
    frame(&mut h, &mut clk, 1_000 + OPEN);
    assert!(is_open(&h, a));
    frame(&mut h, &mut clk, 1_000 + OPEN + 16);
    let (panel, _) = h.tree.hint_panel(a).unwrap();
    let anchor_r = h.tree.abs_bounds(a);
    let panel_r = h.tree.abs_bounds(panel);
    assert!(
        panel_r.x >= anchor_r.right(),
        "面板应在锚点右侧: {panel_r:?} vs {anchor_r:?}"
    );
}

#[test]
fn pointer_can_move_into_panel_and_it_stays_open() {
    let mut clk = Clock::new();
    let mut h = host();
    frame(&mut h, &mut clk, 1_000);
    let a = first_anchor(&h);
    mv(&mut h, Point::new(20, 20));
    frame(&mut h, &mut clk, 1_000);
    frame(&mut h, &mut clk, 1_000 + OPEN);
    frame(&mut h, &mut clk, 1_000 + OPEN + 16);
    let c = panel_center(&h, a);
    // 先经过缝隙再进面板：悬停链断开的那几帧由宽限接住。
    let gap_y = h.tree.abs_bounds(a).bottom() + 2;
    mv(&mut h, Point::new(20, gap_y));
    frame(&mut h, &mut clk, 1_000 + OPEN + 40);
    mv(&mut h, c);
    frame(&mut h, &mut clk, 1_000 + OPEN + 80);
    frame(&mut h, &mut clk, 1_000 + OPEN + 80 + CLOSE * 3);
    assert!(is_open(&h, a), "指针在面板里，不应收起");
    assert_eq!(
        h.tree
            .hit_test(c)
            .and_then(|n| h.tree.hint_of(n))
            .map(|x| x.1),
        Some(true)
    );
}

#[test]
fn leaving_closes_after_grace() {
    let mut clk = Clock::new();
    let mut h = host();
    frame(&mut h, &mut clk, 1_000);
    let a = first_anchor(&h);
    mv(&mut h, Point::new(20, 20));
    frame(&mut h, &mut clk, 1_000);
    frame(&mut h, &mut clk, 1_000 + OPEN);
    assert!(is_open(&h, a));
    mv(&mut h, Point::new(280, 200));
    frame(&mut h, &mut clk, 1_200);
    assert!(is_open(&h, a), "宽限内仍开着");
    frame(&mut h, &mut clk, 1_200 + CLOSE);
    assert!(!is_open(&h, a));
}

#[test]
fn click_pins_then_outside_click_closes() {
    let mut clk = Clock::new();
    let mut h = host();
    frame(&mut h, &mut clk, 1_000);
    let a = first_anchor(&h);
    h.on_pointer(PointerEvent::single(
        PointerKind::Down,
        Point::new(20, 20),
        MouseButton::Left,
    ));
    h.on_pointer(PointerEvent::single(
        PointerKind::Up,
        Point::new(20, 20),
        MouseButton::Left,
    ));
    frame(&mut h, &mut clk, 1_010);
    assert!(is_open(&h, a), "点击应立即打开并固定");
    mv(&mut h, Point::new(280, 200));
    frame(&mut h, &mut clk, 1_010 + CLOSE * 5);
    assert!(is_open(&h, a), "固定后移开不收");
    down(&mut h, Point::new(280, 200));
    frame(&mut h, &mut clk, 1_010 + CLOSE * 5 + 16);
    assert!(!is_open(&h, a), "点外部收起");
}

#[test]
fn escape_closes_pinned_hint() {
    let mut clk = Clock::new();
    let mut h = host();
    frame(&mut h, &mut clk, 1_000);
    let a = first_anchor(&h);
    down(&mut h, Point::new(20, 20));
    frame(&mut h, &mut clk, 1_010);
    assert!(is_open(&h, a));
    h.on_key(crate::event::KeyEvent {
        key: Key::Escape,
        pressed: true,
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    });
    frame(&mut h, &mut clk, 1_030);
    assert!(!is_open(&h, a));
}

#[test]
fn hint_suppresses_the_pointer_anchored_tooltip() {
    let mut clk = Clock::new();
    let mut h = host();
    frame(&mut h, &mut clk, 1_000);
    let a = first_anchor(&h);
    assert_eq!(h.tree.node_tooltip(a), None);
}
