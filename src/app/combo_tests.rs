//! `Element::combo_box` 经真实宿主路径（指针 / 按键 → 分发 → 出帧）的交互契约。
//!
//! 测试台里放两个组合框：A **显式设宽**，C **不设宽**（Wrap 宽，见 AGENTS §5「宽度不能
//! 只测一种」）——列表与锚点同宽、箭头区命中都依赖分到的宽度，两侧都要覆盖。

use crate::app::test_support::key_ev;
use crate::app::{App, UiHost};
use crate::core::NodeId;
use crate::event::{Key, MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Point, Rect, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use std::cell::RefCell;
use std::rc::Rc;
use tiny_skia::Pixmap;

const W: i32 = 320;
const H: i32 = 420;
const OPTS: [&str; 5] = ["9", "10", "12", "14", "18"];

struct Bench {
    h: UiHost,
    pm: Pixmap,
    a: Signal<String>,
    c: Signal<String>,
    commits: Rc<RefCell<Vec<String>>>,
}

impl Bench {
    fn new(a0: &str) -> Self {
        Self::with(a0, |el, _| el)
    }

    /// `tweak` 加工 A（挂 `numeric` / `options_signal` 等），第二个参数是 A 的正文信号。
    fn with(a0: &str, tweak: impl FnOnce(Element, Signal<String>) -> Element) -> Self {
        let a = signal(a0.to_string());
        let b = signal(String::new());
        let c = signal(String::new());
        let commits = Rc::new(RefCell::new(Vec::new()));
        let log = commits.clone();
        let combo_a = tweak(
            Element::combo_box(a, OPTS.to_vec())
                .width(200)
                .on_commit(move |_| log.borrow_mut().push(a.get())),
            a,
        );
        let app = App::new("t", W, H).content(
            Element::col()
                .padding(10)
                .spacing(8)
                .child(combo_a)
                .child(Element::text_input(b, "乙").width(200))
                // C 不设宽，且放在最底下：列表若装不下会往上翻，顺带覆盖翻边。
                .child(Element::leaf().height(250))
                .child(Element::combo_box(c, vec!["alpha", "Beta", "gamma"])),
        );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        let mut b = Self {
            h,
            pm: Pixmap::new(W as u32, H as u32).unwrap(),
            a,
            c,
            commits,
        };
        b.frame();
        b
    }

    fn frame(&mut self) {
        self.h.render(
            &mut PixmapTarget {
                pixmap: &mut self.pm,
            },
            Size::new(W, H),
        );
    }

    fn child(&self, i: usize) -> NodeId {
        let root = self.h.tree.root.unwrap();
        self.h.tree.get(root).unwrap().children[i]
    }
    fn rect(&self, id: NodeId) -> Rect {
        self.h.tree.abs_bounds(id)
    }
    /// 组合框挂着的浮层列表节点。
    fn list_of(&self, combo: NodeId) -> NodeId {
        self.h.tree.get(combo).unwrap().children[0]
    }
    fn open(&self, combo: NodeId) -> bool {
        self.h
            .tree
            .get(self.list_of(combo))
            .unwrap()
            .effective_visible()
    }
    fn combo_a(&self) -> NodeId {
        self.child(0)
    }
    fn input_b(&self) -> NodeId {
        self.child(1)
    }
    fn combo_c(&self) -> NodeId {
        self.child(3)
    }

    fn click(&mut self, p: Point) {
        self.h.on_pointer(PointerEvent::single(
            PointerKind::Down,
            p,
            MouseButton::Left,
        ));
        self.h
            .on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
        self.frame();
    }
    /// 点箭头区（右沿内 10px）。
    fn click_chevron(&mut self, combo: NodeId) {
        let r = self.rect(combo);
        self.click(Point::new(r.right() - 10, r.y + r.h / 2));
    }
    /// 点文字区。
    fn click_text(&mut self, combo: NodeId) {
        let r = self.rect(combo);
        self.click(Point::new(r.x + 20, r.y + r.h / 2));
    }
    /// 点列表第 `row` 个可见行的中线。
    fn click_row(&mut self, combo: NodeId, row: i32) {
        let l = self.rect(self.list_of(combo));
        self.click(Point::new(l.x + l.w / 2, l.y + 6 + row * 32 + 16));
    }
    fn pointer(&mut self, kind: PointerKind, p: Point) {
        self.h
            .on_pointer(PointerEvent::single(kind, p, MouseButton::Left));
    }
    fn key(&mut self, k: Key) {
        self.h.on_key(key_ev()(k));
        self.frame();
    }
    fn type_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.key(Key::Char(ch));
        }
    }
    fn commits(&self) -> Vec<String> {
        self.commits.borrow().clone()
    }
}

/// 点箭头展开全部选项，列表与锚点同宽、贴在下方；当前值是高亮起点（↓ 一次落到下一项）。
#[test]
fn chevron_opens_full_list_starting_at_current_value() {
    let mut b = Bench::new("12");
    let a = b.combo_a();
    b.click_chevron(a);
    assert!(b.open(a), "点箭头应展开");
    let (ar, lr) = (b.rect(a), b.rect(b.list_of(a)));
    assert_eq!(lr.w, ar.w, "列表与锚点同宽");
    assert!(lr.y >= ar.bottom(), "列表贴在锚点下方：{lr:?} vs {ar:?}");
    assert_eq!(lr.h, 5 * 32 + 12, "未过滤：五项全列出");

    b.key(Key::Down);
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "14", "高亮起点是当前值 12，↓ 一次到 14");
    assert!(!b.open(a), "Enter 选中后收起");
    assert_eq!(b.commits(), vec!["14".to_string()], "选中即提交一次");

    b.click_chevron(a);
    b.click_chevron(a);
    assert!(!b.open(a), "展开时再点箭头收起");
}

/// 收起时 ↓ 展开；当前值不在列表里时从无高亮开始，↓ 落到第一项。
#[test]
fn down_key_opens_list() {
    let mut b = Bench::new("自定义");
    let a = b.combo_a();
    b.click_text(a);
    assert!(!b.open(a), "点文字区只是聚焦，不展开");
    b.key(Key::Down);
    assert!(b.open(a), "↓ 展开");
    b.key(Key::Down);
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "9");
}

/// 打字按前缀过滤并自动展开；不预选，Enter 提交的是打的字。
#[test]
fn typing_filters_and_enter_keeps_typed_text() {
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_text(a);
    b.type_str("1");
    assert!(b.open(a), "有匹配：自动展开");
    assert_eq!(b.rect(b.list_of(a)).h, 4 * 32 + 12, "按前缀 1 过滤剩 4 项");
    b.type_str("3");
    assert!(!b.open(a), "13 没有匹配：收起");
    b.key(Key::Backspace);
    assert!(b.open(a), "退回 1 又有匹配");
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "1", "未挑选时 Enter 不拿匹配项顶替打的字");
    assert!(!b.open(a));
    assert_eq!(
        b.commits(),
        vec!["1".to_string()],
        "Enter 照 text_input 口径提交"
    );
}

/// 过滤后 ↓ 挑的是**过滤后**的第一项。
#[test]
fn down_after_typing_picks_from_filtered_list() {
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_text(a);
    b.type_str("1");
    b.key(Key::Down);
    b.key(Key::Down);
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "12", "过滤后为 10/12/14/18，第二项是 12");
}

/// 前缀匹配不区分大小写（不设宽的 C 上测，顺带覆盖 Wrap 宽）。
#[test]
fn prefix_filter_ignores_case_on_wrap_width_combo() {
    let mut b = Bench::new("");
    let c = b.combo_c();
    b.click_text(c);
    b.type_str("b");
    assert!(b.open(c));
    let (cr, lr) = (b.rect(c), b.rect(b.list_of(c)));
    assert_eq!(lr.h, 32 + 12, "b 只匹配 Beta");
    assert_eq!(lr.w, cr.w, "不设宽时列表同样与锚点同宽");
    assert!(lr.bottom() <= cr.y, "底部装不下：翻到锚点上方");
    b.click_row(c, 0);
    assert_eq!(b.c.get(), "Beta");
}

/// Esc 收起列表且不改正文；列表收起后再按 Esc 不再被组合框吃掉（照常上达宿主的关窗兜底）。
#[test]
fn escape_closes_list() {
    let mut b = Bench::new("10");
    let a = b.combo_a();
    b.click_chevron(a);
    b.key(Key::Down);
    b.key(Key::Escape);
    assert!(!b.open(a));
    assert_eq!(b.a.get(), "10", "Esc 不采纳高亮项");
    assert!(b.commits().is_empty());
    assert!(!b.h.close, "第一下 Esc 归列表，不该走到关窗");
    b.key(Key::Escape);
    assert!(b.h.close, "列表收起后 Esc 与 text_input 一样上达宿主");
}

/// 收起后列表那块要真被擦掉（浮层不在锚点的脏区里，靠整窗帧兜）。
#[test]
fn closing_erases_list_pixels() {
    // 浮层有淡入：钉住时钟，展开后推过过渡再取像素。
    crate::anim::pin_clock(Some(1_000));
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_chevron(a);
    crate::anim::pin_clock(Some(5_000));
    b.frame();
    let lr = b.rect(b.list_of(a));
    let (cx, cy) = ((lr.x + lr.w / 2) as u32, (lr.y + lr.h / 2) as u32);
    let bg = b.pm.pixel(W as u32 - 4, cy).unwrap();
    assert_ne!(b.pm.pixel(cx, cy).unwrap(), bg, "前提：展开时列表盖在那里");
    b.key(Key::Escape);
    crate::anim::pin_clock(Some(9_000));
    b.frame();
    let after = b.pm.pixel(cx, cy).unwrap();
    crate::anim::pin_clock(None);
    assert_eq!(after, bg, "收起后列表区应恢复成窗口底色");
}

/// 回归：在文字区按下拖选、松手落在箭头区——这次 Up 必须交还文本框，否则拖选的
/// 指针捕获不释放，之后整个窗口都点不动。
#[test]
fn drag_from_text_released_on_chevron_frees_capture() {
    let mut b = Bench::new("123456");
    let a = b.combo_a();
    let r = b.rect(a);
    let (from, to) = (
        Point::new(r.x + 14, r.y + r.h / 2),
        Point::new(r.right() - 10, r.y + r.h / 2),
    );
    b.pointer(PointerKind::Down, from);
    b.pointer(PointerKind::Move, to);
    b.pointer(PointerKind::Up, to);
    b.frame();
    assert!(!b.open(a), "拖选结束在箭头上不算点箭头");
    let br = b.rect(b.input_b());
    b.click(Point::new(br.x + 20, br.y + br.h / 2));
    assert_eq!(
        b.h.focus.current,
        Some(b.input_b()),
        "捕获已释放：点别处焦点照常转走"
    );
}

/// 回归：`on_commit` 里改写正文（钳制 / 规整）后，Enter 不该再按新正文把列表打开。
#[test]
fn rewriting_text_in_on_commit_does_not_reopen_list() {
    let mut b = Bench::with("", |el, a| el.on_commit(move |_| a.set("12".into())));
    let a = b.combo_a();
    b.click_text(a);
    b.type_str("99");
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "12");
    assert!(!b.open(a), "Enter 里应用改写正文不是打字");
}

/// 回归：展开着时选项被换成更短的表，旧的滚动位置要对齐——不重新展开，行照样点得中。
#[test]
fn options_shrinking_while_open_keeps_rows_clickable() {
    let many: Vec<String> = (0..20).map(|i| format!("项{i}")).collect();
    let opts = signal(many);
    let mut b = Bench::with("", |el, _| el.options_signal(opts));
    let a = b.combo_a();
    b.click_chevron(a);
    b.key(Key::Up); // 无高亮时 ↑ 落到末项，滚到底（首行 项12）
    opts.set(vec!["x".into(), "y".into(), "z".into()]);
    b.frame();
    b.click_row(a, 1);
    assert_eq!(b.a.get(), "y", "缩短后的表照样点得中");
}

/// 回归：展开着时选项被换成同样长度、顺序不同的表，旧高亮下标指着别的项——
/// Enter 不该静默选中它。
#[test]
fn options_reordered_while_open_drop_stale_highlight() {
    let opts = signal(vec!["a".to_string(), "b".into(), "c".into()]);
    let mut b = Bench::with("", |el, _| el.options_signal(opts));
    let a = b.combo_a();
    b.click_chevron(a);
    b.key(Key::Down); // 高亮 a
    opts.set(vec!["c".to_string(), "b".into(), "a".into()]);
    b.frame();
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "", "选项换过之后旧高亮作废，Enter 不取任何项");
}

/// 超过 8 项时滚动：键盘高亮跟着滚，滚轮按行滚。
#[test]
fn long_list_scrolls_with_keys_and_wheel() {
    let many: Vec<String> = (0..20).map(|i| format!("项{i}")).collect();
    let opts = signal(many);
    let mut b = Bench::with("", |el, _| el.options_signal(opts));
    let a = b.combo_a();
    b.click_chevron(a);
    assert_eq!(b.rect(b.list_of(a)).h, 8 * 32 + 12, "最多同时显示 8 行");
    b.key(Key::Up);
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "项19", "↑ 回绕到末项");

    // 再展开时高亮落在当前值 项19 上，已滚到底（首行 项12）；往上滚 3 行。
    b.click_chevron(a);
    let l = b.rect(b.list_of(a));
    let at = Point::new(l.x + l.w / 2, l.y + l.h / 2);
    for _ in 0..3 {
        b.h.on_pointer(PointerEvent::single(
            PointerKind::Wheel(120),
            at,
            MouseButton::Left,
        ));
    }
    b.frame();
    b.click_row(a, 0);
    assert_eq!(b.a.get(), "项9", "从 项12 起上滚 3 行，首行是 项9");
}

/// 指针碰巧停在列表上不算挑选：打字后 Enter 仍提交打的字。
#[test]
fn pointer_hover_is_not_adopted_by_enter() {
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_text(a);
    b.type_str("1");
    let l = b.rect(b.list_of(a));
    b.pointer(PointerKind::Move, Point::new(l.x + 40, l.y + 6 + 16));
    b.frame();
    b.key(Key::Enter);
    assert_eq!(b.a.get(), "1");
}

/// 删空正文收起列表，而不是弹出整张表。
#[test]
fn deleting_to_empty_closes_list() {
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_text(a);
    b.type_str("1");
    assert!(b.open(a));
    b.key(Key::Backspace);
    assert!(!b.open(a));
}

/// 只读：箭头与 ↓ 都展不开，正文不变。
#[test]
fn read_only_never_opens() {
    let mut b = Bench::with("10", |el, _| el.read_only());
    let a = b.combo_a();
    b.click_chevron(a);
    assert!(!b.open(a));
    b.key(Key::Down);
    assert!(!b.open(a));
    assert_eq!(b.a.get(), "10");
}

/// 指针点中列表行：写入、收起、提交，焦点留在组合框里。
#[test]
fn clicking_a_row_picks_it_and_keeps_focus() {
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_chevron(a);
    b.click_row(a, 3);
    assert_eq!(b.a.get(), "14");
    assert!(!b.open(a));
    assert_eq!(b.commits(), vec!["14".to_string()]);
    assert_eq!(b.h.focus.current, Some(a), "点列表行不该把焦点弄丢");
    // 选中后光标在末尾、无选区：接着打字是续写。
    b.type_str("2");
    assert_eq!(b.a.get(), "142");
}

/// `.select_all_on_pick()`：选中后全选，接着打字整体替换。
#[test]
fn select_all_on_pick_replaces_on_typing() {
    let mut b = Bench::with("", |el, _| el.select_all_on_pick());
    let a = b.combo_a();
    b.click_chevron(a);
    b.click_row(a, 3);
    assert_eq!(b.a.get(), "14");
    b.type_str("2");
    assert_eq!(b.a.get(), "2");
}

/// 失焦（点别处）收起列表；Tab 离开同样收起。
#[test]
fn losing_focus_closes_list() {
    let mut b = Bench::new("");
    let a = b.combo_a();
    b.click_chevron(a);
    assert!(b.open(a));
    let br = b.rect(b.input_b());
    b.click(Point::new(br.x + 20, br.y + br.h / 2));
    assert!(!b.open(a), "点别处收起");

    b.click_chevron(a);
    b.key(Key::Tab);
    assert!(!b.open(a), "Tab 离开收起");
    assert_eq!(b.h.focus.current, Some(b.input_b()));
}

/// `.numeric()`：字母打不进去，数字 / 小数点 / 负号照常。
#[test]
fn numeric_rejects_non_digits() {
    let mut b = Bench::with("", |el, _| el.numeric());
    let a = b.combo_a();
    b.click_text(a);
    b.type_str("a-1x.5.");
    assert_eq!(b.a.get(), "-1.5");
}

/// `.options_signal`：选项换了，展开的就是新表。
#[test]
fn options_signal_replaces_static_options() {
    let opts = signal(vec!["宋体".to_string(), "黑体".to_string()]);
    let mut b = Bench::with("", |el, _| el.options_signal(opts));
    let a = b.combo_a();
    b.click_chevron(a);
    assert_eq!(b.rect(b.list_of(a)).h, 2 * 32 + 12);
    b.click_row(a, 1);
    assert_eq!(b.a.get(), "黑体");

    opts.set(vec!["楷体".to_string()]);
    b.frame();
    b.click_chevron(a);
    assert_eq!(b.rect(b.list_of(a)).h, 32 + 12, "改信号后展开的是新表");
}

/// 选项为空时箭头 / ↓ 都不展开出一块空面板。
#[test]
fn empty_options_never_open() {
    let opts = signal(Vec::<String>::new());
    let mut b = Bench::with("", |el, _| el.options_signal(opts));
    let a = b.combo_a();
    b.click_chevron(a);
    assert!(!b.open(a));
    b.key(Key::Down);
    assert!(!b.open(a));
}
