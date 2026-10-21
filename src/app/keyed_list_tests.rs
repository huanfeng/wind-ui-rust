//! 键控列表（`Element::list_signal` 的 `KeyedList`）走真实宿主路径的契约：写数据信号 → 宿主
//! 重排时按键比对 → 行节点复用 / 增删 / 重排，行上的 `animate_item` 据此过渡。
//!
//! 每行是一条纯色带（颜色由数据 id 决定），按像素找色带实际画在哪；节点身份直接比 `NodeId`。

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::core::NodeId;
use crate::event::{MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 200;
const H: i32 = 300;
const ROW_H: i32 = 30;
const PROBE_X: i32 = 50;

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

/// 数据：(id, 文案版本)。id 决定颜色与键；版本变了 = 同一个键、数据变了。
type Item = (u8, u8);

fn color(id: u8) -> Color {
    match id {
        1 => Color::rgb(255, 0, 0),
        2 => Color::rgb(0, 255, 0),
        3 => Color::rgb(0, 0, 255),
        _ => Color::rgb(255, 0, 255),
    }
}

fn host(data: Signal<Vec<Item>>, animate: bool) -> UiHost {
    let app = App::new("t", W, H).content(Element::col().fill().bg(Color::WHITE).child(
        Element::list_signal(
            data,
            |it: &Item| it.0,
            move |it: Item| {
                let row = Element::leaf().width_match().height(ROW_H).bg(color(it.0));
                if animate {
                    row.animate_item()
                } else {
                    row
                }
            },
        ),
    ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn render(h: &mut UiHost) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    pm
}

/// 色带 `id` 在探测列上第一个纯色像素的 y。
fn top_of(pm: &Pixmap, id: u8) -> Option<i32> {
    let c = color(id);
    (0..H).find(|&y| {
        let p = pm.pixel(PROBE_X as u32, y as u32).unwrap();
        p.red() == c.r && p.green() == c.g && p.blue() == c.b
    })
}

/// 写数据并要求重排。真实应用里数据在事件回调或定时器里写，宿主随后必重排；测试里在
/// 事件外裸写信号，只有整帧或请求了重排的帧才会跑 `on_update`。
fn set<T: 'static>(h: &mut UiHost, data: Signal<Vec<T>>, v: Vec<T>) {
    data.set(v);
    h.damage.needs_relayout = true;
}

fn rows(h: &UiHost) -> Vec<NodeId> {
    let col = h.tree.root.unwrap();
    let list = h.tree.get(col).unwrap().children[0];
    h.tree.get(list).unwrap().children.clone()
}

#[test]
fn reorder_reuses_nodes_and_slides() {
    let clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0), (3, 0)]);
    let mut h = host(data, true);
    let pm = render(&mut h);
    assert_eq!(
        (top_of(&pm, 1), top_of(&pm, 3)),
        (Some(0), Some(2 * ROW_H)),
        "前提：首批行不播进场，首帧即在位"
    );
    let before = rows(&h);
    set(&mut h, data, vec![(3, 0), (2, 0), (1, 0)]);
    let pm = render(&mut h);
    let after = rows(&h);
    assert_eq!(
        after,
        vec![before[2], before[1], before[0]],
        "节点原样复用、按新顺序排"
    );
    assert_eq!(top_of(&pm, 3), Some(2 * ROW_H), "重排那一帧仍画在旧位置");
    clock.at(1_100);
    let mid = top_of(&render(&mut h), 3).unwrap();
    assert!(mid > 0 && mid < 2 * ROW_H, "半程在新旧之间：{mid}");
    clock.at(2_000);
    let pm = render(&mut h);
    assert_eq!((top_of(&pm, 3), top_of(&pm, 1)), (Some(0), Some(2 * ROW_H)));
}

#[test]
fn removed_row_exits_in_place_then_is_reaped() {
    let clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0), (3, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    let ghost = rows(&h)[1];
    set(&mut h, data, vec![(1, 0), (3, 0)]);
    render(&mut h);
    assert_eq!(rows(&h).len(), 3, "离场中的行仍在树上（残影）");
    assert_eq!(rows(&h)[1], ghost, "残影留在原处");
    assert_ne!(
        h.tree.hit_test(Point::new(PROBE_X, ROW_H + 5)),
        Some(ghost),
        "残影点不到"
    );
    clock.at(1_100);
    let pm = render(&mut h);
    let blue = top_of(&pm, 3).unwrap();
    assert!(
        blue > ROW_H && blue < 2 * ROW_H,
        "后面的行随残影收起逐帧上移：{blue}"
    );
    for t in [1_300, 1_600, 2_000] {
        clock.at(t);
        render(&mut h);
    }
    assert_eq!(rows(&h).len(), 2, "离场播完后真正移除");
    assert!(h.tree.get(ghost).is_none(), "残影节点已回收");
    assert_eq!(top_of(&render(&mut h), 3), Some(ROW_H));
}

#[test]
fn inserted_row_enters_initial_rows_do_not() {
    let clock = Clock::new();
    let data = signal(vec![(1, 0), (3, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    set(&mut h, data, vec![(1, 0), (2, 0), (3, 0)]);
    let pm = render(&mut h);
    assert_eq!(top_of(&pm, 2), None, "插入那一帧从隐藏起（高度 0）");
    assert_eq!(top_of(&pm, 3), Some(ROW_H), "后面的行还没被推开");
    clock.at(1_100);
    let pm = render(&mut h);
    let blue = top_of(&pm, 3).unwrap();
    assert!(blue > ROW_H && blue < 2 * ROW_H, "进场半程：{blue}");
    // 新行自己也在淡入：半程不是纯色（构建时落定为"已显示"的话这里已是纯绿）。
    assert_eq!(top_of(&pm, 2), None, "进场半程新行半透明");
    clock.at(2_000);
    let pm = render(&mut h);
    assert_eq!(
        (top_of(&pm, 2), top_of(&pm, 3)),
        (Some(ROW_H), Some(2 * ROW_H))
    );
}

#[test]
fn changed_data_rebuilds_that_row_only() {
    let _clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0), (3, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    let before = rows(&h);
    set(&mut h, data, vec![(1, 0), (2, 1), (3, 0)]);
    render(&mut h);
    let after = rows(&h);
    assert_eq!((after[0], after[2]), (before[0], before[2]), "没变的行复用");
    assert_ne!(after[1], before[1], "数据变了的行重建");
    assert!(h.tree.get(before[1]).is_none(), "旧节点已回收");
}

#[test]
fn readding_during_exit_revives_the_ghost() {
    let clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0), (3, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    let (red, green, blue) = (rows(&h)[0], rows(&h)[1], rows(&h)[2]);
    set(&mut h, data, vec![(1, 0), (3, 0)]);
    render(&mut h);
    clock.at(1_060);
    render(&mut h);
    set(&mut h, data, vec![(1, 0), (2, 0), (3, 0)]);
    render(&mut h);
    assert_eq!(
        rows(&h),
        vec![red, green, blue],
        "三行都是原来的节点，复活的就是那个残影"
    );
    clock.at(3_000);
    let pm = render(&mut h);
    assert_eq!(rows(&h).len(), 3);
    assert_eq!(
        (top_of(&pm, 2), top_of(&pm, 3)),
        (Some(ROW_H), Some(2 * ROW_H))
    );
}

#[test]
fn rows_without_transition_are_removed_immediately() {
    let _clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0), (3, 0)]);
    let mut h = host(data, false);
    render(&mut h);
    set(&mut h, data, vec![(1, 0), (3, 0)]);
    let pm = render(&mut h);
    assert_eq!(rows(&h).len(), 2, "无过渡：不留残影");
    assert_eq!(top_of(&pm, 3), Some(ROW_H), "一帧到位");
}

#[test]
fn duplicate_keys_fall_back_to_full_rebuild() {
    let _clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    let before = rows(&h);
    set(&mut h, data, vec![(1, 0), (1, 1), (2, 0)]);
    let pm = render(&mut h);
    let after = rows(&h);
    assert_eq!(after.len(), 3, "按数据建出全部行");
    assert!(
        before.iter().all(|id| !after.contains(id)),
        "整批重建，不复用"
    );
    assert_eq!(top_of(&pm, 2), Some(2 * ROW_H), "不播过渡，一帧到位");
}

#[test]
fn row_signals_are_reclaimed_with_their_rows() {
    // 每行现造一个信号：删除 / 重建的行，其信号随节点一同回收，反复增删不累积。
    use crate::signal::stats;
    let _clock = Clock::new();
    let data = signal(vec![1u8, 2, 3]);
    let app = App::new("t", W, H).content(Element::list_signal(
        data,
        |n: &u8| *n,
        |n: u8| Element::label(signal(format!("第 {n} 行"))),
    ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    let cycle = |h: &mut UiHost, i: u8| {
        set(h, data, vec![1, 2 + i % 2, 4 + i]);
        render(h);
    };
    for i in 0..3 {
        cycle(&mut h, i);
    }
    let steady = stats().live;
    for i in 3..30 {
        cycle(&mut h, i);
    }
    assert_eq!(stats().live, steady, "反复增删后活跃信号数不应增长");
}

#[test]
fn reorder_keeps_focus_on_reused_row() {
    let _clock = Clock::new();
    let data = signal(vec![1u8, 2, 3]);
    let app = App::new("t", W, H).content(Element::list_signal(
        data,
        |n: &u8| *n,
        |n: u8| Element::button(format!("按钮 {n}")).height(ROW_H),
    ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    let list = h.tree.root.unwrap();
    let third = h.tree.get(list).unwrap().children[2];
    let r = h.tree.abs_bounds(third);
    let at = Point::new(r.x + 5, r.y + 5);
    h.on_pointer(PointerEvent::single(
        PointerKind::Down,
        at,
        MouseButton::Left,
    ));
    h.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
    render(&mut h);
    assert_eq!(h.focus.current, Some(third), "前提：第三行拿到焦点");
    set(&mut h, data, vec![3, 1, 2]);
    render(&mut h);
    assert_eq!(h.focus.current, Some(third), "重排后焦点仍在同一行上");
    // 宿主不会立刻清掉失效的焦点 id，只比 id 会放过"行已被重建"——再确认节点还活着、
    // 并且就是排到最前面的那一行。
    assert!(h.tree.get(third).is_some(), "获焦的行节点被复用而非重建");
    assert_eq!(h.tree.get(list).unwrap().children[0], third);
}

#[test]
fn duplicate_then_unique_keys_do_not_leak_nodes() {
    // 键重复时整批重建，旧行里留下同键的几行；下一次键唯一时它们必须全部移除，
    // 而不是在按键建表时被静默覆盖、节点却留在树上。
    let _clock = Clock::new();
    let data = signal(vec![(1, 0), (1, 1), (1, 2)]);
    let mut h = host(data, false);
    render(&mut h);
    let before = rows(&h);
    assert_eq!(before.len(), 3, "前提：键重复时按数据建出全部行");
    set(&mut h, data, vec![(2, 0)]);
    render(&mut h);
    assert_eq!(rows(&h).len(), 1);
    for id in before {
        assert!(h.tree.get(id).is_none(), "旧行节点应已移除");
    }
}

#[test]
fn data_change_mid_enter_continues_the_enter() {
    // 乐观插入后很快拿到服务端确认、同一个键的数据变了：新节点接着原进度进场，
    // 而不是一下子满尺寸冒出来、压在仍在滑动的后续行上。
    let clock = Clock::new();
    let data = signal(vec![(1, 0), (3, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    set(&mut h, data, vec![(1, 0), (2, 0), (3, 0)]);
    render(&mut h);
    clock.at(1_040);
    render(&mut h);
    set(&mut h, data, vec![(1, 0), (2, 1), (3, 0)]);
    let pm = render(&mut h);
    assert_eq!(top_of(&pm, 2), None, "接着进场：仍半透明、未满尺寸");
    let blue = top_of(&pm, 3).unwrap();
    assert!(
        blue > ROW_H && blue < 2 * ROW_H,
        "后续行仍在让位途中：{blue}"
    );
    clock.at(2_000);
    let pm = render(&mut h);
    assert_eq!(
        (top_of(&pm, 2), top_of(&pm, 3)),
        (Some(ROW_H), Some(2 * ROW_H))
    );
}

#[test]
fn ghosts_keep_old_order_including_front_and_adjacent() {
    // 删首行（残影落在最前）、相邻两行一起删（残影挂在残影后面），同时剩下的行重排。
    let _clock = Clock::new();
    let data = signal(vec![(1, 0), (2, 0), (3, 0), (4, 0)]);
    let mut h = host(data, true);
    render(&mut h);
    let ids = rows(&h);
    set(&mut h, data, vec![(4, 0), (3, 0)]);
    render(&mut h);
    // 旧顺序 1 2 3 4：1、2 都没有仍在的前驱，排最前且保持 1 在 2 前；3、4 按新顺序。
    assert_eq!(rows(&h), vec![ids[0], ids[1], ids[3], ids[2]]);
}

#[test]
fn clearing_a_long_animated_list_does_not_overflow_the_stack() {
    // 连续删除的残影曾按"挂在前一个残影后面"串成链、递归展开：清空几千行时在 1MB 栈
    // （Windows 主线程默认）上溢出崩溃。放进 1MB 栈的线程里跑，debug 下 2000 行即可复现。
    let r = std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(|| {
            let _clock = Clock::new();
            let all: Vec<(u16, u8)> = (0..3000).map(|i| (i, 0)).collect();
            let data = signal(all);
            let app = App::new("t", W, H).content(Element::list_signal(
                data,
                |it: &(u16, u8)| it.0,
                |it: (u16, u8)| {
                    Element::leaf()
                        .width_match()
                        .height(2 + (it.0 % 2) as i32)
                        .animate_item()
                },
            ));
            let mut h = app.into_handler_for_test();
            h.set_scale(1.0);
            render(&mut h);
            set(&mut h, data, Vec::new());
            render(&mut h);
        })
        .unwrap()
        .join();
    // 视口外的残影不在绘制路径上，随即落定回收，所以不断言剩余行数；要的是比对那一步不崩。
    assert!(r.is_ok(), "清空长列表不应崩溃");
}
