# windui — 多语言（i18n）设计

> 状态：**P0 + P1 + P2 已实现**（2026-09-17）。本文是设计依据；与实现不同的地方
> 已就地标注「实到」。落地代码见 `src/i18n/`，可运行示例见 `examples/i18n.rs`，
> 使用指南见 `docs/API_GUIDE.md` §12。
> 相关：`docs/API_GUIDE.md` §7（主题）、`src/ui/text_content.rs`（文案载体）、
> `src/app/mod.rs`（`ThemeHandle` 运行期句柄范式）。

---

## 1. 目标与非目标

**目标**

- **小应用零负担**：译文编译期内嵌，不带任何外部文件也能发多语言版本。
- **大应用可外挂**：译文放外部文件，运行期加载并**按 key 覆盖**内置值；第三方/用户可只交差异。
- **位置化格式**：`{name}` / `{0}` 占位，参数值在**显示时**才代入 —— 换语言不必重建界面树。
- **格式面向未来**：复数、量词、上下文变体（select）、数组，**格式与解析器第一期就吃下**，
  运行期规则可以后补。格式定死了再改是破坏性的，规则后补不是。

**非目标（明确不做，写进 `API_GUIDE.md` §11 已知约束）**

- **RTL / BiDi 排版**。本库多行文本是自绘视觉行布局，按字符切分行，
  阿拉伯语/希伯来语会错位。这不是 i18n 层能补的洞，属文字栈的活。
- **完整 ICU MessageFormat**（内联 `{count, plural, one{...} other{...}}` 子语法）。
  理由见 §4.4：复数被外提到 TOML 的表层级，不塞回字符串里。
- **数字/日期/货币的本地化格式化**。第一期不引依赖；预留见 §8 P3。

---

## 2. 结论先行：三个决定

| 决定 | 内容 | 为什么不是另一种 |
|---|---|---|
| **缝在 `TextContent`** | 加 `TextContent::Msg(Rc<Message>)` 变体，`resolve()` 时查当前目录 | 不是给每个控件加 `_i18n` 孪生构造器（`text_content.rs` 已论证过这条路会复制三态动画与截断算法）；也不是让应用自己 `tr!()` 出 `String` 传进去 —— 那样换语言只能重建整棵树 |
| **目录是叠加链** | `外部文件 > 内置译文 > 回退语言 > key 本身`，**逐 key 合并**不是整文件替换 | 整文件替换会逼外部提供方补齐全量 key；漏一条就是空白界面，而不是回退到内置 |
| **格式是 TOML，值可为字符串或表** | `copy = "复制"` 与 `[file.selected] one/other = ...` 并存 | JSON 不带注释、译者读不了；Fluent(.ftl) 要新依赖 + 新语法；TOML 已在依赖树里（主题用它），**零新增依赖**，且表层级天然承载复数/变体 |

---

## 3. 接入点：现状与改动

### 3.1 文案载体 —— `src/ui/text_content.rs`

现状两个变体：`Static(String)` / `Bound(Signal<String>)`，`resolve(&self) -> Cow<'_, str>`
在每次 `measure`/`paint` 时被调用。

改动：

```rust
pub enum TextContent {
    Static(String),
    Bound(Signal<String>),
    /// 待翻译消息：key + 参数。每帧按当前语言现取并格式化。
    Msg(Rc<Message>),
}
```

`resolve()` 对 `Msg` 返回 `Cow::Owned`（与 `Bound` 同代价，理由也同：目录存在
`RefCell` 保护的线程局部里，借着它跨整个绘制过程会把一次不相干的换语言变成 panic）。

无参消息的快路径：`Message.args` 为空时目录里存的是 `Rc<str>`，直接取出再 `to_string()`；
若后续 profile 显示热，可把返回类型收敛成一个 `ResolvedText` 小枚举 —— 但**不在第一期做**，
先量再改。

`impl From<Message> for TextContent` 让 `t!()` 宏的产物能直接进任何收
`impl Into<TextContent>` 的构造器。**对下游的规则仍是一句话**：凡是接受文案的参数，
`&str` / `String` / `Signal<String>` / `t!(..)` 四者可互换。

### 3.2 运行期句柄 —— 对齐 `ThemeHandle`

```rust
// App 上
pub fn locale_handle(&mut self) -> LocaleHandle;

#[derive(Clone)]
pub struct LocaleHandle { inner: Rc<RefCell<Rc<Catalog>>> }

impl LocaleHandle {
    pub fn detached(c: Catalog) -> Self;      // 同 ThemeHandle::detached，供下游测自己的状态结构
    pub fn set(&self, lang: &str);            // 切到已加载的语言
    pub fn current(&self) -> Rc<Catalog>;
    pub fn reload(&self);                     // 重扫外部目录（不做文件监听，见 §9）
    pub fn available(&self) -> Vec<LocaleId>; // 供设置页填下拉
}

// 控件侧（与 theme::current() 同形）
pub fn i18n::current() -> Rc<Catalog>;
```

`set()` 的失效处理 —— **这一条是本设计里最容易写错的地方**：

```rust
crate::anim::request_repaint();
crate::signal::mark_cross_window_dirty();   // 目录全窗共享，不标这笔只有当前窗变（换肤踩过）
```

**实到：两笔就够，原设计里的另外两笔是多余的。**

- *整窗重排*不用单独标：`request_repaint` 请求的是一帧**全窗**帧，而全窗路径必定跑
  `layout_root`（见 `UiHost::prepare_full_frame`）。设计时以为要显式 `mark_dirty_all`，
  读了帧路径才发现那条路已经包含重排。
- *清文字测量缓存*不用做：缓存以 (文本, 样式) 哈希为键，旧语言的条目自然不再命中，且
  引擎侧有 4096 条上限会自行清空（`text/dwrite.rs` 的 `MEASURE_CACHE_CAP`）。为它专门
  开一条从 i18n 到平台文字引擎的通路，换来的只是早几分钟回收几 KB。

整窗重排的理由仍然成立，见「渲染失效/局部重绘」：per-node 脏区只对**自包含**视觉安全。
换语言后按钮文字变宽会顶动整行，属非局部变更。**验收用例已落地**——
`app::tests::switching_language_moves_the_sibling_control`：一个 `row` 里放「短标签 + 按钮」，
换语言后断言按钮被顶右。已按「故意破坏」验收：把 `TextContent::Msg` 改成不查目录，它立刻红。

### 3.3 目录（Catalog）与加载链

```rust
pub struct Catalog { /* 当前语言 + 回退链，key -> Entry */ }
pub enum Entry { One(Rc<str>), Variants(HashMap<Box<str>, Rc<str>>), List(Vec<Rc<str>>) }

Locales::builder()
    .embed(include_str!("../i18n/zh-CN.toml"))   // 内置：语言 id 从文件 [meta] 里读
    .embed(include_str!("../i18n/en.toml"))
    .load_dir("i18n")                             // 可选：外部覆盖，同名 key 压过内置
    .fallback("en")                               // 全局兜底
    .initial(Initial::System)                     // 或 Initial::Fixed("zh-CN")
    .build()                                      // -> Locales，交给 App::locales(..)
```

查找顺序（**逐 key**）：`外部(当前语言) → 内置(当前语言) → 外部(回退语言) → 内置(回退语言) → miss`。

miss 的表现分档，这点故意不对称：

- `debug`：返回 `⟪menu.copy⟫`（尖括号标记）**且** `log::warn!` 一次（按 key 去重，避免每帧刷屏）。
  醒目的乱码比静默空串好 —— 空串在界面上看着像"这里本来就没字"。
- `release`：返回 key 本身（`menu.copy`）。至少能看出是哪条漏了，且不吓用户。

### 3.4 系统语言探测 —— 收口平台层

跨平台缝在 `platform`（同 click_count / 剪贴板的既定分工）：

```rust
// platform/mod.rs
pub(crate) fn system_locales() -> Vec<String>;   // BCP-47，按用户偏好顺序
// win32:  GetUserDefaultLocaleName / GetUserPreferredUILanguages
// macOS:  NSLocale.preferredLanguages
```

匹配用**宽松降级**：`zh-Hans-CN` 找不到就试 `zh-Hans`、`zh-CN`、`zh`，再走 fallback。

### 3.5 框架自带文案

当前硬编码在库里的用户可见串（非注释）：

| 位置 | 内容 |
|---|---|
| `src/ui/inputs.rs:1223` | 剪切 / 复制 / 粘贴 / 全选（输入框右键菜单） |
| `src/event.rs:226` `system_menu_items()` | 还原 / 最小化 / 最大化 / 关闭 |
| 其余控件空态、占位 | 按实现时 grep 结果补 |

迁到 `windui.` 命名空间（`windui.menu.cut` …），随 crate 内嵌 `zh-CN` + `en` 两份，
下游可用同名 key 覆盖，也可提供第三种语言。

**兼容性决策（重要）**：默认语言**不跟随系统**，恒为 `zh-CN`，行为与今天逐字相同。
跟随系统是 `Initial::System` 显式开启的。否则新版本一发布，所有装英文系统的现有下游
右键菜单突然变英文 —— 那是没人要求过的行为变更。

### 3.6 平台持有的字符串：本设计的真实缺口

窗口标题、托盘 tooltip、托盘菜单、菜单栏 —— 这些串**不经 paint**，是一次性交给系统的，
`TextContent::Msg` 的"每帧现取"救不了它们。当前仓库里连运行期改标题的 API 都没有
（只有 `Window::new(title, ..)` 建窗时定死）。

处理分两类：

- **每次弹出重建的**（右键菜单、托盘菜单、下拉项）：用 `tr!()` 立即求值成 `String` 即可，
  `MenuItem.label: String` 这个 pub 字段**不动** —— 改它是破坏性变更，而收益为零。
- **长期持有的**（窗口标题、托盘 tooltip）：需要新 API。

**实到：`App::title(impl Into<TextContent>)`，拉取式而不是 `ctx.set_window_title` 那种
推送式。** 差别值得记一笔——推送式要求"谁改了标题谁负责通知"，而标题的两种来源
（`t!` 的译文、`Signal<String>`）**都不经过任何显式调用就会变**：换语言只动线程局部的
译文目录，写信号只动信号运行时，谁都不会顺手去排一条"改标题"的意图。于是推送式必须在
每个可能改变它的地方补一次通知，漏一处就是"换了语言标题还是旧的"。

拉取式把这件事收敛成一个问题：平台在**事件分发后与出帧后**各问一次
`AppHandler::take_window_title()`，宿主现算当前标题、与上次推送的比对，变了才返回。
两个时机缺一不可——语言可以在点击回调里换（事件路径），也可以在 `on_interval` 里换
（帧路径）。托盘 tooltip 已有 `TrayHandle::set_tooltip`，无需新增。

子窗口随后也接上了（`Window::title` → `WindowRequest::title_src` → 子窗自己的 `UiHost`）。
子窗各有一份宿主，所以这不是"主窗那套顺带生效"，而是来源要真的一路送到子窗宿主上——
中间任一环漏接的症状都是"子窗标题不跟随"，而子窗要真窗口才看得见。

---

## 4. 文本描述格式

### 4.1 一份完整示例

```toml
# i18n/zh-CN.toml
[meta]
locale   = "zh-CN"          # BCP-47，文件名仅作提示，以此为准
fallback = "en"             # 可选，覆盖全局 fallback
name     = "简体中文"        # 给设置页下拉直接用，省得应用自己写死一张语言名表

[menu]
cut   = "剪切"
copy  = "复制"
paste = "粘贴"

[file]
title    = "文件"
# 位置格式化：命名占位（推荐）
deleted  = "已删除 {name}"
# 命名占位不按顺序取 —— 译文调换语序时不必动代码
moved    = "已把 {name} 移到 {dest}"
# 位置占位（兼容简短场景）
ratio    = "{0} / {1}"

# 复数/量词：值是表而不是串。类别名用 CLDR：zero/one/two/few/many/other
[file.selected]
other = "已选 {count} 个文件"        # 中文只有 other，写一条就够

# 数组：星期、月份这类有序列表
[calendar]
weekdays = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
```

```toml
# i18n/en.toml —— 同一批 key，复数类别按本语言需要给全
[meta]
locale = "en"
name   = "English"

[menu]
cut = "Cut"
copy = "Copy"
paste = "Paste"

[file.selected]
one   = "{count} file selected"
other = "{count} files selected"
```

### 4.2 key 空间

TOML 的表层级扁平化成点路径：`menu.copy`、`file.selected.other`、`calendar.weekdays`。
`windui.` 前缀留给框架自带文案，应用不要占用（不是硬性拦截，是约定 + lint 会警告）。

### 4.3 占位符语法

- `{name}` 命名占位（推荐），`{0}` `{1}` 位置占位。
- `{{` / `}}` 转义出字面花括号。
- **未提供的参数**：debug 下原样保留 `{name}` 并 warn（看得见才改得掉），release 下替换为空串。
- **多余的参数**：静默忽略 —— 译文里少用一个变量是译者的正当选择，不该是错误。

解析产物缓存：目录加载时把每条值预编译成 `Vec<Piece>`（`Lit(Rc<str>) | Named(u8) | Pos(u8)`），
paint 期只做拼接，不做扫描。这是"每帧现取"能便宜的前提。

### 4.4 复数：为什么外提成表，而不是内联 ICU 子语法

ICU 写法是 `"{count, plural, one {# file} other {# files}}"` —— 一条值里嵌一门小语言，
解析器要处理嵌套、递归、`#` 替换、空白敏感。而 TOML 已经**免费**给了层级：

```toml
[file.selected]
one = "..."
other = "..."
```

代价是一条消息里**只能有一个**复数轴（`t!("file.selected", count = n)` 的 `count` 即选择轴）。
真需要两个轴（"3 个文件夹里的 5 个文件"）的句子极少，且总能拆成两条消息拼装。
用这条限制换掉一整个内联语法解析器，划算。

**第一期的行为**：解析器接受表形式，但只取 `other`（缺 `other` 则取表里任一并 warn）。
即 `Entry::Variants` 这条数据通路和文件格式从第一天就是真的，只有**选择规则**是桩。
P2 接 CLDR 规则表时，**文件不用改一个字**。

### 4.5 上下文变体（select）预留

同一张表结构复用给非复数选择：

```toml
[greeting]
male   = "他来了"
female = "她来了"
other  = "TA 来了"
```

`t!("greeting", select = gender)` 时按 `select` 参数值取类别，取不到落 `other`。
与复数共用 `Entry::Variants`，**不新增格式**。

### 4.6 数组

`Entry::List`，两个入口：

```rust
i18n::list("calendar.weekdays")      // -> Rc<[Rc<str>]>
t!("calendar.weekdays", index = i)   // 控件侧：按下标取一条，越界落 fallback
```

---

## 5. API 面

### 5.1 两个宏，分工要背下来

```rust
t!("menu.copy")                              // -> Message，进控件，跟随换语言
t!("file.selected", count = n)
t!("file.moved", name = &f.name, dest = dir)

tr!("menu.copy")                             // -> String，立即求值，定格
tr!("file.deleted", name = &f.name)
```

- **`t!` 进控件**（任何 `impl Into<TextContent>` 的参数）：值不求值、参数存着，换语言自动跟随。
- **`tr!` 出 `String`**：给 `MenuItem::run`、日志、`ctx.toast`、窗口标题这类**需要 `String`** 的地方。
  它的产物换语言**不跟随**，这是有意的 —— 那些场合本来就是"当场一次性使用"。

误用的表现：把 `tr!` 塞进 `Element::label` 也能编过（`String` 有 `Into<TextContent>`），
只是换语言那条文案不变。这是 lint 能查的（§7），不是编译期能拦的。

### 5.2 参数值：`ArgValue` 接受信号

```rust
pub enum ArgValue {
    Str(Rc<str>), Int(i64), Float(f64),
    SigStr(Signal<String>), SigInt(Signal<i64>),   // 现取，与 TextContent::Bound 同哲学
}
```

`Signal` 是 `Copy` 句柄（见「Signal 状态原语」），装进 `Message` 不增负担。于是
**动态数字 + 多语言**是一条路而不是两条：

```rust
let n = signal(0i64);
Element::button(t!("file.selected", count = n))   // 计数变了自动刷，换语言也自动刷
```

这条同时消灭一类常见错误：把**已翻译的整句**塞进 `Signal<String>`（换语言不跟随），
而不是把**变量值**塞进去。文档里要正面写这条规则。

### 5.3 prelude 增补

`Locales`、`LocaleHandle`、`t!`、`tr!`。
按「公开 API 可达性」的教训：**参数类型也要进 prelude** —— `Initial`、`LocaleId`
是 `Locales::builder()` 链上的参数类型，不进去下游就得写深路径 import。
验收方式也照那条教训：写一个**库外视角**的 `examples/i18n.rs`，只 `use windui::prelude::*;`
能把整套流程写完，才算这条做完。

---

## 6. 模块布局

```
src/i18n/
  mod.rs        // Catalog / current() / LocaleHandle 对接 / 宏
  parse.rs      // TOML -> Entry，值的预编译（Piece）
  format.rs     // Piece + args -> String
  plural.rs     // P2：CLDR 规则子集；第一期是取 other 的桩
i18n/           // crate 自带译文（zh-CN.toml / en.toml），include_str! 进二进制
```

`src/lib.rs` 加 `pub mod i18n;`。

**不加 feature gate**：核心只依赖已有的 `serde` + `toml`，自带译文两份约 2KB。
为这点体积加一个开关，换来的是"下游要先读 Cargo.toml 才知道有这回事"（`gpu` feature
的注释里已经把这笔账算过一遍）。

---

## 7. 验证策略

多语言最容易出的不是崩溃，是**静默错**：漏 key、格式串参数对不上、某语言没覆盖全。
按「测试自证循环的坑」，期望值不能从被测实现反推，得有独立信源：

1. **key 集合一致性测试**（`cargo test`）：所有内置 locale 文件的 key 集合必须相等，
   差集直接列出来。信源是文件本身，不是解析器。
2. **占位符一致性测试**：同一 key 在各语言里的**占位符名字集合**必须相等。
   `en` 写了 `{count}` 而 `zh-CN` 写成 `{num}` —— 这是最典型的静默错，跑起来才发现是空串。
3. **代码 key 存在性 lint**。**实到：一张手写清单 + 一个源码扫描器，两条都要，不是二选一。**

   原设计只打算扫源码。那会踩「测试自证循环」：从源码 grep 出 key 再校验，等于让被测
   对象自己提供期望值——删掉一处 `tr!` 调用，测试跟着不再检查它，照样全绿。所以先落的是
   **手写清单**（`i18n::tests::builtin_covers_every_key_the_library_asks_for`），信源独立。

   但清单只守一个方向。`tr!("windui.menu.cutt")` 拼错一个字母，清单照样绿（它查的是"译文
   里有没有"，而那条译文确实有），界面上只会安静地显示 `⟪windui.menu.cutt⟫`——没有任何
   编译期信号。于是补上第二条：`framework_keys_written_in_the_source_all_have_translations`
   扫本库 `src/` 里写出来的 `windui.*`，查"写出来的对不对"。

   两条的信源不同、方向相反，**互补而非重复**。扫描那条自己也可能退化（词法缺陷、或某次
   重构把调用藏进帮手函数），所以它额外带一条下限断言「至少扫到 8 条」——否则扫描器一瞎
   它就成了摆设，这正是自证循环反对意见在这里的正确答案。

   前两项则做成了**库的公开 API**：`i18n::lint::check(&[&str])` 查译文之间一致，
   `i18n::lint::check_usage(dir, &Locales)` 查"用了但没翻译"（带文件名+行号）。下游应用会
   遇到一模一样的静默错，没理由让每个项目各写一遍。`check_usage` 只查这一个方向——反向
   （译文没人用）因动态 key 必然误报，而误报的 lint 很快会被加例外、然后被整个关掉。
4. **换语言的布局回归**：§3.2 那个"标签变长顶动按钮"的截图用例。
   **验收靠故意破坏**：把整窗重排那一步去掉，这个测试必须红。
5. **miss 回退矩阵**：外部缺 / 内置缺 / 回退语言缺 / 全缺，四档各断言一次。

---

## 8. 分期

### P0 — 地基（可用的最小闭环）✅ 已实现
- `Catalog` / `Entry` / `Message` / `ArgValue`；TOML 解析 + 值预编译。
- `TextContent::Msg` + `resolve()` 接入。
- `Locales::builder()`：`embed` / `load_dir` / `fallback` / `initial(Fixed)`。
- `LocaleHandle` + `i18n::current()` + 四步失效（§3.2）。
- `t!` / `tr!`；prelude 增补。
- 表形式与数组**能解析**（复数取 `other`，数组可取）。
- §7 的 1/2/4/5 项测试；`examples/i18n.rs`。

### P1 — 框架自身与运行期完整性 ✅ 已实现
- 框架自带文案迁到 `windui.*`（`i18n/zh-CN.toml` / `i18n/en.toml`），文本框右键菜单与
  无边框窗口系统菜单已改走 `tr!`。截图核验：`artifacts/i18n_menu_en.png`。
- `Initial::System` + `platform::system_locales()`：win32 用 `GetUserPreferredUILanguages`
  （不是 `GetUserDefaultLocaleName`——那是区域格式，不是界面语言），macOS 用
  `NSLocale::preferredLanguages`。两端都已编译 + 跑过测试。
- `App::title` 拉取式标题（§3.6）。
- §7 第 3 项 lint 落成 `i18n::lint::check`（见下）。

### P2 — 语言规则 ✅ 已实现（提前到首批一起做）
- CLDR 复数规则子集（`plural.rs`）：zh/ja/ko/vi/th/id…（仅 other）、en/de/es/… 及未收录
  语言（one/other）、fr/pt（0 与 1 同属 one）、ru/uk/be、pl、cs/sk、lt、ar（六类）。
  规则表手写 + 逐语言用 CLDR 官方样例数字做断言，不引 `icu` 全家桶。
- `select` 变体、`Catalog::list` / `index` 取下标均已生效。

**为什么提前**：原计划第一期只取 `other` 当桩。但桩的表现是"英文界面显示 `1 files`"——
一个看着像小疏忽、实际要人工逐语言复查才能发现的错。而规则表本身约 100 行、有独立信源
（CLDR 样例数字）可断言，做掉比留着一个会骗过自测的桩划算。

### P3 — 远期（按需求再排）
- 数字/日期本地化格式（可选 feature，或对接 `icu_*`）。
- RTL：需要文字栈先支持 BiDi 重排，属另一条线。
- 译文热重载的文件监听。

---

## 9. 已知风险

| 风险 | 表现 | 处置 |
|---|---|---|
| RTL 排版 | 阿拉伯语逐字反序、标点错位 | 不支持，写进 §11 已知约束。目录可以加 `[meta] rtl = true` 占位，但**不假装能画对** |
| 字体回退 | `TextStyle.family = None` 靠系统回退；换到日文后行高变化 | 布局本就整窗重排（§3.2），行高变化被覆盖。`[meta] font_family` 预留，P1 前不做 |
| 测量缓存膨胀 | 多语言切换后旧串条目滞留 | **不处理**：引擎侧 4096 条上限自行清空（§3.2） |
| 长译文溢出 | 德语按钮文字比中文长 60%，顶破固定宽度布局 | 这是**应用侧布局问题**，但示例与文档要示范：文案区用 `weight` 占剩余空间，别写死 `width`（见「设置页七件套」的同名教训） |
| `tr!` 误用 | 换语言时个别文案不变 | lint 只能查 key 存在性，查不出这个。靠文档 + code review |
| 第三种语言缺 `windui.*` | 日文界面里冒出 Cut/Copy/Paste（回退链正常工作，但看着像 bug） | debug 下 warn + `Locales::languages_missing_framework_strings` 供应用写进测试 |
| 外部文件语法错 | 用户改坏 TOML，整个语言加载失败 | **单文件失败只丢那一个文件**并 warn，不影响其它语言与内置译文；绝不 panic |

---

## 10. 对现有 API 的破坏性评估

| 改动 | 破坏性 | 说明 |
|---|---|---|
| `TextContent` 加 `Msg` 变体 | **有**（该 enum 是 pub 且可被 match） | 库内 match 全在自己手里；下游若 match 过它会编译失败。可加 `#[non_exhaustive]` 一并了结这类未来变更 |
| `MenuItem.label: String` | 无 | 不动，见 §3.6 |
| 框架自带文案默认语言 | 无 | 默认恒 `zh-CN`，与今天逐字相同 |
| prelude 新增符号 | 极低 | 仅名字冲突风险 |

结论：一次 minor 版本可以承载 P0 + P1，`TextContent` 那条随版本说明写清并补
`#[non_exhaustive]`。
