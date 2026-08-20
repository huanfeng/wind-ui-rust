//! Copy 句柄细粒度状态（Leptos 0.5+ 风格运行时 arena）。
//!
//! 句柄 [`Signal<T>`] 是 `Copy` 的小整数索引，指向线程局部运行时里的真实存储——`move`
//! 闭包直接捕获、无需 `.clone()`，消灭"Rc clone 病"。写值经 [`Signal::set`]/[`Signal::update`]
//! 自动触发重绘（接入失效通道，见 `notify_changed`），无需手写 `ctx.mark_dirty()`。
//!
//! 存储是线程局部的，所以句柄是 `!Send` + `!Sync`——只能在创建它的线程（UI 线程）
//! 上使用，跨线程更新状态请走 `App::channel`，详见 [`Signal`] 的线程约束一节。
//!
//! # 谁拥有一个信号
//!
//! 绝大多数信号是**应用状态**：在 `main` 里建好、按值散进各处闭包、活到进程退出。它们没有
//! owner 也不需要 owner——运行时随线程存活就是正确答案，回收它们没有意义。
//!
//! 剩下一小撮是**构建期临时信号**：在 `Element` 构建过程中被创建（控件内部需要一处可写状态，
//! 或调用方在行构建闭包里现造一个）。这一撮的问题在于**元素构建会重复发生**——本库有三处
//! 会按数据变化重建子树的宿主（`Element::list_signal` 的 `DynList`、`reorder_list_signal`
//! 的行源、以及可排序表格的行/表头），每重建一轮就再造一批。没有回收，这批槽位随线程存活。
//!
//! 所以本模块的所有权模型是**两级**的，而不是 leptos / floem 那样的单一作用域树：
//!
//! - **默认无主**：`signal()` 在任何作用域之外调用时不归属任何人，永不回收。应用状态走这条。
//! - **显式归属**：在 [`SignalScope::collect`] 内调用的 `signal()` 归该作用域所有，
//!   作用域 [`dispose`](SignalScope::dispose) 或析构时批量回收其槽位。
//!
//! 之所以不引入贯穿全库的隐式作用域树：本库是**保留模式的控件树**，不是响应式图，没有一棵
//! 现成的所有权树可挂；而隐式回收会让"谁杀了我的信号"变得不可追溯——菜单动作闭包、toast
//! 回调、`App::channel` 的消息处理器都能合法地比控件节点活得久，隐式作用域一旦圈错边界就是
//! 运行期悬垂。显式 `collect` 把边界写在代码里，读代码就能看见。
//!
//! 三处重建宿主已各自持有一个 `SignalScope`，重建时先回收上一轮、再在新作用域里造新行——
//! 调用方在 `row_fn` 里 `signal(..)` 是安全的，不会累积。见 [`SignalScope`] 的示例。
//!
//! # 悬垂句柄
//!
//! `Signal<T>` 是 `Copy` 句柄，复制出去的副本互不知情，所以**不能**靠 `Drop` 做引用计数式
//! 回收——这也正是 `Copy` 的代价。回收后槽位 `generation` 自增，所有旧句柄一并失效：
//! [`Signal::with`]/[`get`](Signal::get) panic，[`set`](Signal::set)/[`update`](Signal::update)
//! 静默丢弃，[`try_with`](Signal::try_with)/[`try_get`](Signal::try_get) 返回 `None`。
//! 读写为何不对称，见 [`Signal::set`] 的文档。
//!
//! # 观测
//!
//! [`stats`] 随时可查活跃/空闲槽位数；环境变量 `WINDUI_SIGNALS` 打开后，活跃槽位每创下新高
//! 就打一行——泄漏表现为这行**持续增长**。详见 [`stats`]。
//!
//! 设计与分期见 `.omc/plans/signal-state-binding.md`。

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;

/// 运行时 slot 键：索引 + 代际（复用 core arena 的失效心智，回收后旧句柄自然失效）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SlotKey {
    index: u32,
    generation: u32,
}

/// 派生映射：源值（`&dyn Any`，实为源类型）→ 派生值（`Box<dyn Any>`）。
///
/// `Rc` 是为了链式 `map` 能**组合**而不是层层嵌套（见 [`Signal::map`]）；类型擦除是因为
/// 槽位表不带类型参数，源类型与派生类型只在 `map` 的那一刻可见。
type ComputeFn = Rc<dyn Fn(&dyn Any) -> Box<dyn Any>>;

/// 派生信号的来源：读时由 `compute(源值)` 现算，本槽位**不存值**。
///
/// 为什么不缓存：缓存要在读路径上判过期、过期就得写回，而写回需要 `RT` 的**可变**借用；
/// 现在的读一律是共享借用，`a.with(|_| b.get())` 这类嵌套读因此合法。一旦读路径改成可变
/// 借用，嵌套读会当场 panic——那是比"每次重算"大得多的代价。映射请保持廉价。
struct Derived {
    src: SlotKey,
    compute: ComputeFn,
}

struct Slot {
    generation: u32,
    /// 派生槽位这里存的是 `()` 占位——只为让 arena 的存活判定与回收记账保持原样，
    /// 真值见 `derived`。
    value: Option<Box<dyn Any>>,
    /// 每次写自增；派生信号不自己记版本，`version()` 转问源。
    version: u64,
    /// 非 `None` 即为派生信号。
    derived: Option<Derived>,
}

/// 信号运行时：generational arena。
struct Runtime {
    slots: Vec<Slot>,
    free: Vec<u32>,
    /// 打开中的作用域收集帧（栈）：`signal()` 把新槽位登记进栈顶帧。
    /// 用栈而非单个"当前作用域"，是因为重建宿主可以嵌套（列表行里又有一张可排序表格）。
    frames: Vec<Vec<SlotKey>>,
    /// 活跃槽位数（`slots.len() - free.len()`，单独记以免每次统计都要遍历）。
    live: usize,
    /// 活跃槽位历史峰值。泄漏的判据是它**持续攀升**而非绝对值大。
    peak: usize,
    /// `WINDUI_SIGNALS` 诊断上次报告时的活跃数。
    reported: usize,
}

impl Runtime {
    const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            frames: Vec::new(),
            live: 0,
            peak: 0,
            reported: 0,
        }
    }

    fn insert(&mut self, value: Box<dyn Any>) -> SlotKey {
        let key = if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            slot.value = Some(value);
            slot.version = 0;
            slot.derived = None;
            // generation 已在 `remove` 时自增过，此处沿用即可——复用槽位天然与旧句柄不同代。
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
                derived: None,
            });
            SlotKey {
                index: idx,
                generation: 0,
            }
        };
        self.live += 1;
        if self.live > self.peak {
            self.peak = self.live;
        }
        if let Some(frame) = self.frames.last_mut() {
            frame.push(key);
        }
        self.maybe_report();
        key
    }

    /// 回收一个槽位：丢值、代际自增（旧句柄随之失效）、索引进空闲链。
    /// 返回是否真的回收了（句柄已失效时为 `false`，重复 dispose 因此是幂等的）。
    fn remove(&mut self, key: SlotKey) -> bool {
        let Some(slot) = self.slots.get_mut(key.index as usize) else {
            return false;
        };
        if slot.generation != key.generation || slot.value.is_none() {
            return false;
        }
        slot.value = None;
        slot.version = 0;
        slot.derived = None;
        // 溢出回绕后理论上可与极老的句柄撞代（同 `Tree` 的节点 id，2^32 次复用同一槽位）；
        // 实际不可达，且撞上也只是读到别人的值而非内存不安全。
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(key.index);
        self.live -= 1;
        true
    }

    /// 建一个派生槽位。走 `insert` 落占位值，故作用域收集、峰值统计、复用逻辑全部照旧。
    fn insert_derived(&mut self, src: SlotKey, compute: ComputeFn) -> SlotKey {
        let key = self.insert(Box::new(()));
        if let Some(slot) = self.slot_mut(key) {
            slot.derived = Some(Derived { src, compute });
        }
        key
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

    /// `WINDUI_SIGNALS` 诊断：活跃数每比上次报告多出一个步长就打一行。
    /// 健康的应用打几行就安静了；泄漏则是稳定增长的一串。
    fn maybe_report(&mut self) {
        let Some(step) = diag_step() else { return };
        if self.live < self.reported + step {
            return;
        }
        self.reported = self.live;
        eprintln!(
            "[windui] signals live={} free={} cap={} peak={}",
            self.live,
            self.free.len(),
            self.slots.len(),
            self.peak
        );
    }
}

/// `WINDUI_SIGNALS` 的报告步长：未设置/为空/为 `0` 时返回 `None`（关闭），
/// 否则取值作步长（非数字回退 `1`）。
///
/// 默认步长是 `1` 而不是某个大数：报告只在活跃数**创下新高**时触发，健康的应用因此
/// 只在启动阶段打几行就永久安静（一个中等应用的峰值往往就几十个槽位，步长设大了根本
/// 不会触发，看起来像坏了）。泄漏才会让新高不断刷新、持续刷屏——这正是要看的现象。
/// 输出太吵可以调大：`WINDUI_SIGNALS=64`。
fn diag_step() -> Option<usize> {
    use std::sync::OnceLock;
    static E: OnceLock<Option<usize>> = OnceLock::new();
    *E.get_or_init(|| match std::env::var("WINDUI_SIGNALS") {
        Ok(v) if v.is_empty() || v == "0" => None,
        Ok(v) => Some(v.parse::<usize>().ok().filter(|n| *n > 0).unwrap_or(1)),
        Err(_) => None,
    })
}

thread_local! {
    static RT: RefCell<Runtime> = const { RefCell::new(Runtime::new()) };
    /// 是否处于节点事件处理期（核心在 call_on_event 前后括起）。
    static EVENT_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// 本次事件处理期内是否写过信号（供核心据当前事件节点局部失效）。
    static TOUCHED: Cell<bool> = const { Cell::new(false) };
    /// 自上次广播以来是否写过信号——供平台层判断要不要让**其他**窗口也重绘。
    /// 见 [`take_cross_window_dirty`]。
    static CROSS_DIRTY: Cell<bool> = const { Cell::new(false) };
}

/// 写信号后触发重绘的钩子。
/// - 事件期内：仅记"写过信号"，由核心在 `end_event` 据当前事件节点产生**局部**脏区
///   （结构签名层会在显隐/布局变化时升级整窗），不强制整窗。
/// - 事件期外（后台 pump / 定时器 / 直接调用）：经 anim 通道请求重绘（整窗兜底）。
fn notify_changed() {
    // 无论在不在事件期都记一笔：信号是跨窗口共享状态的唯一原语（`Signal` 是 `Copy`
    // 句柄，传进子窗即可共享），而上面两条路都只能让**当前**窗口重绘——事件期那条
    // 走当前事件节点的局部脏区，事件期外那条走 anim 通道由本窗口的帧消费。
    // 于是"在设置窗里改了名字，主窗显示的还是旧的"。见 [`take_cross_window_dirty`]。
    mark_cross_window_dirty();
    if EVENT_ACTIVE.with(|c| c.get()) {
        TOUCHED.with(|c| c.set(true));
    } else {
        crate::anim::request_repaint();
    }
}

/// 标记「跨窗口可见状态已变」，让平台在分发收尾时刷新其余窗口。
///
/// 信号写入自动置位；此外还有一个非信号的来源——运行期换主题
/// （[`ThemeHandle::set`](crate::app::ThemeHandle::set)），它改的是所有窗口共享的
/// 主题源，却不经过任何信号。少了这一处，"换肤联动"就只在应用碰巧同时写了信号时
/// 才成立（比如用一个 `Signal<bool>` 记当前明暗），换个写法就失效。
pub(crate) fn mark_cross_window_dirty() {
    CROSS_DIRTY.with(|c| c.set(true));
}

/// 取走并清除「写过信号」标志，供平台层决定要不要让**其他**窗口也失效。
///
/// 只在多窗口下有实际作用：信号可以被任意窗口的控件写入，而读它的控件可能在别的窗口
/// 里。发起写入的那个窗口自有精确脏区，其余窗口无从知道自己该重绘，故由平台在事件分发
/// 收尾时统一广播一次。
///
/// 按"写过信号"而不是"需要重绘"来广播：后者每次 hover 都成立，会让所有窗口跟着刷；
/// 而信号写入是人手速度的低频事件，且正是跨窗共享状态唯一可能变化的途径。
pub(crate) fn take_cross_window_dirty() -> bool {
    CROSS_DIRTY.with(|c| c.replace(false))
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
///
/// # 线程约束
///
/// 信号的存储是 **线程局部** 的（见本模块的 `RT`），所以句柄**只能在创建它的线程**
/// （即 UI 线程）上读写。为此 `Signal<T>` 刻意实现为 `!Send` + `!Sync`——把句柄搬进
/// 另一个线程会编译失败，而不是在运行时静默丢值。
///
/// ```compile_fail,E0277
/// use windui::prelude::*;
/// let s = signal(1i32);
/// std::thread::spawn(move || s.set(42)); // Signal 不是 Send，编译失败
/// ```
///
/// 借引用共享同样不行（`!Sync`）：
///
/// ```compile_fail,E0277
/// use windui::prelude::*;
/// let s = signal(1i32);
/// std::thread::scope(|sc| {
///     sc.spawn(|| s.get()); // &Signal 要求 Sync，编译失败
/// });
/// ```
///
/// 后台线程要更新状态，走消息通道回到 UI 线程再写信号：用 `App::channel` 取得
/// `Sender<Msg>`（`Send`，可 move 进工作线程），在 UI 线程执行的 `on_message`
/// 回调里对信号 `set`/`update`：
///
/// ```no_run
/// use windui::prelude::*;
/// let count = signal(0u32);
/// let mut app = App::new("demo", 320, 200);
/// // 回调在 UI 线程跑，除信号外还收一个 EventCtx（可 toast、可关窗）
/// let tx = app.channel::<u32>(move |_ctx, n| count.set(n));
/// std::thread::spawn(move || {
///     let _ = tx.send(42); // 跨线程送的是消息，不是信号句柄
/// });
/// ```
pub struct Signal<T> {
    key: SlotKey,
    _t: PhantomData<fn() -> T>,
    /// 负标记：裸指针使 `Signal` 成为 `!Send` + `!Sync`，把"只能在 UI 线程用"
    /// 变成编译期约束。零大小，不影响 `Copy`。
    _not_send: PhantomData<*const ()>,
}

/// 句柄相等 = 指向同一 slot（含代际）。供以 Signal 为身份键的场景
/// （如富文本按折叠信号识别各 Section 的动画状态）。
impl<T> PartialEq for Signal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl<T> Eq for Signal<T> {}

/// 打印**句柄标识**（slot + 代际）而非值——`T` 未必 `Debug`，且绝大多数调试场景
/// 想知道的是"这两处引用的是不是同一个信号"。值请自行 `sig.get()`。
impl<T> std::fmt::Debug for Signal<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Signal({:?})", self.key)
    }
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
        _not_send: PhantomData,
    }
}

/// 一批临时信号的所有者：在 [`collect`](Self::collect) 内创建的信号归它所有，
/// [`dispose`](Self::dispose) 或析构时批量回收。
///
/// # 为什么是显式作用域，而不是隐式的所有权树
///
/// leptos / floem 那样的 `Scope` 建立在**响应式图**上——每个信号天然有一个创建它的
/// reactive owner。本库是**保留模式控件树**：信号绝大多数在 `main` 里创建、活到进程退出，
/// 根本没有 owner 可言。硬造一棵隐式所有权树，代价是"谁回收了我的信号"变得不可追溯，
/// 而收益只覆盖极少数真正临时的信号。
///
/// 于是本库反过来：**默认无主、永不回收**（应用状态的正确语义），只有明确圈进
/// `collect` 的才归属作用域。边界写在代码里，读代码就能看见谁会死。
///
/// # 谁在用它
///
/// 库内三处会按数据重建子树的宿主各持有一个：`Element::list_signal` 的 `DynList`、
/// `Element::reorder_list_signal` 的行源、以及可排序表格的行/表头。它们在重建时先回收
/// 上一轮的作用域、再在新作用域里构建新行——所以**在 `row_fn` 里 `signal(..)` 是安全的**，
/// 每轮重建的临时信号会随该轮子树一起消失，不会累积。
///
/// # 回收后旧句柄会失效
///
/// 被回收的槽位代际自增，`Copy` 出去的每一份句柄一并失效。若一个句柄可能比它的作用域
/// 活得久（菜单动作、toast 回调、跨线程消息处理器都可能），读它要用
/// [`Signal::try_get`] / [`Signal::try_with`]。
///
/// ```
/// use windui::prelude::*;
/// use windui::signal::SignalScope;
///
/// let mut scope = SignalScope::new();
/// let tmp = scope.collect(|| signal(7i32));
/// assert_eq!(tmp.get(), 7);
/// assert_eq!(scope.len(), 1);
///
/// scope.dispose();            // 整批回收
/// assert!(!tmp.is_alive());   // 旧句柄随之失效
/// assert_eq!(tmp.try_get(), None);
/// ```
///
/// # 线程约束
///
/// 同 [`Signal`]，作用域是 `!Send` + `!Sync`。它记的是**本线程运行时**里的槽位下标，
/// 搬到别的线程再析构会拿这些下标去操作那个线程的 arena——轻则什么都没释放，重则下标
/// 与代际恰好撞上、把别人的信号误杀。所以这条约束是编译期的，而不是文档里的口头约定：
///
/// ```compile_fail,E0277
/// use windui::signal::SignalScope;
/// let scope = SignalScope::new();
/// std::thread::spawn(move || drop(scope)); // SignalScope 不是 Send，编译失败
/// ```
#[derive(Default)]
pub struct SignalScope {
    owned: Vec<SlotKey>,
    /// 负标记：使作用域成为 `!Send` + `!Sync`（理由见类型文档「线程约束」）。零大小。
    _not_send: PhantomData<*const ()>,
}

impl SignalScope {
    /// 新建一个空作用域，尚不拥有任何信号。
    pub fn new() -> Self {
        Self {
            owned: Vec::new(),
            _not_send: PhantomData,
        }
    }

    /// 运行 `f`，期间 `signal()` 创建的所有信号归本作用域所有（追加，不清空既有的）。
    ///
    /// 可嵌套：内层 `collect` 期间创建的信号归内层作用域，不会重复登记到外层。
    /// `f` panic 时收集帧仍会正确关闭，已创建的部分照常归属本作用域。
    pub fn collect<R>(&mut self, f: impl FnOnce() -> R) -> R {
        RT.with(|rt| rt.borrow_mut().frames.push(Vec::new()));
        // 守卫保证 `f` 即使 panic 也把收集帧弹回来，否则栈会永久错位。
        struct FrameGuard<'a> {
            owned: &'a mut Vec<SlotKey>,
        }
        impl Drop for FrameGuard<'_> {
            fn drop(&mut self) {
                let frame = RT.with(|rt| rt.borrow_mut().frames.pop().unwrap_or_default());
                self.owned.extend(frame);
            }
        }
        let _guard = FrameGuard {
            owned: &mut self.owned,
        };
        f()
    }

    /// 回收本作用域拥有的全部信号，作用域随即回到空状态（可继续 `collect` 复用）。
    pub fn dispose(&mut self) {
        if self.owned.is_empty() {
            return;
        }
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            for key in self.owned.drain(..) {
                rt.remove(key);
            }
        });
    }

    /// 本作用域当前拥有的信号数量。
    pub fn len(&self) -> usize {
        self.owned.len()
    }

    /// 本作用域是否不拥有任何信号。
    pub fn is_empty(&self) -> bool {
        self.owned.is_empty()
    }
}

impl Drop for SignalScope {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// 信号运行时的槽位统计（见 [`stats`]）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SignalStats {
    /// 当前活跃（已创建且未回收）的槽位数。
    pub live: usize,
    /// 已回收、等待复用的槽位数。
    pub free: usize,
    /// arena 容量（`live + free`），即历史同时存活过的最大槽位数。
    pub capacity: usize,
    /// `live` 的历史峰值。
    pub peak: usize,
}

/// 当前线程信号运行时的槽位统计。
///
/// 用来把"泄漏"变成可观测的数字：怀疑某段交互在漏信号，就在交互前后各取一次
/// `stats().live` 比对——反复做同一件事（切页、刷新列表、排序）而 `live` 稳定上涨即是泄漏。
///
/// 也可以不改代码：设环境变量 `WINDUI_SIGNALS=1` 运行，活跃槽位每创下新高就打一行
/// `[windui] signals live=.. free=.. cap=.. peak=..`（输出到 stderr）。健康的应用在启动
/// 阶段打几行就永久安静，泄漏则持续刷屏。变量值即报告步长——嫌吵就调大（`=64` 表示
/// 活跃数每多 64 个才报一次）；`0` 或不设即关闭。
///
/// ```
/// use windui::prelude::*;
/// use windui::signal::{stats, SignalScope};
///
/// let before = stats().live;
/// let mut scope = SignalScope::new();
/// scope.collect(|| {
///     for i in 0..10 {
///         signal(i);
///     }
/// });
/// assert_eq!(stats().live, before + 10);
/// scope.dispose();
/// assert_eq!(stats().live, before, "作用域回收后活跃数应回到原点");
/// ```
pub fn stats() -> SignalStats {
    RT.with(|rt| {
        let rt = rt.borrow();
        SignalStats {
            live: rt.live,
            free: rt.free.len(),
            capacity: rt.slots.len(),
            peak: rt.peak,
        }
    })
}

impl<T: 'static> Signal<T> {
    /// 借用读取（免 clone）。
    ///
    /// 句柄已失效（槽位被 [`SignalScope`] 或 [`Signal::dispose`] 回收）时 **panic**。
    /// 读一个已死的信号没有合理的返回值可编——给个默认值只会把 bug 推迟到更远的地方，
    /// 所以这里选择就地炸掉。若该句柄**可能**已被回收（比如它来自一个会重建的列表行，
    /// 而当前代码活得比那一行久），用 [`try_with`](Self::try_with) 显式处理缺失。
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        self.try_with(f).expect(
            "signal 句柄已失效：槽位已被回收（SignalScope/dispose）。\
             可能失效的句柄请改用 try_with/try_get",
        )
    }

    /// 借用读取，句柄已失效时返回 `None` 而不是 panic。
    ///
    /// 用在"句柄合法地可能已死"的地方：菜单动作闭包、toast 回调、`App::channel` 的消息
    /// 处理器都可能比创建该信号的控件子树活得久。
    pub fn try_with<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        RT.with(|rt| {
            let rt = rt.borrow();
            let slot = rt.slot(self.key)?;
            // 派生信号：现算。借用全程是**共享**的，故映射闭包里再读别的信号也合法
            // （写则不然，见 `Signal::map` 的约束）。
            if let Some(d) = &slot.derived {
                let src = rt.slot(d.src)?;
                let raw = (d.compute)(src.value.as_ref()?.as_ref());
                let v = raw.downcast_ref::<T>().expect("派生信号的值类型与句柄不符");
                return Some(f(v));
            }
            let v = slot
                .value
                .as_ref()
                .unwrap()
                .downcast_ref::<T>()
                .expect("signal 值类型与句柄不符");
            Some(f(v))
        })
    }

    /// 句柄是否仍指向活着的槽位。
    pub fn is_alive(&self) -> bool {
        RT.with(|rt| rt.borrow().slot(self.key).is_some())
    }

    /// **派生信号**：映射本信号的值，得到一个只读的 `Signal<U>`。
    ///
    /// 解决的是"同一份状态要以另一种形态喂给控件"——例如把 `Signal<Tone>` 映成
    /// `Signal<Role>` 交给 [`Element::fg_role_signal`](crate::ui::Element::fg_role_signal)，
    /// 或把 `Signal<Vec<T>>` 映成 `Signal<String>` 做计数文案。没有它只能建两个节点各自
    /// `visible_when` 互斥，或者在每个写入点同时维护两个信号（漏一处就静默不同步）。
    ///
    /// # 语义
    ///
    /// - **读时现算，不缓存**：每次 `get`/`with` 都会调一次映射闭包。请保持闭包廉价。
    ///   不缓存是为了让读路径维持**共享借用**——现在 `a.with(|_| b.get())` 这类嵌套读是
    ///   合法的，而缓存必须在读路径上写回，写回要可变借用，嵌套读就会当场 panic。
    /// - **只读**：`set` / `update` 在派生信号上是空操作（debug 下 panic 提示）。要改值请
    ///   改源信号。
    /// - [`version`](Self::version) **转问源**：源变而映射结果不变时仍报"变过"。这是保守
    ///   方向——变更检测宁可多重建一次，也不能漏。
    /// - 源被回收后，本信号的读退化为 `None`（`try_get`）／panic（`get`），与普通信号
    ///   句柄失效一致。
    /// - **链式 `map` 会扁平化**：`a.map(f).map(g)` 只产生一个派生槽位（组合闭包），
    ///   不是两层嵌套。故链再长也只有一次槽位开销、一层借用。
    ///
    /// # 约束
    ///
    /// 映射闭包**只能读不能写**信号：读期间持着 `RT` 的共享借用，闭包里 `set` 会撞上
    /// `RefCell` 的借用冲突而 panic。映射本就该是纯函数。
    ///
    /// ```
    /// use windui::prelude::*;
    /// let n = signal(3usize);
    /// let label = n.map(|v| format!("共 {v} 条"));
    /// assert_eq!(label.get(), "共 3 条");
    /// n.set(10);
    /// assert_eq!(label.get(), "共 10 条", "派生值跟随源");
    /// ```
    pub fn map<U: 'static>(self, f: impl Fn(&T) -> U + 'static) -> Signal<U> {
        // 本次映射：源值(&dyn Any，实为 T) → U。
        let mine: ComputeFn = Rc::new(move |any| {
            let t = any.downcast_ref::<T>().expect("map 的输入类型与句柄不符");
            Box::new(f(t)) as Box<dyn Any>
        });
        let key = RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            // 自身已是派生就**组合**而不是再套一层：链式 map 恒为单层，读时也只解一次。
            let prev = rt
                .slot(self.key)
                .and_then(|s| s.derived.as_ref())
                .map(|d| (d.src, d.compute.clone()));
            let (src, compute) = match prev {
                Some((src, prev)) => {
                    let composed: ComputeFn = Rc::new(move |any| {
                        // prev: 源类型 → T；mine: T → U。
                        let mid = prev(any);
                        mine(&*mid)
                    });
                    (src, composed)
                }
                None => (self.key, mine),
            };
            rt.insert_derived(src, compute)
        });
        Signal {
            key,
            _t: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// 本信号是否为 [`map`](Self::map) 派生而来（派生信号只读）。
    pub fn is_derived(&self) -> bool {
        RT.with(|rt| {
            rt.borrow()
                .slot(self.key)
                .is_some_and(|s| s.derived.is_some())
        })
    }

    /// 立即回收本信号的槽位。
    ///
    /// 回收后所有指向同一槽位的句柄（`Copy` 出去的每一份）一并失效，槽位可被后续
    /// `signal()` 复用。重复 dispose 是幂等的——第二次因代际已变而什么都不做。
    ///
    /// 成批的临时信号用 [`SignalScope`] 更省心；本方法留给"就是要现在杀掉这一个"的场景。
    ///
    /// ```
    /// use windui::prelude::*;
    /// let s = signal(1i32);
    /// assert!(s.is_alive());
    /// s.dispose();
    /// assert!(!s.is_alive());
    /// assert_eq!(s.try_get(), None);
    /// ```
    pub fn dispose(self) {
        RT.with(|rt| rt.borrow_mut().remove(self.key));
    }

    /// 写入新值并触发重绘。
    ///
    /// 句柄失效（槽位已回收）时 debug 断言失败、release 静默丢弃——**故意**与
    /// [`Signal::with`] 的 panic 不对称：写进一块已经没人看的状态是一次定义良好的空操作，
    /// 而读它没有值可返回。这个不对称让"控件子树刚被重建、其上一次点击排队的回调才跑到"
    /// 这类竞态在 release 里退化为无害的丢弃，而不是崩溃。
    pub fn set(&self, value: T) {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if let Some(slot) = rt.slot_mut(self.key) {
                // 派生信号只读：真写下去会把占位的 `()` 换成 T，此后读它就 panic
                // （类型不符），而且症状离写入点很远。就地拦下。
                if slot.derived.is_some() {
                    debug_assert!(false, "派生信号（Signal::map）只读，请改源信号");
                    return;
                }
                slot.value = Some(Box::new(value));
                slot.version = slot.version.wrapping_add(1);
            } else {
                debug_assert!(false, "signal 句柄已失效");
            }
        });
        notify_changed();
    }

    /// 原地修改并触发重绘（避免 get→改→set 的一次 clone）。
    ///
    /// 失效语义同 [`Signal::set`]。
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        RT.with(|rt| {
            let mut rt = rt.borrow_mut();
            if let Some(slot) = rt.slot_mut(self.key) {
                // 理由同 [`Signal::set`]。
                if slot.derived.is_some() {
                    debug_assert!(false, "派生信号（Signal::map）只读，请改源信号");
                    return;
                }
                if let Some(v) = slot.value.as_mut().and_then(|b| b.downcast_mut::<T>()) {
                    f(v);
                    slot.version = slot.version.wrapping_add(1);
                } else {
                    debug_assert!(false, "signal 值类型与句柄不符");
                }
            } else {
                debug_assert!(false, "signal 句柄已失效");
            }
        });
        notify_changed();
    }
}

impl<T: 'static> Signal<T> {
    /// 当前写入版本号（每次 `set`/`update` 自增）。用于变更检测：缓存上次版本，
    /// 不相等则说明值已更新。信号已被释放时返回 `0`。
    pub fn version(self) -> u64 {
        RT.with(|rt| {
            let rt = rt.borrow();
            let slot = rt.slot(self.key)?;
            // 派生信号自己不记版本：它没有"写入"这回事。转问源——源变即报变，哪怕映射
            // 结果没变。保守方向：变更检测宁可多重建一次，也不能漏。
            match &slot.derived {
                Some(d) => rt.slot(d.src).map(|s| s.version),
                None => Some(slot.version),
            }
        })
        .unwrap_or(0)
    }
}

impl<T: Clone + 'static> Signal<T> {
    /// 读取当前值（克隆）。需要追踪依赖的派生场景用 `with` 更省。
    ///
    /// 句柄已失效时 panic，理由同 [`Signal::with`]；可能已死的句柄用
    /// [`try_get`](Self::try_get)。
    pub fn get(&self) -> T {
        self.with(|v| v.clone())
    }

    /// 读取当前值，句柄已失效时返回 `None`。
    pub fn try_get(&self) -> Option<T> {
        self.try_with(|v| v.clone())
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

    // ---------------------------------------------------------- 槽位回收

    #[test]
    fn dispose_invalidates_handle() {
        let s = signal(5i32);
        assert!(s.is_alive());
        s.dispose();
        assert!(!s.is_alive());
        assert_eq!(s.try_get(), None, "回收后 try_get 应返回 None 而非旧值");
        assert_eq!(s.try_with(|v: &i32| *v), None);
    }

    #[test]
    fn dispose_invalidates_every_copy_of_the_handle() {
        // Copy 句柄的关键不变式：回收的是槽位，复制出去的每一份一并失效。
        let a = signal(1i32);
        let b = a; // Copy
        let c = a;
        a.dispose();
        assert!(!b.is_alive());
        assert!(!c.is_alive());
    }

    #[test]
    #[should_panic(expected = "signal 句柄已失效")]
    fn reading_disposed_signal_panics() {
        let s = signal(1i32);
        s.dispose();
        let _ = s.get();
    }

    #[test]
    fn dispose_is_idempotent() {
        let s = signal(1i32);
        let live = stats().live;
        s.dispose();
        assert_eq!(stats().live, live - 1);
        s.dispose(); // 第二次因代际已变而空转，不应把活跃数再减一
        s.dispose();
        assert_eq!(stats().live, live - 1, "重复 dispose 不应重复计数");
    }

    #[test]
    fn disposed_slot_is_reused_with_bumped_generation() {
        let old = signal(1i32);
        let old_key = old.key;
        old.dispose();

        let new = signal(2i32);
        assert_eq!(new.key.index, old_key.index, "空闲槽位应被复用");
        assert_eq!(
            new.key.generation,
            old_key.generation + 1,
            "复用时代际应自增一次"
        );
        assert!(!old.is_alive(), "旧句柄不因槽位复用而复活");
        assert_eq!(new.get(), 2);
        assert!(old != new, "代际不同 → 句柄不相等");
    }

    #[test]
    fn generation_increments_once_per_reuse() {
        let mut idx = None;
        let mut last_gen = None;
        for round in 0..5u32 {
            let s = signal(round);
            match idx {
                None => idx = Some(s.key.index),
                Some(i) => assert_eq!(s.key.index, i, "每轮都应复用同一个槽位"),
            }
            assert_eq!(s.key.generation, round, "代际应逐轮 +1");
            last_gen = Some(s.key.generation);
            s.dispose();
        }
        assert_eq!(last_gen, Some(4));
    }

    #[test]
    fn reused_slot_starts_with_fresh_version() {
        let a = signal(1i32);
        a.set(2);
        a.set(3);
        assert_eq!(a.version(), 2);
        a.dispose();
        let b = signal(9i32);
        assert_eq!(b.version(), 0, "复用槽位的写版本号应重置");
        assert_eq!(a.version(), 0, "失效句柄查版本号返回 0");
    }

    // ---------------------------------------------------------- 作用域

    #[test]
    fn scope_owns_signals_created_inside_it() {
        let before = stats().live;
        let mut scope = SignalScope::new();
        let (a, b) = scope.collect(|| (signal(1i32), signal(2i32)));
        assert_eq!(scope.len(), 2);
        assert_eq!(stats().live, before + 2);

        scope.dispose();
        assert!(!a.is_alive());
        assert!(!b.is_alive());
        assert!(scope.is_empty());
        assert_eq!(stats().live, before, "整批回收后活跃数回到原点");
    }

    #[test]
    fn signals_created_outside_any_scope_are_unowned() {
        // 本库的默认所有权语义：作用域之外创建的信号不归任何人，永不被回收。
        let outside = signal(1i32);
        let mut scope = SignalScope::new();
        scope.collect(|| signal(2i32));
        scope.dispose();
        assert!(outside.is_alive(), "作用域外的信号不该被它回收");
    }

    #[test]
    fn scope_captures_only_creations_not_reads_of_outer_signals() {
        let outer = signal(1i32);
        let mut scope = SignalScope::new();
        scope.collect(|| {
            let _ = outer.get(); // 读外部信号不构成归属
            outer.set(2);
        });
        assert_eq!(scope.len(), 0);
        scope.dispose();
        assert!(outer.is_alive());
    }

    #[test]
    fn scope_disposes_on_drop() {
        let s = {
            let mut scope = SignalScope::new();
            let s = scope.collect(|| signal(1i32));
            assert!(s.is_alive());
            s
        }; // scope 在此析构
        assert!(!s.is_alive(), "作用域析构应回收其信号");
    }

    #[test]
    fn nested_scopes_each_own_their_own() {
        let mut outer = SignalScope::new();
        let mut inner = SignalScope::new();
        let (o, i) = outer.collect(|| {
            let o = signal(1i32);
            let i = inner.collect(|| signal(2i32));
            (o, i)
        });
        assert_eq!(outer.len(), 1, "内层创建的不该重复登记到外层");
        assert_eq!(inner.len(), 1);

        inner.dispose();
        assert!(!i.is_alive());
        assert!(o.is_alive(), "内层回收不应波及外层");

        outer.dispose();
        assert!(!o.is_alive());
    }

    #[test]
    fn scope_can_be_reused_after_dispose() {
        let mut scope = SignalScope::new();
        let first = scope.collect(|| signal(1i32));
        scope.dispose();
        let second = scope.collect(|| signal(2i32));
        assert!(!first.is_alive());
        assert!(second.is_alive());
        assert_eq!(scope.len(), 1);
        assert_eq!(second.get(), 2);
    }

    #[test]
    fn collect_accumulates_across_calls() {
        let mut scope = SignalScope::new();
        scope.collect(|| signal(1i32));
        scope.collect(|| signal(2i32));
        assert_eq!(scope.len(), 2, "collect 追加而非清空");
    }

    #[test]
    fn collect_frame_is_closed_even_if_closure_panics() {
        let mut scope = SignalScope::new();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scope.collect(|| {
                signal(1i32);
                panic!("boom");
            })
        }));
        assert!(r.is_err());
        assert_eq!(scope.len(), 1, "panic 前已创建的信号仍归本作用域");

        // 关键：收集帧必须已经弹出，否则后续"作用域外"的信号会被错误登记。
        let after = signal(2i32);
        scope.dispose();
        assert!(after.is_alive(), "panic 后收集帧未正确关闭，帧栈已错位");
    }

    // ---------------------------------------------------------- 诊断

    #[test]
    fn stats_track_live_free_and_peak() {
        let before = stats();
        let a = signal(1i32);
        let b = signal(2i32);
        let mid = stats();
        assert_eq!(mid.live, before.live + 2);
        assert!(mid.peak >= mid.live);

        a.dispose();
        b.dispose();
        let after = stats();
        assert_eq!(after.live, before.live);
        assert_eq!(after.free, before.free + 2, "回收的槽位进空闲链");
        assert_eq!(
            after.capacity,
            after.live + after.free,
            "capacity = live + free"
        );
        assert_eq!(after.peak, mid.peak, "峰值只涨不落");
    }

    #[test]
    fn repeated_scope_cycles_do_not_grow_the_arena() {
        // 这是整个方案要保证的东西：重建宿主反复重建，槽位总量不涨。
        let mut scope = SignalScope::new();
        for _ in 0..3 {
            scope.dispose();
            scope.collect(|| {
                for i in 0..8 {
                    signal(i);
                }
            });
        }
        let steady = stats().capacity;
        for _ in 0..50 {
            scope.dispose();
            scope.collect(|| {
                for i in 0..8 {
                    signal(i);
                }
            });
        }
        assert_eq!(
            stats().capacity,
            steady,
            "反复重建应完全复用槽位，arena 不应增长"
        );
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

    /// 派生值跟随源，且 `version` 转问源（供 `DynList` 一类的变更检测）。
    ///
    /// 没有它时错在哪：派生信号若自己记版本，它永远是 0——绑了它的响应式宿主
    /// （`list_signal`/`host_signal`）就永远不重建，界面停在首帧那份数据上。
    #[test]
    fn derived_follows_source_including_version() {
        let n = signal(3usize);
        let label = n.map(|v| format!("共 {v} 条"));
        assert_eq!(label.get(), "共 3 条");
        assert!(label.is_derived() && !n.is_derived());

        let v0 = label.version();
        n.set(10);
        assert_eq!(label.get(), "共 10 条", "派生值应跟随源");
        assert_ne!(label.version(), v0, "源变过，派生的 version 必须跟着变");
    }

    /// 链式 `map` **扁平化**：只多一个槽位，不是每层一个。
    ///
    /// 没有这条时错在哪：层层嵌套的话，读一次要沿链逐层解引用，且每层都占一个槽位——
    /// 在会重建的子树里（每次重建都重新 map）槽位会成倍累积，`WINDUI_SIGNALS` 的活跃数
    /// 一路攀升，看起来像信号泄漏。
    #[test]
    fn chained_map_flattens_to_a_single_slot() {
        let n = signal(2i32);
        let before = stats().live;
        let out = n.map(|v| v * 3).map(|v| v + 1).map(|v| format!("{v}"));
        assert_eq!(out.get(), "7", "三层组合应等于依次施加");
        assert_eq!(
            stats().live - before,
            3,
            "三次 map 各产生一个句柄，但每个都是单层——不会因组合而多占"
        );
        n.set(5);
        assert_eq!(out.get(), "16", "链尾仍跟随源");
    }

    /// 映射闭包里**读别的信号**是合法的——读路径全程共享借用。
    ///
    /// 没有这条约束时错在哪：若为了缓存而把读路径改成可变借用，这种写法（以及既有的
    /// `a.with(|_| b.get())`）会当场 `already mutably borrowed` panic，而且是运行期才炸。
    #[test]
    fn map_closure_may_read_other_signals() {
        let unit = signal(String::from("条"));
        let n = signal(4usize);
        let label = n.map(move |v| format!("共 {v} {}", unit.get()));
        assert_eq!(label.get(), "共 4 条");
        unit.set(String::from("项"));
        assert_eq!(
            label.get(),
            "共 4 项",
            "闭包每次读时现算，故也跟随它读的信号"
        );
    }

    /// 派生信号只读：`set`/`update` 不生效。
    ///
    /// 没有守卫时错在哪：写入会把派生槽位里的 `()` 占位换成真值，此后每次读都因类型不符
    /// 而 panic——症状离写入点很远，很难查。
    #[test]
    #[should_panic(expected = "派生信号")]
    fn derived_rejects_writes() {
        let n = signal(1i32);
        let d = n.map(|v| v * 2);
        d.set(99);
    }

    /// 源被回收后，派生信号的读退化为 `None`，与普通句柄失效一致（不 panic 在 try_ 路径）。
    #[test]
    fn derived_dies_with_its_source() {
        let n = signal(7i32);
        let d = n.map(|v| v + 1);
        assert_eq!(d.try_get(), Some(8));
        n.dispose();
        assert_eq!(d.try_get(), None, "源没了，派生读不出值");
        assert!(d.is_alive(), "派生槽位自身还在（要单独回收），只是读不出值");
    }

    /// 派生槽位随 [`SignalScope`] 一并回收——它走的是同一条 `insert` 路径。
    ///
    /// 没有这条时错在哪：会重建的子树里每轮 map 都留下一个不归任何作用域管的槽位，
    /// 一轮一个地漏。
    #[test]
    fn derived_slots_are_collected_by_scope() {
        let src = signal(1i32);
        let before = stats().live;
        let mut scope = SignalScope::new();
        scope.collect(|| {
            let _a = src.map(|v| v * 2);
            let _b = src.map(|v| v * 3);
        });
        assert_eq!(stats().live - before, 2, "作用域内建了两个派生槽位");
        scope.dispose();
        assert_eq!(stats().live, before, "作用域回收后派生槽位应一并归还");
    }
}
