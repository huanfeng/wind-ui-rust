# 自绘标题栏系统菜单设计方案（windui）

无边框模式（`App::frameless()`）下，在自绘标题栏的拖动区右键弹出**窗口系统菜单**，
项按窗口的当前状态与能力自动禁用。这是 Windows 桌面应用的通用惯例——用户按到标题栏
就期待有这一下，而 frameless 恰恰把这条原生通路掐断了。

## 1. 目标与非目标

### 目标

- frameless 窗口的 `window_drag()` 区域右键 → 弹出**框架自绘、随主题**的系统菜单。
- 菜单四项：**还原 / 最小化 / 最大化 / 关闭**，按窗口状态与能力自动置灰。
- 对**所有窗口**同构生效：主窗（`App`）与 `ctx.open_window` 开出的子窗走同一条逻辑，
  不可缩放的子窗（对话框式窗口）自动得到"最大化/还原置灰"，不需要调用方多写一行。
- **默认接管**：frameless 窗口开箱即有；同一节点上写 `on_context_menu` 则用户菜单优先；
  `App::system_menu(false)` / `Window::system_menu(false)` 彻底关掉。
- 补上 `EventCtx::window_state()` 这条缺失的查询缝，并让它可被菜单构建器读到。

### 非目标（本版边界）

- **不做「移动」/「大小」两项**。它们必须走 `WM_SYSCOMMAND(SC_MOVE/SC_SIZE)` 进入系统的
  模态键盘拖动循环——那是个会同步重入 `wnd_proc` 的模态消息循环（铁律 6 的重灾区），
  且 macOS 无任何对应物。边框拖拽已经覆盖了实际用法。
- **不做 macOS**。macOS 系统本身没有"标题栏右键出系统菜单"这个习惯，本版只在 win32
  落地；跨平台缝按既有规矩留好（`AppHandler` 缺省实现 + macOS 侧不推送状态），
  编译得过、行为为空。这条差异必须在 `API_GUIDE` 平台表里写明。
- 不做应用内 `dialog_panel` 浮层的模拟菜单——它没有独立 HWND，"最小化"对它无意义。
- 不做 `Alt+Space` 键盘唤起（P2，见 §9）。

## 2. 现状盘点：三个决定形状的事实

| # | 事实 | 位置 | 后果 |
|---|------|------|------|
| 1 | frameless 拖动区在 `WM_NCHITTEST` 返回 `HTCAPTION` | `platform/win32/mod.rs::handle_nchittest` | 该区域的右键走 `WM_NCRBUTTONDOWN/UP`，**客户区收不到**，`on_context_menu` 在标题栏上天然失效；而当前未拦截这两条消息，`DefWindowProcW` 会弹出**系统灰色原生菜单**（与自绘标题栏视觉割裂） |
| 2 | 框架无窗口状态查询 | `core.rs::EventCtx` 只有 `minimize`/`toggle_maximize` | "自动禁用"无从判定；`WindowButton::Maximize` 最大化后仍画方框而非还原图标，同一缺口的另一个症状 |
| 3 | `WindowOp` 只有 `ToggleMaximize` | `event.rs::WindowOp` | 菜单需要**并列**的「最大化」与「还原」两项（一项可用一项置灰），toggle 表达不了 |
| 4 | macOS 右键已直达控件树 | `platform/macos/window.rs::rightMouseDown:` | macOS 侧无需平台改动，将来开启只是"允不允许"的开关问题 |
| 5 | `resizable(false)` 会剥掉 `WS_THICKFRAME \| WS_MAXIMIZEBOX` | `win32/mod.rs` 建窗处 | **能力位天然可从 `resizable` 推导**，禁用规则不需要额外配置项 |

## 3. 交互设计

### 3.1 菜单内容与禁用矩阵

```
┌──────────────────┐
│  还原            │   enabled = maximized
│  最小化          │   enabled = minimizable
│  最大化          │   enabled = maximizable && !maximized
│ ─────────────── │
│  关闭    Alt+F4  │   恒可用
└──────────────────┘
```

| 窗口 | 还原 | 最小化 | 最大化 | 关闭 |
|------|------|--------|--------|------|
| 普通窗口，未最大化 | 灰 | 可用 | 可用 | 可用 |
| 普通窗口，已最大化 | 可用 | 可用 | 灰 | 可用 |
| `resizable(false)`（对话框式） | 灰 | 可用 | 灰 | 可用 |

**为什么置灰而不是隐藏**：Windows 系统菜单本身就是恒定四行、只改可用性。项数固定后
用户的肌肉记忆才成立（"第三行是最大化"）；动态增删会让同一个位置每次点到不同的东西。
这与 `menu.rs::normalize_separators` 面对的是同一类问题的相反侧——那里是"条件生成的组
需要收掉空分隔线"，这里是"固定项集刻意不做条件生成"。

**不设图标**：Windows 系统菜单无图标列。给了图标会让 `menu.rs` 预留 `MENU_ICON_W` 图标列，
菜单整体变宽，反而不像系统菜单。

**「还原」在最小化态**：不可达——窗口最小化时标题栏点不到。故不为该分支写逻辑，
但 `WindowState.minimized` 字段仍保留（`WindowButton` 与将来的托盘菜单会用到）。

### 3.2 触发时机：`WM_NCRBUTTONUP`，合成一次 `Down`

**在抬起时弹**，不在按下时弹——Windows 原生标题栏系统菜单就是抬起才出，且"右键按住
不放"在标题栏上没有任何其他语义，提前弹只会在用户按住移动时跟着乱跑。

**但合成进控件树的是 `PointerKind::Down`**，不是 `Up`。这处不对称是刻意的，必须写在
代码注释里，否则半年后一定被"顺手改成 Up"：框架的上下文菜单统一在 `Down` 上开
（`Tree::dispatch_pointer` 里 `secondary && Down` 才建 `MenuRequest`），而 `MenuHost`
的 `Up` 分支只负责结束滚动条拖拽、是无害空操作。合成 `Up` 等于什么也不会发生。

真实的 `WM_NCRBUTTONUP` **不再转交 `DefWindowProcW`**（返回 `LRESULT(0)`），
这是"不出现两个菜单"的唯一保证。`WM_NCRBUTTONDOWN` 同样吞掉（否则 DefWindowProc
会做 NC 区按下的默认处理）。

### 3.3 覆盖与关闭

三档，从"什么都不写"到"完全自己来"：

```rust
// 1) 默认：frameless 窗口的 window_drag() 区域自动获得系统菜单，零代码。

// 2) 覆盖：同一节点写 on_context_menu，用户菜单优先；想要"系统菜单 + 自己的项"
//    就把内置构建器拼进去。
let title_bar = Element::row().window_drag()
    .on_context_menu(|| {
        let mut items = windui::system_menu_items();   // 读线程局部窗口状态
        items.push(MenuItem::separator());
        items.push(MenuItem::run("关于 windui", |ctx| ctx.toast("v0.x"), true));
        items
    });

// 3) 关掉：
App::new(..).frameless().system_menu(false)
```

覆盖之所以"自动生效"，是因为注入点在 `dispatch_pointer` **之后**且只在 `res.menu.is_none()`
时补——用户的 `on_context_menu` 已经填了 `res.menu`，宿主就不再插手。不需要额外的
优先级规则。

## 4. 设计原则对齐

| 铁律 | 本方案的落实 |
|------|-------------|
| 3. 坐标系 | NC 消息给的是**屏幕坐标**，须 `ScreenToClient` → 物理像素 → 交 `on_pointer` 由宿主 ÷`scale` 转逻辑。漏掉任一步的症状是"高 DPI 下菜单弹偏" |
| 4. 视觉走主题 | 完全复用 `app/menu.rs` 浮层，零新绘制代码，运行期换肤自动跟随 |
| 5. 平台缝合 | 窗口状态经 `AppHandler::on_window_state` 单向推送；`WindowOp::{Maximize,Restore}` 是意图、不是句柄 |
| 6. 两段式借用 | 合成派发复用既有 `dispatch_pointer_event` 通路，菜单打开与 `ShowWindow` 都在借用释放后由 `apply_window_op` 落地 |
| 7. 空闲零 CPU | 菜单是事件驱动浮层，无常驻动画 |

## 5. 架构分层

```
┌─ 平台层 win32 ──────────────────────────────────────────┐
│ WM_NCRBUTTONDOWN/UP(HTCAPTION) → 屏幕坐标转客户区        │  ← 新增：打通黑洞
│ WM_SIZE / 建窗后 → on_window_state(WindowState)          │  ← 新增：推送状态
│ run_window_op: + Maximize/Restore                        │  ← 新增：两个 op
└──────────────────────────────────────────────────────────┘
                     ↓ 合成 PointerEvent(Down, Right)
┌─ 宿主层 app/mod.rs ─────────────────────────────────────┐
│ UiHost { frameless, system_menu, window_state }          │  ← 新增字段
│ on_pointer: dispatch 后若 res.menu 为空且命中拖动区      │  ← 新增注入点
│             → res.menu = system_menu_items(state)        │
│ 每次分发/绘制前把 window_state 注入线程局部              │  ← 供 MenuFn 读取
└──────────────────────────────────────────────────────────┘
                     ↓ MenuRequest
┌─ 菜单浮层 app/menu.rs ──────────────────────────────────┐
│ 完全复用，零改动                                          │
└──────────────────────────────────────────────────────────┘
                     ↓ MenuAction::Run(|ctx| ...)
┌─ 核心 core.rs ──────────────────────────────────────────┐
│ EventCtx::{maximize, restore, window_state}              │  ← 新增
└──────────────────────────────────────────────────────────┘
```

## 6. API 设计

### 6.1 `WindowState`（`src/event.rs`）

```rust
/// 窗口的当前状态与能力快照。平台经 `AppHandler::on_window_state` 单向推送，
/// 宿主缓存一份，供菜单构建、标题栏按钮图标等读取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    pub maximized: bool,
    pub minimized: bool,
    /// 可最大化。win32 下等价于窗口有 WS_MAXIMIZEBOX（`resizable(false)` 会剥掉它）。
    pub maximizable: bool,
    /// 可最小化。win32 下等价于窗口有 WS_MINIMIZEBOX。
    pub minimizable: bool,
}
```

**默认值必须从 `WindowConfig` 派生，不能是 `Default::default()`。** 这条是硬要求：
`maximizable` 若默认 `true`，一个 `resizable(false)` 的窗口在平台推第一次状态之前
（或 macOS 这种压根不推的平台上）就会给出"最大化可用"这个**看起来合理的错误答案**——
和 `CoreTextEngine` 漏实现 `scale()` 拿默认 1.0 是同一类事故：默认值本身就是错的，
且不报错。故 `UiHost` 构造时即 `maximizable = cfg.resizable`，并配一条契约测试钉住。

### 6.2 `EventCtx` 新增（`src/core.rs`）

```rust
impl EventCtx<'_> {
    /// 当前窗口的状态与能力快照。
    pub fn window_state(&self) -> WindowState;
    /// 请求最大化（已最大化则无操作）。
    pub fn maximize(&mut self);
    /// 请求从最大化/最小化还原。
    pub fn restore(&mut self);
    /// 在 `pos`（逻辑坐标）弹出窗口系统菜单。默认接管之外的手动入口
    /// （如自定义快捷键、标题栏图标左键点击）。
    pub fn show_system_menu(&mut self, pos: Point);
}
```

### 6.3 自由函数（`src/lib.rs` 导出）

```rust
/// 当前窗口的状态快照。**仅在事件回调 / 菜单构建 / paint 期间有效**——
/// 由宿主在进入这些阶段前注入线程局部，与 `theme::current()` 同一机制。
pub fn window_state() -> WindowState;

/// 构建标准窗口系统菜单四项（读 `window_state()` 决定禁用态）。
/// 供 `on_context_menu` 拼接自定义项使用。
pub fn system_menu_items() -> Vec<MenuItem>;
```

之所以需要自由函数而非只靠 `EventCtx`：`MenuFn` 的签名是 `Fn() -> Vec<MenuItem>`，
**不收 `ctx`**。要么加一个收 ctx 的 `on_context_menu_ctx` 变体（API 膨胀，且 `MenuFn`
在 `Tree` 里被克隆进每一级面板，改签名波及面大），要么走线程局部——后者与框架里
`theme::current()` 的既有范式一致，是更小的口子。

### 6.4 Builder（`src/app/mod.rs`）

```rust
App::system_menu(bool)      // 默认 true
Window::system_menu(bool)   // 默认 true，随 WindowRequest 传给子窗宿主
```

非 frameless 窗口该开关无意义（系统标题栏自己有原生菜单），文档写明；不 panic。

### 6.5 `WindowOp` 扩展（`src/event.rs`）

```rust
pub enum WindowOp {
    Minimize, ToggleMaximize, Show, Hide,
    Maximize,   // 新增
    Restore,    // 新增
}
```

macOS 侧必须补上这两个分支才编译得过（`match` 穷尽）——映射为 `zoom` / 未最大化时
`deminiaturize`，并写注释说明"系统菜单本身在 macOS 未启用，但 op 是跨平台的"。

## 7. 平台层：win32 打通细节

### 7.1 消息拦截

```rust
// wnd_proc 新增两条分支，均位于 is_frameless 守卫之下：
WM_NCRBUTTONDOWN if is_frameless(hwnd) => LRESULT(0),   // 吞掉，不交 DefWindowProc
WM_NCRBUTTONUP   if is_frameless(hwnd) => {
    if wparam.0 as u32 == HTCAPTION { nc_right_click(hwnd, lparam); }
    LRESULT(0)
}
```

`nc_right_click` 做三件事：`ScreenToClient` 转坐标 → 合成
`PointerEvent { kind: Down, button: Right, pos: 客户区物理像素 }` → 走既有的
`dispatch_pointer_event` 通路（**不新开一条**，否则 hover/capture/脏区/两段式借用四套
状态各自走偏）。

**`handle_pointer` 需要小重构**：它现在从 `lparam` 自行解包客户区坐标，而 NC 消息的
`lparam` 是屏幕坐标。把坐标解包提到调用方，函数改收 `(x, y)`。改动小，但要一并核对
既有 6 处调用点。

### 7.2 状态推送

```rust
WM_SIZE => { ...既有逻辑...; push_window_state(hwnd); }
// 建窗完成后推一次初始值（否则首次右键之前状态是构造默认值）
```

`push_window_state` 从 `IsZoomed` / `IsIconic` / `GetWindowLongPtrW(GWL_STYLE)` 的
`WS_MAXIMIZEBOX`/`WS_MINIMIZEBOX` 位读出真值——**从 style 位读而不是缓存
`cfg.resizable`**：将来若加运行期 `set_resizable`，缓存那份会悄悄过期。

两段式借用：`push_window_state` 先取完 OS 侧的值，再借 `state_from(hwnd)` 写入。

### 7.3 `run_window_op` 扩展

```rust
Some(WindowOp::Maximize) => { ShowWindow(hwnd, SW_MAXIMIZE); }
Some(WindowOp::Restore)  => { ShowWindow(hwnd, SW_RESTORE); }
```

## 8. 测试策略

### 单元测试（经真实路径，不 mock）

放 `src/app/mod.rs` 测试模块（宿主级）与 `src/event.rs`（纯函数级）：

1. **禁用矩阵**（参数化 4 组）：`system_menu_items` 在 `(maximized, maximizable)`
   四种组合下各项 `enabled` 与预期一致。
2. **默认接管**：frameless host + 拖动区节点，`dispatch_pointer(Down, Right)` 后
   `res.menu` 非空且项为四项系统菜单。
3. **用户覆盖**：同一节点带 `on_context_menu` 时，`res.menu` 来自用户构建器
   （断言项标签），宿主不追加。
4. **开关关闭**：`system_menu(false)` 时右键拖动区不产生 `MenuRequest`。
5. **非拖动区**：右键正文区域不产生系统菜单。
6. **非 frameless**：普通窗口右键拖动区（若有）不产生系统菜单。
7. **契约测试（防静默默认）**：`resizable(false)` 构造出的宿主，其初始
   `window_state().maximizable == false`——不等平台推送就已经是对的。
8. **动作落地**：经 `MenuHost` 真实激活「最小化」项 → `take_window_op() == Some(Minimize)`；
   「还原」→ `Some(Restore)`；「最大化」→ `Some(Maximize)`。

### 截图验证

`examples/frameless.rs` 已有 `screenshot_from_args()`，直接可用：

```
cargo run --example frameless -- --screenshot artifacts/frameless-sysmenu.png --rclick 200 18
```

**但必须写明这张图验的是什么、不是什么**：`--rclick` 合成的是**客户区**右键，走的是
宿主注入点；win32 上真实用户的右键走的是 `WM_NCRBUTTONUP`。**截图覆盖不到那一段。**
把这条当成"已验证"是最容易犯的错——菜单在截图里长得完全正确，真窗口里可能一下都弹不出来。

### 手工验证清单（win32 真窗口，必须逐条跑）

- [ ] 右键标题栏空白 → 弹**主题化**菜单；**不**出现系统灰色原生菜单（两个都出=没吞 DefWindowProc）
- [ ] 右键标题栏上的窗口按钮 → 不弹菜单（`interactive_at` 优先级生效）
- [ ] 右键标题栏里的 `clickable()` 入口（frameless 示例的「历史」）→ 行为符合预期
- [ ] 最大化后右键 → 「还原」可用、「最大化」置灰；点「还原」真的还原
- [ ] `resizable(false)` 窗口 → 「最大化」「还原」双灰
- [ ] `ctx.open_window` 开出的子窗右键 → 同样生效，且用的是**子窗自己的**状态
- [ ] 靠近屏幕右/下边缘时菜单不出屏（`MENU_EDGE_MARGIN` 生效）
- [ ] 弹过菜单后**拖动窗口仍正常**、Aero Snap 未失效（吞 `WM_NCRBUTTONDOWN` 的副作用检查）
- [ ] 双击标题栏仍能最大化/还原（`WM_NCLBUTTONDBLCLK` 未被殃及）
- [ ] 150% DPI 下菜单弹在光标处而非偏移位置
- [ ] 明暗两套主题下菜单配色正确

### 回归面

- `cargo build --examples`（`WindowOp` 加变体、`handle_pointer` 改签名）
- macOS 侧必须实机编译 + 跑一遍 frameless 示例（平台层改动两边都要跑，见记忆）

## 9. 分期实施顺序

每一期独立可提交、可验证，**顺序不可换**（后一期依赖前一期的验证结论）：

| 期 | 内容 | 验收 |
|----|------|------|
| **P1** | `WindowState` + `WindowOp::{Maximize,Restore}` + `EventCtx` 三个方法 + 平台推送（win32 实推、macOS 留缝） | 契约测试 7、单测 8 绿；两平台编译 |
| **P2** | `system_menu_items()` + 宿主注入点 + `system_menu(bool)` 开关 + 线程局部注入 | 单测 1–6 绿；`--rclick` 截图 |
| **P3** | win32 `WM_NCRBUTTONDOWN/UP` 打通 + `handle_pointer` 重构 | 手工清单全过 |
| **P4** | 文档（`API_GUIDE` 章节 + 平台差异表）、`CHANGELOG`、`examples/frameless.rs` 演示覆盖用法 | `cargo build --examples` |

### 实施后的修正（与上面的方案有出入之处）

1. **默认接管加了平台门控**（`fill_system_menu` 里 `cfg!(target_os = "windows")`）。原方案
   以为"macOS 侧留缝不实现"就等于不生效——错了：注入点在**宿主层**、完全平台无关，
   而 macOS 的 frameless 右键本就直达控件树，不门控它照样会弹。且 macOS 不推送窗口状态，
   弹出来「还原」永远是灰的。宁可不弹，也不弹一个状态说谎的菜单。
2. **必须显式置 `res.repaint = true`**。原方案漏了这条：平台层只在 `on_pointer` 返回 true 时
   `InvalidateRect`。不置它，菜单已经建在宿主里却一直不上屏——**逻辑测试全绿**，只有真窗口
   才看得见。既有的 `on_context_menu` 路径没有显式置它却能工作，靠的是紧随其后的
   `WM_RBUTTONUP`（菜单浮层的 `Up` 分支无条件返回 true，顺带触发了那一帧）；NC 路径刻意
   不合成 `Up`，那份"顺带"就没有了。
3. **`handle_pointer` 用加一层而非改签名**：拆出 `handle_pointer_at(hwnd, kind, button, x, y)`，
   原函数保留为解包 lParam 的薄包装。既有 5 处调用点因此一行没动。
4. **踩到一个 API 陷阱**：`MenuItem::run(label, f, bool)` 的第三个参数是 `checked`，而紧邻的
   `MenuItem::key(label, key, bool)` 是 `enabled`。同形状、同为 bool、含义不同，写错不报错。
   `system_menu_items` 一律走 `.enabled(..)` builder 绕开；`API_GUIDE` §8.2 加了警示。

### 顺带可做（不默认包含，需单独确认）

- **`WindowButton::Maximize` 的还原图标**：P1 落地后，该按钮就能读到 `window_state()`，
  最大化时改画 Windows 的双叠框还原图标。这是个**既有缺口**（现在最大化后图标不变），
  修它的成本在 P1 之后近乎为零，但它不在本次需求范围内。
- **`Alt+Space` 唤起**：拦 `WM_SYSCOMMAND` 的 `SC_KEYMENU`（wparam 低位、lParam=' '），
  改弹自绘菜单于标题栏左下角。Windows 惯例的另一半，但属于键盘通路，独立成期更清楚。

## 10. 风险与需要盯住的地方

| 风险 | 症状 | 对策 |
|------|------|------|
| `WM_NCRBUTTONUP` 未吞 DefWindowProc | 自绘菜单与灰色原生菜单**同时**出现 | 手工清单第 1 条专盯；分支恒返回 `LRESULT(0)` |
| 吞 `WM_NCRBUTTONDOWN` 波及拖动/Snap | 右键过后窗口拖不动 | 手工清单第 8 条；守卫限定 `is_frameless` |
| NC 坐标未转换或漏 `scale` | 高 DPI 下菜单弹在偏离光标几十像素处 | 手工清单第 10 条；坐标转换集中在 `nc_right_click` 一处 |
| `WindowState` 默认值撒谎 | 对话框式窗口"最大化"可点，点了没反应 | §6.1 从 `WindowConfig` 派生 + 契约测试 7 |
| 线程局部未按窗口刷新 | 多窗口下 A 窗的菜单读到 B 窗的状态 | 注入点放在**每次**分发/绘制入口，不做"变了才写"的优化 |
| 模态浮层下的标题栏 | 对话框弹出时右键标题栏仍出系统菜单 | **刻意保持**：与既有"模态下标题栏仍可拖窗"（`hit_test_for_drag` 穿透遮罩）一致，两者同源 |
| `handle_pointer` 签名重构 | 既有 6 处调用点漏改一处 | 改签名靠编译器兜底，但**触摸提升路径**（`is_touch_event`）需人工复核 |
