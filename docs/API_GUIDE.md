# windui — 第三方开发指南

> 面向使用本库构建应用的开发者。讲清 **API 风格、设计思路、命名规范、扩展方式与约定**。
> 架构内幕（内存模型、三阶段布局原理）见 [DESIGN.md](./DESIGN.md)。

---

## 1. 设计哲学

windui 是一个**轻量、命令式、retained-mode** 的 Windows 桌面 GUI 库。五条核心原则，决定了所有 API 的样子：

1. **命令式 Builder，零解析**。UI 用纯 Rust 链式调用构建，无 DSL、无宏、无运行时解析。类型即文档，编译期即校验。
2. **共享可变状态用 `Rc<Cell<T>>`**。控件不持有"模型"，而是绑定到外部状态单元。你改 cell、UI 下一帧自然反映。这是贯穿全库的统一心智模型。
3. **retained + 空闲零 CPU**。控件树常驻，无事件/无脏区时不重绘、不唤醒。动画按需驱动（见 §8）。
4. **样式两层**：控件不硬编码颜色/间距，全部走 `Theme`（全局调色板 + 每控件覆盖层），可映射 TOML。单点视觉调整走内联 `Style` 修饰符。
5. **平台差异收口在平台层**。控件与核心层平台无关，为后续 macOS 预留（见 DESIGN.md 跨平台缝合）。

---

## 2. 三分钟上手

```rust
use std::cell::Cell;
use std::rc::Rc;
use windui::prelude::*;

fn main() {
    // 1) 状态：外部持有，控件绑定
    let count = Rc::new(Cell::new(0i64));

    // 2) UI：命令式 Builder 组装控件树
    let c = count.clone();
    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(12)
        .bg(Color::hex(0xF5F6FA))
        .child(Element::label("计数器").font_size(20.0))
        .child(Element::button("点我 +1").on_click(move |_| {
            c.set(c.get() + 1);
            println!("count = {}", c.get());
        }));

    // 3) 窗口：配置并运行
    App::new("Demo", 360, 240)
        .bg(Color::hex(0xF5F6FA))
        .content(ui)
        .run();
}
```

`use windui::prelude::*;` 引入最常用的 `App / Element / Color / Insets / Point / Rect / Size / Align / Axis / Dimension / Style / Theme`。

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

### 3.2 状态绑定：`Rc<Cell<T>>` 模型

控件**不存数据**，只持有一个指向外部状态的共享引用。改状态 → UI 反映。各控件对应的状态类型：

| 控件 | 状态类型 | 含义 |
|------|----------|------|
| `checkbox` / `switch` | `Rc<Cell<bool>>` | 开关 |
| `radio` / `dropdown` / `list` / `tabs` | `Rc<Cell<usize>>` | 选中索引 |
| `slider` / `progress` | `Rc<Cell<f32>>` | 0.0–1.0 |
| `stepper` | `Rc<Cell<f64>>` | 数值 |
| `text_input` | `Rc<RefCell<String>>` | 文本 |
| `dialog` / `visible_when` | `Rc<Cell<bool>>` / 闭包 | 显隐 |

**惯用法**：状态在 `main`（或你的 App 结构）里创建，`.clone()` 进控件和回调。`Rc::clone` 只增引用计数，开销极小。

```rust
let dark = Rc::new(Cell::new(false));
Element::switch(dark.clone());                    // 控件读写它
Element::label("x").visible_when({                // 另一处按它显隐
    let d = dark.clone();
    move || d.get()
});
```

---

## 4. API 命名规范

第三方写扩展或阅读代码时，按这套约定即可预测 API 形状：

- **构造器 = 控件名（名词）**：`col`、`row`、`button`、`dropdown`…，全小写蛇形。
- **布局/样式修饰符 = 属性名**：`width`、`padding`、`bg`、`corner`…，设置型方法**不加** `set_` 前缀（builder 惯例）。
- **颜色用缩写**：背景 `bg`、前景 `fg`，全库一致（`Element::bg`、`App::bg`、`Style.bg`、`EventCtx::set_bg`）。
- **文本标签统一 `impl Into<String>`**：`button`、`label`、`dropdown`、`list`、`tabs` 的标题/选项均可传 `&str` 或 `String`。
- **事件回调 = `on_<动作>`**：目前 `on_click`。回调签名见 §7。
- **`xxx_xy(h, v)`** = 水平/垂直两参版本：`padding_xy`、`margin_xy`。注意这里 `h`=horizontal、`v`=vertical（与 `size(w, h)` 的 `h`=height 不同名同义，按方法语境区分）。
- **`xxx_match` / `fill`** = 撑满父容器：`width_match`、`height_match`、`fill`（= 两者）。
- **getter 不加 `get_`**（Rust 惯例）：`EventCtx::bounds()`、`id()`、`scroll_metrics()`。
- **`set_` 仅用于命令式副作用 setter**（非 builder）：`EventCtx::set_scroll()`、`set_bg()`。

---

## 5. 控件目录

全部经 `Element::` 构造。`impl Into<String>` 处可传 `&str` 或 `String`。

### 容器 / 布局
```rust
Element::col()                       // 纵向线性容器
Element::row()                       // 横向线性容器
Element::stack()                     // 层叠（Frame，后者覆盖前者）
Element::leaf()                      // 叶子（自定义控件载体，见 §9）
Element::scroll()                    // 垂直滚动容器（支持鼠标滚轮 + 触摸滑动/惯性）
Element::divider()                   // 分隔线
Element::tabs(selected, vec![("标签", page_element), ...])
Element::tabs_icons(selected, vec![("标签", icon, page), ...])  // 带图标的标签（icon: ImageContent）
Element::dialog(show, content)       // 模态浮层（show: Rc<Cell<bool>>）
```

### 基础控件
```rust
Element::label("文本")
Element::button("确定").on_click(|ctx| { /* ... */ })
Element::checkbox("启用", state)                 // state: Rc<Cell<bool>>
Element::switch(state)                            // state: Rc<Cell<bool>>
Element::radio("选项", group, index)             // group: Rc<Cell<usize>>
Element::slider(value)                            // value: Rc<Cell<f32>> (0..=1)
Element::dropdown(vec!["A", "B"], selected)       // selected: Rc<Cell<usize>>
Element::stepper(value, min, max, step)           // value: Rc<Cell<f64>>
Element::list(vec!["行1", "行2"], selected)       // selected: Rc<Cell<usize>>
Element::list_icons(vec![("收件箱", icon), ..], selected)  // 带前置图标的行（icon: ImageContent）
Element::progress(value)                          // value: Rc<Cell<f32>> (确定进度)
Element::progress_indeterminate()                 // 不确定进度（忙碌动画）
```

### 文本输入
```rust
Element::text_input(text, "占位符")               // text: Rc<RefCell<String>>
    .password()        // 密码遮蔽（仅对 text_input 有效）
    .multiline()       // 多行
    .wrap(true)        // 多行时是否自动折行（默认 true）
```
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
  Element::button("新建").icon_bytes(include_bytes!("plus.png"))  // 或 .icon(path) / .icon_rgba(w,h,&rgba)
  Element::button("提交").icon(path).enabled(can_submit.clone())  // 禁用时背景/图标/文字一起置灰
  Element::button("删除").icon(path).disabled(true)               // 静态禁用
  ```

### 图片的状态处理
图片原语与控件**状态解耦**：控件把自身状态映射成通用 `VisualState`（Normal/Hover/Pressed/Selected/Disabled）传给图片，原语据此调制。三种手段（可组合）：
- **调制**：按状态调不透明度——禁用自动置灰（`VisualState::opacity`）。
- **着色**：`.tint(color)` 把**单色图标**按颜色重着色（随主题/状态变色，用 alpha 作模板），结果按层缓存，不影响彩色图。
- **换图**：`ImageContent::on_state(state, image)` 为特定状态备专图，命中用专图、否则回退基图。
```rust
// 高级用法：预组装内容原语，再交给控件
let icon = ImageContent::from_bytes(base).tint(Color::WHITE)
    .on_state(VisualState::Disabled, gray_png);
Element::button("X").icon_content(icon);
Element::image_content(icon);   // 也可作独立控件
```
> **禁用是核心级通用能力**：`.enabled(Rc<Cell<bool>>)` / `.disabled(bool)` 可用于**任意控件或容器**。核心统一拦事件、跳 Tab，并把启用态传入控件 paint 令其置灰；**禁用沿父链继承**——禁用一个容器即禁用其全部子节点（适合按条件禁用整个表单区）。各表单控件（Button/CheckBox/Switch/RadioButton/Slider/Dropdown/Stepper/TextInput）均已实现置灰。

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

### 文件拖放
```rust
Element::col().fill().on_drop_files(|ctx, paths| {   // paths: &[PathBuf]
    for p in paths { /* ... */ }
    ctx.mark_dirty();                                // 改了状态记得请求重绘
})
```
- **任意元素可接收**：`.on_drop_files(f)` 挂到 `.fill()` 根容器即"全窗接收"；落点会路由到落点下的元素，再沿父链冒泡到首个设了回调的节点（禁用子树不接收）。
- 平台经 `WM_DROPFILES` 解出路径 + 落点交宿主路由（`Tree::dispatch_files`）；回调签名 `FnMut(&mut EventCtx, &[PathBuf])`，可读写共享状态、`mark_dirty`。完整示例见 `examples/file_drop.rs`。

### 系统托盘
```rust
let notify_on = Rc::new(Cell::new(true));
App::new("…", w, h).tray(
    Tray::new()
        .tooltip("后台运行中")
        .icon_rgba(16, 16, &rgba)            // 可选；默认用系统应用图标
        .on_left_click(|ctx| ctx.show_window())
        .on_double_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("显示窗口", |ctx| ctx.show_window()),
            TrayMenuItem::separator(),       // 分隔线
            TrayMenuItem::check("启用通知", notify_on.clone(), move |ctx| { /* 翻转状态 */ }),  // 勾选项
            TrayMenuItem::item("退出", |ctx| ctx.quit()),
        ]),
).content(ui).run();
```
- **右键菜单走原生 `TrackPopupMenu`**（真 OS 弹出，显示在托盘旁）；支持**勾选项**（`check` 绑 `Rc<Cell<bool>>`，弹出时按当前值显示对勾）与**分隔线**。
- 回调拿 `TrayCtx`：`show_window()` / `hide_window()` / `quit()` / `notify(title, body)`（气泡通知）。
- 图标可 `.icon_rgba(w,h,&rgba)`（零依赖，从 RGBA 造 HICON），未设则用系统默认应用图标。窗口销毁时托盘自动清理。完整示例见 `examples/tray.rs`。

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

---

## 7. 样式与主题

两条路径，**按层级选择**：

### 7.1 内联 `Style` 修饰符（单点覆盖）
直接挂在 Element 上，只影响该节点：
```rust
Element::label("标题")
    .fg(Color::hex(0x1A1A2E))     // 文字色
    .font_size(22.0)
    .bg(Color::WHITE)
    .border(Color::hex(0xDDDDDD), 1)
    .corner(8.0)
    .text_align(Align::Center)
```
`Color` 构造：`Color::rgb(r,g,b)`、`rgba(..)`、`hex(0xRRGGBB)`、`from_hex_str("#7C5CFC")`，常量 `WHITE/BLACK/TRANSPARENT`。

### 7.2 `Theme`（全局 + 每控件覆盖层）
控件默认视觉**不从内联 Style 取**，而从当前 `Theme` 取。`Theme` 两层：
- `palette`（`Palette`）：accent / bg / surface / text / border … 全局色板。
- `metrics`（`Metrics`）：圆角、边框宽、间距、字号等度量。
- 每控件覆盖层：`button` / `input` / `toggle` / `dropdown` / `menu` / `tab` / `progress` / `stepper` / `list`，每个字段是 `Option<Color>`，`None` 时回退到 palette。

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

---

## 8. 事件与交互

### 8.1 点击回调
```rust
Element::button("保存").on_click(|ctx: &mut EventCtx| {
    // ctx 提供与框架交互的能力
    ctx.request_close();      // 关窗
});
```
`on_click` 接 `FnMut + 'static`（可改捕获的状态）。`visible_when` 接 `Fn`（纯查询）。

### 8.2 上下文菜单
文本输入已内建右键菜单（剪切/复制/粘贴/全选）。自定义控件可在 `on_event` 里：
```rust
ctx.show_context_menu(pos, vec![
    MenuItem::run("操作", || { /* ... */ }, false),
]);
```
菜单项两种动作：`MenuItem::run(label, closure, checked)` 跑闭包；`MenuItem::key(label, key_event, enabled)` 向焦点控件合成按键。

### 8.3 焦点与键盘
- Tab / Shift+Tab 在 `focusable()` 控件间导航（框架自动维护焦点环）。
- 自定义控件实现 `Widget::focusable() -> true` 即加入导航链。

### 8.4 右键约定
**右键默认不触发控件**（桌面习惯）。框架在分发层拦截非左键的 Down/Up；仅需右键的控件 override `Widget::wants_right_click() -> true`。新控件**默认即正确**。

---

## 9. 扩展：自定义控件

实现 `Widget` trait，挂到 `Element::leaf().widget(...)`（或任意容器的 `.widget()`）。

`Widget` 是**纯内容接口**——不持有、不访问树。所有方法都有默认实现，按需覆盖：

```rust
use windui::core::{Widget, EventCtx};
use windui::event::{Event, PointerKind};
use windui::geometry::{Size, Rect, Color};
use windui::render::{Canvas, Paint};
use windui::style::Style;
use windui::text::TextEngine;

struct Dot { on: std::rc::Rc<std::cell::Cell<bool>> }

impl Widget for Dot {
    // ① 测量：返回内容固有尺寸（不含 padding）
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(24, 24)
    }

    // ② 绘制：bounds=节点全矩形, content=扣 padding 后的内容矩形
    fn paint(&self, _bounds: Rect, content: Rect, _focused: bool,
             canvas: &mut dyn Canvas, _style: &Style) {
        let c = if self.on.get() { Color::hex(0x2ECC71) } else { Color::hex(0xCCCCCC) };
        let cx = content.x as f32 + content.w as f32 / 2.0;
        let cy = content.y as f32 + content.h as f32 / 2.0;
        canvas.fill_circle(cx, cy, 10.0, &Paint::fill(c));
    }

    // ③ 事件：返回是否消费（消费则停止冒泡）
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Pointer(p) = ev {
            if p.kind == PointerKind::Up && p.button == windui::event::MouseButton::Left {
                self.on.set(!self.on.get());
                ctx.mark_dirty();   // 请求重绘
                return true;
            }
        }
        false
    }

    fn focusable(&self) -> bool { true }   // 可选：加入 Tab 导航
}

// 使用
let dot = Element::leaf().widget(Dot { on: state.clone() });
```

**三阶段契约**：`measure`（算固有尺寸）→ 框架 `arrange`（定位，你不参与）→ `paint`（在分配到的 `bounds`/`content` 内绘制）。坐标在 `on_event` 收到的是**逻辑坐标**（已 ÷DPI scale）。

**`EventCtx` 能力**：`mark_dirty()` 重绘、`bounds()` 取绝对矩形、`capture()/release_capture()` 拖拽捕获、`request_focus()/request_close()`、`scroll_by()/set_scroll()/scroll_metrics()`、`clipboard_get()/clipboard_set()`、`show_menu()/show_context_menu()`、`set_bg()`。

**`Canvas` 图元**：`fill_rect`、`fill_round_rect`、`stroke_round_rect`、`draw_line`、`fill_circle`、`draw_text`、`measure_text`、`save/restore/clip_rect`（裁剪用 save→clip_rect→绘制→restore）。坐标为 f32 绝对窗口坐标。

**持续动画**：在 `paint` 中调用 `windui::anim::request_repaint()` 即请求下一帧；框架会按显示器刷新率（≤60fps）驱动，停止请求即回到零 CPU 空闲。

---

## 10. 第三方开发规范（Do / Don't）

**Do**
- ✅ 状态用 `Rc<Cell<T>>`（`String` 用 `Rc<RefCell<String>>`），在外部创建、`clone` 进控件与回调。
- ✅ 成体系视觉走 `Theme`，一次性微调走内联 `Style` 修饰符。
- ✅ 自定义控件实现 `Widget`，`paint` 读 `theme::current()` 而非硬编码颜色（与内建控件一致）。
- ✅ 滚动内容放进 `Element::scroll()`，触摸/惯性自动可用。
- ✅ 回调里只改 cell 状态，靠 `mark_dirty()` / 下一帧反映，不要试图直接操作节点树。

**Don't**
- ❌ 不要在 `on_click`/`on_event` 里长时间阻塞（同步渲染，会卡 UI 线程）。
- ❌ 不要把 text_input 专属修饰符（`password/multiline/wrap`）链到其他控件——debug 期会 panic 提示误用。
- ❌ 不要假设 `Widget` 能访问父/子节点——它是纯内容接口，跨节点协调走共享状态。
- ❌ 不要在控件里写死颜色/间距/字号——破坏主题一致性。

---

## 11. 已知约束

**功能约束**
- 仅 Windows（架构预留 macOS 边界，未实现）。
- CPU 软光栅，适合中小工具；不适合大面积高频全屏动画。
- `list` 当前每行是独立 Tab 停靠点，超长列表会拉长焦点链（计划：单 Tab 停靠 + 方向键导航）。

> 命名一致性已收敛：背景/前景统一 `bg`/`fg`；所有文本标签统一 `impl Into<String>`；
> text_input 专属修饰符误用在 debug 期 panic 提示。框架处于早期，以"最新设计 + 统一"为准，
> **不承诺向后兼容**——API 可能继续演进，第三方请跟随本指南最新版。

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
