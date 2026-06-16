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
