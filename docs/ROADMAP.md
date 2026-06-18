# windui — 实施路线图（MVP）

> **状态：MVP 完成（Phase 0–6 全部交付并经独立审查）。** 实测 release 二进制 0.49MB、
> 私有内存 3.65MB（520×560 窗口）。详见 README。

每个阶段都**必须能编译运行并产出一张可验证的截屏 PNG**，完成后做一次审查（code-reviewer）再 commit。

截屏机制：demo 支持 `--screenshot <路径>` 模式 —— 启动 → 布局 → 渲染一帧 → `pixmap.save_png()` → 自动退出。CI/自动化与人工都可直接比对 PNG。

---

## Phase 0 — 工程骨架 + 呈现管线 ✅ 验收：纯色窗口截屏
- `Cargo.toml`（依赖 + release 体积裁剪）、模块骨架。
- `geometry.rs`：Point/Size/Rect/Insets/Color。
- `arena.rs`：generational arena + NodeId。
- `platform/windows`：注册窗口类、WndProc、消息循环、`CreateDIBSection` + `BitBlt` 呈现。
- tiny-skia `Pixmap` 后备缓冲，清屏填色。
- `--screenshot` 模式 + `pixmap.save_png`。
- **验收**：弹出纯色窗口；截屏 PNG 为预期底色。

## Phase 1 — 核心树 + 布局 + 图形 ✅ 验收：彩色矩形布局截屏
- `node.rs` + `Widget` trait + 三阶段（measure/arrange/paint）。
- `spec.rs`：MeasureSpec/Dimension/Gravity。
- `Canvas` trait + tiny-skia 实现（fill_rect/round_rect/stroke/line/circle/clip/translate）。
- `LinearLayout`（Column/Row，两遍扫描 + weight）+ `FrameLayout`。
- `Panel`（背景/圆角/边框块）作为首个可见控件。
- **验收**：嵌套 Row/Column + 彩色 Panel，截屏布局正确。

## Phase 2 — DirectWrite 文字 ✅ 验收：中文文字截屏
- `text/dwrite.rs`：IDWriteFactory 单例、测量、灰度 AA 字形位图合成进 pixmap。
- 测量缓存。`Label` 控件（字体/字号/颜色/对齐）。
- `Canvas::draw_text`。
- **验收**：窗口显示中英文混排文字，截屏清晰。

## Phase 3 — 事件 + Button + 焦点 ✅ 验收：交互态截屏 + 回调日志
- 事件类型 + hit-test + 冒泡 + `EventCtx` + 指针捕获 + hover。
- `FocusManager`（Tab 导航）。
- `Button`（normal/hover/press 三态 + on_click）。
- 脏标记驱动的局部重绘。
- **验收**：模拟/真实点击触发回调；hover/press 态截屏正确。

## Phase 4 — 基础输入控件 ✅ 验收：表单截屏
- `TextInput`（单行：光标、选区、键盘输入、占位符）。
- `CheckBox` / `RadioButton`(+Group) / `Switch` / `Slider` / `Dropdown`。
- **验收**：综合表单截屏，各控件状态正确。

## Phase 5 — 容器 / 导航 ✅ 验收：综合 demo 截屏
- `ScrollView`（滚动 + 滚动条 + 滚轮）。
- `Tabs`（标签切换）、`Divider`、`Dialog`（模态遮罩）。
- **验收**：含滚动/标签/弹窗的综合界面截屏。

## Phase 6 — 综合 demo + 自动截屏收尾 ✅ 验收：fullshowcase
- `examples/fullshowcase`：集中展示全部控件 + 多场景。
- 截屏脚本：一键生成各场景 PNG 供回归比对。
- README + API 用例。
- **验收**：fullshowcase 多张截屏齐全，内存占用实测记录。

---

## 阶段工作流（每个 Phase 固定动作）
1. 实现该阶段代码。
2. `cargo build` + `cargo clippy` 通过。
3. 运行 demo `--screenshot` 生成 PNG，人工/工具核对渲染。
4. **code-reviewer 审查**（独立 lane，不自审）。
5. 修复审查问题。
6. `git commit`（conventional commit，简洁，不含 AI 元信息）。

## 内存验收
每阶段记录 demo 运行时工作集（任务管理器 / `Get-Process`）。目标终值 2–5MB（典型小工具尺寸）。

---

# Post-MVP 规划

> 来源：一次外部架构分析 + 本仓库实际代码核对后的采纳决策。
> **定位红线**：始终是"轻量小工具"。两条不可动摇的约束——
> ① **不切换软件渲染架构**（不引入 GPU/D2D/WGPU），维持 tiny-skia CPU 软光栅；
> ② 不为"通用大型框架"过度工程，凡增复杂度/内存的设计须先证明对小工具有净收益。
>
> 每项落地仍遵循 MVP 的阶段工作流（实现 → build/clippy → 截图验证 → 独立 code-reviewer → commit），
> 并按"新控件/能力接入清单"（见 `AGENTS.md`）接入主题、示例与契约测试。

## 采纳决策矩阵

图例：🟢 采纳　🟡 可选/按需　🔴 暂缓或排除

### 功能能力
| 项 | 评级 | 优先级 | 说明 |
|----|------|--------|------|
| 原生文件对话框（打开/保存/选目录） | 🟢 | P0 | 自绘 `dialog` 无法取真实文件路径，近乎必需 |
| 系统托盘（图标/菜单/事件） | 🟢 | P1 | 小工具核心驻留场景 |
| 拖拽文件输入（OLE Drag & Drop） | 🟢 | P1 | 桌面小工具高频交互 |
| 虚拟化列表（仅渲染可见视口） | 🟢 | P1 | `list` 每行一个 Node，海量数据爆炸（已知限制） |
| TextInput 水平滚动 | 🟢 | P1 | 已记录缺陷 |
| 全局热键 | 🟡 | P2 | 仅"后台唤出"类需要 |
| 无边框窗口 + 自定义标题栏 | 🟡 | P2 | 视觉现代化，按产品需要 |
| 暗色模式自动感知 | 🟡 | P2 | 有价值；Mica/Acrylic 仅锦上添花 |
| 多窗口（设置子窗 / Tooltip） | 🟡 | P2 | 多数小工具单窗够用，改动大 |
| 轻量补间动画（Tween / Easing） | 🟡 | P2 | 补 fling 之外的通用插值；保持轻量 |
| TextInput 撤销/重做 | 🟡 | P2 | 常用但非紧急 |
| 富文本 / 行内混排 | 🔴 | — | 小工具少需、成本高 |

### 架构演进
| 项 | 评级 | 优先级 | 说明 |
|----|------|--------|------|
| 系统服务层 SPI（`SystemService` trait 群） | 🟢 | P0 | **地基**：统一剪贴板/IME/对话框/托盘，承载多项功能并为 macOS 铺路；呼应既有"平台收口" |
| 虚拟列表 Adapter 模式 | 🟢 | P1 | 同上痛点 |
| 脏矩形局部重绘 + 增量布局 | 🟢 | P1（按需） | 见下"现状勘误"；不涉 GPU、最契合约束。触发条件=大窗/频繁局部动画致 CPU 偏高 |
| 多 UiHost + 单消息循环（多窗口） | 🟡 | P2 | 价值明确但改动大，与多窗口功能绑定 |
| Signals 响应式数据流 | 🔴 | — | 对小工具偏过度工程；`Rc<Cell>` 已够。若真遇多级联动，作为**可选层**引入、不替换现模型 |
| GPU / 软硬混合光栅 | 🔴 | — | 违反定位红线①，明确排除 |

## 现状勘误（重要）

外部分析称框架"仅脏标记时才布局、Clip Rect 脏区裁剪避免全屏绘制"——**与现状不符**。
实际 `WindowState::paint`：每次重绘 = 全缓冲 `fill` 清屏 → 全树 `layout_root` 重新布局 → 全树 `paint` →
全窗 `SetDIBitsToDevice` 拷屏；事件重绘走 `InvalidateRect(hwnd, None)` 全窗失效。
`mark_dirty` 仅决定"是否触发一次（全量）重绘"，**当前无任何脏矩形/增量布局**。

因此在"不动渲染架构"前提下，**脏矩形局部重绘 + 增量布局是最大且最对路的 CPU 优化空间**
（也是 fling/progress 动画期 CPU 敏感的根因）。对当前小窗口够快，故定为 P1 按需。

## 建议落地顺序

- **P0 地基**：抽 `SystemService` SPI（收编现有 `ClipboardProvider`/IME 进统一接口）→ 首个消费者落**原生文件对话框**（即用即验接口）。
- **P1 高价值**：系统托盘 → 拖拽文件 → 虚拟列表 → TextInput 水平滚动；脏矩形重绘按 CPU 实测插入。
- **P2 增强**：全局热键 / 无边框标题栏 / 暗色感知 / 多窗口 / 轻量补间 / 撤销重做。
- **不做**：Signals 响应式、富文本、GPU。
