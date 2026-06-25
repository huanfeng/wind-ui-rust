//! Copy 句柄细粒度状态（Leptos 0.5+ 风格运行时 arena）。
//!
//! 句柄 [`Signal<T>`] 是 `Copy` 的小整数索引，指向线程局部运行时里的真实存储——`move`
//! 闭包直接捕获、无需 `.clone()`，消灭"Rc clone 病"。写值经 [`Signal::set`]/[`Signal::update`]
//! 自动触发重绘（接入失效通道，见 `notify_changed`），无需手写 `ctx.mark_dirty()`。
//!
//! 设计与分期见 `.omc/plans/signal-state-binding.md`。Phase 1：原语 + 自动 dirty。
//! 释放作用域（动态列表回收 slot）留作后续；当前单一全局运行时随线程存活（静态树可接受）。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;

/// 运行时 slot 键：索引 + 代际（复用 core arena 的失效心智，回收后旧句柄自然失效）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SlotKey {
    index: u32,
    generation: u32,
}

struct Slot {
    generation: u32,
    value: Option<Box<dyn Any>>,
    /// 每次写自增；供 `memo` 依赖比对（Phase 1 暂存，memo 在后续增量接入）。
    version: u64,
}

/// 信号运行时：generational arena。
struct Runtime {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl Runtime {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    fn insert(&mut self, value: Box<dyn Any>) -> SlotKey {
        if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            slot.value = Some(value);
            slot.version = 0;
            SlotKey {
                index: idx,
                generation: slot.generation,
            }
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot {
                generation: 0,
                value: Some(value),
                version: 0,
            });
            SlotKey {
                index: idx,
                generation: 0,
            }
        }
    }

    fn slot(&self, key: SlotKey) -> Option<&Slot> {
        self.slots
            .get(key.index as usize)
            .filter(|s| s.generation == key.generation && s.value.is_some())
    }

    fn slot_mut(&mut self, key: SlotKey) -> Option<&mut Slot> {
        self.slots
            .get_mut(key.index as usize)
            .filter(|s| s.generation == key.generation && s.value.is_some())
    }
}

thread_local! {
    static RT: RefCell<Runtime> = const { RefCell::new(Runtime::new()) };
    /// 是否处于节点事件处理期（核心在 call_on_event 前后括起）。
    static EVENT_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// 本次事件处理期内是否写过信号（供核心据当前事件节点局部失效）。
    static TOUCHED: Cell<bool> = const { Cell::new(false) };
}

/// 写信号后触发重绘的钩子。
/// - 事件期内：仅记"写过信号"，由核心在 `end_event` 据当前事件节点产生**局部**脏区
///   （结构签名层会在显隐/布局变化时升级整窗），不强制整窗。
/// - 事件期外（后台 pump / 定时器 / 直接调用）：经 anim 通道请求重绘（整窗兜底）。
fn notify_changed() {
    if EVENT_ACTIVE.with(|c| c.get()) {
        TOUCHED.with(|c| c.set(true));
    } else {
        crate::anim::request_repaint();
    }
}

/// 核心：进入某节点事件处理前调用——标记事件期开始、清"写过信号"标志。
pub(crate) fn begin_event() {
    EVENT_ACTIVE.with(|c| c.set(true));
    TOUCHED.with(|c| c.set(false));
}

/// 核心：退出节点事件处理后调用——结束事件期，返回这期间是否写过信号。
pub(crate) fn end_event() -> bool {
    EVENT_ACTIVE.with(|c| c.set(false));
    TOUCHED.with(|c| c.replace(false))
}

/// `Copy` 状态句柄。指向运行时存储，可自由按值传入控件/闭包，无需 clone。
pub struct Signal<T> {
    key: SlotKey,
    _t: PhantomData<fn() -> T>,
}

impl<T> Clone for Signal<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Signal<T> {}

/// 新建一个信号，返回其 `Copy` 句柄。
pub fn signal<T: 'static>(value: T) -> Signal<T> {
    let key = RT.with(|rt| rt.borrow_mut().insert(Box::new(value)));
    Signal {
        key,
        _t: PhantomData,
    }
}

impl<T: 'static> Signal<T> {
    /// 借用读取（免 clone）。句柄已失效（slot 回收）时 panic——句柄不应超出其运行时存活期。
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        RT.with(|rt| {
            let rt = rt.borrow();
            let slot = rt.slot(self.key).expect("signal 句柄已失效");
            let v = slot
                .value
                .as_ref()
                .unwrap()
                .downcast_ref::<T>()
                .expect("signal 类型不匹配");
            f(v)
        })
    }

    /// 写入新值并触发重绘。
    pub fn set(&self, value: T) {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if let Some(slot) = rt.slot_mut(self.key) {
                slot.value = Some(Box::new(value));
                slot.version = slot.version.wrapping_add(1);
            }
        });
        notify_changed();
    }

    /// 原地修改并触发重绘（避免 get→改→set 的一次 clone）。
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if let Some(slot) = rt.slot_mut(self.key) {
                if let Some(v) = slot.value.as_mut().and_then(|b| b.downcast_mut::<T>()) {
                    f(v);
                    slot.version = slot.version.wrapping_add(1);
                }
            }
        });
        notify_changed();
    }
}

impl<T: Clone + 'static> Signal<T> {
    /// 读取当前值（克隆）。需要追踪依赖的派生场景用 `with` 更省。
    pub fn get(&self) -> T {
        self.with(|v| v.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_set_roundtrip() {
        let s = signal(3i32);
        assert_eq!(s.get(), 3);
        s.set(7);
        assert_eq!(s.get(), 7);
    }

    #[test]
    fn update_in_place() {
        let s = signal(10i32);
        s.update(|v| *v += 5);
        assert_eq!(s.get(), 15);
    }

    #[test]
    fn copy_handle_into_closures() {
        // Copy：无需 clone 即可多处捕获同一信号。
        let s = signal(0i32);
        let inc = move || s.update(|v| *v += 1);
        let read = move || s.get();
        inc();
        inc();
        assert_eq!(read(), 2);
        assert_eq!(s.get(), 2, "原句柄与闭包内句柄指向同一存储");
    }

    #[test]
    fn distinct_signals_are_independent() {
        let a = signal(1i32);
        let b = signal(100i32);
        a.set(2);
        assert_eq!(a.get(), 2);
        assert_eq!(b.get(), 100);
    }

    #[test]
    fn with_borrows_without_clone() {
        let s = signal(String::from("hello"));
        let len = s.with(|v| v.len());
        assert_eq!(len, 5);
        s.update(|v| v.push_str(" world"));
        assert_eq!(s.with(String::len), 11);
    }

    #[test]
    fn set_in_event_marks_touched() {
        let s = signal(0i32);
        begin_event();
        s.set(1);
        assert!(end_event(), "事件期内写信号应标记 touched");
    }

    #[test]
    fn set_outside_event_not_touched() {
        let _ = end_event(); // 幂等保证非事件期（防同线程上个测试残留）
        let s = signal(0i32);
        s.set(9);
        begin_event();
        assert!(!end_event(), "事件期外的写不应记入下一次事件 touched");
    }

    #[test]
    fn non_clone_type_supported_via_with() {
        // 不要求 T: Clone，仅用 with/update。
        struct NoClone(i32);
        let s = signal(NoClone(42));
        assert_eq!(s.with(|v| v.0), 42);
        s.update(|v| v.0 = 7);
        assert_eq!(s.with(|v| v.0), 7);
    }
}
