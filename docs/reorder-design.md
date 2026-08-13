# 可手动排序列表设计方案（windui）

面向设置类应用的**拖拽重排列表**：每行是一个完整的表单行（标签 + 开关/下拉/按钮），
用户可拖动手柄调整顺序。目标是"操作方便且符合桌面主流习惯"。

## 1. 目标与非目标

### 目标

- 行内可含任意交互控件（Switch / Dropdown / TextInput / Button），拖拽不与其抢事件。
- 行高**可以不等**（表单行常带副标题、徽章），让位计算必须支持。
- 拖动过程有明确落点反馈：被拖行浮起跟手，其余行平滑让位。
- 提供多条操作入口：拖动手柄（主）、键盘拾起、右键菜单、可选 ▲▼ 按钮。
- 提交后行内控件**状态不丢**。
- 拖动全程**零 relayout 冲突**——布局结果始终权威，视觉位移独立叠加。

### 非目标（本版边界）

- 跨列表拖动（A 列表拖到 B 列表）。
- 水平方向重排（标签页拖动排序另立控件）。
- 树形嵌套拖拽（改变层级）。
- 虚拟化长列表（万行级）——本控件面向设置页的十行量级。

## 2. 设计原则对齐

| 铁律 | 本方案的落实 |
|------|-------------|
| Widget 是纯内容，不持有节点树 | `DragHandle` 与 `ReorderList` 通过共享 `Rc<RefCell<ReorderCtl>>` 协调，不存 `NodeId` |
| 控件不硬编码视觉 | 新增 `ReorderTheme`，颜色/时长/尺寸全部走主题回退 |
| 三阶段布局 | `offset` 是**绘制期偏移**，不参与 measure/arrange，布局结果不被污染 |
| 空闲零 CPU | 动画期 `anim::request_relayout()` 续帧，收敛后自动停 |
| 平台无关 | 全部在 core / ui 层，零平台代码 |

## 3. 交互设计

### 3.1 为什么是"拖动手柄 + 空位腾挪"

行内含交互控件时，**整行拖拽是反模式**——按在 Switch 上到底是拨动还是拖动，无法可靠区分。
主流桌面应用（macOS 系统设置、VS Code、Notion、Linear）在这种场景下一致采用独立拖动手柄。

落点反馈选**空位腾挪**（其余行让位，被拖行浮起）而非插入指示线：前者所见即所得，
用户能直接看到"放下后长什么样"。指示线范式留作 P2 的 `.indicator_mode(Line)` 选项，
供大列表在软件光栅下降低开销。

```
┌─────────────────────┐
│ ⠿ 拼音方案   [开关] │
│                     │  ← 空位（其余行让位，150ms EaseOut）
│ ⠿ 五笔方案   [开关] │
│ ⠿ 双拼方案   [开关] │
└─────────────────────┘
    ╭───────────────────╮
    │ ⠿ 英文方案 [开关] │ ← 浮起跟手 + 投影 + raised
    ╰───────────────────╯
```

### 3.2 四条操作入口

| 入口 | 操作 | 说明 |
|------|------|------|
| 拖动手柄 | 按住 `⠿` 拖动 | 主路径。超 4px 阈值才进入拖动，防误触 |
| 键盘拾起 | Tab 聚焦手柄 → `Space` 拾起 → `↑↓` 移动 → `Enter` 放下 / `Esc` 取消 | 无障碍标准方案（dnd-kit / macOS 同款） |
| 右键菜单 | 行上右键 → 上移 / 下移 / 移到顶部 / 移到底部 | 长列表比拖拽快 |
| ▲▼ 按钮 | `.arrows()` 开启 | 默认关闭，避免行内元素过多 |

不做 `Alt+↑/↓`：`KeyEvent` 当前无 `alt` 字段，为此扩展事件结构不划算，
而键盘拾起模式已完整覆盖无障碍需求。

### 3.3 拖动过程细节

- **阈值**：`Down` 后位移超 4px 才进入 `Dragging`，之前是 `Pressed`（点击手柄不产生任何位移）。
- **水平锁定**：只跟随指针 Y，X 恒为 0——垂直列表的水平漂移是纯噪声。
- **浮起视觉**：`raised = true`（同级最后绘制）+ 投影 + 主题可配的拖动底色。
- **让位动画**：受影响区间的行 `offset.y` 补间到目标位（`AnimTheme::normal`，EaseOut）。
- **松手回落**：被拖行补间回落到目标槽位（`AnimTheme::fast`），**动画结束后再提交**，避免瞬移跳变。
- **取消**：`Esc` 期间所有 offset 补间归零，不触发回调。

## 4. 架构分层

```
L1  core：Node::offset / Node::raised          ← 通用能力，非拖拽专用
L2  ui/reorder.rs：ReorderCtl 状态机 + 两个 Widget
L3  Element::reorder_list* 构造器 + 修饰符
L4  ReorderTheme
```

L1 是唯一侵入核心的改动，且**刻意做成通用能力**：后续列表增删的 FLIP 动画、
抽屉滑出、浮层位移都能直接复用，不是为拖拽定制的特例。

## 5. 第 1 层：核心绘制偏移

### 5.1 为什么不能直接改 `bounds`

`Tree::layout_root` 只在 `needs_relayout` 时跑，所以"拖动中直接改子节点 `bounds.y`"
在多数帧里看似能用——但任何一次 relayout（窗口缩放、行内控件展开、主题切换）
都会把它冲掉。更糟的是它把"布局结果"与"临时视觉状态"混为一谈，是隐性 bug 源。

正解是引入一个**不参与布局的绘制偏移**：布局结果保持权威，拖拽只是视觉平移。

### 5.2 字段

```rust
pub struct Node {
    /// 绘制/命中偏移（逻辑 px），不参与 measure/arrange。
    /// 拖拽让位、FLIP 动画等"视觉位移但布局不变"的场景使用。
    pub offset: Point,
    /// 同级绘制顺序提升：为 true 的子节点在其余兄弟之后绘制、命中时优先测试。
    /// 拖拽浮起行用，避免被下方兄弟盖住。
    pub raised: bool,
}
```

### 5.3 同步点（已核对，共 4 处）

| 位置 | 改动 |
|------|------|
| `Tree::paint_node` | `abs` 叠加 `offset`；子节点循环分两趟——先非 `raised`，后 `raised` |
| `Tree::hit_node` | `abs` 叠加 `offset`；倒序遍历时 `raised` 组优先 |
| `Tree::abs_bounds` | 父链累加时叠加各级 `offset` |
| `Tree::layout_signature` | 把 `offset`/`raised` 一并哈希 |

最后一条是关键设计：签名把 offset 纳入后，拖动导致签名变化 →
宿主自动判定"结构变化"→ 升级整窗重绘。**不需要为拖拽写任何重绘特例分支**，
同时 hover 重同步、隐藏交互复位等既有修正也自动生效。

`EventCtx` 配套暴露：

```rust
pub fn set_node_offset(&mut self, id: NodeId, off: Point);
pub fn set_node_raised(&mut self, id: NodeId, raised: bool);
```

### 5.4 动画由谁每帧驱动

`Widget::on_update` 在 `layout_root` 内广播，而 layout 只在 `needs_relayout` 时跑——
所以补间不能靠"每帧自然会调 on_update"。

复用既有的**布局动画正规门**（`rich.rs` 的高度补间同款）：

```
ReorderList::on_update  ——推进补间、写各行 offset
        │
        └─ 补间仍活跃 → anim::request_relayout()
                              │
                              ▼
              宿主下一帧 needs_relayout → layout_root → on_update …
```

收敛后停止请求，回到空闲零 CPU。`request_relayout` 而非 `request_repaint`
是必需的：后者不会触发 layout，on_update 就断帧了。

## 6. 第 2 层：状态机与两个 Widget

### 6.1 共享状态

```rust
pub(super) type ReorderCtl = Rc<RefCell<Ctl>>;

struct Ctl {
    phase: Phase,
    /// 手柄按下时写入的意图：(手柄 NodeId, 按下点)，由 ReorderList 在冒泡中消费。
    pending: Option<(NodeId, Point)>,
    /// Esc 取消请求（手柄写入，列表在下一帧 on_update 消费）。
    cancel: bool,
    /// 拖动源行下标（算目标位与回调实参用）。
    from: usize,
    /// 被拖行的节点 id：样式与 raised 的还原按它定位，不用 from。
    row_id: Option<NodeId>,
    to: usize,
    start_y: i32,
    cur_y: i32,
    /// 拖动开始时的行槽位快照：(y, h)，支持不等高。
    slots: Vec<(i32, i32)>,
    /// 每行的位移补间。
    tweens: Vec<Transition<f32>>,
    /// 被拖行浮起时被临时改写的样式，退出时逐字段还原。
    saved: Option<SavedStyle>,
}

enum Phase { Idle, Pressed, Dragging, Settling }   // 键盘拾起的 Grabbed 态归 P1
```

两处细节值得说明：

- **`pending` 存 `NodeId` 而非行下标**，看似与"Widget 不持有节点树"的铁律相抵，实际不然：
  它在**同一次 `dispatch_pointer` 的冒泡中**就被消费掉，不跨帧存活；换来的是手柄不必
  自知身处第几行，行的增删也不会让它持有一个过期下标。
- **`row_id` 必须与 `from` 并存**。样式还原若按下标定位，一旦拖动期间上游重建过子节点
  （`DynList`、`visible_when` 联动），`from` 指向的就是另一个节点：真正被改过的那行会
  永久留着浮起底色与投影，`raised` 也会留在错误的行上一直盖住兄弟。

### 6.2 事件协作：让最内层声明意图，外层消费

`DragHandle` 挂在每行的手柄叶子节点上，`Down` 时：

1. 把 `(行下标, 起点)` 写进 `Ctl.pending`
2. `ctx.capture()`（锁定后续指针事件）
3. `ctx.request_focus()`（为 Esc 与键盘拾起铺路）
4. **返回 `false`** —— 不消费，让事件继续冒泡

`ReorderList` 挂在列容器节点上，收到冒泡上来的 `Down` 时读 `Ctl.pending` 进入状态机。

这条协作路径成立的依据：`dispatch_pointer` 在 capture 生效时目标锁定为手柄，
但**冒泡链依然是 手柄 → 行 → ReorderList → Scroll**。于是手柄拿捕获、
列表拿逻辑，两者零耦合；`ReorderList` 消费事件后也不会再传给外层滚动容器，
天然不抢事件。

反向设计（让 `ReorderList` 自己判断落点是不是某行的手柄）需要反查子孙节点相对位置，
是脆的——最内层节点声明意图、外层消费才是这套事件模型的正确用法。

### 6.3 状态机

```
Idle ──手柄Down──▶ Pressed ──位移>4px──▶ Dragging ──Up──▶ Settling ──动画完──▶ Idle(提交)
  │                   │                      │
  │                   └──Up（未超阈值）──────┴──Esc──▶ Idle(还原，不提交)
  │
  └──手柄聚焦+Space──▶ Grabbed ──↑↓──▶ Grabbed ──Enter──▶ Idle(提交)   ← P1，未实现
                                          └──Esc──▶ Idle(还原)
```

## 7. 不等高让位算法

表单行高度天然不一致（副标题、徽章、多行说明），所以不能用"等高行 ± 一个行高"的简化算法。

统一走**重新堆叠**：

1. 拖动开始时快照各行 `(y, h)` 到 `slots`。
2. 每帧算目标插入位 `to`：被拖行的当前中心 y 与各行中心线比较（用**中心线越过**而非
   边界越过，落点在边界处不会抖动）。
3. 从 `slots` 抽掉被拖行 → 在 `to` 位插入等于被拖行高度的空位 → 按顺序重新累加 y。
4. 每行 `offset.y = new_y - orig_y`，喂给各自的 `Transition<f32>`。

等高列表是该算法的特例，不写两套。被拖行自身不走补间——它直接跟指针，补间会产生"橡皮筋"滞后感。

## 8. 提交模式

`DynList`（数据驱动重建）与 `ReorderList` 都要挂在容器节点上，而一个节点只能有一个 widget。
故提交分两档，让 `ReorderList` 保持非泛型、职责单一：

| 模式 | 行为 | 适用 |
|------|------|------|
| `Children`（默认） | 内部直接重排 `node.children`，**不重建行** → 行内控件状态天然保留，不要求 `T: Clone` | 固定若干设置行 |
| `Callback` | 不动 children，只回调 `on_reorder(ctx, from, to)`，由应用改 `Signal<Vec<T>>` 触发重建 | `reorder_list_signal` 的动态列表 |

两档都会调 `on_reorder`，区别只在"children 由谁负责"。
`Element::reorder_list_signal` 内部自动选 `Callback`。

### 8.1 为什么必须有数据驱动的一档

`Children` 模式下**顺序只活在节点树里**，应用没有把顺序**推回**控件的通道。
「恢复默认」「配置重新载入」「后台刷新」这类反向同步于是全部落空——用户点了恢复默认，
勾选状态回去了、顺序还留在他上次拖出来的样子。凡顺序需要持久化的场景（本控件的主战场）
都会碰到这一条，所以数据驱动不是可选的加强档，而是完整度的下限。

### 8.2 `RowSource`：把重建塞进同一个 widget

`DynList` 与 `ReorderList` 都要挂在列容器上，而一个节点只能挂一个 widget（§8 开头）。
两条出路：外面再套一层 `host_signal` 宿主（调用方每次都得手写一次 epoch 信号的样板），
或者把"按信号重建 children"抽成非泛型接口内嵌进 `ReorderList`。取后者：

```rust
pub(super) trait RowSource {
    /// 数据版本变了就重建 children；返回是否真的重建过。
    fn sync(&mut self, ctx: &mut EventCtx) -> bool;
}
```

泛型只落在实现 `SignalRows<T>` 上，`ReorderList` 保持非泛型（否则 `on_reorder`
/`commit_mode` 的 `downcast_mut::<ReorderList>()` 会因类型参数不定而失效）。

两处调用时机是这套的关键，各自都有非它不可的理由：

| 时机 | 条件 | 理由 |
|------|------|------|
| `on_update` 开头 | **仅 `Phase::Idle`** | 拖动中重建会把槽位快照、补间下标与浮起样式所指的节点整批换掉，让位算法当场失准。积压的版本差会一直留到落定后补做 |
| `finish()` 末尾 | 无条件 | 回调刚在这里改完数据，而本帧偏移已清零、children 还是旧序——不在同帧补上重建，就会闪一帧旧顺序再跳正 |

### 8.3 手柄位置交还调用方

`reorder_list` 自动前置手柄，`reorder_list_signal` 则把手柄作为 `row_fn` 的第二个参数
交回去。不是为了灵活性，是**被事件模型逼出来的**：

- 行若有整体选中背景/左缘指示条，手柄并排在外会被排除在选中视觉之外，高亮凭空缩进一截。
- 更硬的一条：**手柄不能是 `clickable()` 容器的后代**。`Clickable` 对 `Down`/`Up` 一律
  返回 `true`，而 `dispatch_pointer` 是 `consumed → break`——冒泡在它那里就断了，
  `ReorderList` 永远收不到 `Down`，拖动根本起不来。整行可点的列表必须把手柄放进 `stack`
  当**同级覆盖层**（与可点行并列，不是嵌套），让手柄那条冒泡链绕开 `Clickable`。

## 9. 第 3 层：Builder API

```rust
// 静态行（设置页典型）——内部重排 children，行内控件状态保留
Element::reorder_list(vec![row_a, row_b, row_c])
    .on_reorder(|ctx, from, to| { /* 持久化顺序 */ })

// 数据驱动（顺序真相源在信号里）——手柄由行自己安放
Element::reorder_list_signal(order, |item, handle| {
    Element::row().width_match().cross(Align::Center)
        .child(handle)
        .child(row_of(item).weight(1.0))
})
.on_reorder(move |_ctx, from, to| {
    order.update(|v| { let x = v.remove(from); v.insert(to.min(v.len()), x); })
})
```

| 修饰符 | 作用 | 状态 |
|--------|------|------|
| `.on_reorder(f)` | 重排回调 `(ctx, from, to)`，顺序未变时不触发 | P0 |
| `.commit_mode(m)` | 切 `Children`（默认）/ `Callback` | P0 |
| `.handle_trailing()` | 手柄放行尾（默认行首） | P1 |
| `.arrows()` | 额外显示 ▲▼ 按钮 | P1 |
| `.whole_row_drag()` | 整行可拖（仅当行内无交互控件时） | P2 |
| `.indicator_mode(Line)` | 改用插入指示线 | P2 |

数据驱动构造器（手柄位置由调用方决定，见 §8.3）：

| 构造器 | 作用 | 状态 |
|--------|------|------|
| `reorder_list_signal(data, row_fn)` | `Signal<Vec<T>>` 驱动，信号变化即整体重建；固定 `Callback` 模式 | P0.5 |

手柄图形是**自绘 2×3 圆点**，不提供 `handle_glyph`：盲文点字符 `⠿` 的字体覆盖不可靠，
缺字会渲染成豆腐块。项目里 `chevron_right` 等也是自绘的，遵循同一取舍。

`reorder_list_signal` 不靠 `DynList`：两者都要挂在容器节点上，而一个节点只能有一个
widget（见 §8）。它把重建能力做成非泛型的 `RowSource` 内嵌进 `ReorderList`，
控件本身仍保持非泛型（见 §8.2）。

命名遵循既有约定：构造器 = 控件名（名词），修饰符 = 属性名不加 `set_`。

## 10. 第 4 层：主题

```rust
pub struct ReorderTheme {
    /// 手柄常态色（回退 palette.text_muted）。
    pub handle: Option<Color>,
    /// 手柄悬停/按住色（回退 palette.text）。
    pub handle_hover: Option<Color>,
    /// 拖动中行底色（回退 palette.surface）。
    pub dragging_bg: Option<Color>,
    /// 拖动中行投影色（回退半透明黑 a=56）。
    pub shadow_color: Option<Color>,
    /// 拖动中行投影模糊半径（回退 12.0）。
    pub shadow_blur: Option<f32>,
    /// 插入指示线色（回退 palette.accent）。
    pub indicator: Option<Color>,
    /// 手柄槽宽 px（回退 20）。
    pub handle_w: Option<i32>,
    /// 拖动中行圆角（回退 metrics.corner_md）。
    pub corner: Option<f32>,
}
```

投影拆成 `shadow_color` + `shadow_blur` 两个标量而非直接放 `Shadow`——后者不实现
`Serialize`，拆开才能进 TOML；控件内部用 `ReorderTheme::shadow()` 组装。

挂 `Theme.reorder`，`#[serde(default)]` 接入 TOML，全字段 `Option` 回退 palette。
动画时长直接取 `theme.anim.normal()` / `fast()`，不另设字段。

`handle_w` 由 `DragHandle::measure` 每帧读取，而非构建期固化——换肤后重新 measure
即生效，不需要重建元素树。

## 11. 测试策略

### 单元测试（经真实路径，不 mock）

L1 核心：

- 设 `offset` 后 `abs_bounds` 与 `hit_test` 同步偏移（两者必须一致，否则点击错位）。
- `raised` 子节点的命中优先于其后的兄弟节点。
- `offset` 变化导致 `layout_signature` 变化（保证自动升整窗）。

L2 控件：

- 构造 `Tree` → `build` → `layout_root` → `dispatch_pointer`：
  按下手柄 → 移动越过邻行中线 → 抬起，断言 children 顺序与 `on_reorder(from, to)` 实参；
  并断言被拖行 offset **精确等于指针位移**（它不走补间，直接跟指针）。
- 未超 4px 阈值抬起 → 顺序不变、回调不触发。
- 拖动中 `Esc` → 顺序不变、所有 offset 归零、`raised` 取消，**且随后松手必须释放指针捕获**。
- `Pressed` 阶段（未起拖）按 `Esc` → 状态机干净收尾，紧接着再拖一次必须正常生效。
- 捕获被系统夺走（合成远处 `Up`）→ 走取消而非提交。
- 不等高行的让位：三行高度 40/60/40，拖第一行到末尾，断言各行 offset 与重堆叠结果一致。
- `Callback` 模式下 children 顺序不变，只触发回调。

关闭动画用一个 `Drop` 守卫（`AnimOff`）而非在函数末尾复位：开关是 thread_local，
`--test-threads=1` 下断言 panic 会让复位被跳过，污染同线程的后续测试。

### 截图验证

`examples/fullshowcase.rs` 控件页新增卡片，用 `--screenshot` 核对静态渲染；
拖动中态用 `--click` 无法覆盖（需要按下+移动序列），改由单测断言 offset 数值。

## 12. 文件改动清单

| 文件 | 改动 |
|------|------|
| `src/core.rs` | `Node::offset` / `Node::raised` 字段；`paint_node` / `hit_node` / `abs_bounds` / `layout_signature` 同步；`EventCtx` 两个 setter |
| `src/ui/reorder.rs` | **新建**：`ReorderCtl` / `Ctl` / `Phase` / `DragHandle` / `ReorderList` |
| `src/ui/mod.rs` | `mod reorder;` + `Element::reorder_list` / `on_reorder` / `commit_mode` + `pub use` |
| `src/theme.rs` | `ReorderTheme` + `Theme.reorder` + TOML |
| `examples/fullshowcase.rs` | 控件页新增展示卡片（硬性要求） |

## 13. 分期

### P0 —— 已交付

核心 `offset`/`raised` + `ReorderList` 手柄拖 + 空位腾挪动画 + `Esc` 取消 +
`ReorderTheme` + `fullshowcase` 控件页卡片 + 契约单测。

### P0.5 —— 已交付

`reorder_list_signal`（`RowSource` + 手柄位置交还调用方）+ `fullshowcase` 数据驱动卡片
+ 三条契约单测（数据变更重建 / 提交同帧重建 / 拖动中延后重建）。

### P1

边缘自动滚动、键盘拾起模式、右键菜单四项移动、`.arrows()`、`.handle_trailing()`，
并把 `examples/settings.rs` 里那对静态 ▲▼ 假 UI 改成真拖拽。

### P2

插入指示线模式、整行拖动模式、触摸长按拖起。

## 14. 风险与取舍

| 风险 | 对策 |
|------|------|
| **指针捕获只能在指针路径上归还** | `DispatchResult` 没有 capture 字段，`Tree::dispatch_key` 也不消费 `o.capture`；`Tree::call_on_update` 更是把整个 `EventOutcome`（除 toast）丢弃。故 `ctx.release_capture()` 在**键盘事件与 `on_update` 相位里都是空操作**。本控件的 Esc 取消因此只置标志，捕获由 `ReorderList` 的 `Up` 兜底臂在用户松手时归还（含宿主补发的合成 Up）。这是框架级缺口，任何想在键盘事件里释放捕获的控件都会静默失败——若将来有第二个这样的需求，应给 `DispatchResult` 补 capture 字段并在宿主键盘路径应用 |
| 捕获被系统夺走（Alt+Tab、别的窗口 `SetCapture`） | 宿主补发坐标为 `(-1_000_000, -1_000_000)` 的合成 `Up`。既有约定是"收尾/复位"而非"确认"（`Slider` 借它复位拖动），故本控件识别该远处坐标后走**取消**——用户只是切了个窗口，顺序不该被悄悄改掉 |
| 软件光栅拖动掉帧（整窗重绘 + 多行补间） | 只对**受影响区间**的行建补间；拖动中无补间活跃时不续帧（`offset` 进签名 → 每帧都会被判为结构变化而整窗重绘，按住不动却满帧重绘是纯浪费）；性能须 `--release` 实测（debug 慢约 5 倍）；大列表留指示线模式兜底 |
| 拖动中 relayout 冲掉视觉状态 | `offset` 独立于 `bounds`，relayout 不影响——这正是不改 `bounds.y` 的理由 |
| 手柄与滚动容器抢事件 | 冒泡链天然分层，`ReorderList` 消费即止（见 §6.2） |
| **手柄嵌在 `clickable()` 行内则拖不动** | `Clickable` 对 `Down`/`Up` 一律返回 `true`，`dispatch_pointer` 遇 consumed 即 break，列表收不到事件。这是两个控件都"按约定行事"却互斥的组合，无法在控件内部化解（若让 `Clickable` 感知拖动状态就成了跨控件耦合）。对策是布局层面绕开：`reorder_list_signal` 把手柄交给调用方安放，整行可点时放进 `stack` 当同级覆盖层（见 §8.3） |
| 拖动中上游数据变更 | `RowSource::sync` 仅在 `Phase::Idle` 执行，版本差积压到落定后补做（见 §8.2） |
| 浮起行超出滚动容器被裁剪 | 与主流实现一致（裁在列表内），不做跨容器浮层 |
| `Callback` 模式下应用忘记改数据 | 文档明确；`reorder_list_signal` 封装好默认行为 |
