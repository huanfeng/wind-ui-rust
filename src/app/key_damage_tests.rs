//! 按键路径上写信号的失效口径：**别的节点**读的信号被写了，读者必须重绘。
//!
//! 按键事件默认只重绘发起者自己（文本框打字高频），于是「Tab 到按钮按空格，回调改的
//! 状态文案不刷新」「查找框 Enter 后命中计数停在旧值」——读者不在脏区里。每条都与
//! 整窗重画逐像素比 label 区域，并把 label 与控件拉开距离，免得控件脏区外扩顺手盖到它。

use crate::app::test_support::key_ev;
use crate::app::{App, UiHost};
use crate::event::{Key, KeyEvent};
use crate::geometry::{Rect, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 400;
const H: i32 = 200;

fn ink(pm: &Pixmap, r: Rect) -> usize {
    (r.y..r.y + r.h)
        .flat_map(|y| (r.x..r.x + r.w).map(move |x| (x, y)))
        .filter(|&(x, y)| pm.pixel(x as u32, y as u32).unwrap().red() < 100)
        .count()
}

/// 顶上一个读 `msg` 的 label，下面隔开放 `control(msg)`；Tab 把焦点送进控件（不用点击：
/// 点击本身就整窗，测不到按键路径），再按 `keys`，断言 label 与整窗重画一致。
fn assert_reader_repaints(
    what: &str,
    control: impl FnOnce(Signal<String>) -> Element,
    keys: &[KeyEvent],
) {
    let msg = signal(String::new());
    let app = App::new("t", W, H).content(
        Element::col()
            .padding(10)
            .child(Element::label(msg).width(300).height(30))
            .child(Element::leaf().height(60))
            .child(control(msg)),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    let mut render =
        |h: &mut UiHost| h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    render(&mut h);
    let root = h.tree.root.unwrap();
    let label = h.tree.abs_bounds(h.tree.get(root).unwrap().children[0]);
    h.on_key(key_ev()(Key::Tab));
    render(&mut h);
    assert!(
        h.focus.current.is_some(),
        "{what}：前提：Tab 把焦点送进了控件"
    );
    for &k in keys {
        h.on_key(k);
        render(&mut h);
    }
    drop(render);
    assert_eq!(msg.get(), "WWWWWWWW", "{what}：前提：回调写了信号");
    let got = ink(&pm, label);
    h.damage.needs_full = true;
    let mut want = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut want }, Size::new(W, H));
    assert!(ink(&want, label) > 0, "{what}：前提：整窗重画时 label 有字");
    assert_eq!(got, ink(&want, label), "{what}：读者应与整窗重画一致");
}

fn write(msg: Signal<String>) {
    msg.set("WWWWWWWW".into());
}

#[test]
fn button_activated_by_space() {
    assert_reader_repaints(
        "按钮空格激活",
        |msg| {
            Element::button("确定")
                .width(80)
                .height(30)
                .on_click(move |_| write(msg))
        },
        &[key_ev()(Key::Space)],
    );
}

#[test]
fn checkbox_toggled_by_space() {
    assert_reader_repaints(
        "复选框空格切换",
        |msg| Element::checkbox("同意", signal(false)).on_toggle(move |_| write(msg)),
        &[key_ev()(Key::Space)],
    );
}

#[test]
fn text_input_on_submit() {
    assert_reader_repaints(
        "单行框 on_submit",
        |msg| {
            Element::text_input(signal(String::new()), "")
                .width(300)
                .height(30)
                .on_submit(move |_| write(msg))
        },
        &[key_ev()(Key::Enter)],
    );
}

#[test]
fn text_input_on_commit() {
    assert_reader_repaints(
        "输入框 on_commit（Enter 提交）",
        |msg| {
            Element::text_input(signal(String::new()), "")
                .width(300)
                .height(30)
                .on_commit(move |_| write(msg))
        },
        &[key_ev()(Key::Char('a')), key_ev()(Key::Enter)],
    );
}

#[test]
fn text_input_on_nav_key() {
    assert_reader_repaints(
        "单行框 on_nav_key",
        |msg| {
            Element::text_input(signal(String::new()), "")
                .width(300)
                .height(30)
                .on_nav_key(move |_, _| {
                    write(msg);
                    true
                })
        },
        &[key_ev()(Key::Down)],
    );
}
