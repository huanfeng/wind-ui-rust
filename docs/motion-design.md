# 动效体系规划（windui）

> 状态：方向已确认（2026-10-20）。M0、M1、M2 已实现。承接 [`animation-design.md`](animation-design.md)（Phase A/B 已交付、Phase C 未做）。
> 本文先对照主流框架找出缺口，再按"对用户可见度 / 成本"排出分期。

## 0. 结论先行

windui 现在有一个**合格的补间内核**（`Transition<T>` + 4 条三次缓动 + 全局开关 + 时长主题），
并且**所有基础交互控件的状态色/滑块都已接上**。与主流框架相比，差距不在"能不能插值"，而在四件事：

1. **进场/离场**（presence）：`visible_when` 是瞬时的——对话框、下拉弹层、菜单、折叠体出现消失都是"啪"一下。
   这是用户最能直接感到"不够顺"的地方，也是所有主流框架都内置的能力。
2. **尺寸/位置变化**（layout animation）：布局结果一变就跳到新位置；折叠展开、列表增删、内容变高全是瞬变。
3. **可打断的物理运动**：只有时间曲线，中途改向时速度归零（有"折角"）；没有弹簧。
4. **动画期的成本**：软件光栅下，`opacity` 每帧分配整窗离屏层、`offset` 变化触发整窗重绘——
   动效一多，就和"极低内存 / 低 CPU"的核心指标正面冲突。**这一条必须和功能一起做，不能事后补。**

推荐路线：**M0 内核补强 → M1 进出场 → M2 展开高度 → M3 节点级隐式过渡 + 成本收口 → M4 布局 FLIP 与内容切换**。
M1+M2 做完，用户可感知的"动效不足"基本消失；M3/M4 是把能力从"控件作者专属"开放给应用作者。

---

## 1. 主流框架怎么做

| 框架 | 隐式（状态变→自动过渡） | 显式（手控时间线） | 进出场 | 布局/尺寸 | 物理 | 减弱动效 |
|------|------|------|------|------|------|------|
| **CSS / Web** | `transition: prop dur ease` | `@keyframes` / WAAPI | 无原生（需 JS，或 `@starting-style` + `transition-behavior: allow-discrete`） | View Transitions API、FLIP 手法 | 无（`linear()` 近似弹簧） | `prefers-reduced-motion` |
| **Framer Motion** | `animate={{…}}` 声明目标值 | `useAnimate` / 时间线 | `AnimatePresence` + `exit` | `layout` 属性（自动 FLIP）、`layoutId` 共享元素 | 默认弹簧，保速度 | `useReducedMotion` |
| **Flutter** | `AnimatedContainer`/`AnimatedOpacity` 等 Implicitly 系列 | `AnimationController` + `Tween` + `Curve` | `AnimatedSwitcher`、`AnimatedList` | `AnimatedSize`、`Hero` | `SpringSimulation`，`animateWith` | `MediaQuery.disableAnimations` |
| **SwiftUI** | `.animation(_, value:)` / `withAnimation {}` | `TimelineView`、`PhaseAnimator`、`KeyframeAnimator` | `.transition(.opacity.combined(with: .move))` | 布局变化默认参与动画；`matchedGeometryEffect` | `.spring(response:dampingFraction:)` 为默认 | `accessibilityReduceMotion` |
| **Jetpack Compose** | `animate*AsState` | `Animatable`、`updateTransition`、`InfiniteTransition` | `AnimatedVisibility(enter, exit)`、`AnimatedContent` | `Modifier.animateContentSize()`、`animateItem()` | `spring()` 默认、保速度 | 系统动画缩放 |
| **WinUI / Composition** | Implicit Animations（属性变即动）、`ThemeTransitions` | `KeyFrameAnimation`、Storyboard | `EntranceThemeTransition`、`PopupThemeTransition` | `RepositionThemeTransition`、`ConnectedAnimation` | `SpringVector3NaturalMotionAnimation` | `UISettings.AnimationsEnabled` |
| **Qt QML** | `Behavior on prop { NumberAnimation }` | `PropertyAnimation`、`SequentialAnimation` | `States` + `Transitions` | `ListView` 的 `add/remove/displaced` 过渡 | `SpringAnimation` | — |
| **GTK4 / libadwaita** | — | `AdwTimedAnimation` | `GtkRevealer`、`GtkStack` 过渡 | `GtkRevealer`（展开） | `AdwSpringAnimation` | `gtk-enable-animations` |
| **Slint** | `animate prop { duration; easing }` | `states` + `transitions` | — | — | `ease-out-elastic` 等曲线 | — |
| **egui / iced** | `ctx.animate_bool` / `animate_value`（一个值） | — | — | — | — | — |

### 共性：五个能力层

主流方案拆开看都是同样五层，差别只在 API 风格：

| 层 | 内容 | windui 现状 |
|----|------|------|
| **L1 曲线** | 缓动曲线（含 cubic-bezier）、弹簧 | 🟡 只有 4 条三次曲线，无 bezier、无弹簧 |
| **L2 值补间** | 单值随时间插值，可中途改向 | ✅ `Transition<T>`，但改向时速度归零；`Lerp` 只支持 `f32`/`Color` |
| **L3 隐式属性过渡** | 属性写入新值时自动过渡（CSS transition / Compose `animate*AsState` / WinUI Implicit） | 🟡 每个控件**手写** retarget-in-paint；应用作者没有入口 |
| **L4 结构过渡** | 进出场、内容切换、尺寸变化、布局位移（presence / switcher / size / FLIP） | ❌ 全部缺失（仅 toast 淡入淡出、拖动排序的行位移是个案） |
| **L5 编排** | 序列、交错（stagger）、关键帧、共享元素 | ❌ 缺失 |

另外两件横向的事：**动效 token**（Material 3 / Fluent 都把"时长 × 曲线"做成设计 token），以及**减弱动效**的分级处理。

### 值得借鉴的设计判断

- **弹簧做默认是有理由的**（SwiftUI、Compose、Framer）：中途改向时延续当前速度，hover 快速进出、反复点击开关不出现"折角"。
  对 hover 这类高频打断场景，体感差异明显。
- **进出场必须由框架托管"离场中"状态**（`AnimatePresence`、`AnimatedVisibility`）：节点在逻辑上已经隐藏，
  但还要再画几帧。控件自己做不到——它没有"我已被隐藏但还在画"的生命周期。windui 的 `vis_cond` 正好是那个拦截点。
- **布局动画用 FLIP 而不是逐帧重排**（Framer `layout`、Compose `animateItem`）：布局只算一次终态，
  用变换把元素从旧位置"拉回"再放开。这和 windui 的 `Node::offset`（绘制/命中偏移、不改 `bounds`）天然契合，
  `reorder.rs` 已经这么做了，只是没有推广。
- **展开用"测自然尺寸 × 进度 + 裁剪"**（`GtkRevealer`、`animateContentSize`）：只有父链需要每帧重排，子树按终态布局一次。
- **减弱动效 ≠ 关掉所有动画**（Apple HIG、WCAG 2.3.3）：位移/缩放/视差改成淡入淡出，颜色过渡保留。
  现在的全局开关是一刀切。

---

## 2. windui 的约束（任何方案都得过这几关）

| 约束 | 对动效设计的含义 |
|------|------|
| 空闲零 CPU | 动画结束必须停止续帧；不允许常驻的 ticker / 动画注册表轮询 |
| 低内存 | 不按节点常驻动画状态；需要状态时**懒分配**（`Option<Box<…>>`），结束后回收 |
| 软件光栅、paint-bound | 每帧成本 ≈ 重绘面积。**动效的损伤区必须收窄到受影响矩形**，否则一个 200ms 淡入就是 12 帧整窗重绘 |
| `Widget::paint(&self)` | 控件内补间继续走 `Cell<Transition>` + retarget-in-paint；框架级动画存 `Node` 上，在 `Tree` 的 `&mut` 阶段推进 |
| 三阶段布局、arena | 布局动画不改 `bounds`（那是布局结果），走 `offset`/裁剪/测量缩放 |
| 截图回归 | 默认截终态；另外需要能**确定性**地截中间帧 |

### 已知的成本隐患（现状代码里）

1. `SkiaCanvas::push_layer`（`src/render/skia.rs:906`）每次分配**与整窗同尺寸**的 pixmap。
   一个节点做 opacity 淡入，就是每帧分配并合成一张整窗缓冲（620×556@2x ≈ 5.5MB/帧）。
   → 层应按子树的 `abs_bounds ∩ cull` 分配。
2. `Node::offset` 进了结构签名（见 `src/ui/reorder.rs:747` 的注释），位移一变就整窗重绘。
   → 位移的损伤应为"旧矩形 ∪ 新矩形"。
3. `Node::offset` 是整数逻辑像素：2x 屏上一步就是 2 物理像素，慢速位移会一顿一顿。
   → 动画位移需要亚像素（物理像素）精度。
4. macOS 的"减弱动态效果"查询是 TODO（`src/platform/macos/mod.rs:109`），Linux 完全没查系统设置。

---

## 3. 分期规划

### M0 —— 内核补强（小、无可见变化，后面全依赖它）　✅ 已实现

> 落地情况：曲线、弹簧（解析解 + 改向继承速度）、`Point/Size/Rect/Insets` 插值、主题 token、
> `WINDUI_ANIM_SCALE`、`--anim-at`、三平台系统设置查询、`MotionLevel` 三档 + `App::reduced_motion`
> 均已落地；既有控件里的位移类补间（开关滑块、分段胶囊、标签页指示条、拖动排序、富文本卷帘、
> 光标滑行）已标 `.movement()`。未做：元组插值、`--frames` 帧序列（无实际需求，按需再加）。

**目标**：把 L1/L2 补到主流水平，并补上调试/测试设施。

- **曲线**：`Easing::CubicBezier(x1, y1, x2, y2)`；预置 token 曲线
  `Standard`（0.2,0,0,1，即 Material 3 / Fluent 的 decelerate 近似）、`Accelerate`、`Decelerate`、`Emphasized`。
- **弹簧** `Spring { stiffness, damping }`（或 `response/damping_ratio` 参数化，同 SwiftUI，更好调）：
  解析解（欠阻尼/临界阻尼两条闭式），**无积分、无状态步进**，所以仍然只靠 `clock_ms()` 取值，可确定性截图。
  `retarget` 时把当前速度带进新弹簧——这是弹簧相对时间曲线唯一的、也是决定性的优势。
  接口上让 `Transition` 持有 `Motion = Tween(dur, easing) | Spring(params)`，调用点不变。
- **`Lerp` 扩展**：`Point`/`Size`/`Rect`/`Insets`（f32 版本）、`(A, B)` 元组。
- **动效 token**：`AnimTheme` 增加曲线与弹簧档位（`standard / emphasized / spring_snappy / spring_gentle`），
  TOML 可配，沿用两层主题回退。
- **调试**：`WINDUI_ANIM_SCALE=<f>` 全局时间放慢倍数（对标 Android 动画缩放、Flutter `timeDilation`）——
  肉眼查动画只能靠这个。
- **截图**：`--anim-at <ms>`：合成交互后把时钟拨到第 N 毫秒再截，用于中间帧回归；
  （可选）`--frames <n>` 输出帧序列。
- **系统设置**：补 macOS `accessibilityDisplayShouldReduceMotion`、Linux `gtk-enable-animations`
  （GSettings `org.gnome.desktop.interface enable-animations`），并在 `anim` 层区分
  `MotionLevel::{Full, Reduced, Off}`——`Reduced` 时位移/缩放类动画退化为淡入淡出，颜色过渡保留。

**验收**：纯单测（bezier 端点/单调、弹簧收敛/改向速度连续/临界阻尼不过冲、ANIM_SCALE 生效）。零控件改动。

### M1 —— 进出场（presence）：优先级最高　✅ 已实现

> 落地情况：`Element::transition(Presence)` + 节点上懒分配的 `PresenceState`；布局/绘制看
> `Node::layout_visible()`、命中/焦点/模态仍看 `effective_visible()`；进度沿父链"逻辑可见"改向，
> 初值在 `build` 时按祖先可见落定。已接：对话框、`popup` 浮层（默认 `popover()`）、菜单与悬停提示
> （只淡入）。**与原计划的出入**：
> - 缩放（scale 0.96→1）未做——画布没有变换，留到 M3 的 `translate`/层合成缩放一起做；
> - toast 保持原有的淡入淡出，未改用统一 token / 未加滑动（观感已成立，收益小）；
> - 菜单与提示框只淡入不滑动、收起瞬时（宿主层画布无平移；收起残影会挡住刚选的结果）；
> - 层按子树边界分配（Q3）未提前：子树可经 `offset` 画出自身矩形之外，按边界裁合成会切掉内容，
>   需要先有可靠的子树视觉边界，仍放 M3；
> - 成本：淡入淡出期间每帧按**整窗**分配并合成一张离屏层（软件后端），对话框约 20 帧、菜单 /
>   提示约 7 帧；对话框遮罩本就全屏，菜单与提示的矩形已知，可先加按矩形分配的层（M3 前置）；
> - 已知小瑕疵：同一次回调里"显示祖先 + 隐藏子节点"会让子节点从显示态播一次离场；
>   过渡期脏区按子树视觉范围算、不扣滚动偏移，长列表会退化成整窗帧（结果正确、成本偏高）；
> - 滑动期间命中、输入法候选窗位置按**终位**算（进场 200ms 内点击落在终位，与 Framer 等一致）；
> - 翻转到锚点上方的浮层仍"自上方落下"，方向应随翻转反过来——等锚定摆放（见
>   `anchored-placement-design.md`）给出翻转方向后再接。

**目标**：节点被隐藏时先播离场动画再真正消失；被显示时播进场动画。

- **API**：
  ```rust
  Element::…
      .visible_when(cond)
      .transition(Enter::fade().slide_y(-8), Exit::fade())   // 也可 .transition(Presence::fade())
  ```
  预置：`fade`、`slide_x/slide_y(px)`、`scale(from)`、`expand_y`（与 M2 共用实现），可组合。
- **机制**：`Node` 上懒分配 `PresenceState { phase: Entering|Shown|Exiting, tr: Transition<f32> }`。
  `vis_cond` 从 true→false 时进入 `Exiting`：**绘制继续、命中与焦点立即排除**（离场中的元素不能被点到——
  这是 Framer/Compose 的共同做法）；进度到 0 才视为真正隐藏、释放状态。
  进度通过已有的 `opacity` 与 `offset` 两个通道作用于整棵子树，控件零改动。
- **接入**（按可见度排序）：
  1. `dialog`：遮罩淡入 + 面板 fade + scale(0.96→1)（Fluent/WinUI 对话框的标准动效）。
  2. 下拉弹层 / 上下文菜单 / 菜单栏子菜单：fade + slide_y(-4)，100–150ms（菜单要快）。
     菜单在 `src/app/menu.rs` 自绘，不走节点，需要单独接一份同参数的进度。
  3. tooltip：只淡入（延迟已有）。
  4. toast：已有淡入淡出，改用统一 token，加上 slide。
- **依赖 M3 的成本收口吗？** 弹层/对话框面积小，先用"层按子树边界分配"这一项（M3 第 1 条）即可，
  可以提前到 M1 一起做。

**验收**：契约测——隐藏后离场期间命中测试落空、继续占位；播完要一帧重排把空间让出来；
进度到底后不再绘制（`PresenceState` 本身常驻节点上，几十字节，不回收）；全局关时隐藏当帧即让位，
与不声明过渡逐帧一致；不在绘制路径上（普通隐藏的祖先之下、被模态收起的浮层）时的显隐就地落定，
露出来时不冒残影、不补播；局部帧里子孙投影随之擦净。截图：`--click` 打开对话框 + `--anim-at 70` 截中间帧。

### M2 —— 展开/收起高度（即原 Phase C）　✅ 已实现

> 落地情况：没有单造 `RevealBox`，而是复用 M1 的进出场机制——`Presence::expand()`（预置
> `Presence::reveal()` = 淡入 + 展开）让测量高度 = 自然高度 × 进度、子树裁到当前高度，离场占位、
> 播完让位、减弱档瞬时、不在绘制路径上落定全部沿用；`collapsible` / `accordion` 的 body 默认
> `reveal()`（自带过渡则尊重）。通用修饰符落地为 `.animate_height()` 而非 `animate_size`：宽度一动，
> 子树文字重新折行、反过来改变自然高度，容易自激，只做高度。箭头旋转在 `nav.rs` 里按角度旋转
> 折线点（端点处直接画两个静态箭头，终态逐像素不变），无需画布变换。
> 嵌套时（`animate_height` 里套正在展开的折叠面板）外层直接跟随子树、不另起过渡——追一个
> 移动的目标必然落后；隐藏期间状态被改的控件经新增的 `Widget::settle_offscreen` 就地落定。
> 卷帘裁剪覆盖节点自绘与子树（叶子 body 也不溢出），过渡期不画滚动条。
> 已知代价：过渡期每帧整树重排一次（与富文本卷帘同一做法），不宜挂在长列表的每一行。

**目标**：折叠面板、手风琴、"更多选项"展开不再瞬变。

- 按 `animation-design.md` Phase C 的 `RevealBox` 方案：`measure` 报 `自然高 × progress`，
  `paint` 用 `clip_rect` 裁剪，子树按自然尺寸布局一次。
- 对外两种用法：
  - `collapsible` / `accordion` 内部直接换用（默认带动画，无 API 变化）。
  - 通用修饰符 `.animate_size()`（对标 Compose `animateContentSize`）：任意容器内容变高/变矮时平滑过渡。
    它需要记住上一帧的测量值，在测量值变化时 retarget。
- 难点（需专门测试）：过渡期间父链每帧都要重排，必须确认 `needs_relayout` 在动画帧里被置位、
  且局部帧路径（不 layout）不会吞掉它——参考记忆里"事件外裸写信号不重排"那类坑。
- 折叠头的箭头旋转（chevron 0°→90°）一并做：需要 `Canvas` 支持旋转绘制路径，或改用两帧图形插值。

**验收**：展开过程中相邻兄弟节点随之平移（不重叠、不跳）；收起完成后子树不命中；
**Wrap 宽与显式宽两种用例都覆盖**（AGENTS §5）。

### M3 —— 节点级隐式过渡 + 动画成本收口

**目标**：应用作者不写控件也能做动效；同时把动画帧的成本压回"只重绘动的那块"。

- **隐式过渡**（对标 CSS `transition` / WinUI Implicit Animations）：
  ```rust
  Element::….bg_when(|| hot.get(), a, b).animate(Prop::Bg | Prop::Opacity, Motion::standard())
  ```
  声明后，凡经 `ctx.set_bg` / `set_opacity` / `set_node_offset` 或条件样式写入的变化，自动从当前值过渡。
  状态存 `Node` 上的懒分配 `ImplicitAnims`（只为声明过的属性分配）。
- **成本收口**（这一组比功能本身更重要）：
  1. `push_layer` 按子树 `abs_bounds ∩ 当前裁剪` 分配，而不是整窗；GPU 后端的纹理池同理按尺寸桶复用。
  2. `offset` 变化的损伤 = 旧矩形 ∪ 新矩形，不再进结构签名触发整窗；需要确认命中、浮层、阴影外扩都按此计。
  3. `offset` 改为 f32（或增加一个仅绘制用的亚像素 `paint_translate`），绘制层按物理像素对齐取整。
  4. `Canvas` 增加 `translate(dx, dy)`（save/restore 栈内）——偏移目前是在各绘制调用里手动加的，
     统一成变换后 M1 的 slide、M4 的 FLIP 都直接受益。缩放（scale）只在离屏层合成时做
     （tiny-skia `draw_pixmap` 带变换），不进入全量绘制路径，避免文字缩放的高成本。
- **度量**：用 `WINDUI_PROF` 对比"关于窗打开对话框"这一动画的每帧 paint 时间与峰值私有内存，
  写进验收：**动画期峰值内存不得超过静止态的一张层缓冲（按弹层面积计）**。

**验收**：单测验证局部损伤矩形；截图回归全绿；`measure_footprint` 在同 DPI 下对比无回归。

### M4 —— 布局 FLIP 与内容切换

**目标**：列表增删、重排、内容切换有连续感。

- **FLIP**：`.animate_layout()` 标记的节点，在 arrange 后比较新旧 `abs_bounds`，差值写入 offset 再补间归零。
  `reorder.rs` 的行位移逻辑可以改为调用这套机制（删掉个案实现）。
- **列表项进出**：`dyn_list` / `list` 的项插入（expand_y + fade）、删除（离场 + 后续项上移，FLIP），
  对标 Compose `animateItem`、Qt `ListView.add/remove/displaced`。虚拟列表只对视口内项生效。
- **内容切换** `Element::switcher(key, content)`（对标 `AnimatedSwitcher` / `AnimatedContent`）：
  key 变化时旧内容离场、新内容进场，交叉淡入或方向滑动。接入 `tabs`（方向随下标增减）、`pager`。
  注意：切换期间新旧两棵子树同时存在，内存会短暂翻倍——只对小子树默认开启。

**验收**：增删/重排期间命中测试按**最终**布局（与 Framer 一致，避免点错行）；
快速连续增删不残留偏移（参照"打断 fling 必复位 over_scroll"那条陷阱）。

### M5 —— 编排（按需，不预排期）

序列 / 交错（列表进场时逐项延迟）、关键帧、共享元素（`matchedGeometryEffect`/`Hero`）、数字滚动。
这些在桌面小工具里需求弱，等下游提出再做。

---

## 4. 明确不做

- 不做通用 CSS 式关键帧 DSL、Lottie/矢量动画播放器、3D 变换、独立合成线程。
- 不做"所有属性默认带动画"（SwiftUI 式）：桌面工具里意外的动画比没有动画更糟，一律显式声明。
- 不为动画常驻任何全局注册表或定时器：一切动画状态挂在节点/控件上，随结束回收。

## 5. 待定问题

| # | 问题 | 倾向 |
|---|------|------|
| Q1 | 默认运动模型用弹簧还是时间曲线？ | hover/开关/滑块用弹簧（高频打断）；进出场与展开用时间曲线（时长可预期，便于设计对齐） |
| Q2 | 减弱动效是否默认跟随系统？ | **已定**：默认随系统；策略由使用者配置——`App::reduced_motion(level)` 指定系统要求减弱时落到哪档（默认 `Reduced`），`App::motion(level)` / `animations(bool)` 强制档位 |
| Q3 | M3 的成本收口是否提前到 M1？ | 层按边界分配（第 1 条）提前；其余随 M3 |
| Q4 | 菜单（非节点、`app/menu.rs` 自绘）是否改为节点化以复用 presence？ | 暂不，单独接进度；节点化另议（与锚定摆放设计稿一起考虑） |
| Q5 | 列表项删除动画期间，回调里拿到的数据下标以谁为准？ | 以数据源为准，离场项只是视觉残影、不参与下标 |

## 6. 与现有文档的关系

- 落地后 `animation-design.md` 的 Phase C 指向本文 M2；`ROADMAP.md` 的"轻量补间动画"一行更新为指向本文并按 M0–M4 拆分。
- 每期交付同步 `API_GUIDE.md`（新增修饰符）、`examples/animation.rs`（展示）、`fullshowcase` 控件页（新控件动效）。
