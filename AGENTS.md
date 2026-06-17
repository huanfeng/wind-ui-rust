# AGENTS.md — windui 开发手册

> 面向在本仓库**改代码**的 AI agent 与贡献者。讲清"怎么干、守什么规矩、别踩什么坑"。
> 用户/库使用者请看 [`docs/API_GUIDE.md`](docs/API_GUIDE.md)；架构原理看 [`docs/DESIGN.md`](docs/DESIGN.md)。

windui = 轻量 Windows 桌面 GUI 框架（Win32 窗口 + GDI 呈现 + tiny-skia 图形 + DirectWrite 文字，无运行时、无 GC）。
核心指标是**极低内存**（私有内存 ~3.6MB）与**空闲零 CPU**——任何改动都不应破坏这两点。

## 文档地图（先读哪个）
| 我要… | 看 |
|-------|-----|
| 用这个库写应用 | `docs/API_GUIDE.md` |
| 懂为什么这样设计（内存模型、三阶段布局、借用规避） | `docs/DESIGN.md` |
| 看已交付的阶段与验收方式 | `docs/ROADMAP.md` |
| 在仓库里加控件/改框架 | **本文件** |

---

## 0. 黄金法则（TL;DR）

1. **Widget 是纯内容**——不持有、不访问节点树。跨节点协调一律走共享状态 `Rc<Cell<T>>`。
2. **控件不硬编码视觉**——颜色/间距/字号一律读 `theme::current()`，新控件须接入 `Theme`。
3. **平台差异收口在 `platform/` 层**——控件与核心层保持平台无关（为 macOS 预留）。
4. **OS 重入前释放借用**——平台层调用可能回调进 `wnd_proc` 的 API 前，先放掉 `state` 的可变借用（两段式）。
5. **作者与审查分离**——实现后由独立 `code-reviewer` 审，不在同一上下文自审自批。
6. **提交不含 AI 元信息**——简体中文 conventional commit，无 `Constraint/Rejected/Confidence` 等 trailer、无 `Co-Authored-By`。

---

## 1. 环境与命令

```bash
cargo build                              # 编译库
cargo build --examples                   # 编译所有示例（改公共 API 后必跑）
cargo clippy --all-targets               # lint（零警告才算过）
cargo test                               # 单元测试（当前 62 个，须全绿）

cargo run --release --example fullshowcase           # 运行综合示例窗口
cargo run --example <name> -- --screenshot out.png   # 离屏渲染存 PNG
powershell scripts/screenshots.ps1                   # 一键生成所有示例截屏
```

平台：Windows，PowerShell 为主（Bash 工具可用于 POSIX 脚本）。包管理 cargo。

---

## 2. 仓库地图

| 路径 | 职责 |
|------|------|
| `src/lib.rs` | crate 门面 + `prelude` |
| `src/app.rs` | `App` 构建器 + `UiHost`（交互宿主：render / 事件 / 焦点 / 菜单 / 触摸惯性） |
| `src/core.rs` | `Tree`（generational arena）+ `Node` + `Widget` trait + `EventCtx` + 三阶段布局 + 事件分发 |
| `src/ui/mod.rs` | `Element` 构建器（控件构造 + 链式修饰符 + `build`） |
| `src/ui/{inputs,select,progress,stepper,list,containers}.rs` | 各 `Widget` 实现 |
| `src/geometry.rs` | `Color / Point / Size / Rect / Insets` |
| `src/spec.rs` | `Align / Axis / Dimension / MeasureSpec` |
| `src/style.rs` | `Style`（内联视觉属性） |
| `src/theme.rs` | `Theme / Palette / Metrics` + thread_local `current()/set_current()` |
| `src/event.rs` | `Event / PointerEvent / KeyEvent / Key / MenuItem` |
| `src/anim.rs` | `request_repaint()` + 帧时钟 |
| `src/render/{mod,skia}.rs` | `Canvas` trait + tiny-skia 后端 |
| `src/text/{mod,dwrite}.rs` | `TextEngine` + DirectWrite |
| `src/platform/{mod,win32/mod,win32/clipboard}.rs` | `AppHandler` trait + Win32 窗口/消息循环/剪贴板 |

**热点文件**（改动频繁且牵一发动全身，改前务必通读相关段落）：`src/ui/inputs.rs`、`src/platform/win32/mod.rs`、`src/core.rs`。

---

## 3. 架构铁律（违反即返工）

1. **Arena 存树，非 `Rc<RefCell>`**：节点在 `Tree` 的 generational arena，`NodeId{index,generation}` 删除后代际失效。布局递归由 `Tree` 独占 `&mut self` 驱动——从根上规避借用冲突。
2. **三阶段**：`measure`（控件返回固有尺寸）→ `arrange`（框架定位，控件不参与）→ `paint`（控件在分配到的 `bounds`/`content` 内绘制）。控件只实现 `measure`/`paint`/`on_event`。
3. **坐标系**：控件树用**逻辑坐标**，绘制层统一 ×`scale` 到物理像素；`on_event` 收到的 pos 已是逻辑坐标。文字测量与绘制按**物理字号同源**，勿混用。
4. **视觉走主题**：控件 `paint` 读 `theme::current()`；`Theme` 两层（`palette`+`metrics` 基底 / 每控件 `Option<Color>` 覆盖层，`None` 回退 palette）。详见记忆与 `docs/API_GUIDE.md` §7。
5. **平台缝合**：点击计数 / 剪贴板 / 输入法光标 / 触摸 / 动画驱动等平台差异，全部以 trait 收口在 `platform` 层；核心与控件零平台依赖。
6. **两段式借用**：`win32` 里凡是先借 `state_from(hwnd)`、随后又要调 `InvalidateRect`/`DestroyWindow`/弹菜单等可能同步重入 `wnd_proc` 的 API 时，**先把意图读出/写完并释放 state 借用，再调 OS API**。参考 `dispatch_pointer_event`/`dispatch_pan`/`dispatch_fling`。
7. **空闲零 CPU**：无动画时阻塞在 `GetMessageW`；需要持续动画的控件在 `paint` 内调 `anim::request_repaint()`，宿主按显示器刷新率（≤60fps，动画期 `timeBeginPeriod(1)`）配速。停止请求即回到零 CPU。

---

## 4. 新增控件接入清单（逐项勾完才算完成）

1. [ ] `Widget` 实现于 `src/ui/<name>.rs`；`paint` 读 `theme::current()`，**不硬编码**颜色/间距/字号。
2. [ ] `Theme` 加对应覆盖层 struct（`Option<Color>` 字段 + 回退方法），并接入 `Theme` 聚合 + TOML（`#[serde(default)]`）。
3. [ ] `Element::<name>()` 构造器（`src/ui/mod.rs`）+ 模块注册 + 必要的 `pub use` 导出。
4. [ ] **在 `examples/fullshowcase.rs` 的"控件"标签页加展示卡片**（硬性要求，集中展示避免单页过长）。
5. [ ] 契约单测：经真实 `dispatch_pointer`/`dispatch_key` + `abs_bounds` 验证交互，**不写 mock**。
6. [ ] 独立 `code-reviewer` 审查 → 修复 → 截图验证 → 单独 commit。

状态绑定沿用统一模型：开关 `Rc<Cell<bool>>`、索引 `Rc<Cell<usize>>`、数值 `Rc<Cell<f32/f64>>`、文本 `Rc<RefCell<String>>`。

---

## 5. 测试与验证规范

- **单测经真实路径**：构造 `Tree`、`build` Element、跑 `layout_root` + `dispatch_*`，断言 `scroll_y`/`abs_bounds`/状态变化。不 mock 框架内部。
- **截图验证**（离屏渲染，`App::screenshot_from_args()`）：
  - `--screenshot <path>`：渲染一帧存 PNG。
  - `--scale <f>`：高 DPI 截图（验证 1.5x/2x 锐利度）。
  - `--click X Y` / `--rclick X Y`：截图前合成左/右键（验证下拉展开、标签切换、列表选中、右键菜单等）。
  - 有动画时离屏路径自动前进一帧。
- 改公共 API 后跑 `cargo build --examples` 确认全部示例适配。
- 触摸惯性等**只能真机验证**的特性，代码自洽 + 单测覆盖可测部分后，明确请用户在触摸屏实测，别声称"已验证"。

---

## 6. 开发工作流

1. 宽泛需求先探索再动手；2+ 独立任务并行。
2. 实现。
3. `cargo build` + `cargo clippy --all-targets` 零警告。
4. `--screenshot` 截图核对渲染（视觉改动必做）。
5. **独立 `code-reviewer` 审查**（不自审），按严重度修复。
6. `git commit`（见 §7）。

---

## 7. 提交规范

- **Conventional commit**：`feat(scope): …` / `fix` / `refactor` / `docs` / `test` / `chore`。
- **简体中文** subject + body，简洁说清"做了什么、为什么"。
- **绝不含 AI 元信息**：不要 `Constraint:`/`Rejected:`/`Confidence:`/`Scope-risk:` 等 trailer，不要 `Co-Authored-By`、不要工具署名。
- **原子提交**：一组相关改动一个 commit（如"文档"与"重构"分开）。
- 只在用户要求时提交；默认分支直接提交沿用本仓库现状。

---

## 8. 命名与 API 约定（细则见 API_GUIDE §4）

- 背景/前景统一缩写 **`bg`/`fg`**（`Element::bg`、`App::bg`、`Style.bg`、`EventCtx::set_bg`）。
- 文本标签统一 **`impl Into<String>`**（`button`/`label`/`dropdown`/`list`/`tabs`）。
- 构造器=控件名（名词），布局/样式修饰符=属性名、**不加 `set_`**；getter **不加 `get_`**；`set_` 仅命令式副作用。
- 右键**默认不激活控件**；仅需右键的控件 override `Widget::wants_right_click() -> true`。
- text_input 专属修饰符（`password/multiline/wrap`）链到他控件 **debug 期 `panic`** 提示误用。

---

## 9. 高频陷阱

| 陷阱 | 规避 |
|------|------|
| 触摸惯性中途打断残留偏移 | 任何打断 fling 的路径（新触摸 Down / 指针 Down / pan / cancel）**必经 `clear_fling` 复位 `over_scroll`**。 |
| 动画冻结 / 空转 | 需要动画忘了 `request_repaint` 会冻结；停止条件忘了清请求会每帧空转。每帧 `reset_request` 后由控件/逻辑重新请求。 |
| DPI 坐标混用 | 控件内一律逻辑坐标；物理像素只在平台层/绘制层出现。 |
| OS 重入借用冲突 | 见铁律 §6 两段式借用。 |
| 改了视觉硬编码颜色 | 走 `theme::current()`，破坏主题一致性会被审查打回。 |
| 改公共 API 漏改示例 | `cargo build --examples` 兜底。 |
| `LF will be replaced by CRLF` 警告 | Windows 换行符提示，无害，忽略。 |

---

## 10. 文档维护（改完别忘了）

- 改/加**公共 API** → 同步 `docs/API_GUIDE.md`。
- 改**架构/不变量** → 同步 `docs/DESIGN.md`。
- 加**控件/能力** → 更新 `README.md` 控件表（必要时 `docs/ROADMAP.md`）。
- 改**开发约定/流程** → 更新本文件。

> 框架处于早期，以"最新设计 + 统一"为准，**不承诺向后兼容**。
