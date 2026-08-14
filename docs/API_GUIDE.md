# windui — 第三方开发指南

> 面向使用本库构建应用的开发者。讲清 **API 风格、设计思路、命名规范、扩展方式与约定**。
> 架构内幕（内存模型、三阶段布局原理）见 [DESIGN.md](./DESIGN.md)。

---

## 1. 设计哲学

windui 是一个**轻量、命令式、retained-mode** 的跨平台桌面 GUI 库（Windows 与 macOS 均已支持）。五条核心原则，决定了所有 API 的样子：

1. **命令式 Builder，零解析**。UI 用纯 Rust 链式调用构建，无 DSL、无宏、无运行时解析。类型即文档，编译期即校验。
2. **共享可变状态用 `Signal<T>`**。控件不持有"模型"，而是绑定到一个 `Copy` 的状态句柄。你 `set` 信号、框架自动请求重绘，UI 下一帧反映。这是贯穿全库的统一心智模型（见 §3.2）。
3. **retained + 空闲零 CPU**。控件树常驻，无事件/无脏区时不重绘、不唤醒。动画按需驱动（见 §8）。
4. **样式两层**：控件不硬编码颜色/间距，全部走 `Theme`（全局调色板 + 每控件覆盖层），可映射 TOML。单点视觉调整走内联 `Style` 修饰符。主题可在运行期热切换（见 §7.3）。
5. **平台差异收口在平台层**。控件与核心层平台无关，Windows 与 macOS 共用同一份 UI 代码（见 DESIGN.md 跨平台缝合）；少数尚未拉齐的能力见 §11。

---

## 2. 三分钟上手

```rust
use windui::prelude::*;

fn main() {
    // 1) 状态：signal() 造一个 Copy 句柄，控件与回调都直接绑它
    let count = signal(0i64);
    let text = signal(String::from("计数：0"));

    // 2) UI：命令式 Builder 组装控件树
    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(12)
        .bg(Color::hex(0xF5F6FA))
        .child(Element::label("计数器").font_size(20.0))
        .child(Element::label_signal(text).font_size(14.0))   // 绑信号的动态标签
        .child(Element::button("点我 +1").on_click(move |_| {
            count.update(|v| *v += 1);                    // 写入自动请求重绘
            text.set(format!("计数：{}", count.get()));
        }));

    // 3) 窗口：配置并运行
    App::new("Demo", 360, 240)
        .bg(Color::hex(0xF5F6FA))
        .content(ui)
        .run();
}
```

注意闭包里**没有** `let c = count.clone();` 这类前戏——`Signal<T>` 是 `Copy` 的，`move` 闭包
直接按值捕获，同一个信号可被任意多个闭包捕获且都指向同一份存储。

`use windui::prelude::*;` 引入最常用的 `App / Element / signal / Signal / Color / Insets /
Point / Rect / Size / Align / Axis / Dimension / Role / Style / Theme / Intent / Sender`。

### 2.1 两段式 App（需要运行期句柄时）

`App` 的多数方法是消费型 builder（`mut self -> Self`），可以一路链下去。但两个方法是
`&mut self`——[`channel`](#85-跨线程更新)（跨线程消息通道）与
[`theme_handle`](#73-运行期换主题)（运行期主题句柄）——它们要在链式消费**之前**取出，
所以写成两段式：

```rust
use windui::prelude::*;

fn main() {
    let progress = signal(0.0f32);

    // 第一段：可变绑定，取运行期句柄
    let mut app = App::new("下载", 360, 180);
    let theme = app.theme_handle();                       // &mut self
    let tx = app.channel::<f32>(move |p| progress.set(p)); // &mut self

    std::thread::spawn(move || {
        let _ = tx.send(0.5);
    });

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(Element::progress(progress).width_match())
        .child(Element::button("暗色").on_click(move |_| theme.set(Theme::dark())));

    // 第二段：消费型链式收尾，content/run 必须在最后
    app.content(ui).run();
}
```

顺序约束只有一条：**`channel` / `theme_handle` 必须在 `content` / `run` 之前**——后两者
消费 `App`，之后就没有 `&mut self` 可借了。

---

## 3. 核心心智模型

### 3.1 一切都是 `Element`

`Element` 是构建期的**控件描述符**（builder）。它有三类方法，链式串起来：

| 类别 | 作用 | 例子 |
|------|------|------|
| **构造器**（关联函数） | 创建一个 Element | `Element::col()`、`Element::button("OK")` |
| **布局修饰符**（`self -> Self`） | 配置尺寸/排布 | `.width(120)`、`.fill()`、`.spacing(8)` |
| **样式修饰符**（`self -> Self`） | 配置视觉 | `.bg(c)`、`.corner(8.0)`、`.fg(c)` |

容器用 `.child()` / `.children()` 嵌套子节点。最终把根 `Element` 交给 `App::content()`，由框架 `build` 成内部节点树。

### 3.2 状态绑定：`Signal<T>` 模型

控件**不存数据**，只持有一个指向外部状态的句柄。改状态 → UI 反映。

```rust
use windui::prelude::*;

let dark = signal(false);              // 自由函数 signal(初值) 创建，从 prelude 引入
Element::switch(dark);                 // 控件读写它——直接传，不用 clone
Element::label("仅暗色时显示").visible_when(move || dark.get());
```

`Signal<T>` 有三个性质，用之前先记住：

1. **它是 `Copy` 的**。句柄本身只是运行时 arena 里的一个下标，按值传进控件、按值被
   `move` 闭包捕获都不消耗原变量。这是相对旧 `Rc<Cell<T>>` 模型最大的人体工学改进——
   再也不用在每个闭包前写 `let d = dark.clone();`。
2. **写入自动触发重绘**。`set` / `update` 内部会请求重绘，**不需要**手写
   `ctx.mark_dirty()`（自定义 `Widget` 里改自有非信号状态时才需要，见 §9）。
3. **只能在 UI 线程用**。存储是线程局部的，`Signal<T>` 刻意实现为 `!Send`——句柄搬进
   别的线程是**编译错误**而非运行期静默丢值。后台线程更新状态见 §8.5。

常用 API 只有五个方法：

```rust
let n = signal(0i64);
n.set(3);                     // 写入（丢弃旧值）
n.update(|v| *v += 1);        // 原地改（省一次 clone；T 不必是 Clone）
let v: i64 = n.get();         // 读（克隆一份，要求 T: Clone）
let cur = n.with(|v| *v);     // 借用读（免 clone，T 不必是 Clone）
let ver: u64 = n.version();   // 写入版本号，每次 set/update 自增（变更检测用）
```

各控件对应的状态类型：

| 控件 | 状态类型 | 含义 |
|------|----------|------|
| `checkbox` / `switch` / `collapsible` / `dialog` / `dialog_panel` | `Signal<bool>` | 开关 / 显隐 |
| `radio` / `dropdown` / `segmented` / `list` / `list_pill` / `tabs` / `tabs_pill` | `Signal<usize>` | 选中索引 |
| `accordion` | `Signal<Option<usize>>` | 选中面板，`None` = 全收起 |
| `slider` / `progress` | `Signal<f32>` | 0.0–1.0 |
| `stepper` | `Signal<f64>` | 数值 |
| `text_input` / `label_signal` / `rich_signal` | `Signal<String>`（`rich_signal` 为 `Signal<RichDoc>`） | 文本 |
| `label` / `button` / `link` / `badge` / `checkbox` / `radio` / `nav_row` / `icon_button` 的**文案参数** | `Signal<String>`（可选，也可给 `&str`） | 跟随状态变化的文案，见 §5「动态文案」 |
| `list_signal` / `host_signal` / `reorder_list_signal` | `Signal<Vec<T>>` | 动态数据源（见 §6.5） |
| `dropdown_signal` | `Signal<Vec<String>>` | 动态选项 |
| `table_editable` | `Vec<Vec<Signal<String>>>` | 每格一个信号 |
| `table_selectable` | `Vec<Signal<bool>>` | 每行一个选中信号 |
| `table_sortable` / `_server` | `Signal<Option<SortKey>>` | 排序列 + 方向（`SortKey { column, order }`） |
| `visible_signal` / `enabled_signal` | `Signal<bool>` | 显隐 / 启用态（启用沿父链继承） |
| `visible_when` / `enabled_when` | 闭包 `Fn() -> bool` | 派生显隐 / 启用 |
| `visible` / `enabled` / `disabled` | `bool` | 静态显隐 / 启用 |

**惯用法**：状态在 `main`（或你的 App 结构）里创建，直接按值传进控件和回调。需要"一处改、
多处联动"时，把同一个信号传给多个控件即可——它们读的是同一份存储。

#### 信号的生命周期：谁拥有它、什么时候回收

**绝大多数情况你不需要想这件事**：在 `main` 里建的信号是应用状态，活到进程退出就是对的，
框架不会去回收它们。下面这一小节只关系到一种情形——**在会被反复重建的子树里创建信号**。

信号的存储是一个线程局部 arena。所有权模型是两级的：

- **默认无主**：任何作用域之外调用 `signal()` 创建的信号**永不回收**。应用状态走这条。
- **归属作用域**：在 `SignalScope::collect(..)` 内创建的信号归该作用域所有，作用域回收
  时整批释放，槽位可被后续 `signal()` 复用。

本库有三处会按数据变化整批重建子树的宿主：`list_signal` / `host_signal`（`DynList`）、
`reorder_list_signal` 的行源、以及可排序表格的表头与正文。**它们各自持有一个作用域**，
重建时先回收上一轮再收集新一轮。所以：

```rust
// 安全：每次数据变化重建行时，上一轮的 caption 会被回收，不会累积
Element::list_signal(tasks, |t| t.id, |t: Task| {
    let caption = signal(format!("{} 项", t.count));   // 行内现造的信号
    Element::button(caption).on_click(move |_| caption.set("已处理".into()))
})
```

需要自己管一批临时信号时用 `SignalScope`（不在 prelude，从 `windui::signal` 引入）：

```rust
use windui::signal::SignalScope;

let mut scope = SignalScope::new();
let tmp = scope.collect(|| signal(0i32));   // 归 scope 所有
scope.dispose();                            // 整批回收；析构时也会自动回收
assert!(!tmp.is_alive());
```

单个信号可以 `sig.dispose()` 直接回收（幂等）。

**回收后旧句柄会失效**。`Signal<T>` 是 `Copy` 的，复制出去的每一份指向同一个槽位，
槽位一回收全部失效：

| 操作 | 句柄已失效时 |
|------|--------------|
| `get()` / `with()` | **panic**（读一个已死的信号没有合理返回值可编） |
| `set()` / `update()` | debug 断言、release 静默丢弃（写进没人看的状态是定义良好的空操作） |
| `try_get()` / `try_with()` | 返回 `None` |
| `is_alive()` | `false` |
| `version()` | `0` |

读写故意不对称：它让"控件子树刚被重建、其上一次点击排队的回调才跑到"这类竞态在 release
里退化为无害的丢弃而不是崩溃。若某个句柄**可能**比它的作用域活得久（菜单动作闭包、toast
回调、`App::channel` 的消息处理器都可能），读它请用 `try_get()` / `try_with()`。

**观测**：`windui::signal::stats()` 随时返回 `{ live, free, capacity, peak }`。怀疑漏信号
就在一次交互前后各取一次 `live` 比对；或者不改代码，设环境变量 `WINDUI_SIGNALS=1` 运行——
活跃槽位每创下新高就往 stderr 打一行 `[windui] signals live=.. free=.. cap=.. peak=..`，
健康的应用在启动阶段打几行就永久安静，泄漏则持续刷屏。变量值即报告步长，嫌吵调大即可
（`=64` 表示活跃数每多 64 个才报一次）；`0` 或不设即关闭。

---

## 4. API 命名规范

第三方写扩展或阅读代码时，按这套约定即可预测 API 形状：

- **构造器 = 控件名（名词）**：`col`、`row`、`button`、`dropdown`…，全小写蛇形。
- **布局/样式修饰符 = 属性名**：`width`、`padding`、`bg`、`corner`…，设置型方法**不加** `set_` 前缀（builder 惯例）。
- **颜色用缩写**：背景 `bg`、前景 `fg`，全库一致（`Element::bg`、`App::bg`、`Style.bg`、`EventCtx::set_bg`）。
- **单条文案参数都是 `impl Into<TextContent>`**：`button`、`label`、`link`、`badge`、`checkbox`、`radio`、`nav_row`、`icon_button` 的文案可传 `&str`、`String`，也可以直接传 `Signal<String>` 让文案跟着状态变（见 §5「动态文案」）。
  成组的文案（`dropdown`/`list`/`tabs` 的 `Vec<impl Into<String>>`、表格的列名与单元格）仍是 `impl Into<String>`：整组内容要动的场景归 `list_signal` 一族管（§6.5），不是逐条绑信号。
- **`_signal` 后缀只用于「参数类型不同」的构造器**：`list_signal(Signal<Vec<T>>)`、`dropdown_signal(Signal<Vec<String>>, Signal<usize>)`……文案绑定不需要这个后缀，因为传信号和传字符串走的是**同一个**构造器。
- **状态参数一律 `Signal<T>`**：`checkbox(label, Signal<bool>)`、`dropdown(options, Signal<usize>)`……第一个参数是内容、第二个是状态，顺序全库一致。
- **事件回调 = `on_<动作>`**：`on_click`、`on_toggle`、`on_row_activate`……签名规则见 §8.1。
  只有"发生了什么之后调"的才叫 `on_`；"每次渲染/构建时调"的**生成器**（`summary`、`actions`、
  `cell_render`、`on_context_menu` 的 `build` 参数）返回内容而非响应事件，故不带前缀、不收 `ctx`。
- **`xxx_xy(h, v)`** = 水平/垂直两参版本：`padding_xy`、`margin_xy`。注意这里 `h`=horizontal、`v`=vertical（与 `size(w, h)` 的 `h`=height 不同名同义，按方法语境区分）。
- **`xxx_match` / `fill`** = 撑满父容器：`width_match`、`height_match`、`fill`（= 两者）。
- **getter 不加 `get_`**（Rust 惯例）：`EventCtx::bounds()`、`id()`、`scroll_metrics()`。
- **`set_` 仅用于命令式副作用 setter**（非 builder）：`EventCtx::set_scroll()`、`set_bg()`。

---

## 5. 控件目录

全部经 `Element::` 构造。`impl Into<String>` 处可传 `&str` 或 `String`；`impl Into<TextContent>` 处还可以传 `Signal<String>`（见本节「动态文案」）。

### 容器 / 布局
```rust
Element::col()                       // 纵向线性容器
Element::row()                       // 横向线性容器
Element::stack()                     // 层叠（Frame，后者覆盖前者）
Element::leaf()                      // 叶子（自定义控件载体，见 §9）
Element::scroll()                    // 垂直滚动容器（支持鼠标滚轮 + 触摸滑动/惯性）
Element::divider()                   // 分隔线
Element::tabs(selected, vec![("标签", page_element), ...])       // selected: Signal<usize>
Element::tabs_icons(selected, vec![("标签", icon, page), ...])  // 带图标的标签（icon: ImageContent）
Element::tabs_pill(selected, vec![("标签", page), ...])         // 胶囊风格标签页（签名同 tabs，仅视觉不同）
Element::grid(cols, gap, items)      // 等宽网格：每行 cols 个、列按权重均分、末行补空对齐
Element::dialog(show, content)       // 模态遮罩 + 居中内容（show: Signal<bool>）
Element::dialog_panel(show, "标题", width, on_close, body, footer)  // 带标题栏/关闭×/底栏的对话框面板
Element::flex_spacer()               // 弹性空白：占满主轴剩余空间（把兄弟推到另一端，如底栏左/右分布）
```

### 表单脚手架

「标签 + 控件」的一行和「标题 + 内容」的卡片，是设置类小工具里重复最多的两块样板。

```rust
Element::field("音量", Element::slider(v).width_match())   // 固定标签列 + 紧随其后的控件
Element::setting_row("隐藏状态栏", Element::switch(hide))   // 标签占左、控件贴右缘
Element::setting_row_desc("模糊音纠错", "z/zh 不区分", Element::switch(fuzzy))  // 标签下加一行说明
Element::card("通知", body)                                 // 标题 + 分隔线 + 内容的圆角卡片
```

- `field` 与 `setting_row` 的差别只有控件的落点：前者紧跟固定宽的标签列（表单感，控件左缘对齐成一条竖线），后者贴右缘（设置页感）。两者都**定高**，一列行才对得齐。
- `setting_row_desc` 是唯一**不定高**的一种——副标题长短不一，定高会把它挤出去，故改由上下内边距撑开。
- 行高、标签列宽、间距、字号一律走主题（`theme.form`，见 §7.2），**不进签名**：一个应用里的表单行必须整齐划一，逐行传尺寸只会让每处各写一个近似值。卡片的圆角/内边距/标题字号同理走 `theme.card`。
- 想要描边卡片：`Element::card("标题", body).border_role(Role::Border, 1)`。

> **这几个构造器返回的是拼好的容器，不是挂了 widget 的控件**（`badge` / `chip` / `grid` / `dialog_panel` 同理）。因此**可以**链容器/样式类修饰符（`.padding()` / `.margin_xy()` / `.bg_role()` / `.corner()` / `.width()` / `.visible_when()` / `.enabled(false)`），但**不能**链控件专属修饰符（`.intent()` / `.small()` / `.outline()` / `.on_click()`）——后者要 downcast 到具体 widget，挂到组合容器上在 debug 下会 `debug_assert` 失败、release 下静默无效。要改控件外观请加在**传进去的那个 control 上**。
>
> 另外它们在**构造期**读主题定尺寸，故自定义主题必须在建树**之前**装好，见 §7.2 末尾。

### 表格族

```rust
// 只读：columns 为 (列标题, 权重)，rows 为每行单元格文本
Element::table(vec![("列名", 2.0), ("大小", 1.0)], vec![vec!["a.txt", "12"]])
// 单元格为任意 Element（可点/可编辑）。注意 columns 这里是 Vec<(String, f32)>，不收 &str
Element::table_custom(vec![("列名".to_string(), 2.0)], rows_of_elements)
// 可编辑：cells: Vec<Vec<Signal<String>>>，点格触发 on_edit(ctx, row, col)
Element::table_editable(columns, cells, |ctx, r, c| { /* 弹编辑框 */ })
// 客户端排序：点表头在 无 → 升序 → 降序 → 无 间循环；sort: Signal<Option<SortKey>>
//   SortKey { column, order }，便捷构造 SortKey::asc(0) / ::desc(0) / ::new(col, ord)
Element::table_sortable(columns, rows, sort)
// 服务端排序/分页：前端不排序，rows: Signal<Vec<Vec<String>>> 由 on_sort 回调里重新拉取写回
Element::table_sortable_server(columns, rows, sort, |ctx, new_sort| { /* 拉数据后 rows.set(..) */ })
// 可多选：首列复选框 + 表头三态全选；selected: Vec<Signal<bool>>（长度 == rows，按原始行下标索引）
Element::table_selectable(columns, rows, selected, sort)
```

扩展修饰符（链在上述表格返回的元素上）：

```rust
.sort_indicator(SortStyle { asc: Some("↑".into()), ..Default::default() })  // 排序箭头样式
.actions("操作", 1.6, |row| Element::button("删除"))   // 尾部追加操作列
.cell_render(|row, col, text| None)                     // 自定义数据单元格；None 回退默认文本
.cell_lines(2)                                          // 默认文本格最多显示几行
.on_row_activate(|ctx, row| { /* 双击行 */ })
.on_row_context_menu(|row| vec![MenuItem::run("删除", |_ctx| {}, false)])
```

> ⚠️ **适用矩阵**：这些修饰符只对部分表格变体生效。误用时 **debug 构建下 `panic` 报错提示、
> release 下静默忽略**（口径同 §5 的 text_input 专属修饰符），panic 位置指向你的调用行。
> 照下表核对：
>
> | 修饰符 | `table` | `table_custom` | `table_editable` | `table_sortable` | `table_sortable_server` | `table_selectable` |
> |---|---|---|---|---|---|---|
> | `sort_indicator` | ✗ | ✗ | ✗ | ✓ | ✓ | ✗ |
> | `actions` | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
> | `cell_render` / `cell_lines` | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
> | `on_row_activate` | ✗ | ✗ | ✗ | ✓ | ✓ | ✗（与首列复选框语义冲突） |
> | `on_row_context_menu` | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
>
> `table` / `table_editable` 内部会转成 `table_custom` 的结构，三者都不带响应式表头/正文
> widget，因此整列扩展点都不适用——需要排序或行级交互，请直接从 `table_sortable` 起步。
>
> **行下标语义**：`actions` / `cell_render` / `on_row_activate` / `on_row_context_menu`
> 拿到的下标，客户端表格（`table_sortable` / `table_selectable`）是**原始行下标**（排序重排
> 后仍锁定同一数据行，可直接索引 `selected[row]`），服务端表格（`table_sortable_server`）
> 是当前页内的**显示下标**。

> **可编辑表格**：`cells: Vec<Vec<Signal<String>>>`（每格一个信号，显示自动跟随）。点单元格触发
> `on_edit(ctx, row, col)`，由 app 据 (row,col) 弹出编辑框（如 `dialog_panel` + `text_input` 绑定临时
> Signal），确认后 `cells[r][c].set(新值)`，表格下一帧自动刷新——**编辑入口与提交解耦，非即时**。
> 完整示例见 `examples/settings.rs`。
> `grid` 适合复选框组、卡片墙；`dialog_panel` 内的 `body` 自行设宽高（表格类用 `.height(360)`）；
> 表格类均需置于限高容器内（正文区滚动）。`flex_spacer` 用于「左按钮 … 右按钮」的底栏布局。

### 基础控件
```rust
Element::label("文本")
Element::button("确定").on_click(|ctx| { /* ... */ })
//  .danger() / .neutral() / .intent(Intent::X)   语义意图色（Button/CheckBox 通用）
//  .accent(color) / .accent_role(Role::X)        自定义意图基色：定色 / 跟随主题（成对，同 fg / fg_role）
//  .outline()      描边变体（透明底 + 意图色边框/文字），可叠加 neutral/danger/accent(_role)
//  .small()        紧凑内边距（密集工具栏用）
Element::icon_button("\u{25B2}").on_click(|ctx| ...)   // 纯图标按钮（字形）：▲▼ 调序 / ⓘ 信息 / × 关闭
Element::icon_button_content(image_content)            // 纯图标按钮（图片/SVG）
//  图标按钮：方形、hover/press 圆底 + 键盘激活 + 手型光标；.size(w,h) 调尺寸、.fg() 取色、.tooltip() 加说明
Element::badge("v0.0.0-alpha")                   // 胶囊徽章（不可删）：pill + 强调色淡底
Element::badge_intent("废弃", Intent::Danger)    // 指定语义色的徽章
Element::chip("分号(;)", |ctx| { /* 移除 */ })   // 可删标签：pill + × 删除按钮（点 × 触发回调）
Element::tag_field("输入…", vec![chip1, chip2])  // 多值标签字段（仿输入框容器，承载一组 chip）
Element::checkbox("启用", state)                 // state: Signal<bool>
//  .danger() / .accent(color) / .accent_role(Role::X)   勾选强调色：危险红 / 自定义（浅底对勾自动转深）
//  .on_toggle(|ctx| ...)        受控点击拦截：不自动翻转，交 app 决定（见 §8.1）
Element::switch(state)                            // state: Signal<bool>
Element::radio("选项", group, index)             // group: Signal<usize>
Element::slider(value)                            // value: Signal<f32> (0..=1)
Element::dropdown(vec!["A", "B"], selected)       // selected: Signal<usize>
Element::dropdown_signal(options, selected)     // 选项也绑信号：options: Signal<Vec<String>>
Element::dropdown_items(vec![item1, item2], selected)       // 富内容项（副标题/徽章/尾随图标）
Element::dropdown_items_signal(items, selected)           // items: Signal<Vec<DropdownItem>>
Element::check_menu("列表显示", vec![             // 下拉式复选菜单：外观同 dropdown，面板是菜单
    CheckMenuItem::check("隐藏未启用", flag)      //   开关项（flag: Signal<bool>）
        .on_change(|_ctx, v| save(v)),             //   翻转后通知（收到新值，默认翻转已执行）
    CheckMenuItem::separator(),
    CheckMenuItem::action("恢复默认", |_ctx| {}),  //   动作项：点了执行并关闭
]).summary(|on| format!("显示 ({})", on.len()))   // 收起态文案（默认恒为标题；用摘要建议配 .width）
//  默认点击即关（同普通菜单）；.stay_open() 改为开关点了不关、可连点，点面板外才收起
Element::stepper(value, min, max, step)           // value: Signal<f64>；min/max/step: f64
Element::list(vec!["行1", "行2"], selected)       // selected: Signal<usize>
Element::list_pill(vec!["方案", "外观"], selected)         // 同 list，选中为内缩圆角 pill（侧栏导航）
Element::list_icons(vec![("收件箱", icon), ..], selected)  // 带前置图标的行（icon: ImageContent）
Element::progress(value)                          // value: Signal<f32> (确定进度)
Element::progress_indeterminate()                 // 不确定进度（忙碌动画）
Element::label_signal(text)                           // 动态标签：text: Signal<String>，信号变即刷新
```

### 动态文案（文案跟随状态变化）

切换类按钮（播放/暂停、展开/收起、隐藏已完成/显示全部）需要按钮上的字随状态翻转。
把 `Signal<String>` 直接传给文案参数即可，**不需要**另找构造器：

```rust
let caption = signal(String::from("隐藏已完成"));
let hide = signal(false);

Element::button(caption).on_click(move |_| {          // 传信号，不是字符串
    let next = !hide.get();
    hide.set(next);
    caption.set(String::from(if next { "显示全部" } else { "隐藏已完成" }));
});
```

- **适用范围**：`label` / `button` / `link` / `badge` / `badge_intent` / `checkbox` /
  `radio` / `nav_row` / `icon_button`（图标按钮传的是字形，绑信号即"图标随状态换"，
  如 `▶` ↔ `⏸`）。签名里写 `impl Into<TextContent>` 的参数都算。
- **宽度会跟着变**：文案在每次 measure 时现取，改了信号下一帧就重新测量，按钮/链接
  的宽度、链接下划线的长度都随之更新——不是只换个字然后被旧尺寸裁掉。
- **绑错类型编译不过**：`Element::button(signal(0i32))` 直接是编译错误（`Signal<i32>`
  没有 `Into<TextContent>`）。这比本库其它修饰符的 `debug_assert` 运行期守卫更早拦下。
  要显示数字就自己 `format!` 进一个 `Signal<String>`。
- **`Element::label_signal(sig)` 仍在**，等价于 `Element::label(sig)`——它比 `TextContent`
  出现得早，保留不动。新代码两种写法都行。

> **一个反例**：`dropdown` / `list` / `tabs` 的选项是 `Vec<impl Into<String>>`，**不**支持
> 逐条绑信号。整组内容会变的场景用 `list_signal` / `dropdown_signal`（§6.5）——那是重建
> 子树的问题，不是换一段文字的问题。

### 导航 / 分组

```rust
Element::segmented(vec!["亮", "暗", "跟随"], selected)   // 连体多段单选（selected: Signal<usize>）
                                                         //   语义同 radio 组，外观更紧凑；聚焦后左右键移动
Element::nav_row("键盘设置").on_click(|ctx| { /* 钻入子页 */ })  // 左标签 + 右 chevron 的导航行，无持久选中态
Element::collapsible("高级选项", expanded, body)         // 可折叠分组（expanded: Signal<bool>）
                                                         //   body 经 visible_when 显隐，收起时不占布局
Element::accordion(selected, vec![("面板一", body1), ("面板二", body2)])
//   手风琴（单开互斥）：selected: Signal<Option<usize>>，None = 全收起，初值即默认展开项
Element::accordion_multi(vec![("面板一", body1), ("面板二", body2)])
//   手风琴（多开）：各面板独立展开，初始全部收起，无需外部状态
```

### 富文本

```rust
Element::rich(
    RichDoc::new()
        .style("headword", SpanStyle::new().size(26.0).bold())
        .para(Para::new().styled("headword", "apple").text("  n. 苹果"))
        .section("例句", collapsed, |s| s.para("An apple a day…")),   // collapsed: Signal<bool>
)
    .on_span_click(|ctx, id| { /* 点了标了 id 的 span（词典交叉引用跳转） */ })
    .copy_menu(false)      // 关掉内建的右键「复制全部」（要挂自定义 on_context_menu 时先关）
Element::rich_signal(doc)      // 动态富文本：doc: Signal<RichDoc>，整篇换文档（词典切词条）
```
`rich` 是**单个自绘节点**，内部按 span 排版并做基线对齐、折叠段带高度动画。
`on_span_click` / `copy_menu` 是 rich 专属修饰符（误用检测同 text_input）。

### 文本输入
```rust
Element::text_input(text, "占位符")               // text: Signal<String>
    .password()        // 密码遮蔽（仅对 text_input 有效）
    .multiline()       // 多行
    .wrap(true)        // 多行时是否自动折行（默认 true）
    .leading_icon('\u{1F50D}')  // 前置图标字形（搜索框等）：左侧留图标区，文字/光标/命中相应右移
```
> 文本框支持输入 emoji 等补充平面字符（自动拼接 UTF-16 代理对），并以整个 emoji 为单位编辑（光标移动、删除按字符走）；emoji 彩色显示。
> ⚠️ `.password()` / `.multiline()` / `.wrap()` 是 **text_input 专属**。本库用单一 `Element` 类型承载所有控件（统一链式是核心一致性），故这几个修饰符链到别的控件**不会编译报错**；但 **debug 构建下会 `panic` 报错提示**误用，release 下静默忽略（无类型分裂代价）。

### 图片
```rust
Element::image("logo.png")                        // 文件路径（按字节嗅探格式）
Element::image_bytes(include_bytes!("logo.png"))  // 嵌入字节
Element::image_rgba(w, h, &rgba)                   // 原始非预乘 RGBA8（len==w*h*4）
    .fit(Fit::Cover)   // Contain（默认）/ Cover / Fill / None
    .corner(8.0)       // 圆角裁剪：复用 Style.corner_radius，与背景/边框同源圆角
```
- **加载失败不 panic**：显示淡灰占位框（错误可见）；需严格处理可直接用 `Image::from_*` 拿 `Result`。
- **`.fit()` 是图片专属**修饰符（误用检测同 text_input）。圆角直接用通用 `.corner()`，无需新方法。
- **可嵌入其它控件**：图片能力下沉为 `ImageContent` 内容原语，控件持有它即可长出图片。例如按钮图标：
  ```rust
  Element::button("新建").icon_bytes(include_bytes!("plus.png"))  // 或 .icon_file(path) / .icon_rgba(w,h,&rgba)
  Element::button("提交").icon_file(path).enabled_signal(can_submit)  // 禁用时背景/图标/文字一起置灰
  Element::button("删除").icon_file(path).disabled(true)          // 静态禁用
  ```

### 图片的状态处理
图片原语与控件**状态解耦**：控件把自身状态映射成通用 `VisualState`（Normal/Hover/Pressed/Selected/Disabled）传给图片，原语据此调制。三种手段（可组合）：
- **调制**：按状态调不透明度——禁用自动置灰（`VisualState::opacity`）。
- **着色**：`.tint(color)` 把**单色图标**按颜色重着色（随主题/状态变色，用 alpha 作模板），结果按层缓存，不影响彩色图。
- **换图**：`ImageContent::on_state(state, image)` 为特定状态备专图，命中用专图、否则回退基图。
```rust
// 高级用法：预组装内容原语，再交给控件
// on_state 的第二参是 Image（不是字节），故先解码；ImageContent 未实现 Clone，
// 一个实例只能交给一个控件——要两处用就组装两份。
let icon = ImageContent::from_bytes(base)
    .tint(Color::WHITE)
    .on_state(VisualState::Disabled, Image::from_bytes(gray_png)?);
Element::button("X").icon_content(icon);
```

也可以不经按钮、直接作独立控件：

```rust
let pic = ImageContent::from_bytes(base).tint(Color::WHITE);
Element::image_content(pic);
```
> **禁用是核心级通用能力**：`.enabled(bool)` / `.enabled_signal(Signal<bool>)` / `.enabled_when(|| ...)` / `.disabled(bool)`（= `enabled(!v)`）可用于**任意控件或容器**。启用轴与可见轴形态一一对应（`visible` / `visible_signal` / `visible_when`），三形态可叠加、取与。核心统一拦事件、跳 Tab，并把启用态传入控件 paint 令其置灰；**禁用沿父链继承**——禁用一个容器即禁用其全部子节点（适合按条件禁用整个表单区）。各表单控件（Button/CheckBox/Switch/RadioButton/Slider/Dropdown/Stepper/TextInput）均已实现置灰。

> **格式扩展**：核心仅内置 PNG（零依赖）。需要 JPEG/WebP 等时，实现 `ImageDecoder` trait 并 `windui::render::image::register_decoder(...)` 注册；`Element::image*` 会按魔数自动分发，核心代码与 API 零改动。

### 链接
```rust
Element::link("打开官网").url("https://example.com")  // 点击用系统默认程序打开
Element::link("自定义").on_click(|_| { /* ... */ })    // 自定义动作（与 url 并存时回调优先）
    .underline(false)                                  // 关闭下划线（默认开）
Element::link("禁用").url("...").disabled(true)        // 核心级禁用：置灰 + 不可点 + 不显手型
```
- **链接色 + 下划线**文本，hover/press 三态（取主题 `link` 覆盖层，回退 accent 家族），点击或回车/空格激活。
- **悬停手型光标**：链接 `Widget::cursor()` 返回 `CursorShape::Hand`；文本输入返回 `Text`（I 形）。宿主取当前悬停控件的形状交平台应答（win32 `WM_SETCURSOR`），**禁用节点统一回退箭头**。
- **`.url()` / `.underline()` 是 link 专属**修饰符（误用检测同 text_input）；打开 URL 经 `EventCtx::open_url` → 平台 `ShellExecute`，控件层不碰平台。

### 可点击容器（卡片 / 自定义行）
```rust
Element::row()                       // 任意容器（col/row/stack）皆可
    .clickable()                     // 补 hover/press 叠层 + 键盘激活 + 手型光标
    .on_click(|ctx| { /* ... */ })
    .bg(Color::WHITE).corner(12.0).border(border, 1).padding(16)
    .child(icon).child(title_and_desc)   // → 一张可点击卡片
```
- `.clickable()` 把容器的占位 widget 换成可交互 widget，hover/press 用**主题自适应的半透明叠层**（明暗主题均成立），点击经既有 `.on_click()`。
- 仅用于容器（叶子控件如 label/button 上调用会 debug panic）。

### 轻提示 Toast
脱离布局树的**居中浮层**，命令式弹出，自动淡入淡出 + 定时消失。任意控件回调内即可调用：
```rust
Element::button("复制").on_click(|ctx| ctx.toast_ok("已添加到剪贴板"));  // 成功（✓）
//  ctx.toast("已保存")          中性信息（ℹ）
//  ctx.toast_err("操作失败")    错误（✕）
//  ctx.toast_with(text, ToastKind::Success, 1800)   完全指定语义与时长(ms)
```
- 不绑定节点、无需状态：宿主接管渲染、计时与消失（深色面板，不随明暗主题翻转）。
- 主题见 `ToastTheme`（bg/success/error/corner，可 TOML 覆盖）。

### 文件拖放
```rust
Element::col().fill().on_drop_files(move |_ctx, paths| {   // paths: &[PathBuf]
    let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    dropped.set(names.join("\n"));                   // 写信号即自动重绘
})
```
- **任意元素可接收**：`.on_drop_files(f)` 挂到 `.fill()` 根容器即"全窗接收"；落点会路由到落点下的元素，再沿父链冒泡到首个设了回调的节点（禁用子树不接收）。
- 平台经 `WM_DROPFILES` 解出路径 + 落点交宿主路由（`Tree::dispatch_files`）；回调签名 `FnMut(&mut EventCtx, &[PathBuf])`，可读写信号（自动重绘）；改的若是自有的非信号状态，则显式 `ctx.mark_dirty()`。完整示例见 `examples/file_drop.rs`。

### 系统托盘
```rust
// 勾选态与禁用态都绑 Signal<bool>，与 UI 控件同一套状态原语。
let notify_on = signal(true);
App::new("…", w, h).tray(
    Tray::new()
        .tooltip("后台运行中")
        .icon_rgba(16, 16, &rgba)            // 可选；默认用系统应用图标
        .on_left_click(|ctx| ctx.show_window())
        .on_double_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("显示窗口", |ctx| ctx.show_window()),
            TrayMenuItem::separator(),       // 分隔线
            TrayMenuItem::check("启用通知", notify_on, move |ctx| { /* 翻转状态 */ }),  // 勾选项
            TrayMenuItem::item("弹个气泡", |ctx| ctx.notify("你好", "…")).enabled(notify_on),  // 灰显绑同一个信号
            TrayMenuItem::item("退出", |ctx| ctx.quit()),
        ]),
).content(ui).run();
```
- **右键菜单走原生 `TrackPopupMenu`**（真 OS 弹出，显示在托盘旁）；支持**勾选项**（`check` 绑 `Signal<bool>`，弹出时按当前值显示对勾）与**分隔线**。勾选态/禁用态都是**弹出时现读**，所以在别处改信号，下次右键就能看到。托盘构建器因此是 `!Send` 的——信号存储线程局部，`Tray` 只能留在建它的 UI 线程，跨线程搬运编译期即失败。
- 回调拿 `TrayCtx`：`show_window()` / `hide_window()` / `quit()` / `notify(title, body)`（气泡通知）。**拿不到窗口句柄**，理由同下文 `HotkeyCtx`：回调在平台层持有窗口状态借用期间执行，直接调 OS 窗口 API 会同步重入消息处理并造成 `&mut` 别名（`AGENTS.md` 铁律 6）。这几个方法只记录意图，由平台层在借用释放后执行。
- 一个回调内可**按顺序调用多个**，逐条生效（如先 `notify(..)` 再 `show_window()`）。例外是 `quit()`：它之后的调用不再执行（窗口已销毁）。
- `quit()` 是应用的真实出口，刻意**不受 `hide_on_close()` 影响**。
- 图标可 `.icon_rgba(w,h,&rgba)`（零依赖，从 RGBA 造 HICON），未设则用系统默认应用图标。窗口销毁时托盘自动清理。完整示例见 `examples/tray.rs`。

### 全局热键与启动即隐藏

常驻后台小工具的骨架：启动不显示窗口，靠热键随时唤起。

```rust
App::new("查词", 480, 360)
    .tray(Tray::new().on_left_click(|ctx| ctx.show_window()))
    .start_hidden()                                   // 启动不显示窗口，无闪烁
    .hide_on_close()                                  // ESC / × 隐藏而非退出
    .hotkey(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| ctx.show_window())
    .hotkey(Hotkey::new(Key::Char('H')).ctrl().alt(), |ctx| ctx.hide_window())
    .content(ui)
    .run();
```

- **全局**：应用无焦点、窗口隐藏时亦可触发。消息由系统投递到窗口队列，**事件驱动不轮询**，空闲仍是零 CPU。
- 修饰键链式声明：`.ctrl()` / `.alt()` / `.shift()` / `.meta()`（`meta` = Win 键 / macOS Command 键）。键位复用 `Key`：`Key::Char('D')`、`Key::Escape` 等；非 ASCII 字符无稳定虚拟键映射，不可作热键。
- **注册可能失败且不报错**：热键是全局独占资源，组合被其他程序占用时系统会拒绝，该热键静默失效，其余热键与应用不受影响——为一个热键冲突让整个应用起不来是不可接受的。
- 回调拿 `HotkeyCtx`，**只有 `show_window()` / `hide_window()`，拿不到窗口句柄**。这是刻意的：回调在平台层持有窗口状态借用期间执行，直接调 OS 窗口 API 会同步重入消息处理并造成 `&mut` 别名（见 `AGENTS.md` 铁律 6）。窗口操作降级为「意图」由平台层在借用释放后执行。
- 控件回调里用 `EventCtx::show_window()` / `hide_window()`（与 `request_close()` 不同：隐藏只改可见性，关闭会销毁窗口并结束消息循环）。
- `hide_on_close()` 把 **ESC 与标题栏 ×** 都转为隐藏。它**优先级低于既有拦截链**：先关最顶层对话框 → 再问 `on_close_request` → 拦截器放行后才轮到它决定关还是隐。故「有未保存数据时弹提示」与「关闭即隐藏」可并存。真正的退出留给托盘菜单的 `ctx.quit()`。
- `start_hidden()` / `hide_on_close()` 须配合托盘或热键——否则窗口隐藏后永远无法唤起，debug 期对此 panic。

> **平台状态**：全局热键当前**仅 Windows 实现**。macOS 上 `App::hotkey` 在 debug 期 panic、release 期静默忽略；托盘、`start_hidden`、窗口显隐在两平台均可用。macOS 热键需 Carbon `RegisterEventHotKey`，见 `src/platform/macos/hotkey.rs`。

完整示例见 `examples/hotkey.rs`。

### 无标题栏窗口（自定义标题栏）
```rust
let title_bar = Element::row().width_match().height(36).cross(Align::Stretch)
    .bg(Color::hex(0x2D3436))
    .window_drag()                                   // 整条可拖（落在按钮上不拖）
    .child(Element::label("  我的应用").fg(Color::WHITE).weight(1.0))
    .child(Element::window_button(WindowButtonKind::Minimize).fg(Color::WHITE))
    .child(Element::window_button(WindowButtonKind::Maximize).fg(Color::WHITE))
    .child(Element::window_button(WindowButtonKind::Close).fg(Color::WHITE));
App::new("…", w, h).frameless().content(Element::col().fill().child(title_bar).child(body.weight(1.0))).run();
```
- `App::frameless()` 去掉系统标题栏，客户区铺满整窗，**保留 Aero 吸附/缩放/投影**（WM_NCCALCSIZE + WS_THICKFRAME + DwmExtendFrameIntoClientArea）。
- `Element::window_drag()` 标记拖动区（自定义标题栏）：命中非交互区拖窗、命中可聚焦控件（按钮/输入）则不拖、交控件处理。
- `Element::window_button(WindowButtonKind::{Minimize,Maximize,Close})`：自绘标准图标 + hover/press（关闭键 hover 转红）；图标色取 `.fg()`（深色标题栏用 `.fg(WHITE)`）。点击调 `EventCtx::minimize()/toggle_maximize()/request_close()`。
- 窗口四边/四角自动可缩放（平台在边缘 N px 内做缩放命中）。完整示例见 `examples/frameless.rs`。
- **窗口圆角跟随系统**：Win11 上显式声明 `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND`，与系统其余窗口一致。显式声明而非依赖 DWM 默认策略——自定义 `WM_NCCALCSIZE` 之后默认行为是否仍成立并无明确保证。Win10 上 DWM 不认识该属性、返回错误码，windui 忽略该错误，故无需版本判断。圆角半径由系统决定。macOS 上 AppKit 对 `FullSizeContentView` 窗口自动保持圆角，无需额外处理。

### GPU 加速渲染（Windows，可选）
```rust
App::new("…", w, h).accelerated(true).content(ui).run();
```
- `App::accelerated(true)` 在 Windows 上 opt-in **Direct2D GPU 后端**：几何/渐变/阴影/文字光栅走 GPU，适合大窗口/多控件下降低软件光栅的逐像素填色开销。**默认关闭**（软渲染）。
- 文字仍走 **DirectWrite**（系统字体缓存、ClearType 字形），与软路径字体/字重一致。
- 自动回退软渲染（绝不 panic）：RDP 远程会话、无可用 GPU、设备创建失败、离屏截图（`--screenshot`）。
- v1 仅适用**不透明窗口**；透明/分层窗口仍走软渲染。示例对比：`cargo run --release --example ime -- --accelerated`。

---

## 6. 布局系统

### 6.1 容器与主轴
- `col` / `row` 是线性容器，沿**主轴**堆叠子节点；`stack` 层叠。
- `spacing(n)`：子节点间距。`cross(Align)`：交叉轴对齐。

### 6.2 尺寸
```rust
.width(px) / .height(px)        // 固定像素
.size(w, h)                     // = width + height
.width_match() / .height_match()// 撑满父容器对应轴
.fill()                         // = width_match + height_match
.weight(f)                      // 主轴按权重瓜分剩余空间（类似 flex-grow）
```
尺寸语义由 `Dimension` 表达（`Fixed` / `Match` / `Weight`）。`weight` 仅在线性容器主轴有意义。

> ⚠️ **陷阱一：横向占剩余空间用 `.weight(n)`，不要用 `.width_match()` / `.fill()`。**
> 在 `row` 里，`width_match` 的语义是"取父容器的宽"，它**不知道**兄弟节点已经占掉了多少——
> 侧栏 240px + 正文 `width_match` 会算出 240 + 全宽，直接溢出父宽。`weight` 才是"瓜分剩余"。
> 同理，`col` 里纵向占剩余高度也用 `weight`。
> ```rust
> Element::row().fill()
>     .child(sidebar.width(240))
>     .child(content.weight(1.0))   // ✅ 占掉侧栏之外的全部宽度
> //  .child(content.fill())        // ❌ 溢出：会再要一整个父宽
> ```
> 见 `examples/settings.rs` 的侧栏 + 正文布局。
>
> ⚠️ **陷阱二：`Label` 不要手写 `.height(N)`。**
> Label 的 `measure` 会在可用宽度内换行并算出实际内容高度，写死高度等于给它一个**上界**——
> 长文案、多行文案会被静默截断（不报错、不省略号，就是看不见）。想让标题在容器宽度内自动
> 换行，给宽度约束（`.width_match()` / `.weight(1.0)`）即可，高度交给它自己。
> 只有确定是单行短文本、且要与兄弟对齐基线时，固定高度才有意义。
>
> ⚠️ **陷阱三：拖动手柄不能是 `clickable()` 容器的后代。**
> `reorder_list` / `reorder_list_signal` 的手柄靠事件冒泡到列表控件；而 `clickable()` 容器
> 会消费 `Down`/`Up`，冒泡到它那里就断了，手柄直接拖不动。整行可点的列表请把手柄放进
> `stack` 里当**同级覆盖层**，与可点行并列而非嵌套。

### 6.3 间距
```rust
.padding(n) / .padding_xy(h, v)   // 内边距
.margin(n)  / .margin_xy(h, v)    // 外边距
.align(Align)                     // 自身在父交叉轴的对齐
```
`Align`：`Start / Center / End / Stretch`。

### 6.4 滚动与触摸
`Element::scroll()` 内的内容超出视口时可滚动。已内建：
- 鼠标滚轮、拖拽滚动条
- **触摸**：直接手指滑动、松手惯性滑行、撞界轻微回弹（见 DESIGN.md / 跨平台缝合）

第三方无需做任何事，把可滚内容放进 `scroll()` 即可。

### 6.5 动态列表（数据驱动的子树重建）

行数会变的列表——搜索结果、过滤后的任务、异步加载到的记录——**不要**在回调里手工增删节点。
windui 的做法是：把整份数据放进一个 `Signal<Vec<T>>`，UI 声明"这段子树由这个信号生成"，
然后每次变化只做一件事：`set` 新的 `Vec`。

```rust
use windui::prelude::*;

#[derive(Clone)]
struct Task { name: String, done: bool }

fn main() {
    let all = vec![
        Task { name: "修复登录崩溃".into(), done: false },
        Task { name: "撰写发布说明".into(), done: true },
    ];
    let hide_done = signal(false);
    let tasks = signal(all.clone());        // 视图数据信号

    let filter_btn = Element::button("隐藏已完成").on_click(move |_| {
        let hide = !hide_done.get();
        hide_done.set(hide);
        // 重算整份视图数据后整体写回——列表自己会跟上
        let mut v = all.clone();
        if hide { v.retain(|t| !t.done); }
        tasks.set(v);
    });

    let list = Element::list_signal(
        tasks,
        |t: &Task| t.name.clone(),          // key_fn：预留给后续 diff 优化，现在随便给
        |t: Task| Element::label(t.name).width_match().padding(8),
    );

    let ui = Element::col().fill().padding(16).spacing(10)
        .child(filter_btn)
        .child(list.weight(1.0));

    App::new("任务", 420, 400).content(ui).run();
}
```

**机制**：`list_signal` 建出的节点被标记为**响应式**（`Element::reactive()` 做的就是这个标记），
框架在每次 layout 之前，对所有已注册的响应式节点调一次 `Widget::on_update`。控件在那里比对
绑定信号的 `version()`（每次 `set`/`update` 自增）与自己缓存的版本号：不等就清空旧子节点、
用 `row_fn` 重建一批新的。所以你只需保证"信号里的 `Vec` 就是当前该显示的内容"，重建时机
不用管。

当前实现是**全量重建**，不做 keyed diff（`key_fn` 是给后续优化预留的参数，传
`|_| ()` 也合法）。因此行内控件的临时状态（未提交的输入、悬停）会随重建丢失——需要保留的
状态请放进信号里，由数据携带。

这一族 API 一览：

| API | 容器形态 | 用途 |
|---|---|---|
| `Element::list_signal(data, key_fn, row_fn)` | **滚动**容器 | 行数会变的长列表 |
| `Element::host_signal(data, build_fn)` | 普通 `col` | 整段结构随状态重建（如列集随类别切换的表格） |
| `Element::reorder_list_signal(data, row_fn)` | `col` + 拖动手柄 | 顺序真相源在信号里的可拖拽排序列表 |
| `Element::dropdown_signal(options, selected)` | 下拉 | 选项列表异步到达 |
| `Element::label_signal(sig)` / `rich_signal(doc)` | 叶子 | 单个文本/文档跟随信号 |
| 文案参数直接传 `Signal<String>` | 任意文本控件 | 单条文案跟随状态（按钮/链接/徽章…，见 §5「动态文案」） |
| `Element::reactive()` | 任意 | 自定义控件手动接入（须自行实现 `on_update`） |

> **`list_signal` 还是 `host_signal`？** 前者内部是 `scroll`，按**无限高度**测量子元素——
> 如果重建出来的子树里有靠 `weight` 占剩余高度的东西（典型是表格正文），高度会崩塌成 0。
> 这种"内容自带滚动或不需要滚动"的场景用 `host_signal`，它是普通 `col`，`weight`/`fill`
> 能拿到确定高度。
>
> `reorder_list_signal` 的 `row_fn` 签名是 `Fn(T, Element) -> Element`：第二个参数是框架给的
> **拖动手柄**，你**必须**把它放进返回的元素树里，否则该行拖不动（另见 §6.2 陷阱三）。

完整可运行示例：`examples/dyn_list.rs`（排序 + 过滤）、`examples/fullshowcase.rs` 的排序页。

---

## 7. 样式与主题

两条路径，**按层级选择**：

### 7.1 内联 `Style` 修饰符（单点覆盖）
直接挂在 Element 上，只影响该节点：
```rust
Element::label("标题")
    .fg(Color::hex(0x1A1A2E))     // 文字色
    .font_size(22.0)
    .font_weight(600)             // 400=常规 500=中 600=半粗 700=粗
    .font_family("Newsreader")    // 字体族名；未设=系统默认
    .bg(Color::WHITE)
    .border(Color::hex(0xDDDDDD), 1)
    .corner(8.0)
    .text_align(Align::Center)
```
`Color` 构造：`Color::rgb(r,g,b)`、`rgba(..)`、`hex(0xRRGGBB)`、`from_hex_str("#7C5CFC")`，常量 `WHITE/BLACK/TRANSPARENT`。

> **样式不沿父链继承**：`font_family` / `font_size` / `fg` 等都只作用于所设的那个节点，给容器设不会传给子节点。沿父链继承的只有 `enabled`、光标形状与 `window_drag` 三项。需要统一字体时自行封装构造函数，或走 `Theme`。
>
> （交叉轴对齐是另一回事：子节点未显式设 `align` 时取**其直接容器**的交叉轴对齐，这是容器对子项的排布，只有一层，不会穿透多层祖先。）
>
> `font_family` 指定的字体**未安装时不报错也不 panic**：Windows 的 DirectWrite 与 macOS 的 CoreText 均静默回退系统默认字体——字体是否存在取决于用户机器，调用方无从保证；需要确保效果应随程序分发字体。
>
> **平台状态**：`font_family` 两平台均生效。`font_weight` **当前仅 Windows 生效**——macOS 的 CoreText 路径尚未接入字重（`src/text/coretext.rs` 构造 `CTFont` 时不传 traits），传入非 400 的值不报错，但没有视觉变化。

### 7.2 `Theme`（全局 + 每控件覆盖层）
控件默认视觉**不从内联 Style 取**，而从当前 `Theme` 取。`Theme` 两层：
- `palette`（`Palette`）：accent / bg / surface / text / border … 全局色板。
- `metrics`（`Metrics`）：圆角、边框宽、间距、字号等度量。
- 每控件覆盖层：`button` / `input` / `toggle` / `dropdown` / `menu` / `tab` / `progress` / `stepper` / `list` / `form` / `card` / …，每个字段是 `Option<_>`，`None` 时回退到 palette / metrics。

其中 `form`（[表单脚手架](#表单脚手架)的行高、标签列宽、间距、标签字号字重、副标题字号）
与 `card`（卡片圆角、内边距、标题字号）承载的是**尺寸**而非颜色。这些量刻意不进构造器签名：
一个应用里的表单行必须整齐划一，逐行传尺寸只会让每处各写一个近似值，最终对不齐。
`examples/ime.rs`、`examples/settings.rs`、`examples/ime_settings.rs` 各演示了一套自定义表单度量。

注入主题：
```rust
let mut theme = Theme::default();
theme.palette.accent = Color::hex(0x7C5CFC);
theme.button.bg = Some(Color::hex(0x7C5CFC));   // 仅覆盖按钮背景

App::new("App", 480, 360)
    .theme(theme)        // 注入；控件 paint 时读取
    .content(ui)
    .run();
```

TOML 互转（做可配置主题）：
```rust
let theme = Theme::from_toml(toml_str)?;   // partial 字段自动回退默认
let s = theme.to_toml()?;
```

**选择原则**：成体系的视觉（品牌色、统一圆角）走 `Theme`；个别节点的一次性微调走内联 `Style` 修饰符。

> ⚠️ **主题要在建树之前装好。** 颜色走 `Role` 的部分是 paint 期解析、随时跟得上；但
> **尺寸**（`form` 的行高、`card` 的圆角、`tag_field`/`accordion` 的圆角）以及 `badge`/`chip`
> 的配色是在 `Element` **构造那一刻**读主题的。`App::theme(t)` 会当场把主题装进当前线程，
> 所以链式写法天然正确：
>
> ```rust
> App::new("App", 480, 360).theme(theme).content(build_ui()).run();
> //                        ^^^^^^^^^^^^ 先执行  ^^^^^^^^^^^^ 后求值，读得到 theme
> ```
>
> 但如果你**先把树建进变量再传**，顺序就反了，自定义度量会静默失效（编译通过、不报错、
> 只是没生效）。这时把建树挪到 `App::theme(..)` 之后即可：
>
> ```rust
> let app = App::new("App", 480, 360).theme(theme);   // 先装主题
> let ui = build_ui();                                // 再建树
> app.content(ui).run();
> ```

### 7.3 运行期换主题

`App::theme(t)` 是**启动期一次性**注入。要在窗口运行中切换（暗色开关、用户选主题），
用 `App::theme_handle()` 取一个句柄，克隆进回调即可：

```rust
use windui::prelude::*;

fn main() {
    let dark = signal(false);

    let mut app = App::new("设置", 480, 360);
    let theme = app.theme_handle();          // &mut self，须在 content/run 之前取（见 §2.1）

    let toggle = {
        let th = theme.clone();              // ThemeHandle 是 Clone（不是 Copy），每个闭包一份
        Element::button("切换暗色").neutral().on_click(move |_| {
            let on = !dark.get();
            dark.set(on);
            th.set(if on { Theme::dark() } else { Theme::default() });
        })
    };

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .bg_role(Role::Bg)                   // ← 用角色而非固定色，换主题才会跟随
        .child(Element::label("外观").font_size(20.0).fg_role(Role::Text))
        .child(toggle);

    app.content(ui).run();
}
```

`ThemeHandle` 三个方法：

```rust
theme.set(Theme::dark());                              // 整体替换
theme.update(|t| t.palette.accent = Color::hex(0x2E9E5B));  // 就地改一处（快照→改→写回）
let snapshot: std::rc::Rc<Theme> = theme.current();    // 读当前主题
```

三者都会请求重绘，下一帧整树跟随。

**关键：想跟随主题的颜色必须用 `Role` 表达。** 控件内建视觉（按钮底色、输入框边框等）
在 paint 期读 `theme::current()`，天然跟随；但你自己写在节点上的 `.bg(Color::hex(..))`
是**定格色**，换主题不会动。要跟随就改用角色修饰符：

```rust
.fg_role(Role::Text)                  // 文字色
.bg_role(Role::Surface)               // 背景色
.bg_role_alpha(Role::Accent, 0.12)    // 角色色 + 透明度（做淡底强调块，明暗主题都成立）
.border_role(Role::Border, 1)         // 边框色 + 宽度
```

`Role` 枚举（`windui::style::Role`，prelude 已导出）：

| 分组 | 角色 |
| --- | --- |
| 表面 | `Bg` / `Surface` / `SurfaceAlt` / `SurfaceInverse` / `OnSurfaceInverse` |
| 文字 | `Text` / `TextMuted` / `TextSubtle` / `TextDisabled` / `Placeholder` |
| 线条 | `Border` / `Divider` / `Track` |
| 强调 | `Accent` / `AccentHover` / `AccentActive` / `OnAccent` |
| 语义 | `Danger` / `Success` / `Warning` |
| 控件专属（带覆盖层回退） | `AccordionBorder` / `AccordionHeaderBg` / `InputBg` / `InputBorder` |

角色在 **paint 期**解析成具体颜色——这正是它能跟随换主题的原因。

`Role` 与 `Intent` 都是 `#[non_exhaustive]`：语义色是会持续补齐的一组（本版就各加了
五个和两个），标注之后再补就不是破坏性变更了。代价是你对它们做 `match` 必须留一条
`_ =>` 兜底分支。

几个容易选错的：

- **`TextSubtle`** 是比 `TextMuted` 更弱的第三档正文（版权行、脚注、时间戳）。它**不是**
  `TextDisabled`（那表示不可交互）、也**不是** `Placeholder`（那表示待填写）——这两个借来用，
  语义会骗人。四档的强弱顺序由单元测试锁住。
- **`SurfaceInverse` / `OnSurfaceInverse`** 是与当前主题**明暗相反**的实底条块及其前景
  （亮色主题下是深色横幅，暗色主题下就翻成浅色）。如果你要的是「不论主题都恒为深色」的
  标题栏（不少工具软件如此），那是一个固定设计而非角色，直接写死颜色更诚实——
  `examples/frameless.rs`、`examples/light_titlebar.rs` 即刻意如此。
- **`Success` / `Warning`** 与 `Danger` 同族，都取自 palette 的语义色槽；控件级用法见
  `Intent::Success` / `Intent::Warning`（同一个色槽的另一条访问路径，不是第二套体系）。
  取值刻意保证对表面 ≥ 3:1，因为语义色经常直接当**前景**用（状态文字、标签边框），
  饱和亮黄当 warning 会糊得看不清。
- **内置意图之外的基色**走 `Intent::Custom(Color)` / `Intent::CustomRole(Role)`，框架据此派生
  整组视觉（hover 变亮、active 变暗、前景按亮度自适应）。两者只差基色何时确定：`Custom` 是
  构建期给的**定色**，换主题不动；`CustomRole` 延迟到 paint 期从当前主题取角色，故跟随换主题。
  控件上对应 `.accent(color)` 与 `.accent_role(role)`——与 `fg` / `fg_role` 是同一套成对约定。

> ⚠️ 反过来说：**构建期不要取色**。像 `let c = theme::current().palette.text;` 然后
> `.fg(c)` 这样，取到的是构建那一刻的颜色，换主题后不会更新。自己封装组合控件时尤其
> 容易犯——把 `Role` 一路传下去，别在中途解析成 `Color`。
>
> 内置的 `Theme::default()`（亮）与 `Theme::dark()`（暗）可直接用；自定义主题走
> `Theme::from_toml(..)`，同样能在运行期 `set` 进去（见 `examples/theming.rs`）。

完整示例：`examples/fullshowcase.rs`（右上角"暗色/亮色"按钮）、`examples/theming.rs`。

### 7.4 图标字体（私用区回退字体）

7.1 提到 `font_family` 在字体未安装时会静默回退——图标字体正是最容易撞上这条的场景。
注册一个私用区回退字体可以绕开安装：

```rust
// 须在 App::run() 之前调用
windui::text::register_private_use_font("assets/fa-solid-900.ttf", "Font Awesome 6 Free")?;

// 之后图标码位就是普通文字，与文本同流布局、随字号缩放
Element::label("\u{f015} 首页").font_size(18.0)
```

注册后，文本里落在 **Unicode 私用区**的码位改用该字体渲染，其余字符不受影响——所以图标可以
和文字混在同一个 `label` 里，不必拆成两个节点或另做图片资源。

- 字体**不需要安装到系统**（安装要管理员权限，还会污染用户字体列表），直接把 `.ttf` 随包分发即可。
- `family` 参数是字体文件**内部的家族名**，不是文件名——双击字体文件即可在预览窗口顶部看到。
  写错的表现是图标仍为方框。
- 三段私用区（BMP `U+E000..=U+F8FF`、补充私用区 A/B `U+F0000..=U+10FFFD`）全部支持，
  图标集用哪一段都行。
- 运行期换字体用 `DWriteEngine::set_private_use_font`；`register_*` 只在引擎创建时读取一次。

> **平台状态**：**当前仅 Windows**。macOS 的 CoreText 路径尚未接入（`src/text/coretext.rs`），
> 该函数在 macOS 上不存在（`#[cfg(windows)]`）——是编译期缺失而非静默失效，跨平台代码需自行
> `cfg` 分支。

---

## 8. 事件与交互

### 8.1 点击回调
```rust
Element::button("保存").on_click(|ctx: &mut EventCtx| {
    // ctx 提供与框架交互的能力
    ctx.request_close();      // 关窗
});
```
**回调签名的四条规矩**（全库一致，看一个就会用其余的）：

1. **`&mut EventCtx` 恒为第一参数**。它是"能力袋"不是数据，位置直觉同 `&mut self`；
   固定在首位后，后面的参数才是这个回调真正关心的数据——
   `on_span_click(|ctx, id| ..)`、`on_reorder(|ctx, from, to| ..)`、`on_row_activate(|ctx, row| ..)`。
2. **一次性动作回调是 `FnMut`**：`on_click` / `on_toggle` / `on_row_activate` / `on_reorder` /
   `on_sort` / `on_edit` / `on_drop_files` / `on_span_click`，闭包里可以改捕获的状态。
3. **`Fn` 只用于要被反复调用或留存多份的闭包**，且每处都在文档里写明理由：
   `visible_when` / `enabled_when`（每帧求值的纯谓词）、`summary` / `actions` / `cell_render` /
   `on_context_menu` 的 `build`（生成器）、`MenuItem::run` 的动作（项会被克隆进浮层各级面板）。
4. **每个回调都拿得到 `ctx`**——包括菜单项动作（见 §8.2）。库里没有"这个回调能弹对话框、
   那个不能"的分层。生成器是例外，它产出的是数据，不响应事件。

**受控复选框 `on_toggle`**：CheckBox 默认点击即翻转绑定的 `state`。需在翻转前介入（如弹确认对话框）时用 `.on_toggle(cb)`——设置后点击/键盘激活**不再自动翻转** `state`，改调回调，由你决定是否 `state.set(..)`。渲染始终跟随 `state` 当前值，确认前框不会勾上、零闪烁。
```rust
Element::checkbox("删除数据", state).on_toggle(move |_ctx| {
    if confirm() { state.set(true); }   // 确认后才置真；否则保持不变
})
```

### 8.2 上下文菜单
文本输入已内建右键菜单（剪切/复制/粘贴/全选）。自定义控件可在 `on_event` 里：
```rust
ctx.show_context_menu(pos, vec![
    MenuItem::run("操作", |ctx| { /* ... */ }, false),
]);
```
菜单项两种动作：`MenuItem::run(label, closure, checked)` 跑闭包；`MenuItem::key(label, key_event, enabled)` 向焦点控件合成按键。

`MenuItem` 是 `#[non_exhaustive]` 的：**只能**经 `run` / `key` / `separator` / `submenu`
四个构造器建，再链设置器改属性（`icon` / `shortcut` / `check` / `subtitle` / `badge` /
`trailing_icon` / `trailing_icon_display` / `stay_open` / `enabled` / `intent` / `danger`），
字面量 `MenuItem { .. }` 不再可用。字段读取不受影响。菜单项的可选修饰只会越来越多，
封住字面量这条路，日后加字段才不必每次都破坏下游。
动作闭包收 `&mut EventCtx`（宿主在浮层里借给它），与 `on_click` 同形——`ctx.toast(..)`、
`ctx.defer_blocking(..)`、`ctx.request_close()` 都能用。它是 `Fn` 不是 `FnMut`：项会被克隆进
浮层的每一级面板、粘滞项还要重建后再执行同一份动作，要改状态请用 `Signal`。

任意元素/容器挂菜单用 `Element::on_context_menu(build)`（命中沿父链冒泡到首个设了回调的节点）；
**表格数据行**用 `Element::on_row_context_menu(|行下标| items)`——行是控件内部构建的，应用拿不到行元素：
```rust
Element::table_sortable_server(cols, rows, sort, on_sort)
    .on_row_context_menu(move |disp| vec![
        MenuItem::run("编辑…", move |_ctx| open_edit(disp), false),
        MenuItem::separator(),
        MenuItem::run("删除", move |_ctx| del(disp), false),
    ])
```
`table_sortable` / `table_sortable_server` / `table_selectable` 三类都支持（右键与首列复选框不冲突）。
菜单项**每次右击现取现建**，`check` / `enabled` 因而总反映右击当刻的数据。

⚠ 一个坑：`on_context_menu` 会让节点**吞命中**（同 `on_drop`/`tooltip`）。挂到原本透明的
纯布局容器上，它会开始拦截指针事件、遮住其下内容——挂在已吞命中的节点（有背景/`clickable()`/
表格行）上。

`on_context_menu` / `on_row_context_menu` 的 `build` 参数是**生成器**（每次右击、以及粘滞项
点击后重跑一遍产出项），不是事件回调：它不收 `ctx`，且必须是 `Fn`。要在菜单里做事的是各项的
**动作**，那里有 `ctx`。

### 8.3 焦点与键盘
- Tab / Shift+Tab 在 `focusable()` 控件间导航（框架自动维护焦点环）。
- 自定义控件实现 `Widget::focusable() -> true` 即加入导航链。

### 8.4 右键约定
**右键默认不触发控件**（桌面习惯）。框架在分发层拦截非左键的 Down/Up；仅需右键的控件 override `Widget::wants_right_click() -> true`。新控件**默认即正确**。

### 8.5 跨线程更新

**信号只能在 UI 线程使用。** `Signal<T>` 的存储是线程局部的，句柄刻意实现为 `!Send`——
把它 `move` 进 `std::thread::spawn` 是**编译错误**，不是运行期静默丢值：

```rust
let s = signal(1i32);
std::thread::spawn(move || s.set(42));   // ❌ 编译失败：Signal 不是 Send
```

正确做法是让后台线程发**消息**，回到 UI 线程再写信号。两种机制：

**`App::channel`**：建立类型化消息通道，签名
`channel<Msg: Send + 'static>(&mut self, on_message: impl FnMut(Msg) + 'static) -> Sender<Msg>`。
`on_message` 回调在 UI 线程执行，可安全写信号；返回的 `Sender` 是 `Send + Sync + Clone`，可克隆
到任意后台线程，`send` 一次即唤醒 UI 渲染一帧。无消息时不唤醒（事件驱动、空闲零 CPU）。

**`App::on_interval`**：注册 UI 线程定时回调（`on_interval(Duration, impl FnMut() + 'static)`），
间隔内不占 CPU（平台定时器驱动）。可多次调用注册多个定时器。

注意 `channel` 是 `&mut self`，须在 `content`/`run` 之前调用（见 §2.1 两段式写法）。

```rust
use std::time::Duration;
use windui::prelude::*;

fn main() {
    let progress = signal(0.0f32);
    let clock = signal(String::from("已运行 0 秒"));
    let ticks = signal(0u32);

    let mut app = App::new("后台任务", 360, 180);

    // 后台线程只持有 Sender（Send）；on_message 在 UI 线程写信号
    let tx = app.channel::<f32>(move |p| progress.set(p));
    std::thread::spawn(move || {
        for i in 1..=100 {
            std::thread::sleep(Duration::from_millis(40));
            if tx.send(i as f32 / 100.0).is_err() {
                break;                       // 窗口已关，通道断开
            }
        }
    });

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(Element::progress(progress).width_match())
        .child(Element::label_signal(clock).width_match());

    // on_interval 的回调也在 UI 线程，可直接写信号
    app.on_interval(Duration::from_secs(1), move || {
        ticks.update(|v| *v += 1);
        clock.set(format!("已运行 {} 秒", ticks.get()));
    })
    .content(ui)
    .run();
}
```

信号只在 `on_message` / `on_interval` / 控件回调里写，框架自动在下一帧读取并渲染。
完整示例见 `examples/background_task.rs`。

### 8.6 阻塞式原生调用的时机

原生模态框（文件对话框、`MessageBoxW`）**不能在事件回调栈内同步弹**——它自带消息泵，
会与还没返回的事件分发冲突（表现为鼠标捕获错乱、对话框卡死）。框架给了两个入口：

| 场景 | 用什么 |
|---|---|
| 弹一个文件对话框 | `ctx.request_pick_file(dlg, on_result)` / `request_save_file` 等 |
| 需要连弹多个（选文件→校验→选目录→确认），或任意阻塞流程 | `ctx.defer_blocking(f)` |

两者都在事件分发**完全返回**后才执行闭包，闭包内可放心直接同步调 `PickDialog::pick_file()`
等阻塞 API。

**哪里有 `ctx`**：所有控件回调，以及菜单项动作（`MenuItem::run` 的闭包，见 §8.2）。
0.12.0 之前菜单动作拿不到 `ctx`，得绕道自由函数 `windui::app::defer_blocking(f)`——
它现在已 `#[deprecated]`，一律改用 `ctx.defer_blocking(f)`。托盘菜单项另有自己的
`TrayCtx`（显隐窗口 / 退出 / 气泡通知）。

> 若你实现了自定义 `AppHandler` 并覆盖了 `take_dialog_request`，且代码里还留着已废弃的自由
> 函数，记得回退到 `crate::app::take_deferred()`，否则那条队列排入的闭包不会跑。

---

## 9. 扩展：自定义控件

实现 `Widget` trait，挂到 `Element::leaf().widget(...)`（或空容器 `col`/`row`/`stack` 的
`.widget()`）。

⚠️ 一个节点只有一个 widget 槽，故 `.widget()` **只能挂到还没有控件的节点上**。挂到
`button`/`label`/`slider` 这类控件，或 `scroll`/`table_*`/`list_signal` 这类内部已挂了控件的
组合构造器上，会把原控件替换掉——debug 下 `debug_assert` 会带调用点报错。要在控件旁边加
自绘内容，用容器把两者并排：`Element::row().child(按钮).child(Element::leaf().widget(自定义))`。

`Widget` 是**纯内容接口**——不持有、不访问树。所有方法都有默认实现，按需覆盖：

```rust
use windui::core::{Widget, EventCtx};
use windui::event::{Event, PointerKind};
use windui::geometry::{Size, Rect, Color};
use windui::render::{Canvas, Paint};
use windui::style::Style;
use windui::text::TextEngine;

use windui::signal::{signal, Signal};

struct Dot { on: Signal<bool> }

impl Widget for Dot {
    // ① 测量：返回内容固有尺寸（不含 padding）
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(24, 24)
    }

    // ② 绘制：bounds=节点全矩形, content=扣 padding 后的内容矩形，
    //    focused=本节点是否持有键盘焦点，enabled=有效启用态（已并入父链继承，据此置灰）
    fn paint(&self, _bounds: Rect, content: Rect, _focused: bool, enabled: bool,
             canvas: &mut dyn Canvas, _style: &Style) {
        let c = match (self.on.get(), enabled) {
            (_, false) => Color::hex(0xE0E0E0),   // 禁用置灰
            (true, _) => Color::hex(0x2ECC71),
            (false, _) => Color::hex(0xCCCCCC),
        };
        let cx = content.x as f32 + content.w as f32 / 2.0;
        let cy = content.y as f32 + content.h as f32 / 2.0;
        canvas.fill_circle(cx, cy, 10.0, &Paint::fill(c));
    }

    // ③ 事件：返回是否消费（消费则停止冒泡）
    fn on_event(&mut self, _ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Pointer(p) = ev {
            if p.kind == PointerKind::Up && p.button == windui::event::MouseButton::Left {
                self.on.set(!self.on.get());   // 写信号自动请求重绘，无需 ctx.mark_dirty()
                return true;
            }
        }
        false
    }

    fn focusable(&self) -> bool { true }   // 可选：加入 Tab 导航
}

// 使用
let state = signal(false);
let dot = Element::leaf().widget(Dot { on: state });
```

**三阶段契约**：`measure`（算固有尺寸）→ 框架 `arrange`（定位，你不参与）→ `paint`（在分配到的 `bounds`/`content` 内绘制）。坐标在 `on_event` 收到的是**逻辑坐标**（已 ÷DPI scale）。

⚠️ `paint` 是 **6 参**（`bounds, content, focused, enabled, canvas, style`）。漏掉 `enabled`
不会有友好的报错，只会是一条"方法不属于该 trait"的编译错误——照本节抄即可。

**何时还需要 `ctx.mark_dirty()`**：只有当控件改的是**自身持有的非信号状态**（hover/press
标志、拖动偏移等）时才需要显式请求重绘。改 `Signal` 一律自动触发。

**`EventCtx` 能力**：`mark_dirty()` 重绘、`bounds()` 取绝对矩形、`capture()/release_capture()` 拖拽捕获、`request_focus()/request_close()`、`scroll_by()/set_scroll()/scroll_metrics()`、`clipboard_get()/clipboard_set()`、`show_menu()/show_context_menu()`、`set_bg()`。

**`Canvas` 图元**：`fill_rect`、`fill_round_rect`、`stroke_round_rect`、`draw_line`、`fill_circle`、`draw_text`、`measure_text`、`save/restore/clip_rect`（裁剪用 save→clip_rect→绘制→restore）。坐标为 f32 绝对窗口坐标。

**持续动画**：在 `paint` 中调用 `windui::anim::request_repaint()` 即请求下一帧；框架会按显示器刷新率（≤60fps）驱动，停止请求即回到零 CPU 空闲。

---

## 10. 第三方开发规范（Do / Don't）

**Do**
- ✅ 状态用 `signal(初值)` 造 `Signal<T>`，在外部创建、按值传进控件与回调（`Copy`，不用 `clone`）。
- ✅ 成体系视觉走 `Theme`，一次性微调走内联 `Style` 修饰符；要跟随运行期换主题的颜色用 `Role`（`fg_role`/`bg_role`/`border_role`）。
- ✅ 自定义控件实现 `Widget`，`paint` 读 `theme::current()` 而非硬编码颜色（与内建控件一致）。
- ✅ 滚动内容放进 `Element::scroll()`，触摸/惯性自动可用。
- ✅ 行数会变的列表交给 `list_signal` / `host_signal`，把整份数据 `set` 回信号（见 §6.5）。
- ✅ 回调里只改信号状态，靠下一帧反映，不要试图直接操作节点树。
- ✅ 横向/纵向"占剩余空间"用 `.weight(n)`。

**Don't**
- ❌ 不要在 `on_click`/`on_event` 里长时间阻塞（同步渲染，会卡 UI 线程）；要弹原生模态框走 §8.6。
- ❌ 不要给 `Label` 写死 `.height(N)`——它会自己算换行后的内容高度，写死会让长文案被静默截断。
- ❌ 不要用 `.width_match()` / `.fill()` 表达"占父容器的剩余宽度"——那是"取父容器全宽"，会溢出（见 §6.2）。
- ❌ 不要在构建期把 `Role` 解析成 `Color` 再 `.fg(c)`——那样换主题不跟随。
- ❌ 不要把 `Signal` 搬进后台线程（编译不过），跨线程更新走 `App::channel`（见 §8.5）。
- ❌ 不要把 text_input 专属修饰符（`password/multiline/wrap`）链到其他控件——debug 期会 panic 提示误用。
- ❌ 不要假设 `Widget` 能访问父/子节点——它是纯内容接口，跨节点协调走共享状态。
- ❌ 不要在控件里写死颜色/间距/字号——破坏主题一致性。

---

## 11. 已知约束

**功能约束**
- CPU 软光栅，适合中小工具；不适合大面积高频全屏动画。Windows 上可 opt-in Direct2D GPU 后端（见 §5）。
- `list` 当前每行是独立 Tab 停靠点，超长列表会拉长焦点链（计划：单 Tab 停靠 + 方向键导航）。
- `list_signal` 一族当前是**全量重建**、无 keyed diff，行内未提交的临时状态会随重建丢失（见 §6.5）。
- 表格扩展修饰符只对部分表格变体生效，误用时 debug 期 panic、release 静默忽略——见 §5 的适用矩阵。
- `Signal` 只能在 UI 线程使用（`!Send`），跨线程更新走 `App::channel`（见 §8.5）。
- **离屏层上的文字没有 ClearType**：子树 `opacity()` 与半透明文字色都要经离屏层合成，
  而次像素抗锯齿要求每个通道各有一个 alpha、RGBA 只有一个，层内因此退化为灰度抗锯齿
  （浏览器给 `opacity` 子树的也是这个取舍）。小字号下会觉得比不透明路径略"细"。
  两个后端都如此，与 GPU 与否无关。全不透明的常规路径不受影响。
- **信号槽位回收只覆盖库内三处重建宿主**：`list_signal` / `host_signal` 的 `DynList`、
  `reorder_list_signal` 的行源、可排序表格的表头与正文。作用域外的 `signal()` **永不回收**，
  这是刻意的——应用状态没有所有者，也不该有。但若应用自己写了"按数据整批重建子树"的
  控件，其构建期信号仍会一轮轮累积，须自持一个 `SignalScope` 管起来
  （`WINDUI_SIGNALS=1` 可在活跃槽位创新高时打印，健康应用启动后应永久安静）。
- **构造期读取的主题尺寸不跟随运行期换主题**：`card` / `field` / `setting_row` /
  `setting_row_desc` 这类组合构建器在**构建时**就把 `CardTheme` / `FormTheme` 的行高、
  内边距、间距、字号烘进 Element。颜色不受影响（走 `Role` 延迟解析，见 §7），
  但换主题后要尺寸也跟着变，必须重建这棵子树。
- **`#[non_exhaustive]` 的五个类型**（`MenuItem` / `DropdownItem` / `CheckMenuItem` /
  `Role` / `Intent`）在下游不能用结构体字面量构造，穷尽 `match` 须留 `_` 兜底。
  一律走构造器 + builder 链（`MenuItem::run(..).icon(..).danger()`）。

**平台状态**

Windows 与 macOS 均已支持——控件树、布局、事件、动画、主题是同一份平台无关代码，
两平台间无需改动。以下几项尚未拉齐：

| 能力 | Windows | macOS |
|---|---|---|
| 窗口 / 事件循环 / 文字 / 触摸 / 剪贴板 / 托盘 / 文件拖放 / 无边框窗口 | ✓ | ✓ |
| 全局热键（`App::hotkey`） | ✓ | ✗ debug 期 panic、release 静默忽略 |
| `font_weight` | ✓ | ✗ 传入非 400 的值不报错但无视觉变化（CoreText 路径未接字重） |
| 私用区回退字体（`text::register_private_use_font`） | ✓ | ✗ 函数在 macOS 上**不存在**（`#[cfg(windows)]`），跨平台代码需自行 `cfg` 分支 |
| Direct2D GPU 后端（`App::accelerated`） | ✓ | — 不适用（macOS 恒软渲染） |

**命名一致性**

背景/前景统一 `bg`/`fg`，各自都有跟随主题的 `_role` 变体（`bg_role`/`fg_role`/`border_role`/
`accent_role`）；控件状态统一 `Signal<T>`；控件专属修饰符误用在 debug 期 panic 提示、release
静默忽略——覆盖 text_input（`password`/`multiline`/`wrap`）、link（`url`/`underline`）、
rich（`max_lines`/`truncate`/`on_span_click`/`copy_menu`）、image（`fit`/`tint`）、
slider（`show_value`）、reorder（`on_reorder`/`commit_mode`）、intent 一族
（`intent`/`danger`/`neutral`/`accent`/`accent_role`）与表格扩展的一整组。
属性设置器统一去掉 `with_` 前缀（`with_badge` → `badge` 等），旧名留 `#[deprecated]`
过渡，编译期会提示新名。单条文案统一收 `impl Into<TextContent>`（`button`、`label`、
`link`、`checkbox`、`radio`、`nav_row`、`badge`、`icon_button`，可传 `&str` / `String` /
`Signal<String>`）；成组文案收 `impl Into<String>`（`dropdown`、`list`、`tabs` 的选项等）。
少数破例仍收裸 `String`：`Element::table_custom(columns: Vec<(String, f32)>, ..)`。碰上编译
错误时补一个 `.to_string()` / `.into()` 即可。

框架处于早期，以"最新设计 + 统一"为准，**不承诺向后兼容**——API 可能继续演进，
第三方请跟随本指南最新版。

---

## 附：模块速查

| 模块 | 内容 |
|------|------|
| `windui::prelude` | 常用类型一站式导入 |
| `windui::app::App` | 窗口配置与启动 |
| `windui::ui::Element` | 控件构建器（第三方主入口） |
| `windui::geometry` | `Color / Point / Size / Rect / Insets` |
| `windui::spec` | `Align / Axis / Dimension` |
| `windui::style::Style` | 内联视觉属性 |
| `windui::theme` | `Theme / Palette / Metrics` + `current()/set_current()` |
| `windui::event` | `Event / PointerEvent / KeyEvent / Key / MenuItem` |
| `windui::core` | `Widget / EventCtx`（自定义控件） |
| `windui::render` | `Canvas / Paint`（自绘图元） |
| `windui::anim` | `request_repaint()`（驱动动画） |

更多可运行示例见 `examples/`（`phase4_form` 表单、`fullshowcase` 全控件、`theming` 主题、`list` 列表等）。
