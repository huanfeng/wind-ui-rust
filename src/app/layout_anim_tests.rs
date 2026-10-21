//! 布局过渡（`Element::animate_layout`，FLIP）走真实宿主路径的契约：点击写信号 → 宿主重排 →
//! 节点从旧位置滑到新位置。帧时钟钉住取确定帧，按像素找红块实际画在哪（不看内部状态）。
//!
//! 场景：顶上一个按钮切换灰块 A 的显隐（A 无过渡、一帧消失），A 下面是声明了布局过渡的
//! 红块 B。A 一隐藏，B 的布局位置上移 A 的高度。

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::event::{MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 300;
const H: i32 = 300;
const RED: Color = Color::rgb(255, 0, 0);
const GRAY: Color = Color::rgb(128, 128, 128);
/// 灰块 A 的高度 = B 要移动的距离。
const A_H: i32 = 60;
/// 时间曲线时长（主题默认 `normal`）。
const DUR: u64 = 200;

struct Clock;

impl Clock {
    fn new(level: MotionLevel) -> Self {
        anim::set_motion_level(level);
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
        anim::set_motion_level(MotionLevel::Full);
    }
}

fn render_at(h: &mut UiHost, size: Size) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, size);
    pm
}

fn render(h: &mut UiHost) -> Pixmap {
    render_at(h, Size::new(W, H))
}

fn click(h: &mut UiHost, p: Point) {
    h.on_pointer(PointerEvent::single(
        PointerKind::Down,
        p,
        MouseButton::Left,
    ));
    h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
}

/// 第 `x` 列第一个纯红像素的 y（红块顶边实际画在哪）。
fn red_top(pm: &Pixmap, x: i32) -> Option<i32> {
    (0..H).find(|&y| {
        let c = pm.pixel(x as u32, y as u32).unwrap();
        c.red() > 240 && c.green() < 15 && c.blue() < 15
    })
}

const TOGGLE: Point = Point::new(10, 10);
const PROBE_X: i32 = 50;

/// 按钮 + 灰块 A（`show` 控制）+ 红块 B（`b` 决定修饰）。
fn host(show: Signal<bool>, b: impl FnOnce(Element) -> Element) -> UiHost {
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("切换")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
            .child(b(Element::leaf().size(100, 30).bg(RED))),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn b_id(h: &UiHost) -> crate::core::NodeId {
    h.tree.get(h.tree.root.unwrap()).unwrap().children[2]
}

#[test]
fn moves_from_old_position_and_hits_final_layout() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let mut h = host(show, |e| e.animate_layout());
    let y0 = red_top(&render(&mut h), PROBE_X).expect("前提：红块可见");
    click(&mut h, TOGGLE);
    assert!(!show.get(), "前提：点击写了信号");
    let first = render(&mut h);
    let y1 = h.tree.abs_bounds(b_id(&h)).y;
    assert_eq!(y1, y0 - A_H, "前提：布局位置上移了 A 的高度");
    assert_eq!(
        red_top(&first, PROBE_X),
        Some(y0),
        "布局变化那一帧仍画在旧位置"
    );

    clock.at(1_000 + DUR / 2);
    let mid = red_top(&render(&mut h), PROBE_X).unwrap();
    assert!(
        mid < y0 && mid > y1,
        "半程应在新旧之间：{mid} ∉ ({y1}, {y0})"
    );
    // 命中按最终布局：终点矩形中心点得到 B，旧位置（已无 A、B 也已离开）点不到 B。
    let fin = Point::new(PROBE_X, y1 + 15);
    assert_eq!(h.tree.hit_test(fin), Some(b_id(&h)), "过渡中命中按最终布局");
    assert_ne!(
        h.tree.hit_test(Point::new(PROBE_X, y0 + 25)),
        Some(b_id(&h))
    );

    clock.at(1_000 + DUR + 50);
    assert_eq!(
        red_top(&render(&mut h), PROBE_X),
        Some(y1),
        "播完落在新位置"
    );
    assert!(!h.wants_animation(), "播完不再续帧（空闲零 CPU）");
}

#[test]
fn without_modifier_jumps_in_one_frame() {
    // 对照组：不声明时同一操作一帧到位，证明上一条的"中间位置"来自布局过渡。
    let _clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let mut h = host(show, |e| e);
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    click(&mut h, TOGGLE);
    assert_eq!(red_top(&render(&mut h), PROBE_X), Some(y0 - A_H));
}

#[test]
fn reversing_midway_is_continuous_and_leaves_no_residue() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let mut h = host(show, |e| e.animate_layout());
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    click(&mut h, TOGGLE);
    render(&mut h);
    clock.at(1_000 + DUR / 2);
    let before = red_top(&render(&mut h), PROBE_X).unwrap();
    // 半程再切回来：布局位置回到 y0，视觉位置不能跳。
    click(&mut h, TOGGLE);
    assert!(show.get());
    let after = red_top(&render(&mut h), PROBE_X).unwrap();
    assert!(
        (after - before).abs() <= 1,
        "改向那一帧不能跳：{before} → {after}"
    );
    clock.at(1_000 + DUR / 2 + 2_000);
    assert_eq!(
        red_top(&render(&mut h), PROBE_X),
        Some(y0),
        "最终无残留偏移"
    );
    assert!(!h.wants_animation());
}

#[test]
fn window_resize_does_not_animate() {
    // B 水平居中：窗口变宽，布局位置右移，但不该过渡。
    let _clock = Clock::new(MotionLevel::Full);
    let app = App::new("t", W, H).content(
        Element::col().fill().bg(Color::WHITE).child(
            Element::leaf()
                .size(40, 30)
                .bg(RED)
                .align(crate::spec::Align::Center)
                .animate_layout(),
        ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let id = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
    render_at(&mut h, Size::new(200, H));
    let x0 = h.tree.abs_bounds(id).x;
    let pm = render_at(&mut h, Size::new(W, H));
    let x1 = h.tree.abs_bounds(id).x;
    assert!(x1 > x0, "前提：改尺寸让布局位置右移");
    let left = (0..W).find(|&x| pm.pixel(x as u32, 5).unwrap().green() < 15);
    assert_eq!(left, Some(x1), "改尺寸那一帧直接画在新位置");
    assert!(!h.wants_animation());
}

#[test]
fn scrolling_does_not_animate() {
    let _clock = Clock::new(MotionLevel::Full);
    let app = App::new("t", W, H).content(
        Element::scroll()
            .fill()
            .bg(Color::WHITE)
            .child(Element::leaf().size(100, 100))
            .child(Element::leaf().size(100, 30).bg(RED).animate_layout())
            .child(Element::leaf().size(100, 600)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    let sc = h.tree.root.unwrap();
    assert!(h.tree.set_scroll_y(sc, 40), "前提：可滚");
    let pm = render(&mut h);
    assert_eq!(red_top(&pm, PROBE_X), Some(y0 - 40), "滚动一帧到位");
    assert!(!h.wants_animation());
}

#[test]
fn nested_child_does_not_double_move() {
    // 父子都声明：父整体上移时，子相对父不动——子不能再自己滑一遍。对照单层：同样的
    // 距离与曲线，半程剩余偏移应一致；子若叠了一份自己的偏移，剩余偏移会翻倍。
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("切换")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
            .child(
                Element::col()
                    .size(100, 60)
                    .animate_layout()
                    .child(Element::leaf().size(100, 20))
                    .child(Element::leaf().size(100, 30).bg(RED).animate_layout()),
            ),
    );
    let mut nested = app.into_handler_for_test();
    nested.set_scale(1.0);
    let show1 = signal(true);
    let mut single = host(show1, |e| e.animate_layout());
    let residue = |h: &mut UiHost| {
        let y0 = red_top(&render(h), PROBE_X).unwrap();
        click(h, TOGGLE);
        render(h);
        clock.at(1_000 + DUR / 2);
        let mid = red_top(&render(h), PROBE_X).unwrap();
        clock.at(1_000 + DUR + 50);
        let end = red_top(&render(h), PROBE_X).unwrap();
        clock.at(1_000);
        assert_eq!(y0 - end, A_H, "前提：红块共上移 A 的高度");
        mid - end
    };
    let single_r = residue(&mut single);
    let nested_r = residue(&mut nested);
    assert!(single_r > 0, "前提：单层半程仍有剩余偏移");
    assert_eq!(nested_r, single_r, "嵌套与单层半程剩余偏移应一致");
}

#[test]
fn reduced_motion_jumps() {
    let _clock = Clock::new(MotionLevel::Reduced);
    let show = signal(true);
    let mut h = host(show, |e| e.animate_layout());
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    click(&mut h, TOGGLE);
    // 不断言 wants_animation：按钮的按下色渐变不是位移类，减弱档下照常播。
    assert_eq!(red_top(&render(&mut h), PROBE_X), Some(y0 - A_H));
}

#[test]
fn follows_expanding_sibling_without_lag() {
    // A 带展开过渡：B 的位置每帧被高度过渡推着走，本身已是平滑的。声明了布局过渡的 B
    // 应与不声明时逐帧重合；若每帧都另起一段过渡去追，会明显落在后面。
    let clock = Clock::new(MotionLevel::Full);
    let track = |flip: bool| {
        let show = signal(false);
        let app = App::new("t", W, H).content(
            Element::col()
                .fill()
                .bg(Color::WHITE)
                .child(
                    Element::button("切换")
                        .size(80, 30)
                        .on_click(move |_| show.set(!show.get())),
                )
                .child(
                    Element::leaf()
                        .size(100, A_H)
                        .bg(GRAY)
                        .visible_signal(show)
                        .transition(crate::ui::Presence::reveal()),
                )
                .child({
                    let b = Element::leaf().size(100, 30).bg(RED);
                    if flip {
                        b.animate_layout()
                    } else {
                        b
                    }
                }),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        clock.at(1_000);
        render(&mut h);
        click(&mut h, TOGGLE);
        let mut ys = Vec::new();
        for t in (0..=300).step_by(25) {
            clock.at(1_000 + t);
            ys.push(red_top(&render(&mut h), PROBE_X).unwrap());
        }
        ys
    };
    let plain = track(false);
    assert!(
        plain.windows(2).any(|w| w[0] != w[1]) && plain[0] != *plain.last().unwrap(),
        "前提：展开过程中 B 被逐帧推动：{plain:?}"
    );
    assert_eq!(track(true), plain, "声明布局过渡后应逐帧跟随，不落后");
}

/// 过渡中的局部帧与整窗帧逐像素一致，且确实走了局部帧。
///
/// 偏移不进结构签名，脏区由节点按子树范围自报；报窄了留残影，而若偏移的取值顺手请求了
/// 整窗续帧，自报就白报了——两种错都只在这里现形。窗口取大些：按钮按下渐变的脏区并进来
/// 超过半窗时宿主会直接整窗，测试就走不到局部帧。
#[test]
fn partial_frames_match_full_frames() {
    const BW: i32 = 600;
    const BH: i32 = 600;
    let clock = Clock::new(MotionLevel::Full);
    let mk = |show: Signal<bool>| {
        let app = App::new("t", BW, BH).content(
            Element::col()
                .fill()
                .bg(Color::WHITE)
                .child(
                    Element::button("切换")
                        .size(80, 30)
                        .on_click(move |_| show.set(!show.get())),
                )
                .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
                .child(
                    Element::leaf()
                        .size(100, 30)
                        .bg(RED)
                        .animate_layout()
                        .shadow(crate::style::Shadow::new(
                            0.0,
                            4.0,
                            6.0,
                            Color::rgb(0, 0, 255),
                        )),
                ),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        h
    };
    let rd = |h: &mut UiHost, pm: &mut Pixmap| {
        h.render(&mut PixmapTarget { pixmap: pm }, Size::new(BW, BH));
    };
    let (mut partial, mut full) = (mk(signal(true)), mk(signal(true)));
    let mut pm = Pixmap::new(BW as u32, BH as u32).unwrap();
    let mut pf = Pixmap::new(BW as u32, BH as u32).unwrap();
    rd(&mut partial, &mut pm);
    rd(&mut full, &mut pf);
    let mut saw_partial = false;
    for round in 0..4u64 {
        click(&mut partial, TOGGLE);
        click(&mut full, TOGGLE);
        let base = 1_000 + round * 5_000;
        for t in [0, 16, 40, 80, 120, 160, 200, 260, 400, 1_000] {
            clock.at(base + t);
            rd(&mut partial, &mut pm);
            saw_partial |= !partial.damage.last_frame_full;
            full.damage.needs_full = true;
            rd(&mut full, &mut pf);
            assert!(
                pm.data() == pf.data(),
                "第 {round} 轮 t={t}：局部帧与整窗帧不一致"
            );
        }
    }
    assert!(saw_partial, "过渡期应走局部帧，而不是每帧整窗");
}

/// 一次点击里 A（无过渡）隐藏、C（展开类）同时收起：B 先因 A 起一段过渡，随后几帧被 C 的
/// 收起推着走。跟随时只记位置、保留正在播的偏移——清零会让剩下那段一帧跳没。
#[test]
fn following_keeps_running_offset() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("切换")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
            .child(
                Element::leaf()
                    .size(100, 20)
                    .bg(Color::rgb(0, 0, 255))
                    .visible_signal(show)
                    .transition(crate::ui::Presence::reveal()),
            )
            .child(Element::leaf().size(100, 30).bg(RED).animate_layout()),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let mut ys = vec![red_top(&render(&mut h), PROBE_X).unwrap()];
    click(&mut h, TOGGLE);
    ys.push(red_top(&render(&mut h), PROBE_X).unwrap());
    for t in (16..=600).step_by(16) {
        clock.at(1_000 + t);
        ys.push(red_top(&render(&mut h), PROBE_X).unwrap());
    }
    assert_eq!(
        ys[0] - ys.last().unwrap(),
        A_H + 20,
        "前提：共上移 A + C 的高度"
    );
    let jump = ys.windows(2).map(|w| (w[0] - w[1]).abs()).max().unwrap();
    assert!(jump < 25, "逐帧位移不应有大跳变：最大 {jump}，{ys:?}");
}

/// 跟随只针对能推动它的展开：在 B **后面**展开的 D 推不动 B，A 隐藏让 B 上移照常过渡。
#[test]
fn unrelated_reveal_does_not_suppress() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let other = signal(false);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::row()
                    .child(
                        Element::button("A")
                            .size(80, 30)
                            .on_click(move |_| show.set(!show.get())),
                    )
                    .child(
                        Element::button("D")
                            .size(80, 30)
                            .on_click(move |_| other.set(!other.get())),
                    ),
            )
            .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
            // B 比 D 宽：B 从旧位置滑过去时会叠在 D 上，探测列取 D 盖不到的那段。
            .child(Element::leaf().size(200, 30).bg(RED).animate_layout())
            .child(
                Element::leaf()
                    .size(100, 80)
                    .bg(Color::rgb(0, 0, 255))
                    .visible_signal(other)
                    .transition(crate::ui::Presence::reveal()),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let y0 = red_top(&render(&mut h), 150).unwrap();
    click(&mut h, Point::new(90, 10));
    render(&mut h);
    clock.at(1_016);
    render(&mut h);
    assert!(h.wants_animation(), "前提：D 正在展开");
    clock.at(1_032);
    click(&mut h, TOGGLE);
    assert_eq!(red_top(&render(&mut h), 150), Some(y0), "B 应从旧位置起步");
}

/// 祖先的高度过渡只裁剪、不推动子节点：卡片 `animate_height` 时里面的行照常过渡。
#[test]
fn ancestor_height_animation_does_not_suppress() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("切换")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(
                Element::col()
                    .animate_height()
                    .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
                    .child(Element::leaf().size(100, 30).bg(RED).animate_layout()),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    click(&mut h, TOGGLE);
    render(&mut h);
    clock.at(1_000 + DUR / 2);
    let mid = red_top(&render(&mut h), PROBE_X).unwrap();
    assert!(
        mid < y0 && mid > y0 - A_H,
        "卡片收缩期间 B 应在新旧之间：{mid}"
    );
    // 卡片高度过渡还在进行时再切回来：B 的布局位置跳回原处，视觉上必须连续。
    // 若把祖先的高度过渡也当成"推着走"，这次变化只记位置不过渡，B 会一帧跳开。
    assert!(h.wants_animation(), "前提：卡片高度过渡仍在进行");
    click(&mut h, TOGGLE);
    let after = red_top(&render(&mut h), PROBE_X).unwrap();
    assert!(
        (after - mid).abs() <= 2,
        "改向那一帧不能跳：{mid} → {after}"
    );
}

/// 横向位移同样过渡（横排里前面的项隐藏）。
#[test]
fn horizontal_move_animates() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("切换")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(
                Element::row()
                    .child(Element::leaf().size(A_H, 30).bg(GRAY).visible_signal(show))
                    .child(Element::leaf().size(40, 30).bg(RED).animate_layout()),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let left = |pm: &Pixmap| {
        (0..W).find(|&x| {
            let c = pm.pixel(x as u32, 45).unwrap();
            c.red() > 240 && c.green() < 15
        })
    };
    let x0 = left(&render(&mut h)).unwrap();
    click(&mut h, TOGGLE);
    render(&mut h);
    clock.at(1_000 + DUR / 2);
    let mid = left(&render(&mut h)).unwrap();
    assert!(mid < x0 && mid > x0 - A_H, "半程应在新旧之间：{mid}");
    clock.at(1_000 + DUR + 50);
    assert_eq!(left(&render(&mut h)), Some(x0 - A_H));
}

/// 节点隐藏期间布局变了，重新出现时直接落在新位置，不补播旧位移。
#[test]
fn reappearing_node_does_not_replay() {
    let _clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let b_on = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("切换")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
            .child(
                Element::leaf()
                    .size(100, 30)
                    .bg(RED)
                    .visible_signal(b_on)
                    .animate_layout(),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    b_on.set(false);
    h.damage.needs_relayout = true;
    render(&mut h);
    click(&mut h, TOGGLE);
    render(&mut h);
    b_on.set(true);
    h.damage.needs_relayout = true;
    assert_eq!(
        red_top(&render(&mut h), PROBE_X),
        Some(y0 - A_H),
        "重新出现时直接在新位置"
    );
}

/// 居中容器里**后面**的兄弟展开：容器长高、重新居中，排在前面的 B 也被逐帧推着走。
/// 只按"先序在前"判断会漏掉它，B 每帧起一段新过渡去追、明显落后；应与不声明时逐帧重合。
#[test]
fn centered_container_follows_later_reveal() {
    let clock = Clock::new(MotionLevel::Full);
    let track = |flip: bool| {
        let show = signal(false);
        let app = App::new("t", W, H).content(
            Element::stack().fill().bg(Color::WHITE).child(
                Element::col()
                    .width(120)
                    .align(crate::spec::Align::Center)
                    .child({
                        let b = Element::leaf().size(100, 30).bg(RED);
                        if flip {
                            b.animate_layout()
                        } else {
                            b
                        }
                    })
                    .child(
                        Element::button("切换")
                            .size(80, 30)
                            .on_click(move |_| show.set(!show.get())),
                    )
                    .child(
                        Element::leaf()
                            .size(100, 100)
                            .bg(GRAY)
                            .visible_signal(show)
                            .transition(crate::ui::Presence::reveal()),
                    ),
            ),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        clock.at(1_000);
        render(&mut h);
        let col = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
        let btn = h.tree.abs_bounds(h.tree.get(col).unwrap().children[1]);
        click(&mut h, Point::new(btn.x + 10, btn.y + 10));
        let mut ys = Vec::new();
        for t in (0..=300).step_by(25) {
            clock.at(1_000 + t);
            ys.push(red_top(&render(&mut h), 120).unwrap());
        }
        ys
    };
    let plain = track(false);
    assert!(
        plain[0] != *plain.last().unwrap(),
        "前提：展开把 B 推动了：{plain:?}"
    );
    assert_eq!(track(true), plain, "应逐帧跟随，不落后");
}

/// 加权区域里居中的 B：区域本身不挪，但后面的兄弟展开时它被压缩，B 随之逐帧上移。
/// 只查 B 那一支顶层节点会漏判，应与不声明时逐帧重合。
#[test]
fn centered_inside_weighted_region_follows() {
    let clock = Clock::new(MotionLevel::Full);
    let track = |flip: bool| {
        let show = signal(false);
        let app = App::new("t", W, H).content(
            Element::col()
                .fill()
                .bg(Color::WHITE)
                .child(Element::stack().width_match().weight(1.0).child({
                    let b = Element::leaf()
                        .size(100, 30)
                        .bg(RED)
                        .align(crate::spec::Align::Center);
                    if flip {
                        b.animate_layout()
                    } else {
                        b
                    }
                }))
                .child(
                    Element::button("切换")
                        .size(80, 30)
                        .on_click(move |_| show.set(!show.get())),
                )
                .child(
                    Element::leaf()
                        .size(100, 100)
                        .bg(GRAY)
                        .visible_signal(show)
                        .transition(crate::ui::Presence::reveal()),
                ),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        clock.at(1_000);
        render(&mut h);
        let btn = h.tree.get(h.tree.root.unwrap()).unwrap().children[1];
        let r = h.tree.abs_bounds(btn);
        click(&mut h, Point::new(r.x + 10, r.y + 10));
        let mut ys = Vec::new();
        for t in (0..=300).step_by(25) {
            clock.at(1_000 + t);
            ys.push(red_top(&render(&mut h), 150).unwrap());
        }
        ys
    };
    let plain = track(false);
    assert!(
        plain[0] != *plain.last().unwrap(),
        "前提：被推动：{plain:?}"
    );
    assert_eq!(track(true), plain, "应逐帧跟随，不落后");
}

/// 层叠容器里的兄弟互相推不动：一层在展开，另一层里 A 隐藏让 B 上移，照常过渡。
#[test]
fn stacked_sibling_reveal_does_not_suppress() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let other = signal(false);
    let app = App::new("t", W, H).content(
        Element::stack()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::col()
                    .child(
                        Element::row()
                            .child(
                                Element::button("A")
                                    .size(80, 30)
                                    .on_click(move |_| show.set(!show.get())),
                            )
                            .child(
                                Element::button("D")
                                    .size(80, 30)
                                    .on_click(move |_| other.set(!other.get())),
                            ),
                    )
                    .child(Element::leaf().size(100, A_H).bg(GRAY).visible_signal(show))
                    .child(Element::leaf().size(100, 30).bg(RED).animate_layout()),
            )
            .child(
                // 另一层：靠右上角，展开不碰 B。
                Element::leaf()
                    .size(60, 80)
                    .bg(Color::rgb(0, 0, 255))
                    .align(crate::spec::Align::Start)
                    .place(200, 0)
                    .visible_signal(other)
                    .transition(crate::ui::Presence::reveal()),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let y0 = red_top(&render(&mut h), PROBE_X).unwrap();
    click(&mut h, Point::new(90, 10));
    render(&mut h);
    clock.at(1_016);
    render(&mut h);
    assert!(h.wants_animation(), "前提：另一层正在展开");
    clock.at(1_032);
    click(&mut h, TOGGLE);
    assert_eq!(
        red_top(&render(&mut h), PROBE_X),
        Some(y0),
        "B 应从旧位置起步"
    );
}

/// 自己居中、又在做高度过渡的容器：它每帧重新居中，里面的 B 跟着被推。展开节点是 B 的
/// 祖先这一情形，只看"祖先自己的高度过渡只裁剪"会漏掉它本身的落点也在变。
#[test]
fn centered_height_animating_ancestor_carries_child() {
    let clock = Clock::new(MotionLevel::Full);
    let track = |flip: bool| {
        let show = signal(false);
        let app = App::new("t", W, H).content(
            Element::stack().fill().bg(Color::WHITE).child(
                Element::col()
                    .width(120)
                    .align(crate::spec::Align::Center)
                    .animate_height()
                    .child({
                        let b = Element::leaf().size(100, 30).bg(RED);
                        if flip {
                            b.animate_layout()
                        } else {
                            b
                        }
                    })
                    .child(
                        Element::button("切换")
                            .size(80, 30)
                            .on_click(move |_| show.set(!show.get())),
                    )
                    .child(Element::leaf().size(100, 100).bg(GRAY).visible_signal(show)),
            ),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        clock.at(1_000);
        render(&mut h);
        let col = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
        let btn = h.tree.abs_bounds(h.tree.get(col).unwrap().children[1]);
        click(&mut h, Point::new(btn.x + 10, btn.y + 10));
        let mut ys = Vec::new();
        for t in (0..=300).step_by(25) {
            clock.at(1_000 + t);
            ys.push(red_top(&render(&mut h), 120).unwrap());
        }
        ys
    };
    let plain = track(false);
    assert!(
        plain.windows(2).filter(|w| w[0] != w[1]).count() > 2,
        "前提：容器逐帧重新居中、B 被逐帧推动：{plain:?}"
    );
    assert_eq!(track(true), plain, "应逐帧跟随，不落后");
}
