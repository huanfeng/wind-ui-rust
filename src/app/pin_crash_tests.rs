//! 图钉崩溃的最小时序复现（真机 Windows 0xc0000409 双重 panic abort）。
//!
//! 真机链路：外观页 Watcher 的 renderPreview RPC 在飞 → 点击图钉 → 代次信号 bump →
//! 重建帧 switcher（KeyedList）换 key，旧页转残影、离场播完 reap（tree.remove +
//! 行作用域信号 scope.dispose 同刻）→ RPC 返回，迟到完成回调（持有行内信号句柄）写死句柄。
//!
//! 这里用「回调队列」等价 NetTasks 的迟到完成回调：构建期在行作用域内注册「完成回调」，
//! 页回收后由测试派发。裸 `set` 死句柄在 debug 构建下断言失败 panic；真机上该回调跑在
//! `begin_frame`（Win32 绘制消息处理栈内），unwind 跨 FFI 边界 → 双重 panic → abort。
//! 修复形态见 `late_callback_guarded_by_try_set_survives_rebuild`：迟到收尾用
//! `try_set`/`try_update` 探活，失效即放弃整条收尾。

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use crate::anim::{self, MotionLevel};
use crate::app::{App, UiHost};
use crate::geometry::{Color, Size};
use crate::platform::AppHandler;
use crate::render::PixmapTarget;
use crate::signal::{signal, Signal};
use crate::ui::Element;
use tiny_skia::Pixmap;

const W: i32 = 400;
const H: i32 = 240;
const BLOCK_W: i32 = 120;
const RED: Color = Color::rgb(255, 0, 0);

/// 迟到回调队列：模拟 NetTasks 的迟到完成回调——构建期在行作用域内注册、页回收后由测试派发。
type LateQueue = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

fn render(h: &mut UiHost) -> Pixmap {
    let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
    h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(W, H));
    pm
}

fn set<T: 'static>(h: &mut UiHost, sig: Signal<T>, v: T) {
    sig.set(v);
    h.damage.needs_relayout = true;
}

/// MotionLevel 是 thread_local 全局档位，断言失败会跳过尾部的手动恢复——按
/// presence_tests 的 Clock 惯例用 RAII 守卫，测试退出（含 panic unwind）必恢复。
struct MotionOff;
impl MotionOff {
    fn on() -> Self {
        anim::set_motion_level(MotionLevel::Off);
        Self
    }
}
impl Drop for MotionOff {
    fn drop(&mut self) {
        anim::set_motion_level(MotionLevel::Full);
    }
}

/// 裸 set 死句柄应 panic：复现崩溃时序（重建 → 回收 → 迟到派发），并钉住
/// 「裸写死句柄必断言」的既有语义——修复不落地在这个断言上。
#[test]
fn switcher_rebuild_then_late_raw_set_panics() {
    let _motion = MotionOff::on();
    let gen = signal(0u64);
    let late: LateQueue = Rc::new(RefCell::new(Vec::new()));
    // 行作用域信号句柄带出来供断言：回收后 probe0 死、probe1 活。
    let probes: Rc<RefCell<Vec<Signal<u32>>>> = Rc::new(RefCell::new(Vec::new()));
    let (q, ps) = (late.clone(), probes.clone());
    let app = App::new("t", W, H).content(Element::col().fill().bg(Color::WHITE).child(
        Element::switcher(gen, move |_| {
            let probe = signal(0u32); // 行作用域信号（真机上即 shown/error/retry_tick）
            ps.borrow_mut().push(probe);
            q.borrow_mut().push(Box::new(move || probe.set(1)));
            Element::col()
                .fill()
                .child(Element::leaf().size(BLOCK_W, 60).bg(RED))
        }),
    ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h); // 初帧：gen0 页在位
    assert_eq!(late.borrow().len(), 1, "前提：gen0 构建期注册了完成回调");

    // 「点击图钉」的等价物：代次 bump → 重建帧。动画关闭 → 离场瞬时 → 当场回收。
    set(&mut h, gen, 1);
    render(&mut h);
    {
        let ps = probes.borrow();
        assert!(
            !ps[0].is_alive(),
            "前提：换 key 帧已回收 gen0 的行作用域信号"
        );
        assert!(ps[1].is_alive(), "新页构建期信号应活着");
    }

    // 迟到派发：裸 set 死句柄 → debug 断言 panic（真机 abort 的等价物）。
    let cb = late.borrow_mut().remove(0);
    let err = catch_unwind(AssertUnwindSafe(cb));
    assert!(err.is_err(), "裸 set 死句柄应 panic（真机 abort 的等价物）");
    // 断言消息带格式化参数（类型名、写出点）时 payload 是 `String`，纯字面量时是
    // `&'static str`——两种都认，别让消息措辞的改动把这条复现测试静默打成假红。
    let payload = err.unwrap_err();
    let msg = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    assert!(
        msg.contains("句柄已失效"),
        "panic 消息应指明句柄已失效，实得 {msg:?}"
    );
}

/// 修复形态：迟到收尾先用 `try_set` 探活，失效即提前 return（放弃整条收尾）。
/// 与 wind-setting 侧修法一致；回收后新页信号不受影响。
#[test]
fn late_callback_guarded_by_try_set_survives_rebuild() {
    let _motion = MotionOff::on();
    let gen = signal(0u64);
    let late: LateQueue = Rc::new(RefCell::new(Vec::new()));
    let probes: Rc<RefCell<Vec<Signal<u32>>>> = Rc::new(RefCell::new(Vec::new()));
    // 迟到收尾里 try_set 的返回值：死句柄必须报 false，调用方靠它放弃整条收尾。
    let wrote: Rc<RefCell<Option<bool>>> = Rc::new(RefCell::new(None));
    let (q, ps, w) = (late.clone(), probes.clone(), wrote.clone());
    let app = App::new("t", W, H).content(Element::col().fill().bg(Color::WHITE).child(
        Element::switcher(gen, move |_| {
            let probe = signal(0u32);
            ps.borrow_mut().push(probe);
            let w = w.clone();
            q.borrow_mut().push(Box::new(move || {
                *w.borrow_mut() = Some(probe.try_set(1));
            }));
            Element::col()
                .fill()
                .child(Element::leaf().size(BLOCK_W, 60).bg(RED))
        }),
    ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    set(&mut h, gen, 1);
    render(&mut h);
    {
        let ps = probes.borrow();
        assert!(!ps[0].is_alive());
        assert!(ps[1].is_alive());
    }
    let cb = late.borrow_mut().remove(0);
    let err = catch_unwind(AssertUnwindSafe(cb));
    assert!(err.is_ok(), "try_set 门禁的迟到收尾不应 panic");
    assert_eq!(*wrote.borrow(), Some(false), "死句柄上 try_set 应报 false");
}

/// 对话框 show 被 detach 永不回收：登记表须跟遮罩节点走，整代重建后不得无界增长。
#[test]
fn modal_registry_does_not_grow_across_switcher_rebuilds() {
    let _motion = MotionOff::on();
    let gen = signal(0u64);
    let app = App::new("t", W, H).content(Element::col().fill().bg(Color::WHITE).child(
        Element::switcher(gen, move |_| {
            let show = signal(false);
            Element::dialog(show, Element::leaf().size(BLOCK_W, 60).bg(RED))
        }),
    ));
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    assert!(h.tree.modal_count() >= 1, "前提：对话框遮罩已登记");
    // 新代先于旧代删除登记，稳态会多一条在途；以第 2 代为基线，之后不得再涨。
    set(&mut h, gen, 1);
    render(&mut h);
    let base = h.tree.modal_count();
    for g in 2..=9u64 {
        set(&mut h, gen, g);
        render(&mut h);
    }
    assert_eq!(h.tree.modal_count(), base, "登记表随重建增长");
}

/// 两个并列对话框同时打开，下面那个所在子树重建后，ESC 仍先关上面那个：
/// 登记表的顺序即关闭顺序，重建只能原位换节点，不能把它挪到末尾。
#[test]
fn rebuilt_dialog_keeps_its_esc_order() {
    let _motion = MotionOff::on();
    let gen = signal(0u64);
    let (lower, upper) = (signal(true), signal(true));
    let app = App::new("t", W, H).content(
        Element::col()
            .fill()
            .bg(Color::WHITE)
            .child(Element::switcher(gen, move |_| {
                Element::dialog(lower, Element::leaf().size(BLOCK_W, 60).bg(RED))
            }))
            .child(Element::dialog(
                upper,
                Element::leaf().size(BLOCK_W, 60).bg(RED),
            )),
    );
    let mut h = app.into_handler_for_test();
    h.set_scale(1.0);
    render(&mut h);
    set(&mut h, gen, 1);
    render(&mut h);
    render(&mut h);
    assert!(h.tree.close_topmost_modal(), "应关掉一个对话框");
    assert!(!upper.get(), "先关的应是上面那个");
    assert!(lower.get(), "下面那个（重建过）应还开着");
}
