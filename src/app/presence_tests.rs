//! 进出场过渡（`Element::transition`）走真实宿主路径的契约：点击写信号 → 宿主重排/绘制，
//! 帧时钟钉住以取确定的中间帧，按像素量化（不看内部状态）。
//!
//! 被测节点是白底上的纯红块：不透明时绿通道为 0，完全消失为 255，半透明落在中间——
//! 绿通道即"残影还剩多少"的读数。

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::event::{MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::{Element, Presence};
use tiny_skia::Pixmap;

const W: i32 = 300;
const H: i32 = 240;
const RED: Color = Color::rgb(255, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 255);

/// 钉住时钟、设定档位；drop 时复原（thread_local，免得污染同线程后续用例）。
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

fn render(h: &mut UiHost) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    pm
}

fn click(h: &mut UiHost, p: Point) {
    h.on_pointer(PointerEvent::single(
        PointerKind::Down,
        p,
        MouseButton::Left,
    ));
    h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
}

fn center(r: Rect) -> Point {
    Point::new(r.x + r.w / 2, r.y + r.h / 2)
}

fn green_at(pm: &Pixmap, p: Point) -> u8 {
    pm.pixel(p.x as u32, p.y as u32).unwrap().green()
}

/// 顶上一个切换按钮，其下依次是红块 A（带过渡）与蓝块 B（不带）。
fn host(show: Signal<bool>, presence: Presence) -> UiHost {
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
                    .size(100, 30)
                    .bg(RED)
                    .visible_signal(show)
                    .transition(presence),
            )
            .child(Element::leaf().size(100, 30).bg(BLUE)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn child_bounds(h: &UiHost, i: usize) -> Rect {
    let root = h.tree.root.unwrap();
    h.tree.abs_bounds(h.tree.get(root).unwrap().children[i])
}

/// 离场：残影逐渐变淡、期间继续占位但点不到；播完把空间让给同级。
#[test]
fn exit_fades_keeps_space_then_releases_it() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let mut h = host(show, Presence::fade());
    let pm = render(&mut h);
    let a = child_bounds(&h, 1);
    let b_before = child_bounds(&h, 2);
    let mid = center(a);
    assert_eq!(green_at(&pm, mid), 0, "首次出现不回放进场：首帧即不透明");

    click(&mut h, Point::new(10, 10));
    assert!(!show.get(), "前提：点击写了信号");
    render(&mut h); // 本帧改向，进度仍为 1
    clock.at(1_000 + 60); // 离场默认 fast = 120ms
    let pm = render(&mut h);
    let g = green_at(&pm, mid);
    assert!(g > 20 && g < 235, "离场半程应是半透明残影，绿通道 {g}");
    assert_eq!(
        child_bounds(&h, 2),
        b_before,
        "离场期间继续占位，同级不补位"
    );
    let a_id = h.tree.get(h.tree.root.unwrap()).unwrap().children[1];
    assert_ne!(h.tree.hit_test(mid), Some(a_id), "离场中的残影点不到");

    clock.at(1_000 + 200);
    let pm = render(&mut h);
    assert_eq!(green_at(&pm, mid), 255, "离场播完：不再绘制");
    // 真窗口里没人要帧就不会再画；测试里直接调 render 总会画整帧（整帧必重排），
    // 不先断言这一条，"空间让不出来"在这里就看不出来。
    assert!(h.wants_animation(), "播完那帧须再要一帧，把空间让出来");
    assert!(
        h.damage.needs_relayout,
        "而且那一帧须重排（只重绘的话空间仍让不出来）"
    );
    render(&mut h);
    assert_eq!(child_bounds(&h, 2).y, a.y, "播完后空间让给同级");
}

/// 进场从透明起，而不是首帧就落定；中途再隐藏从当前值掉头。
#[test]
fn enter_fades_in_and_reverses_midway() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(false);
    let mut h = host(show, Presence::fade());
    render(&mut h);
    click(&mut h, Point::new(10, 10));
    assert!(show.get());
    render(&mut h);
    let mid = center(child_bounds(&h, 1));
    clock.at(1_000 + 40); // 进场默认 normal = 200ms
    let early = green_at(&render(&mut h), mid);
    clock.at(1_000 + 100);
    let later = green_at(&render(&mut h), mid);
    assert!(
        early > later && later > 0,
        "进场应逐渐变实：{early} → {later}"
    );

    click(&mut h, Point::new(10, 10)); // 半路隐藏
    let turned = green_at(&render(&mut h), mid);
    assert!(
        (turned as i32 - later as i32).abs() <= 2,
        "掉头瞬间应保持当前浓度：{later} → {turned}"
    );
    clock.at(1_000 + 100 + 60);
    assert!(green_at(&render(&mut h), mid) > turned, "掉头后往透明走");
}

/// 对话框形态：遮罩显隐，面板自己声明滑动。面板藏在从未弹出过的遮罩里，第一次弹出
/// 也必须进场——它的初值得按"祖先不可见"落定。
#[test]
fn panel_inside_hidden_scrim_enters_on_first_show() {
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(false);
    let app = App::new("t", W, H).content(
        Element::stack()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("弹出")
                    .size(80, 30)
                    .on_click(move |_| show.set(true)),
            )
            .child(
                Element::stack()
                    .fill()
                    .visible_signal(show)
                    .transition(Presence::fade().duration(0, 0))
                    .child(
                        Element::leaf()
                            .size(120, 60)
                            .bg(RED)
                            .align(crate::spec::Align::Center)
                            .transition(Presence::default().slide(0, 40)),
                    ),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    click(&mut h, Point::new(10, 10));
    render(&mut h);
    // 面板终位：居中，顶边 y = (240-60)/2 = 90。沿中线找第一行红色。
    let top_edge = |pm: &Pixmap| (0..H).find(|&y| green_at(pm, Point::new(W / 2, y)) == 0);
    clock.at(1_000 + 60);
    let mid = top_edge(&render(&mut h)).expect("进场中面板应已可见");
    assert!(mid > 90 && mid < 130, "进场半路应在终位下方，顶边 y={mid}");
    clock.at(1_000 + 1_000);
    assert_eq!(top_edge(&render(&mut h)), Some(90), "进场结束落在终位");
}

/// 减弱档：滑动取消、淡入保留。全开档作对照，免得"本来就不滑"被当成"减弱生效"。
#[test]
fn reduced_motion_drops_slide_keeps_fade() {
    let probe = |level: MotionLevel| -> (Rect, u8) {
        let clock = Clock::new(level);
        let show = signal(false);
        let mut h = host(show, Presence::fade().slide(40, 0));
        render(&mut h);
        click(&mut h, Point::new(10, 10));
        render(&mut h);
        clock.at(1_000 + 60);
        let pm = render(&mut h);
        let a = child_bounds(&h, 1);
        // 红块左缘：布局位置的左侧 1px 处若已染色，说明画面相对布局右移了。
        let shifted = (a.x..a.x + 60).find(|&x| green_at(&pm, Point::new(x, center(a).y)) < 250);
        let left = Rect::new(shifted.unwrap_or(a.x), a.y, a.w, a.h);
        (left, green_at(&pm, Point::new(a.x + 90, center(a).y)))
    };
    let (full_rect, _) = probe(MotionLevel::Full);
    let (reduced_rect, reduced_g) = probe(MotionLevel::Reduced);
    assert!(
        full_rect.x > 0,
        "对照：全开档进场半路应右移，左缘 {}",
        full_rect.x
    );
    assert_eq!(reduced_rect.x, 0, "减弱档不滑动");
    assert!(
        reduced_g > 20 && reduced_g < 235,
        "减弱档淡入保留，绿通道 {reduced_g}"
    );
}

/// 下拉菜单展开淡入：与终态的像素差随时间单调收敛到 0。
#[test]
fn menu_fades_in_on_open() {
    let clock = Clock::new(MotionLevel::Full);
    let (mut h, _) = crate::app::test_support::dropdown_handler();
    click(&mut h, Point::new(110, 25));
    assert!(h.menu.is_open(), "前提：点下拉展开了菜单");
    let frame = |h: &mut UiHost, ms: u64| {
        clock.at(ms);
        h.damage.needs_full = true;
        render(h)
    };
    let start = frame(&mut h, 1_000);
    let mid = frame(&mut h, 1_000 + 40);
    let done = frame(&mut h, 1_000 + 1_000);
    // 只比菜单面板内：下拉框自己的边框色补间也在这几帧里跑，整窗比会把它算进来。
    let panel = h.menu.active.as_ref().unwrap().levels[0].rect;
    let diff = |a: &Pixmap| -> u64 {
        let mut sum = 0;
        for y in panel.y..panel.y + panel.h {
            for x in panel.x..panel.x + panel.w {
                let (p, q) = (
                    a.pixel(x as u32, y as u32).unwrap(),
                    done.pixel(x as u32, y as u32).unwrap(),
                );
                sum += (p.red() as i64 - q.red() as i64).unsigned_abs()
                    + (p.green() as i64 - q.green() as i64).unsigned_abs()
                    + (p.blue() as i64 - q.blue() as i64).unsigned_abs();
            }
        }
        sum
    };
    assert!(diff(&start) > diff(&mid), "淡入应逐渐接近终态");
    assert!(diff(&mid) > 0, "40ms 时尚未完成（fast = 120ms）");
    assert_eq!(diff(&frame(&mut h, 1_000 + 2_000)), 0, "淡入结束后静止");
}

/// 普通隐藏的祖先（切走的标签页）期间子节点被隐藏：切回来时既不能以旧状态冒出残影并
/// 占位，也不补播离场——那段时间它不在绘制路径上，进度须就地落定。
#[test]
fn hidden_while_ancestor_hidden_does_not_ghost_on_reveal() {
    let clock = Clock::new(MotionLevel::Full);
    let page = signal(true);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("页")
                    .size(80, 30)
                    .on_click(move |_| page.set(!page.get())),
            )
            .child(
                Element::button("示")
                    .size(80, 30)
                    .on_click(move |_| show.set(!show.get())),
            )
            .child(
                Element::col()
                    .visible_signal(page)
                    .child(
                        Element::leaf()
                            .size(100, 30)
                            .bg(RED)
                            .visible_signal(show)
                            .transition(Presence::fade()),
                    )
                    .child(Element::leaf().size(100, 30).bg(BLUE)),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    click(&mut h, Point::new(10, 10)); // 切走
    render(&mut h);
    click(&mut h, Point::new(10, 45)); // 切走期间隐藏提示
    render(&mut h);
    clock.at(5_000);
    render(&mut h);
    click(&mut h, Point::new(10, 10)); // 切回
    let pm = render(&mut h);
    let red_rows = (60..H)
        .filter(|&y| {
            let p = pm.pixel(50, y as u32).unwrap();
            p.green() < 128 && p.blue() < 128
        })
        .count();
    assert_eq!(red_rows, 0, "切回时不得冒出提示的残影");
    let blue_top = (60..H).find(|&y| pm.pixel(50, y as u32).unwrap().red() < 128);
    assert_eq!(
        blue_top,
        Some(60),
        "提示已隐藏，蓝块应直接顶到页首，不替残影占位"
    );
    // 不看 `wants_animation`：被点的按钮自己的悬停补间也在要帧。
    let page_col = h.tree.get(h.tree.root.unwrap()).unwrap().children[2];
    let hint = h.tree.get(page_col).unwrap().children[0];
    let p = h.tree.get(hint).unwrap().presence.as_ref().unwrap();
    assert!(!p.is_active(), "也不补播离场");
}

/// 对话框（经快捷键等非指针路径）弹出时收起的浮层，在对话框关闭后不得以旧状态重现。
#[test]
fn overlay_shut_by_modal_does_not_reappear() {
    let clock = Clock::new(MotionLevel::Full);
    let open = signal(false);
    let dlg = signal(false);
    let app = App::new("t", W, H).content(
        Element::stack()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::col().child(
                    Element::button("开")
                        .size(80, 30)
                        .on_click(move |_| open.set(true))
                        .popup(open, Element::leaf().size(100, 60).bg(RED)),
                ),
            )
            .child(Element::dialog(dlg, Element::leaf().size(40, 40).bg(BLUE))),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    click(&mut h, Point::new(10, 10));
    render(&mut h);
    clock.at(3_000);
    let spot = Point::new(50, 60);
    assert_eq!(green_at(&render(&mut h), spot), 0, "前提：浮层已完整展开");
    // 不经指针弹出对话框（快捷键 / 定时器那类），走不到轻量关闭。
    dlg.set(true);
    h.damage.needs_relayout = true;
    render(&mut h);
    assert!(!open.get(), "前提：对话框弹出后域外浮层被收起");
    clock.at(10_000);
    render(&mut h);
    dlg.set(false);
    h.damage.needs_relayout = true;
    clock.at(20_000);
    // 判"偏红"而非绿通道：这一帧对话框自己的遮罩正在淡出，灰色会压低绿通道。
    let p = render(&mut h).pixel(spot.x as u32, spot.y as u32).unwrap();
    assert!(
        (p.red() as i32) - (p.green() as i32) < 40,
        "对话框关闭后，被它收起的浮层不得以残影重现：rgb=({},{},{})",
        p.red(),
        p.green(),
        p.blue()
    );
}

/// 动画关闭时，声明了过渡的节点与没声明的逐帧一致：隐藏当帧即让位，不多一帧空洞。
#[test]
fn off_level_hides_without_a_gap_frame() {
    let _clock = Clock::new(MotionLevel::Off);
    let show = signal(true);
    let mut h = host(show, Presence::fade());
    render(&mut h);
    let a = child_bounds(&h, 1);
    click(&mut h, Point::new(10, 10));
    render(&mut h);
    assert_eq!(child_bounds(&h, 2).y, a.y, "隐藏当帧同级即补位");
    assert!(!h.wants_animation());
}

/// 局部帧（跨帧复用同一块画布）里，离场节点子孙的投影也要随之擦掉——脏区只按离场节点
/// 自己的矩形报，伸出去的投影会孤零零地留在画面上。
#[test]
fn exit_damage_covers_descendant_shadow() {
    // 窗口放大：投影外延按 3×blur 算后，小窗口里这张卡片的脏区会超过半窗、被升成整窗帧，
    // 测不到局部帧——而在常见尺寸的窗口里它应当仍走局部帧（末尾的断言即锁住这一点）。
    const BW: i32 = 800;
    const BH: i32 = 600;
    let clock = Clock::new(MotionLevel::Full);
    let show = signal(true);
    let app =
        App::new("t", BW, BH).content(
            Element::stack()
                .fill()
                .bg(Color::WHITE)
                .child(
                    Element::button("切")
                        .size(60, 30)
                        .on_click(move |_| show.set(!show.get())),
                )
                .child(
                    Element::col()
                        .align(crate::spec::Align::Center)
                        .visible_signal(show)
                        .transition(Presence::fade())
                        .child(Element::leaf().size(80, 40).bg(RED).shadow(
                            crate::style::Shadow::new(0.0, 8.0, 14.0, Color::rgb(0, 0, 0)),
                        )),
                ),
        );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let mut pm = Pixmap::new(BW as u32, BH as u32).unwrap();
    let frame = |h: &mut UiHost, pm: &mut Pixmap| {
        h.render(&mut PixmapTarget { pixmap: pm }, Size::new(BW, BH))
    };
    frame(&mut h, &mut pm);
    let card = {
        let root = h.tree.root.unwrap();
        let wrap = h.tree.get(root).unwrap().children[1];
        h.tree.abs_bounds(h.tree.get(wrap).unwrap().children[0])
    };
    let shadow = Point::new(card.x + card.w / 2, card.y + card.h + 8);
    assert!(green_at(&pm, shadow) < 250, "前提：卡片下方有投影");
    click(&mut h, Point::new(10, 10));
    let mut partial_seen = false;
    for t in [0u64, 30, 60, 90, 120, 150, 300, 400] {
        clock.at(1_000 + t);
        frame(&mut h, &mut pm);
        partial_seen |= !h.damage.last_frame_full;
        if green_at(&pm, center(card)) == 255 {
            assert_eq!(
                green_at(&pm, shadow),
                255,
                "{t}ms：卡片已擦掉，投影不得残留"
            );
        }
    }
    assert!(
        partial_seen,
        "大投影卡片的过渡在常见尺寸窗口里应仍走局部帧（否则脏区估算过大，也测不到脏区）"
    );
    assert_eq!(green_at(&pm, shadow), 255);
}

/// 页上开着浮层时切走标签页，之后另一页上任何结构变化都不得让浮层整块闪到当前页上。
#[test]
fn overlay_in_hidden_tab_does_not_flash_on_unrelated_change() {
    let clock = Clock::new(MotionLevel::Full);
    let page = signal(true);
    let open = signal(false);
    let other = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("页")
                    .size(80, 30)
                    .on_click(move |_| page.set(!page.get())),
            )
            .child(
                Element::button("他")
                    .size(80, 30)
                    .on_click(move |_| other.set(!other.get())),
            )
            .child(Element::leaf().size(10, 10).bg(BLUE).visible_signal(other))
            .child(
                Element::col().visible_signal(page).child(
                    Element::button("开")
                        .size(80, 30)
                        .on_click(move |_| open.set(true))
                        .popup(open, Element::leaf().size(100, 60).bg(RED)),
                ),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    let anchor = {
        let r = h.tree.root.unwrap();
        let c = h.tree.get(r).unwrap().children[3];
        h.tree.abs_bounds(h.tree.get(c).unwrap().children[0])
    };
    click(&mut h, center(anchor));
    render(&mut h);
    clock.at(3_000);
    let spot = Point::new(anchor.x + 50, anchor.y + anchor.h + 30);
    assert_eq!(green_at(&render(&mut h), spot), 0, "前提：浮层已展开");
    // 切页与随后的变化都走非指针路径（快捷键 / 定时器那类）：点击会顺手把浮层"点外收起"，
    // 展开信号变 false，就测不到"信号还开着、锚点却已不在绘制路径上"这一形态了。
    page.set(false);
    h.damage.needs_relayout = true;
    render(&mut h);
    assert!(open.get(), "前提：非指针切页不收起浮层");
    clock.at(6_000);
    render(&mut h);
    other.set(false); // 另一页上不相干的结构变化
    h.damage.needs_relayout = true;
    // 跨帧复用同一块画布（与真窗口一致）：其后是局部帧，新建画布的话脏区外是透明黑。
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    for t in [6_000u64, 6_030, 6_060] {
        clock.at(t);
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
        assert_eq!(
            green_at(&pm, spot),
            255,
            "{t}ms：切走的页上的浮层不得闪到当前页"
        );
    }
}

/// 落定规则不能误伤正常收起：点浮层外收起时照常淡出。
#[test]
fn popup_still_fades_out_on_normal_close() {
    let clock = Clock::new(MotionLevel::Full);
    let open = signal(false);
    let app = App::new("t", W, H).content(
        Element::stack().fill().bg(Color::WHITE).child(
            Element::col().child(
                Element::button("开")
                    .size(80, 30)
                    .on_click(move |_| open.set(true))
                    .popup(open, Element::leaf().size(100, 60).bg(RED)),
            ),
        ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    click(&mut h, Point::new(10, 10));
    render(&mut h);
    clock.at(3_000);
    let spot = Point::new(50, 60);
    assert_eq!(green_at(&render(&mut h), spot), 0, "前提：浮层已展开");
    click(&mut h, Point::new(250, 200)); // 点浮层外，轻量关闭
    assert!(!open.get(), "前提：已收起");
    render(&mut h);
    clock.at(3_000 + 60);
    let g = green_at(&render(&mut h), spot);
    assert!(g > 20 && g < 235, "正常收起应淡出，半程绿通道 {g}");
}

/// 显隐变化只经过一次整窗帧（别的窗口写了共享信号，本窗只收到整窗失效、没有重排请求）
/// 也必须触发落定：整窗帧补布局时若直接覆盖签名，这次变化就被吞掉了。
#[test]
fn visibility_change_absorbed_by_full_frame_still_settles() {
    let clock = Clock::new(MotionLevel::Full);
    let page = signal(true);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("页")
                    .size(80, 30)
                    .on_click(move |_| page.set(!page.get())),
            )
            .child(
                Element::col()
                    .visible_signal(page)
                    .child(
                        Element::leaf()
                            .size(100, 30)
                            .bg(RED)
                            .visible_signal(show)
                            .transition(Presence::fade()),
                    )
                    .child(Element::leaf().size(100, 30).bg(BLUE)),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    click(&mut h, Point::new(10, 10)); // 切走
    render(&mut h);
    clock.at(3_000);
    render(&mut h);
    show.set(false); // 外部写入：只来一次整窗失效
    h.damage.needs_relayout = false;
    h.damage.needs_full = true;
    render(&mut h);
    clock.at(5_000);
    render(&mut h);
    click(&mut h, Point::new(10, 10)); // 切回
    let pm = render(&mut h);
    let red_rows = (30..H)
        .filter(|&y| {
            let p = pm.pixel(50, y as u32).unwrap();
            p.green() < 128 && p.blue() < 128
        })
        .count();
    assert_eq!(red_rows, 0, "切回时不得冒出残影");
}

/// 蓝块顶边 y（沿 x=50 自上而下找第一行纯蓝）。
fn blue_top(pm: &Pixmap) -> Option<i32> {
    (0..H).find(|&y| {
        let p = pm.pixel(50, y as u32).unwrap();
        p.blue() > 200 && p.red() < 60 && p.green() < 60
    })
}

/// 染了红的行数（沿 x=50）：白底上的红，不论淡入到几成（绿蓝两通道等量压低）。
fn red_rows(pm: &Pixmap) -> usize {
    (0..H)
        .filter(|&y| {
            let p = pm.pixel(50, y as u32).unwrap();
            p.red() > 200 && p.green() < 245 && (p.green() as i32 - p.blue() as i32).abs() < 10
        })
        .count()
}

/// 折叠面板（标题头 + 红色 body 60）其下一个蓝块。
fn collapsible_host(expanded: crate::signal::Signal<bool>) -> UiHost {
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(Element::collapsible(
                "分组",
                expanded,
                // 红块包在容器里：裁剪管的是子树，body 自己的背景本就按缩小后的矩形画。
                Element::col().child(Element::leaf().size(100, 60).bg(RED)),
            ))
            // 只有 4px 高：裁剪若失效，溢出的 body 不会被它整块盖住而看不出来。
            .child(Element::leaf().size(100, 4).bg(BLUE)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

/// 折叠面板展开：body 高度从 0 长到 60，期间蓝块平滑下移、body 被裁到当前高度；
/// 收起时反向。减弱动效档位下瞬时到位（对照组）。
#[test]
fn collapsible_body_expands_and_collapses() {
    let clock = Clock::new(MotionLevel::Full);
    let expanded = signal(false);
    let mut h = collapsible_host(expanded);
    let pm = render(&mut h);
    let header_h = blue_top(&pm).expect("前提：蓝块可见");
    assert_eq!(red_rows(&pm), 0, "初始收起");

    click(&mut h, Point::new(50, header_h / 2));
    assert!(expanded.get(), "前提：点标题头展开");
    render(&mut h);
    clock.at(1_000 + 60);
    render(&mut h); // 按上一帧请求重排
    let pm = render(&mut h);
    let mid = blue_top(&pm).unwrap();
    assert!(
        mid > header_h && mid < header_h + 60,
        "展开中途蓝块应在两端之间：{header_h} < {mid} < {}",
        header_h + 60
    );
    assert_eq!(
        red_rows(&pm) as i32,
        mid - header_h,
        "body 裁到当前占位高度，不压到同级上"
    );
    clock.at(1_000 + 1_000);
    render(&mut h);
    let pm = render(&mut h);
    assert_eq!(blue_top(&pm), Some(header_h + 60), "展开结束");
    assert_eq!(red_rows(&pm), 60);

    click(&mut h, Point::new(50, header_h / 2));
    render(&mut h);
    clock.at(1_000 + 1_000 + 40); // 收起走 fast = 120ms
    render(&mut h);
    let mid = blue_top(&render(&mut h)).unwrap();
    assert!(mid > header_h && mid < header_h + 60, "收起中途 {mid}");
    clock.at(1_000 + 3_000);
    render(&mut h);
    render(&mut h);
    assert_eq!(
        blue_top(&render(&mut h)),
        Some(header_h),
        "收起结束让出空间"
    );

    drop(clock);
    let clock = Clock::new(MotionLevel::Reduced);
    let expanded = signal(false);
    let mut h = collapsible_host(expanded);
    render(&mut h);
    click(&mut h, Point::new(50, header_h / 2));
    render(&mut h);
    clock.at(1_000 + 60);
    render(&mut h);
    assert_eq!(
        blue_top(&render(&mut h)),
        Some(header_h + 60),
        "减弱档：展开瞬时到位"
    );
}

/// `animate_height`：内容自然高度 30 → 60 时，占位高度平滑过渡，同级随之移动。
#[test]
fn animate_height_follows_natural_height() {
    let clock = Clock::new(MotionLevel::Full);
    let more = signal(false);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(
                Element::button("多")
                    .size(80, 30)
                    .on_click(move |_| more.set(!more.get())),
            )
            .child(
                Element::col()
                    .animate_height()
                    .child(Element::leaf().size(100, 30).bg(RED))
                    .child(Element::leaf().size(100, 30).bg(RED).visible_signal(more)),
            )
            .child(Element::leaf().size(100, 20).bg(BLUE)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    assert_eq!(
        blue_top(&render(&mut h)),
        Some(60),
        "首次测量直接落定，不从 0 长出来"
    );
    click(&mut h, Point::new(10, 10));
    render(&mut h);
    clock.at(1_000 + 60);
    render(&mut h);
    let pm = render(&mut h);
    let mid = blue_top(&pm).unwrap();
    assert!(mid > 60 && mid < 90, "过渡中途 {mid}");
    assert_eq!(red_rows(&pm) as i32, mid - 30, "内容裁到当前高度");
    clock.at(1_000 + 1_000);
    render(&mut h);
    assert_eq!(blue_top(&render(&mut h)), Some(90), "过渡结束");
}

/// 叶子当折叠内容（label 是最常见的写法）：展开中途文字不得画出 body 的当前矩形。
/// 卷帘若只裁子树，叶子自己的绘制不受裁剪，会压到同级上。
#[test]
fn leaf_body_does_not_overflow_while_expanding() {
    let clock = Clock::new(MotionLevel::Full);
    let expanded = signal(false);
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(Element::collapsible(
                "分组",
                expanded,
                Element::label("HHHH")
                    .fg(RED)
                    .font_size(40.0)
                    .width(200)
                    .height(60),
            )),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    click(&mut h, Point::new(50, 10));
    render(&mut h);
    let body_id = {
        let col = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
        h.tree.get(col).unwrap().children[1]
    };
    let mut saw_partial = false;
    for t in [16u64, 32, 48, 64] {
        clock.at(1_000 + t);
        render(&mut h);
        let pm = render(&mut h);
        let body = h.tree.abs_bounds(body_id);
        saw_partial |= body.h > 0 && body.h < 60;
        let outside = (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .filter(|&(x, y)| !body.contains(Point::new(x, y)))
            .filter(|&(x, y)| {
                let p = pm.pixel(x as u32, y as u32).unwrap();
                p.red() > 150 && p.green() < 120 && p.blue() < 120
            })
            .count();
        assert_eq!(
            outside, 0,
            "{t}ms：body 外不得出现它的红字（body={body:?}）"
        );
    }
    assert!(saw_partial, "前提：取样落在展开途中");
}

/// 展开结束后不再续帧、不再请求重排（空闲零 CPU）；减弱档下淡入期间也不逐帧重排
/// （高度不变，重排白做）。
#[test]
fn expand_goes_idle_and_reduced_does_not_relayout() {
    let clock = Clock::new(MotionLevel::Full);
    let expanded = signal(false);
    let mut h = collapsible_host(expanded);
    render(&mut h);
    click(&mut h, Point::new(50, 10));
    h.on_pointer(PointerEvent::single(
        PointerKind::Move,
        Point::new(W - 5, H - 5),
        MouseButton::Left,
    ));
    render(&mut h);
    clock.at(1_000 + 2_000);
    render(&mut h);
    render(&mut h);
    assert!(!h.wants_animation(), "展开结束后不再续帧");
    assert!(!h.damage.needs_relayout, "也不再请求重排");
    drop(clock);

    let clock = Clock::new(MotionLevel::Reduced);
    let expanded = signal(false);
    let mut h = collapsible_host(expanded);
    render(&mut h);
    click(&mut h, Point::new(50, 10));
    render(&mut h);
    clock.at(1_000 + 60);
    render(&mut h);
    assert!(h.wants_animation(), "前提：减弱档下淡入仍在播");
    assert!(!h.damage.needs_relayout, "减弱档高度不变，不该逐帧重排");
}

/// 蓝块在 `t` 毫秒时相对静止位置的下移量。`nested` 时折叠面板外再套一层 `animate_height`。
fn expand_offset_at(nested: bool, t: u64) -> i32 {
    let clock = Clock::new(MotionLevel::Full);
    let expanded = signal(false);
    let panel = Element::collapsible(
        "分组",
        expanded,
        Element::col().child(Element::leaf().size(100, 60).bg(RED)),
    );
    let panel = if nested {
        Element::col().animate_height().child(panel)
    } else {
        panel
    };
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(panel)
            .child(Element::leaf().size(100, 4).bg(BLUE)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let rest = blue_top(&render(&mut h)).unwrap();
    click(&mut h, Point::new(50, 10));
    render(&mut h);
    let mut ms = 0;
    while ms < t {
        ms = (ms + 16).min(t);
        clock.at(1_000 + ms);
        render(&mut h);
    }
    blue_top(&render(&mut h)).unwrap() - rest
}

/// 外层 `animate_height` 套着正在展开的折叠面板：外层每帧改向，须继承速度追上去，
/// 而不是每帧从静止重起、远远落在后面（内容大半时间被裁掉）。
#[test]
fn nested_animate_height_keeps_up() {
    let plain = expand_offset_at(false, 64);
    let nested = expand_offset_at(true, 64);
    assert!(plain > 20, "前提：单独展开 64ms 已走过一段，{plain}");
    assert!(
        nested * 2 >= plain,
        "嵌套时外层不应远远落后：单独 {plain}，嵌套 {nested}"
    );
    assert_eq!(expand_offset_at(true, 2_000), 60, "嵌套最终收敛到自然高度");
}

/// 标题头区域的像素（x 全宽、y 0..header_h）。
fn header_pixels(pm: &Pixmap, header_h: i32) -> Vec<u8> {
    (0..header_h)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            let p = pm.pixel(x as u32, y as u32).unwrap();
            [p.red(), p.green(), p.blue()]
        })
        .collect()
}

/// 箭头旋转：中途既不是 `>` 也不是 `v`；转完与"一开始就展开"的静态箭头逐像素一致
/// （旋转后的 `>` 与静态 `v` 的尖端差 1px，不补就在最后一帧跳一下）。
#[test]
fn chevron_turns_and_lands_exactly_on_the_static_arrow() {
    let clock = Clock::new(MotionLevel::Full);
    let mut fixed = collapsible_host(signal(true));
    let expanded_static = render(&mut fixed);
    let header_h = blue_top(&render(&mut collapsible_host(signal(false)))).unwrap();
    let want = header_pixels(&expanded_static, header_h);

    let expanded = signal(false);
    let mut h = collapsible_host(expanded);
    let collapsed = header_pixels(&render(&mut h), header_h);
    click(&mut h, Point::new(50, 10));
    // 指针移开，免得悬停底色混进比较。
    h.on_pointer(PointerEvent::single(
        PointerKind::Move,
        Point::new(W - 5, H - 5),
        MouseButton::Left,
    ));
    render(&mut h);
    clock.at(1_000 + 60);
    let mid = header_pixels(&render(&mut h), header_h);
    assert_ne!(mid, collapsed, "转到一半不应还是 >");
    assert_ne!(mid, want, "转到一半不应已是 v");
    clock.at(1_000 + 2_000);
    render(&mut h);
    let done = header_pixels(&render(&mut h), header_h);
    assert!(done == want, "转完须与静态展开箭头逐像素一致");
}

/// 隐藏期间展开态被程序改过：露出来时箭头直接是新状态，不补播旋转。
#[test]
fn chevron_settles_when_changed_offscreen() {
    let clock = Clock::new(MotionLevel::Full);
    let page = signal(true);
    let expanded = signal(false);
    let app =
        App::new("t", W, H).content(
            Element::col().fill().bg(Color::WHITE).child(
                Element::col()
                    .visible_signal(page)
                    .child(Element::collapsible(
                        "分组",
                        expanded,
                        Element::col().child(Element::leaf().size(100, 60).bg(RED)),
                    )),
            ),
        );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    // 指针移开：结构变化后宿主按最后指针位置重同步悬停，悬停底色会混进比较。
    h.on_pointer(PointerEvent::single(
        PointerKind::Move,
        Point::new(W - 5, H - 5),
        MouseButton::Left,
    ));
    page.set(false);
    h.damage.needs_relayout = true;
    render(&mut h);
    expanded.set(true); // 隐藏期间程序改状态
    h.damage.needs_relayout = true;
    render(&mut h);
    clock.at(5_000);
    page.set(true);
    h.damage.needs_relayout = true;
    let shown = render(&mut h);
    clock.at(9_000);
    render(&mut h);
    h.damage.needs_full = true; // 每帧新建画布，局部帧的脏区外是透明黑
    let later = render(&mut h);
    assert!(
        header_pixels(&shown, 40) == header_pixels(&later, 40),
        "露出那一帧箭头就该是终态，不补播旋转"
    );
    assert_eq!(red_rows(&shown), 60, "内容也直接是展开态");
}

/// 外层 `animate_height` 跟随完内层展开、树已空闲之后，下一次无关的高度变化仍要过渡，
/// 不能沿用"跟随子树"的落定一帧跳到位。按宿主要帧的节奏出帧（帧间隔大时更容易在
/// 内层停下那帧留下零头），改动走非指针路径（指针 Down 的那次重排会顺手掩盖问题）。
#[test]
fn animate_height_stops_following_once_inner_settles() {
    for step in [16u64, 50, 110] {
        let clock = Clock::new(MotionLevel::Full);
        let expanded = signal(false);
        let more = signal(false);
        let app = App::new("t", W, H).content(
            Element::col()
                .fill()
                .bg(Color::WHITE)
                .child(
                    Element::col()
                        .animate_height()
                        .child(Element::collapsible(
                            "分组",
                            expanded,
                            Element::col().child(Element::leaf().size(100, 60).bg(RED)),
                        ))
                        .child(Element::leaf().size(100, 40).bg(RED).visible_signal(more)),
                )
                .child(Element::leaf().size(100, 4).bg(BLUE)),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        render(&mut h);
        click(&mut h, Point::new(50, 10));
        let mut t = 1_000;
        for frame in 0.. {
            render(&mut h);
            if !h.wants_animation() && !h.damage.needs_relayout {
                break;
            }
            assert!(frame < 200, "step={step}：展开应收敛");
            t += step;
            clock.at(t);
        }
        let blue = |h: &UiHost| child_bounds(h, 1).y;
        let base = blue(&h);
        more.set(true);
        h.damage.needs_relayout = true;
        render(&mut h);
        clock.at(t + 60);
        render(&mut h);
        render(&mut h);
        let mid = blue(&h);
        assert!(
            mid > base && mid < base + 40,
            "step={step}：空闲后的高度变化应过渡，{base} < {mid} < {}",
            base + 40
        );
    }
}
