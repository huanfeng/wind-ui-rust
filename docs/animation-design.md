# 动画支持设计（windui）

目标：在既有帧驱动之上补「缓动补间 + 全局开关 + 控件接入」，让状态切换与展开过渡平滑，
且可一键关闭（尊重系统无障碍/省电设置）。

## 现状（已具备的基建）

- 帧请求/时钟：`anim::request_repaint()`（控件 paint 内请求下一帧）、`anim::clock_ms()`（单调帧时钟）。
- 宿主每帧：`reset_request` → `set_clock_ms(elapsed)` → 绘制 → `wants_animation()` 决定是否续帧。
- Win32 帧节流：动画期提 1ms 定时器、按 VREFRESH 限 ≤60fps、`MsgWaitForMultipleObjectsEx` 等到帧截止；
  空闲回到阻塞 `GetMessageW`（零 CPU）。
- 已落地动画：fling/bounce 惯性、不确定进度、tooltip 延时——均手搓 clock 数学。

缺口：①无缓动/补间助手 ②状态切换（hover/press/focus）瞬时 ③折叠/手风琴/下拉展开瞬时 ④无全局开关。

## 架构：四层

### L1 缓动 `Easing`（`anim.rs`）

```rust
pub enum Easing { Linear, EaseIn, EaseOut, EaseInOut }  // 默认 EaseInOut
impl Easing { pub fn apply(self, t: f32) -> f32 }        // t,返回均 [0,1]，三次曲线
```

### L2 补间 `Transition<T>` + `Lerp`

```rust
pub trait Lerp: Copy { fn lerp(self, to: Self, t: f32) -> Self; }   // f32 / Color
pub struct Transition<T: Lerp> { from, to, start_ms, duration_ms, easing }  // Copy
impl<T: Lerp> Transition<T> {
    pub fn new(value: T) -> Self;                                   // 静止于 value
    pub fn target(&self) -> T;
    pub fn retarget(&mut self, to: T, duration_ms: u32, e: Easing); // 从当前值起新动画(start=clock_ms())
    pub fn value(&self) -> T;        // 读 clock_ms()；全局关/时长0/已结束 → to
    pub fn is_active(&self) -> bool; // 读 clock_ms()
    pub fn animate(&self) -> T;      // = value()，且 is_active 时 request_repaint()
}
```

**接入模式（retarget-in-paint）**：`Widget::paint` 是 `&self`，故补间存 `Cell<Transition<T>>`（T Copy）。
每帧 paint：据当前状态算 `target` → 若 `target != tr.target()` 则 `retarget`（此处 clock 最新）→ `animate()` 取值。
事件处理器只需 `mark_dirty()`（已有）。这样动画逻辑全收口在 paint，时钟永远新鲜。

### L3 全局开关（`anim.rs`）

```rust
pub fn enabled() -> bool;        // thread-local，默认 true
pub fn set_enabled(on: bool);
```

- 关闭时 `Transition::value/animate` 直接返回 `to`、`is_active=false`、不续帧 → 所有动画瞬时收敛。
- 默认来源：`App::animations(Option<bool>)` 覆盖；未设则平台查系统「显示动画」
  （Win32 `SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION)`）。`run_windowed` 开循环前 `set_enabled(effective)`。
- 运行期可 `anim::set_enabled` 切换。
- 截图/离屏：现有路径在 `wants_animation` 时 sleep 300ms 前进一帧，≤300ms 的过渡可自然收尾，截图稳定。

### L4 时长主题 `AnimTheme`（`theme.rs`）

```rust
pub struct AnimTheme { fast: Option<u32>, normal: Option<u32>, slow: Option<u32> }  // 回退 120/200/300ms
```
挂 `Theme.anim`，TOML 可配（两层主题模式）。控件从 `theme.anim.normal()` 等取时长。

## 分阶段接入

### Phase A —— 引擎（本轮实现）
L1–L4 + 单测：缓动端点/单调、`Lerp` 插值、`Transition` 收敛与中途 retarget、全局关瞬时、AnimTheme 回退。零控件改动。

### Phase B —— 值动画（本轮实现）
不改布局的「值」过渡，统一 retarget-in-paint + `Cell<Transition>`：
- **Switch**：滑块位置 `Transition<f32>`（0↔1）+ 轨道色 `Transition<Color>`。
- **Button**：hover/press 背景 `Transition<Color>` 淡入淡出。
- **nav 头家族**：`paint_panel_header`（NavRow/Collapsible/Accordion 共用）hover 底色淡入——一处接入覆盖三控件。
- **Checkbox**：勾选透明度/缩放过渡。
- 其余（Dropdown 箭头、Segmented 选中滑块、Radio）同款模式，后续机械接入，不在本轮。

### Phase C —— 布局/展开高度动画（本轮只设计，后做）
手风琴/折叠/下拉的高度展开。方案：新增 `RevealBox` 包装控件包住 body，
持 `Transition<f32>` progress(0..1)：
- `measure`：测 body 自然尺寸，报告高度 = `自然高 × progress`（动画值）。
- `paint`：`canvas.clip_rect` 裁到揭示高度（**依赖既有 `Canvas::clip_rect`**，已具备），body 顶对齐绘制。
- 收起完成（progress=0）→ 等价 `visible_when(false)`，不占布局、不命中。
`Element::collapsible`/`accordion` 改用 `RevealBox` 替换 `visible_when`。全局关时 progress 瞬时跳变。
难点：过渡期 body 需按自然高布局再裁剪；与现有 arena 布局的交互需专门测试，故单列一阶段。

## 测试策略
- 引擎纯单测（不依赖渲染）：缓动、插值、补间收敛、retarget 中途、全局开关。
- 控件契约测：状态切换后补间 target 正确、全局关时即时取终值；截图验证视觉（中间帧靠 clock 注入）。
- 回归：全局关闭时所有控件与接入前像素一致（动画不改变终态）。

## 风险/取舍
- 仅时间缓动，不含弹簧物理（后续可加 `Easing::Spring` 或独立 spring）。
- 补间存 `Cell` 而非全局注册表：贴合本库常驻 widget 结构，零额外生命周期。
- 全局时钟是 thread-local 单值：多窗口共用 UI 线程时一致，无需 per-window。
