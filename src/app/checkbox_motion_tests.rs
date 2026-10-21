//! 复选框动效走真实宿主路径的契约：按下缩小、松开回弹，对勾沿笔画画出。
//!
//! 帧时钟钉住取确定帧，按像素量化：白底上找方框的非白像素外接框量尺寸，
//! 在强调色底上数近白像素量对勾已经画到哪。

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::event::{MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 200;
const H: i32 = 80;
/// 方框所在区域：方框在 (10,10) 起 18px，四周留余量但不出白底。
const BOX: Rect = Rect::new(8, 8, 28, 24);

struct Clock;

impl Clock {
    fn new() -> Self {
        anim::set_motion_level(MotionLevel::Full);
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

fn host(state: Signal<bool>) -> UiHost {
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .padding(10)
            .child(Element::checkbox("", state)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn render(h: &mut UiHost) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    pm
}

fn ptr(h: &mut UiHost, kind: PointerKind, p: Point) {
    h.on_pointer(PointerEvent::single(kind, p, MouseButton::Left));
}

/// `BOX` 内非白像素的外接框宽度（方框描边/填充的可见宽度）。
fn box_width(pm: &Pixmap) -> i32 {
    let xs: Vec<i32> = (BOX.y..BOX.y + BOX.h)
        .flat_map(|y| (BOX.x..BOX.x + BOX.w).map(move |x| (x, y)))
        .filter(|&(x, y)| pm.pixel(x as u32, y as u32).unwrap().red() < 230)
        .map(|(x, _)| x)
        .collect();
    xs.iter().max().unwrap() - xs.iter().min().unwrap() + 1
}

/// 方框 `[x0, x1)` 列内的近白像素数——底色已是强调色时即对勾的墨量。
fn check_ink(pm: &Pixmap, x0: i32, x1: i32) -> usize {
    // 行列都避开方框四个圆角：角外露出的是白底，会被误数成对勾。
    (12..26)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let c = pm.pixel(x as u32, y as u32).unwrap();
            c.red() > 200 && c.green() > 200 && c.blue() > 200
        })
        .count()
}

#[test]
fn press_shrinks_box_and_release_outside_restores_without_toggling() {
    let clock = Clock::new();
    let state = signal(false);
    let mut h = host(state);
    let idle = box_width(&render(&mut h));
    assert_eq!(idle, 18, "前提：静止时方框 18px 宽");
    let at = Point::new(19, 19);
    ptr(&mut h, PointerKind::Down, at);
    render(&mut h);
    clock.at(1_500);
    let pressed = box_width(&render(&mut h));
    assert!(pressed < idle - 1, "按下应缩小方框：{pressed} vs {idle}");
    // 拖出控件再松开：不切换，但按下态必须解除（靠捕获收到 Up）。
    let out = Point::new(150, 60);
    ptr(&mut h, PointerKind::Move, out);
    ptr(&mut h, PointerKind::Up, out);
    render(&mut h);
    clock.at(2_500);
    let after = box_width(&render(&mut h));
    assert!(!state.get(), "控件外松开不应切换");
    assert_eq!(after, idle, "松开后方框应回到原大");
}

#[test]
fn check_mark_draws_along_its_stroke() {
    let clock = Clock::new();
    let state = signal(false);
    let mut h = host(state);
    render(&mut h);
    let at = Point::new(19, 19);
    ptr(&mut h, PointerKind::Down, at);
    ptr(&mut h, PointerKind::Up, at);
    render(&mut h);
    assert!(state.get(), "前提：点击勾选");
    // 方框 x∈[10,28)；对勾短边落在左半、长边的笔尖在右侧 x≥21（圆角列 26、27 不数）。
    let (left, right) = ((12, 19), (21, 26));
    clock.at(1_150);
    let mid = render(&mut h);
    clock.at(2_000);
    let end = render(&mut h);
    assert!(
        check_ink(&end, right.0, right.1) > 0,
        "前提：画完后右侧有笔尖"
    );
    assert!(check_ink(&mid, left.0, left.1) > 0, "中途：短边已画出");
    assert_eq!(
        check_ink(&mid, right.0, right.1),
        0,
        "中途：笔尖还没走到右侧（对勾应沿笔画画出，而非整体淡入）"
    );
}

/// 方框内强调色（偏蓝）像素数——底色铺了多少。
fn fill_ink(pm: &Pixmap) -> usize {
    (12..26)
        .flat_map(|y| (12..26).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let c = pm.pixel(x as u32, y as u32).unwrap();
            c.blue() as i32 - c.red() as i32 > 60
        })
        .count()
}

#[test]
fn cancel_midway_rewinds_only_the_travelled_part() {
    // 勾到 100ms 又取消：倒放只需走回这 100ms，而不是用满整段时长。
    let clock = Clock::new();
    let state = signal(false);
    let mut h = host(state);
    render(&mut h);
    let at = Point::new(19, 19);
    let click = |h: &mut UiHost| {
        ptr(h, PointerKind::Down, at);
        ptr(h, PointerKind::Up, at);
    };
    click(&mut h);
    render(&mut h);
    clock.at(1_100);
    assert!(
        fill_ink(&render(&mut h)) > 0,
        "前提：100ms 时底色已铺开一部分"
    );
    click(&mut h);
    assert!(!state.get(), "前提：第二次点击取消勾选");
    render(&mut h);
    clock.at(1_100 + 110);
    assert_eq!(
        fill_ink(&render(&mut h)),
        0,
        "走过多久就倒放多久，此时应已褪尽"
    );
}

#[test]
fn release_without_own_press_does_not_toggle() {
    // 在空白处按下（无人捕获），拖到复选框上松开：Up 落到复选框，但它没收到过按下。
    let _clock = Clock::new();
    let state = signal(false);
    let mut h = host(state);
    render(&mut h);
    ptr(&mut h, PointerKind::Down, Point::new(150, 60));
    ptr(&mut h, PointerKind::Move, Point::new(19, 19));
    ptr(&mut h, PointerKind::Up, Point::new(19, 19));
    assert!(!state.get(), "别处按下、这里松开不算点击");
}
