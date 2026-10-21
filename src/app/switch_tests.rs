//! 内容切换过渡走真实宿主路径的契约：标签页（常驻各页，切换时按方向滑入滑出）与
//! `Element::switcher`（按键重建内容、交叉淡入淡出）。帧时钟钉住取确定帧，按像素量化。
//!
//! 每页内容是左上角一块窄色块（比内容区窄，才看得出左右两缘各自滑到了哪）。

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::core::NodeId;
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 400;
const H: i32 = 240;
const BLOCK_W: i32 = 120;
const COLORS: [Color; 3] = [
    Color::rgb(255, 0, 0),
    Color::rgb(0, 0, 255),
    Color::rgb(0, 160, 0),
];

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

/// 写信号并要求重排（事件外裸写信号不会自己触发重排，见 keyed_list_tests 的同名说明）。
fn set<T: 'static>(h: &mut UiHost, sig: Signal<T>, v: T) {
    sig.set(v);
    h.damage.needs_relayout = true;
}

fn block(i: usize) -> Element {
    Element::col().child(Element::leaf().size(BLOCK_W, 60).bg(COLORS[i]))
}

fn tabs_host(sel: Signal<usize>) -> UiHost {
    let app =
        App::new("t", W, H).content(Element::col().fill().bg(Color::WHITE).padding(20).child(
            Element::tabs(
                sel,
                vec![("一", block(0)), ("二", block(1)), ("三", block(2))],
            ),
        ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

/// 标签页内容区（各页叠放的那个容器）的绝对矩形。
fn tab_content(h: &UiHost) -> (NodeId, Rect) {
    let col = h.tree.root.unwrap();
    let tabs = h.tree.get(col).unwrap().children[0];
    let content = h.tree.get(tabs).unwrap().children[1];
    (content, h.tree.abs_bounds(content))
}

/// 第 `y` 行上偏向颜色 `c` 的像素的最左 / 最右 x（含半透明，按色相判断）。
fn span(pm: &Pixmap, y: i32, c: Color) -> Option<(i32, i32)> {
    let tinted = |x: i32| {
        let p = pm.pixel(x as u32, y as u32).unwrap();
        let (r, g, b) = (p.red() as i32, p.green() as i32, p.blue() as i32);
        match (c.r, c.g, c.b) {
            (255, 0, 0) => r - g > 20 && r - b > 20,
            (0, 0, 255) => b - r > 20 && b - g > 20,
            _ => g - r > 20 && g - b > 20,
        }
    };
    let xs: Vec<i32> = (0..W).filter(|&x| tinted(x)).collect();
    Some((*xs.first()?, *xs.last()?))
}

#[test]
fn tab_switch_slides_by_direction() {
    let clock = Clock::new(MotionLevel::Full);
    let sel = signal(0usize);
    let mut h = tabs_host(sel);
    let pm = render(&mut h);
    let (_, content) = tab_content(&h);
    let y = content.y + 30;
    let x0 = content.x;
    assert_eq!(
        span(&pm, y, COLORS[0]),
        Some((x0, x0 + BLOCK_W - 1)),
        "前提：第一页在位"
    );

    // 0 → 1：新页从右侧滑入（左缘在 x0 右边），旧页向左滑出（右缘在原右缘左边）。
    set(&mut h, sel, 1);
    render(&mut h);
    clock.at(1_060);
    let pm = render(&mut h);
    let (nl, _) = span(&pm, y, COLORS[1]).expect("新页半程可见");
    let (_, or) = span(&pm, y, COLORS[0]).expect("旧页半程可见");
    assert!(nl > x0, "新页从右侧进来：左缘 {nl} 应在 {x0} 右边");
    assert!(or < x0 + BLOCK_W - 1, "旧页向左出去：右缘 {or}");
    clock.at(1_600);
    let pm = render(&mut h);
    assert_eq!(
        span(&pm, y, COLORS[1]),
        Some((x0, x0 + BLOCK_W - 1)),
        "播完新页在位"
    );
    assert_eq!(span(&pm, y, COLORS[0]), None, "旧页已不画");

    // 1 → 0：方向反过来。
    set(&mut h, sel, 0);
    render(&mut h);
    clock.at(1_660);
    let pm = render(&mut h);
    let (_, nr) = span(&pm, y, COLORS[0]).expect("新页半程可见");
    let (ol, _) = span(&pm, y, COLORS[1]).expect("旧页半程可见");
    assert!(nr < x0 + BLOCK_W - 1, "往回切：新页从左侧进来，右缘 {nr}");
    assert!(ol > x0, "往回切：旧页向右出去，左缘 {ol}");
}

#[test]
fn tab_switch_hits_the_new_page_midway() {
    // 1 → 0：离场的第 2 页在子节点顺序里排在后面、倒序命中时先被测到——它必须点不到。
    let clock = Clock::new(MotionLevel::Full);
    let sel = signal(1usize);
    let mut h = tabs_host(sel);
    render(&mut h);
    let (content, r) = tab_content(&h);
    set(&mut h, sel, 0);
    render(&mut h);
    clock.at(1_060);
    render(&mut h);
    let page1 = h.tree.get(content).unwrap().children[0];
    let hit = h.tree.hit_test(Point::new(r.x + 30, r.y + 30)).unwrap();
    let mut n = Some(hit);
    while let Some(id) = n {
        if id == page1 {
            return;
        }
        n = h.tree.get(id).and_then(|x| x.parent);
    }
    panic!("过渡中命中应落在新页里");
}

#[test]
fn tab_switch_reduced_motion_fades_without_sliding() {
    let clock = Clock::new(MotionLevel::Reduced);
    let sel = signal(0usize);
    let mut h = tabs_host(sel);
    render(&mut h);
    let (_, content) = tab_content(&h);
    set(&mut h, sel, 1);
    render(&mut h);
    clock.at(1_060);
    let pm = render(&mut h);
    let x0 = content.x;
    assert_eq!(
        span(&pm, content.y + 30, COLORS[1]).map(|s| s.0),
        Some(x0),
        "减弱档不滑动，只淡入"
    );
}

#[test]
fn rapid_tab_switching_settles_cleanly() {
    let clock = Clock::new(MotionLevel::Full);
    let sel = signal(0usize);
    let mut h = tabs_host(sel);
    render(&mut h);
    let (_, content) = tab_content(&h);
    for (t, i) in [(1_000, 1), (1_040, 2), (1_070, 0), (1_100, 2)] {
        clock.at(t);
        set(&mut h, sel, i);
        render(&mut h);
    }
    clock.at(3_000);
    let pm = render(&mut h);
    let y = content.y + 30;
    assert_eq!(
        span(&pm, y, COLORS[2]),
        Some((content.x, content.x + BLOCK_W - 1))
    );
    assert_eq!(
        (span(&pm, y, COLORS[0]), span(&pm, y, COLORS[1])),
        (None, None)
    );
    // 离场播完那帧会再要一帧重排把位置让出来（见 presence_tests），之后才静止。
    render(&mut h);
    assert!(!h.wants_animation(), "播完不再续帧");
}

fn switcher_host(key: Signal<usize>) -> UiHost {
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(Element::switcher(key, block)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn switcher_children(h: &UiHost) -> Vec<NodeId> {
    let col = h.tree.root.unwrap();
    let sw = h.tree.get(col).unwrap().children[0];
    h.tree.get(sw).unwrap().children.clone()
}

#[test]
fn switcher_crossfades_and_reaps_old_content() {
    let clock = Clock::new(MotionLevel::Full);
    let key = signal(0usize);
    let mut h = switcher_host(key);
    let pm = render(&mut h);
    assert_eq!(
        span(&pm, 30, COLORS[0]),
        Some((0, BLOCK_W - 1)),
        "前提：首帧直接在位"
    );
    let old = switcher_children(&h)[0];
    set(&mut h, key, 1);
    render(&mut h);
    clock.at(1_060);
    let pm = render(&mut h);
    let mid = pm.pixel(30, 30).unwrap();
    assert!(
        mid.red() > 20 && mid.blue() > 20,
        "半程新旧交叠：既有红又有蓝 {:?}",
        (mid.red(), mid.green(), mid.blue())
    );
    assert_eq!(switcher_children(&h).len(), 2, "旧内容播离场期间仍在");
    for t in [1_200, 1_400, 2_000] {
        clock.at(t);
        render(&mut h);
    }
    assert_eq!(switcher_children(&h).len(), 1, "旧内容播完即移除");
    assert!(h.tree.get(old).is_none());
    assert_eq!(span(&render(&mut h), 30, COLORS[1]), Some((0, BLOCK_W - 1)));
}

#[test]
fn switcher_same_key_does_not_rebuild() {
    let _clock = Clock::new(MotionLevel::Full);
    let key = signal(1usize);
    let mut h = switcher_host(key);
    render(&mut h);
    let before = switcher_children(&h);
    set(&mut h, key, 1);
    render(&mut h);
    assert_eq!(switcher_children(&h), before);
}

#[test]
fn switcher_reclaims_content_signals() {
    use crate::signal::stats;
    let clock = Clock::new(MotionLevel::Full);
    let key = signal(0usize);
    let app = App::new("t", W, H).content(Element::switcher(key, |k| {
        Element::label(signal(format!("第 {k} 页")))
    }));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    let mut t = 1_000;
    let mut cycle = |h: &mut UiHost, i: usize| {
        set(h, key, i % 3);
        render(h);
        t += 2_000;
        clock.at(t);
        render(h);
        render(h);
    };
    for i in 1..4 {
        cycle(&mut h, i);
    }
    let steady = stats().live;
    for i in 4..30 {
        cycle(&mut h, i);
    }
    assert_eq!(stats().live, steady, "反复切换后活跃信号数不应增长");
}

/// 外层容器自己带过渡（对话框、手风琴）开合时，当前页跟着外层的层淡入淡出，自己不滑。
#[test]
fn ancestor_transition_does_not_slide_the_current_page() {
    let clock = Clock::new(MotionLevel::Full);
    let sel = signal(0usize);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col().fill().bg(Color::WHITE).padding(20).child(
            Element::col()
                .fill()
                .visible_signal(show)
                .transition(crate::ui::Presence::fade())
                .child(Element::tabs(sel, vec![("一", block(0)), ("二", block(1))])),
        ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    let outer = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
    let tabs = h.tree.get(outer).unwrap().children[0];
    let content = h.tree.abs_bounds(h.tree.get(tabs).unwrap().children[1]);
    let (y, full) = (content.y + 30, Some((content.x, content.x + BLOCK_W - 1)));
    for (t, v) in [(1_000, false), (3_000, true)] {
        clock.at(t);
        set(&mut h, show, v);
        render(&mut h);
        clock.at(t + 60);
        let pm = render(&mut h);
        assert_eq!(
            span(&pm, y, COLORS[0]),
            full,
            "外层{}途中当前页不应横移",
            if v { "打开" } else { "关闭" }
        );
    }
}

/// 标签页整体隐藏期间换了页，露出来后再切：方向按真实的前后页算。
#[test]
fn switching_while_hidden_keeps_direction_right() {
    let clock = Clock::new(MotionLevel::Full);
    let sel = signal(0usize);
    let show = signal(true);
    let app = App::new("t", W, H).content(
        Element::col().fill().bg(Color::WHITE).padding(20).child(
            Element::col()
                .fill()
                .visible_signal(show)
                .child(Element::tabs(
                    sel,
                    vec![("一", block(0)), ("二", block(1)), ("三", block(2))],
                )),
        ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    set(&mut h, show, false);
    render(&mut h);
    set(&mut h, sel, 2);
    render(&mut h);
    set(&mut h, show, true);
    render(&mut h);
    let outer = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
    let tabs = h.tree.get(outer).unwrap().children[0];
    let content = h.tree.abs_bounds(h.tree.get(tabs).unwrap().children[1]);
    let y = content.y + 30;
    assert_eq!(
        span(&render(&mut h), y, COLORS[2]),
        Some((content.x, content.x + BLOCK_W - 1)),
        "前提：露出来时直接在第 3 页"
    );
    clock.at(2_000);
    set(&mut h, sel, 1);
    render(&mut h);
    clock.at(2_060);
    let pm = render(&mut h);
    let (_, nr) = span(&pm, y, COLORS[1]).expect("新页半程可见");
    assert!(
        nr < content.x + BLOCK_W - 1,
        "2 → 1 往回切：新页从左侧进来，右缘 {nr}"
    );
}

/// 放进自适应高度的父容器（可滚动的表单里）不塌成 0，内容自己设的尺寸照用。
#[test]
fn switcher_sizes_to_its_content() {
    let _clock = Clock::new(MotionLevel::Full);
    let key = signal(0usize);
    let app = App::new("t", W, H).content(
        Element::scroll().fill().child(
            Element::col()
                .width_match()
                .child(Element::switcher(key, |_| {
                    Element::leaf().size(80, 40).bg(COLORS[0])
                })),
        ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    let col = h.tree.get(h.tree.root.unwrap()).unwrap().children[0];
    let sw = h.tree.get(col).unwrap().children[0];
    let inner = h.tree.get(sw).unwrap().children[0];
    assert_eq!(h.tree.abs_bounds(sw).h, 40, "switcher 按内容取高");
    let r = h.tree.abs_bounds(inner);
    assert_eq!((r.w, r.h), (80, 40), "内容自己设的尺寸不被覆盖");
}
