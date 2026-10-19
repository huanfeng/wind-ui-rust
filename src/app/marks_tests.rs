//! `Element::marks`（输入框区间底色）的宿主级契约测试：经真实 build / 布局 / 绘制，
//! 按像素断言标记画在哪、改表后是否重绘。

use crate::app::test_support::key_ev;
use crate::app::{App, UiHost};
use crate::event::Key;
use crate::geometry::{Color, Rect, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use crate::ui::TextMark;
use tiny_skia::Pixmap;

const MARK: Color = Color::rgb(255, 0, 255);
const W: i32 = 400;
const H: i32 = 200;

fn is_mark(p: tiny_skia::PremultipliedColorU8) -> bool {
    p.red() > 200 && p.green() < 60 && p.blue() > 200
}

/// `r` 内含标记色的列，归并成连续段 `[x0, x1)`。
fn mark_runs(pm: &Pixmap, r: Rect) -> Vec<(i32, i32)> {
    let mut runs: Vec<(i32, i32)> = Vec::new();
    for x in r.x..r.x + r.w {
        let hit = (r.y..r.y + r.h).any(|y| is_mark(pm.pixel(x as u32, y as u32).unwrap()));
        if hit {
            match runs.last_mut() {
                Some(last) if last.1 == x => last.1 = x + 1,
                _ => runs.push((x, x + 1)),
            }
        }
    }
    runs
}

/// `r` 内含标记色的行数。相邻视觉行的标记首尾相接（铺满行盒、行间无缝），
/// 数不出「段」，只能比高度。
fn mark_rows(pm: &Pixmap, r: Rect) -> i32 {
    (r.y..r.y + r.h)
        .filter(|&y| (r.x..r.x + r.w).any(|x| is_mark(pm.pixel(x as u32, y as u32).unwrap())))
        .count() as i32
}

fn host(input: Element) -> UiHost {
    let app = App::new("t", W, H).content(Element::col().padding(10).child(input));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    h
}

fn frame(h: &mut UiHost) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    pm
}

fn marks(ranges: &[(usize, usize)]) -> Vec<TextMark> {
    ranges
        .iter()
        .map(|&(s, e)| TextMark::new(s..e).bg(MARK))
        .collect()
}

/// 单行框里各种区间画出的段。每次改表都整窗重画一帧：这里测的是**画在哪**，
/// 「改表后会不会重绘」另有用例。
fn runs_for(fixed_width: bool, password: bool, ranges: &[(usize, usize)]) -> Vec<(i32, i32)> {
    let list: Signal<Vec<TextMark>> = signal(marks(ranges));
    let input = Element::text_input(signal("abcdefgh".to_string()), "")
        .height(30)
        .marks(list);
    let input = if fixed_width { input.width(300) } else { input };
    let input = if password { input.password() } else { input };
    let mut h = host(input);
    let pm = frame(&mut h);
    let r = h.tree.abs_bounds(h.focus.order[0]);
    mark_runs(&pm, r)
}

/// 标记画在对应字下面：相邻区间首尾相接、合并区间等于两段并集；越界钳制、空区间
/// 与倒置区间不画、空表不画、密码框不画。期望值全由**另一组区间**画出来的段推出，
/// 不从实现反推字宽。Wrap 宽与显式宽各跑一遍（`avail.w != content.w` 那条路）。
#[test]
fn marks_paint_under_their_chars() {
    for fixed in [true, false] {
        let tag = if fixed { "显式宽" } else { "Wrap 宽" };
        assert!(runs_for(fixed, false, &[]).is_empty(), "{tag}：空表不画");

        let a = runs_for(fixed, false, &[(0, 2)]);
        let b = runs_for(fixed, false, &[(2, 4)]);
        let ab = runs_for(fixed, false, &[(0, 4)]);
        assert_eq!(
            (a.len(), b.len(), ab.len()),
            (1, 1, 1),
            "{tag}：各画成一段 {a:?} {b:?} {ab:?}"
        );
        let (a, b, ab) = (a[0], b[0], ab[0]);
        assert!(a.1 > a.0 && b.1 > b.0, "{tag}：段非空");
        assert!(
            (b.0 - a.1).abs() <= 1,
            "{tag}：[0,2) 与 [2,4) 首尾相接：{a:?} {b:?}"
        );
        assert!(
            (ab.0 - a.0).abs() <= 1 && (ab.1 - b.1).abs() <= 1,
            "{tag}：[0,4) 是两段的并集：{ab:?} vs {a:?}+{b:?}"
        );

        let tail = runs_for(fixed, false, &[(6, 8)]);
        assert_eq!(tail.len(), 1, "{tag}：前提：[6,8) 画得出来");
        assert_eq!(
            runs_for(fixed, false, &[(6, 999)]),
            tail,
            "{tag}：越界钳到正文长度"
        );
        assert!(
            runs_for(fixed, false, &[(9, 12)]).is_empty(),
            "{tag}：整段越界不画"
        );
        let reversed = runs_for(fixed, false, &[(5, 3)]);
        assert!(reversed.is_empty(), "{tag}：倒置区间不画");
        assert!(
            runs_for(fixed, true, &[(0, 4)]).is_empty(),
            "{tag}：密码框不画"
        );
    }
}

/// 多行折行框：跨视觉行的标记逐行铺。期望高度由「只标首字」那一行的高度推出。
/// 显式宽与 Wrap 宽各一遍：折行宽度在两者下来源不同（`avail.w` vs `content.w`）。
#[test]
fn marks_span_wrapped_lines() {
    let text = "aaaa bbbb cccc dddd ".repeat(8);
    for fixed in [true, false] {
        let tag = if fixed { "显式宽" } else { "Wrap 宽" };
        let rows_for = |ranges: &[(usize, usize)]| {
            let input = Element::text_input(signal(text.clone()), "")
                .multiline()
                .wrap(true)
                .height(150)
                .select_range(0, 0)
                .marks(signal(marks(ranges)));
            let input = if fixed { input.width(80) } else { input };
            let mut h = host(input);
            let pm = frame(&mut h);
            let r = h.tree.abs_bounds(h.focus.order[0]);
            mark_rows(&pm, r)
        };
        let one_line = rows_for(&[(0, 1)]);
        assert!(one_line > 0, "{tag}：前提：首字的标记画得出来");
        let all = rows_for(&[(0, text.chars().count())]);
        assert!(
            all >= 3 * one_line,
            "{tag}：整段标记应覆盖折出来的每一视觉行（至少三行）：{all} vs 单行 {one_line}"
        );
    }
}

/// 多行框滚动后只有视口里的标记出现：光标在文末（默认）→ 视口跟到末尾，首行的标记
/// 滚出去不画、末行的照画。守的是「只遍历可见行」那段区间算法的两端。
#[test]
fn marks_follow_scroll() {
    let lines = 12;
    let text = vec!["aaaa"; lines].join("\n");
    let last = text.chars().count();
    let runs = |range: (usize, usize)| {
        let input = Element::text_input(signal(text.clone()), "")
            .multiline()
            .width(200)
            .height(60)
            .marks(signal(marks(&[range])));
        let mut h = host(input);
        frame(&mut h);
        let pm = frame(&mut h);
        mark_runs(&pm, h.tree.abs_bounds(h.focus.order[0]))
    };
    assert!(runs((0, 4)).is_empty(), "首行已滚出视口，不画");
    assert_eq!(runs((last - 4, last)).len(), 1, "末行在视口里，照画");
}

/// 视口底部被切掉一半的那一行也得画：标记整段文字，最低的标记行应贴到内容区底边
/// （多行框四周留 `TEXT_PAD`）。高度取两个值，免得恰好是行高整数倍、没有半行可测。
#[test]
fn marks_reach_partially_visible_bottom_line() {
    let text = ["aaaa"; 12].join("\n");
    let n = text.chars().count();
    for height in [61, 67] {
        let input = Element::text_input(signal(text.clone()), "")
            .multiline()
            .width(200)
            .height(height)
            .select_range(0, 0)
            .marks(signal(marks(&[(0, n)])));
        let mut h = host(input);
        let pm = frame(&mut h);
        let r = h.tree.abs_bounds(h.focus.order[0]);
        let bottom = (r.y..r.y + r.h)
            .rev()
            .find(|&y| (r.x..r.x + r.w).any(|x| is_mark(pm.pixel(x as u32, y as u32).unwrap())))
            .expect("前提：标记画得出来");
        let inner_bottom = r.y + r.h - crate::ui::inputs::TEXT_PAD - 1;
        assert_eq!(
            bottom, inner_bottom,
            "高 {height}：标记应铺到内容区底边（半行也画）"
        );
    }
}

/// 改表要立刻反映到画面上——写表的往往是**别的**控件：查找框里按 Enter
/// （`on_submit`，按键路径默认只重绘发起者自己）、点「查找」按钮。
#[test]
fn writing_marks_from_another_control_repaints_the_box() {
    use crate::event::{MouseButton, PointerEvent, PointerKind};
    use crate::geometry::Point;
    let list: Signal<Vec<TextMark>> = signal(Vec::new());
    let query = signal(String::new());
    let app = App::new("t", W, H).content(
        Element::col()
            .padding(10)
            .child(
                Element::text_input(signal("abcdefgh".to_string()), "")
                    .width(300)
                    .height(30)
                    .marks(list),
            )
            // 拉开距离：查找框的局部脏区外扩（焦点环 / AA / 网格对齐）不得顺带盖到大框，
            // 否则大框底下几行被「顺手」重画，测试会假绿。
            .child(Element::leaf().height(60))
            .child(
                Element::text_input(query, "查找")
                    .width(300)
                    .height(30)
                    .on_submit(move |_| list.set(marks(&[(0, 2)]))),
            )
            .child(
                Element::button("清除")
                    .width(80)
                    .height(30)
                    .on_click(move |_| list.set(Vec::new())),
            ),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    let mut render =
        |h: &mut UiHost| h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    render(&mut h);
    let (big, find, clear) = (h.focus.order[0], h.focus.order[1], h.focus.order[2]);
    let center = |h: &UiHost, id| {
        let r = h.tree.abs_bounds(id);
        Point::new(r.x + r.w / 2, r.y + r.h / 2)
    };
    let click = |h: &mut UiHost, p: Point| {
        h.on_pointer(PointerEvent::single(
            PointerKind::Down,
            p,
            MouseButton::Left,
        ));
        h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
    };
    let big_r = h.tree.abs_bounds(big);
    let (pf, pc) = (center(&h, find), center(&h, clear));

    click(&mut h, pf);
    render(&mut h);
    h.on_key(key_ev()(Key::Char('a')));
    render(&mut h);
    h.on_key(key_ev()(Key::Enter));
    render(&mut h);
    drop(render);
    // 与整窗重画逐像素比：只数「有没有标记色」分不出「整行都画了」与「只重画了边角」。
    h.damage.needs_full = true;
    let mut want = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut want }, Size::new(W, H));
    assert_eq!(mark_runs(&want, big_r).len(), 1, "前提：整窗重画时标记在");
    let rows = |pm: &Pixmap| mark_rows(pm, big_r);
    assert_eq!(
        rows(&pm),
        rows(&want),
        "查找框 Enter 写表 → 大框标记整段画出（与整窗重画一致）"
    );

    let mut render =
        |h: &mut UiHost| h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    click(&mut h, pc);
    render(&mut h);
    drop(render);
    assert!(mark_runs(&pm, big_r).is_empty(), "按钮清表 → 大框标记消失");
}

/// 输入合成串时，标记仍落在**正文**的那几个字上：合成串插在光标处，光标之后的标记整体
/// 右移一个合成串宽；**起点恰在插入点**的标记从合成串之后开始，不把合成串盖进去。
/// 全用同一个字母，于是「带合成串标 [2,4)」与「不带合成串标 [4,6)」应画在同一处——
/// 期望值由另一组不带合成串的绘制给出，不从实现反推字宽。
#[test]
fn marks_shift_past_preedit() {
    let runs = |ranges: &[(usize, usize)], preedit: bool| {
        let input = Element::text_input(signal("aaaaaaaa".to_string()), "")
            .width(300)
            .height(30)
            .select_range(2, 2)
            .marks(signal(marks(ranges)));
        let mut h = host(input);
        frame(&mut h);
        let id = h.focus.order[0];
        if preedit {
            let pe = crate::event::Preedit {
                text: "aa".into(),
                caret: 2,
                ..Default::default()
            };
            assert!(h.tree.set_preedit(id, &pe), "前提：合成串送进了输入框");
            h.damage.needs_full = true;
        }
        let pm = frame(&mut h);
        mark_runs(&pm, h.tree.abs_bounds(id))
    };
    let want = runs(&[(4, 6)], false);
    assert_eq!(want.len(), 1, "前提：[4,6) 画得出来");
    assert_eq!(
        runs(&[(2, 4)], true),
        want,
        "起点在插入点：从合成串之后开始"
    );
    assert_eq!(
        runs(&[(0, 2)], true),
        runs(&[(0, 2)], false),
        "插入点之前的标记不受合成串影响"
    );
}
