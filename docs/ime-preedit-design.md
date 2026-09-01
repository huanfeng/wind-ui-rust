# 输入法合成串（preedit）内联显示设计方案（windui）

打通「输入法未提交合成串」从平台层到控件层的数据通路，并在 `TextInput` 内**内联绘制**
合成串（下划线 + 合成内光标 + 参与换行）。

**触发原因**：macOS 用户反馈"输入时看不到拼音编码"。这不是环境问题，是
`src/platform/macos/window.rs:566-572` 与 `docs/MACOS_PORTING.md:234-238` 已记录的
未消除语义差——合成串在 macOS 上被平台层丢弃，且上层无 API 承载。

## 1. 问题定位

### 1.1 两平台的输入法模型不同

| | Windows（现状正常） | macOS（现状不显示） |
|---|---|---|
| 谁画合成串 | **系统 IME 自己画** | **只能客户端画**，系统绝不代画 |
| 库的职责 | 用 `ImmSetCompositionWindow(CFS_POINT)` + `ImmSetCompositionFontW` 告诉 IME 画在哪、多大（`win32/mod.rs:2584-2618`） | 实现 `NSTextInputClient`，自己接住 marked text 并渲染 |
| 合成期间自绘光标 | 藏起来是在**消除双光标**（`inputs.rs:1551`） | 藏起来变成**既无合成串也无光标** |

Win32 的 IMM32 允许应用停在 over-the-spot 由系统代画，这是 Win32 的历史特例，不是通例。
AppKit 的 `NSTextInputClient` **只有 on-the-spot（内联）一档**——实现了协议就等于承诺自己画。
Linux 的 GTK/IBus 同样是 `preedit-changed` 信号交客户端画。

因此当前"Windows 正常"是平台特性带来的运气，不是设计已覆盖。

### 1.2 缺口的精确位置

```rust
// src/platform/macos/window.rs:582-588 —— 字符串被丢弃，只留一个 bool
fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
    let composing = !anyobject_to_string(string).is_empty();
    self.ivars().borrow_mut().composing = composing;
    self.dispatch_composing(composing);
}
```

这个 bool 的下游：`AppHandler::set_ime_composing`（`app/mod.rs:2558`）→
`Tree::set_composing`（`core.rs:2026`）→ `Widget::set_composing`（`core.rs:212`，默认空实现）
→ `TextInput::set_composing`（`inputs.rs:1906`，只存进 `Cell<bool>`）。

全库唯一消费点是 `inputs.rs:1551`——抑制自绘光标。合成串本身**从未离开平台层**。

### 1.3 行业通行做法

| 框架 | 平台层抛出什么 | 谁渲染 |
|---|---|---|
| AppKit 原生 `NSTextView` | — | 合成串真正插入 `NSTextStorage`，带下划线属性与 `NSMarkedClauseSegment`，走正常 TextKit 排版 |
| winit | `Ime::Preedit(String, Option<(usize, usize)>)` / `Ime::Commit(String)` | 上层应用；winit 自己不画 |
| egui | 消费 winit 的 `Ime` 事件 | `TextEdit` 把 preedit 插到光标处一起排版 |
| Qt | `QInputMethodEvent`（preeditString + `Attribute` 携带 TextFormat/Cursor） | 控件在 `inputMethodEvent()` 里合进显示文本 |
| Chromium / Blink | `ImeSetComposition` | 合成串作为文本节点参与布局 |
| Flutter | `TextEditingValue` 带 `composing` range | Dart 侧 `EditableText` |
| SDL | `SDL_TextEditingEvent`（text + start + length） | 应用自己画 |

**没有一行是"系统帮我画"。** winit 与本库处在同一抽象位置（平台窗口层），
它抛出的是完整字符串 + 光标范围；本库当前只抛一个 `bool`——这就是缺口的准确定位。

## 2. 目标与非目标

### 目标

- macOS 上合成串**内联显示**在文本框内，参与测量与换行，与原生观感一致。
- 合成串带下划线；IME 报告的合成内光标位置正确绘制。
- 候选窗跟随合成串推移（而非钉在合成开始前的原光标处）。
- 补齐 `NSTextInputClient` 四处协议 stub，使第三方 IME（搜狗/微信等）不走降级路径。
- Windows 行为**零变化**（继续系统内联），不引入双份合成串。
- 通路为 Linux 预留：抽象层不含 macOS 专有概念。

### 非目标（本版边界）

- Windows 改为自绘（见 §9，收敛方向但不在本轮）。
- 日文分节转换的**分句级**下划线粗细区分（需 `NSMarkedClauseSegment`，见 §5.4，留 P1）。
- `RichText` / 其他文本控件的 preedit（本轮只覆盖 `TextInput`）。
- 输入法的「重转换」（reconversion）与上下文预测所需的完整文档访问。

## 3. 设计原则对齐

| 铁律 | 本方案的落实 |
|------|-------------|
| Widget 是纯内容，不持有节点树 | preedit 存在 `TextInput` 自身字段，经 `Widget::set_preedit` 下发 |
| 控件不硬编码视觉 | 下划线颜色/粗细走 `TextInputTheme` 回退，不写死 |
| 平台差异收口在平台层 | 平台层统一抛 `Preedit` 值；Windows 恒抛空，控件层无 `cfg(target_os)` |
| 三阶段布局 | preedit 经 `display_string()` 参与 measure/arrange，不做绘制期特判 |
| 空闲零 CPU | 合成串变化才标脏，无常驻续帧 |

## 4. 架构：四层

```
L1 平台层   macOS: setMarkedText: 取串 + selectedRange  → dispatch_preedit
            win32: 不变（系统内联），恒不上报 preedit
                             ↓
L2 SPI      AppHandler::set_ime_preedit(&Preedit) -> bool
                             ↓
L3 核心     Tree::set_preedit(NodeId, &Preedit) → Widget::set_preedit
                             ↓
L4 控件     TextInput：display_string() 插入合成串 + 索引映射 + 下划线绘制
```

### 核心类型（放 `src/event.rs`，与 `KeyEvent` 同处）

```rust
/// 输入法未提交的合成串（preedit / marked text）。
/// 空 `text` 表示合成结束；平台层保证 `caret` / `sel` 以**字符**（非 UTF-16、非字节）计。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preedit {
    /// 合成串本体。空串 = 无合成。
    pub text: String,
    /// 合成串内的光标位置（字符索引，0..=text.chars().count()）。
    pub caret: usize,
    /// 合成串内当前"选中分句"的字符范围（日文分节转换用；中文拼音一般为整段或 None）。
    pub sel: Option<(usize, usize)>,
}

impl Preedit {
    pub fn is_active(&self) -> bool { !self.text.is_empty() }
}
```

**为什么 `caret` 用字符索引而不是 UTF-16**：平台边界是唯一知道 UTF-16 的地方，
换算必须在那里做完（§5.3）。让字符索引穿过 SPI，上层就与平台编码无关，Linux 接入不用再改。

## 5. L1：macOS 平台层（`src/platform/macos/window.rs`）

### 5.1 `ViewState` 字段

```rust
// 替换现有的 composing: bool
/// 输入法合成串（marked text）。`is_active()` 即原先的 composing 语义。
preedit: Preedit,
```

`composing` 的所有读点（`window.rs:1361` keyDown 分流、`window.rs:1546` abort、
`markedRange` / `hasMarkedText`）改读 `preedit.is_active()`。

### 5.2 `setMarkedText:selectedRange:replacementRange:`

```rust
fn set_marked_text(&self, string: &AnyObject, selected: NSRange, _replacement: NSRange) {
    let text = anyobject_to_string(string);   // 已同时处理 NSString / NSAttributedString
    let caret = utf16_to_char_index(&text, selected.location);
    let sel = (selected.length > 0).then(|| (caret, utf16_to_char_index(&text, selected.location + selected.length)));
    let pe = Preedit { text, caret, sel };
    self.ivars().borrow_mut().preedit = pe.clone();
    self.dispatch_preedit(&pe);
}
```

`unmarkText` / `insertText:` / `abort_composition`（`window.rs:1545`）三处改为写入
`Preedit::default()` 并 `dispatch_preedit`。

### 5.3 UTF-16 ↔ 字符索引换算（**静默出错点**）

`NSRange` 以 **UTF-16 码元**计，Rust `String` 以字节存、按 `char` 迭代。BMP 内的中文两者
一致，**emoji 与部分生僻字（U+10000 以上）在 UTF-16 里占 2 个码元**，直接当字符索引用会
偏移。这类偏移不会 panic，只表现为合成内光标位置错——必须显式换算：

```rust
/// UTF-16 码元下标 → 字符下标。越界钳到末尾。
fn utf16_to_char_index(s: &str, utf16_idx: NSUInteger) -> usize {
    let target = utf16_idx as usize;
    let mut u16_pos = 0usize;
    for (ci, ch) in s.chars().enumerate() {
        if u16_pos >= target { return ci; }
        u16_pos += ch.len_utf16();
    }
    s.chars().count()
}
```

反向换算（`char_index_to_utf16`）供 `markedRange` 使用。

### 5.4 补齐四处协议 stub

| 方法 | 现状（行号） | 改为 |
|---|---|---|
| `markedRange` | `:601-608` 合成中返回 `{0,0}`，长度恒 0——与 `hasMarkedText` 自相矛盾，部分 IME 据此走降级路径 | `{0, preedit UTF-16 长度}`；无合成时 `{NSNotFound, 0}` |
| `selectedRange` | `:596-599` 恒 `{0,0}` | 经新增 `AppHandler::ime_selection()` 取焦点控件真实选区（UTF-16） |
| `firstRectForCharacterRange:` | `:626-632` 恒返回原光标矩形 | 返回请求 range 起点的矩形。合成串内联后 `ime_caret()` 天然随合成推移，候选窗即自动跟随 |
| `attributedSubstringForProposedRange:` | `:615-622` 返回 `None` | 返回请求范围的纯文本 `NSAttributedString`；影响重转换与部分 IME 的上下文预测 |

另：`validAttributesForMarkedText`（`:624-629`）返回空数组等于告诉系统"不支持任何属性"。
中文拼音只有一段，纯文本够用；日文分节转换需要 `NSMarkedClauseSegment` 才能知道
哪一段是当前分句。**本轮保持空数组**（`sel` 从 `selectedRange` 参数已能拿到粗粒度信息），
日文完整支持列为 P1。

### 5.5 合成期间的鼠标点击

macOS 原生行为：合成中点击别处会先提交合成串。当前 `mouseDown` 未做处理，会让合成串
悬空。改为：`dispatch_pointer` 前若 `preedit.is_active()`，先调 `abort_composition()`
（内含 `ic.discardMarkedText()`）。与 `windowDidResignKey:` 的兜底同一条路径。

## 6. L2/L3：SPI 与核心（`src/platform/mod.rs`、`src/core.rs`、`src/app/mod.rs`）

```rust
// platform/mod.rs —— AppHandler，紧邻现有 set_ime_composing(:726)
/// 输入法合成串变化。返回 true 表示需重绘。
/// 平台层若走系统内联绘制（win32）则永不调用本方法。
fn set_ime_preedit(&mut self, _pe: &Preedit) -> bool { false }
/// 焦点文本控件的当前选区（字符索引），供输入法查询。无焦点返回 None。
fn ime_selection(&self) -> Option<(usize, usize)> { None }
```

```rust
// core.rs —— Widget，紧邻现有 set_composing(:212)
fn set_preedit(&mut self, _pe: &Preedit) {}
fn selection_range(&self) -> Option<(usize, usize)> { None }
```

`set_ime_composing` / `Widget::set_composing` **保留不动**：win32 仍只走它。
两条路互不干扰——win32 永不调 `set_ime_preedit`，macOS 永不调 `set_ime_composing`
（合成态由 `Preedit::is_active()` 隐含）。

> **注意**：`TextInput::set_composing` 抑制光标的逻辑（`inputs.rs:1551`）在 macOS 路径下
> 必须**失效**——合成串内联后我们要自己画合成内光标。实现上让 `set_preedit` 同时更新
> `composing` 字段（`composing = pe.is_active()`），并把 `:1551` 的判据从
> "非 composing 才画" 改为 "非 composing **或** 有 preedit 时按合成规则画"（§7.4）。

## 7. L4：`TextInput` 内联绘制（`src/ui/inputs.rs`）

### 7.1 接入点：`display_string()`

`display_string()`（`:865-874`）已是"实际用于显示与测量的字符串"的唯一收口，
paint 中只调用一次（`:1405`），后续 `rebuild_layout` / 选区 / 光标全部基于返回的 `disp`。
preedit 接在同一处：

```rust
fn display_string(&self) -> String {
    let base = self.text.with(|t| if self.config.password {
        t.chars().map(|_| PASSWORD_MASK).collect()
    } else { t.clone() });
    let pe = self.preedit.borrow();
    if !pe.is_active() { return base; }
    // 合成串插在逻辑光标处。密码模式下合成串**不**掩码：IME 候选就在屏幕上，
    // 掩码合成串既无安全收益又让用户无法确认输入（原生 NSSecureTextField 同此）。
    let mut out: String = base.chars().take(self.cursor).collect();
    out.push_str(&pe.text);
    out.extend(base.chars().skip(self.cursor));
    out
}
```

`rebuild_layout` 的缓存键（`:1027-1040`）第一项就是 `disp`，合成串变化自动触发重排，
**无需额外失效逻辑**。

### 7.2 索引映射（**本改动最容易静默出错的地方**）

密码掩码是**等字符数替换**，故 `:869` 注释说"光标/选区索引可直接复用"。
preedit 是**插入**，这条前提失效。`:1435` 的

```rust
let cursor = self.cursor.min(disp.chars().count());
```

把逻辑索引直接当显示索引用，插入 preedit 后光标与选区高亮会偏移 `preedit.len` 个字符。
引入显式映射：

```rust
/// 逻辑字符索引 → 显示串字符索引。合成串插在 self.cursor 处，故其后的索引整体右移。
fn to_display_index(&self, i: usize) -> usize {
    let pe = self.preedit.borrow();
    if !pe.is_active() || i <= self.cursor { i } else { i + pe.text.chars().count() }
}
/// 显示串字符索引 → 逻辑字符索引。落在合成串内部的一律钳到 self.cursor
/// （合成中点击会先中止合成，见 §5.5，故此情形只在同帧竞态出现）。
fn to_logical_index(&self, d: usize) -> usize {
    let pe = self.preedit.borrow();
    if !pe.is_active() { return d; }
    let n = pe.text.chars().count();
    if d <= self.cursor { d } else if d < self.cursor + n { self.cursor } else { d - n }
}
```

### 7.3 跨映射层的同步点（已核对，共 5 处）

按 `grep -n "self\.cursor\|self\.anchor" src/ui/inputs.rs` 全量核对。编辑路径
（`insert` `:917` / `backspace` `:932` / `delete` `:948` / `move_to` `:966` / `paste` `:1248`
/ `select_word` `:974` / `select_all` `:980` / `select_para` `:1013`）在合成期间**不会被触发**
——macOS 合成中所有按键交 IME（`window.rs:1362`），点击先中止合成（§5.5）。
真正跨层的只有下列 5 处，全在 paint 与命中路径：

| # | 位置 | 现状 | 改为 |
|---|---|---|---|
| 1 | `:1435` paint 光标索引 | `self.cursor.min(disp_len)` | `self.to_display_index(self.cursor).min(disp_len)`；有 preedit 时再加 `pe.caret` |
| 2 | `:1491` paint 选区高亮（`selection()` `:884` 的结果直接索引 `disp`） | 逻辑索引直用 | 两端各过 `to_display_index` |
| 3 | `caret_line_x(&lay, self.cursor…)`（`move_vertical`） | 逻辑索引查视觉行 | **实现时判定为不可达，未改**：见下 |
| 4 | `cursor_line(&lay, self.cursor…)`（`cur_line_bounds`） | 同上 | 同上 |
| 5 | `pos_to_index`（定义 `:1119`，调用 `:1696` 等 5 处）命中测试 | 返回显示索引当逻辑索引用 | 在**函数内部**收口 `to_logical_index`，而非在 5 个调用点各包一层 |

> **实现修正（#3/#4）**：这两处走的是键盘导航路径（上下移动、Home/End）。macOS 合成期间
> 所有按键都交给 IME（`window.rs` 的 `on_key` 早退），win32 上 `preedit` 恒空，
> 两边都到不了这里，映射会是恒等。与其加一层未经测试、且需要反向映射的代码，
> 不如如实记下不可达。若将来某平台在合成期间仍下发导航键，这两处要一并补上。

`caret_local`（`:1546-1547`）记录的是**合成内光标**的位置——这正是候选窗要跟随的点，
因此 `ime_caret()` 无需改动即自动正确（§5.4 第三行）。

### 7.4 绘制规范

合成串区间 `[to_display_index(cursor), +pe.text.len)` 在正文之上叠加：

- **下划线**：合成段全长一条 1dp 实线（HiDPI 下 `max(1, round(1 * scale))`），
  颜色取 `InputTheme.preedit_underline`（`src/theme.rs:339`）→ 回退到文字色 60% alpha。
- **选中分句**（`pe.sel` 为 `Some` 时）：该子段下划线加粗至 2dp。**不**用背景高亮——
  会与选区高亮撞色，且原生 macOS 也是靠下划线粗细区分。
  实现上必须把每个视觉行的合成段按分句边界切成「细—粗—细」最多三段分别绘制；
  按整行判一个粗细会让「分句与本行有交集就整行加粗」，分节转换完全看不出转换到哪一段
  （初版实现就是这个 bug，靠视觉验证发现）。
- **合成内光标**：在 `pe.caret` 处按现有 `CaretOpts` / `caret.rs` 画，
  但**恒实心不闪烁**（合成期间闪烁会与 IME 候选窗的动效互相干扰，原生同此）。
- 合成期间**不画**正文的普通光标（避免双光标）。

`:1551` 的判据相应改为：

```rust
if focused {
    if pe.is_active() { /* 画合成内光标，恒实心 */ }
    else if !self.composing.get() { /* 原逻辑：闪烁光标 */ }
}
```

保留 `!self.composing.get()` 分支是为了 win32——那边仍是系统内联，仍须藏光标。

### 7.5 脏区

合成串改变文本宽度与换行，属**非局部变更**。按既有规则（per-node 脏区只对自包含视觉安全），
`set_preedit` 必须走整窗失效（`mark_dirty_all`），不能用 per-node 脏区。
`Tree::set_preedit` 返回 true 时由 `app/mod.rs` 触发整窗重绘，与 `set_composing` 现有路径一致。

## 8. Windows 侧：保持不动

win32 继续走 `WM_IME_*` + `ImmSetCompositionWindow`（`win32/mod.rs:1563-1583, 2584-2618`），
`set_ime_preedit` 永不调用，`TextInput::preedit` 恒为空，`display_string()` 走原路径。

**绝不能两边同时自绘**：Windows 系统 IME 已经在 `ImmSetCompositionWindow` 指定处画了一份，
再自绘一份就是双份合成串。这是本方案唯一的致命回归形态，必须在测试里显式覆盖（§11）。

## 9. 长期收敛方向（不在本轮）

三大平台里两个要求客户端自绘（macOS、Linux/IBus），Windows 是唯一特例。长期看让 win32
也改自绘（`WM_IME_COMPOSITION` 取 `GCS_COMPSTR`、吞掉默认处理不传 `DefWindowProc`）
会让三平台语义统一，且本方案的 L2/L3/L4 全部可直接复用——**届时只需删掉 win32 的
`set_ime_composing` 分支，改调 `set_ime_preedit`**。本轮刻意不动，因为 Windows 路径当前
工作正常，回归风险不对称。

## 10. 分期

| 期 | 内容 | 风险 |
|---|---|---|
| **P0** | §4 类型 + §5 macOS 平台层（含四处 stub、UTF-16 换算、§5.5 点击中止）+ §6 SPI + §7 控件内联 | 中：集中在 §7.3 的 5 处索引同步点 |
| P1 | `NSMarkedClauseSegment` 支持，日文分节转换的分句级下划线 | 低，纯增量 |
| P2 | win32 改自绘，三平台语义统一（§9） | 高，动已正常路径 |

## 11. 测试策略

### 单元 / 契约测试（经真实路径，不 mock）

1. **索引映射**：`to_display_index` / `to_logical_index` 往返一致性，覆盖
   `i < cursor` / `i == cursor` / `i > cursor` / 无 preedit 四档。
2. **UTF-16 换算**：`utf16_to_char_index` 对纯 ASCII、BMP 中文、**emoji（代理对）**
   三类输入。emoji 那档是唯一能暴露"直接把 UTF-16 下标当字符下标"的用例。
3. **display_string 插入**：给定 text/cursor/preedit，断言输出串与字符数；
   密码模式下断言合成串**未**被掩码。
4. **布局重排触发**：`set_preedit` 后 `rebuild_layout` 的缓存键失配（合成串变化必须重排）。
5. **合成内光标位置**：`pe.caret` 落在合成串首/中/尾时 `caret_local` 的 x 单调递增。
6. **win32 无回归**：断言 win32 路径下 `TextInput::preedit` 恒空、`display_string()`
   与改动前逐字节相同——这是 §8 双份合成串回归的守卫。

**按既有约定做破坏性验收**：上述 1、2、6 三条必须靠**故意改坏实现**验证会红
（例如把 `to_display_index` 的 `i <= self.cursor` 改成 `i < self.cursor`），
否则不算通过——避免期望值从被测实现反推的自证循环。

### 截图验证

`examples/ime.rs` 已存在，扩展一个走 `set_preedit` 注入合成串的截图用例
（不依赖真实 IME）：断言合成段有下划线、光标在合成内、正文未被破坏。
按既有约定**量化**核验（墨量而非像素数），且改动后跑整页截图回归。

### 真机验证的现实约束

macOS 验证机**没有辅助功能授权**，无法合成真实键盘事件驱动 IME，
所以"打拼音看合成串"的端到端只能人工在真机上验。自动化能覆盖的上限是
`set_preedit` 注入后的渲染契约。文档需如实记录这一条，不能把契约测试通过
说成端到端已验证。

**人工验证清单**（真机，`examples/ime.rs`）：

- [ ] 中文拼音输入：合成串内联可见、带下划线、候选窗跟在合成串下方而非原光标处
- [ ] 合成串跨行：长拼音串触发换行，正文正确让位
- [ ] 合成中 Cmd+Tab 切走再切回：合成串消失、光标恢复闪烁、已输入拼音不莫名上屏
- [ ] 合成中点击文本框别处：合成先提交/放弃，不悬空
- [ ] emoji 输入（Ctrl+Cmd+空格）：合成内光标位置正确
- [ ] Windows 回归：同一 example 在 Windows 上仍是**单份**合成串

## 12. 文件改动清单

| 文件 | 改动 |
|---|---|
| `src/event.rs` | 新增 `Preedit` 类型 |
| `src/lib.rs` | 导出 `Preedit` |
| `src/platform/mod.rs` | `AppHandler::set_ime_preedit` / `ime_selection` 默认实现（`:726` 邻位） |
| `src/platform/macos/window.rs` | `ViewState.composing` → `preedit`；`setMarkedText:` 取串；`unmarkText`/`insertText:`/`abort_composition` 清空；四处协议 stub 补齐；UTF-16 换算助手；`mouseDown` 中止合成 |
| `src/platform/win32/mod.rs` | **不改** |
| `src/core.rs` | `Widget::set_preedit` / `selection_range` 默认实现（`:212` 邻位）；`Tree::set_preedit`（`:2026` 邻位） |
| `src/app/mod.rs` | `set_ime_preedit` / `ime_selection` 转发到焦点节点（`:2558` 邻位） |
| `src/ui/inputs.rs` | `TextInput.preedit` 字段；`display_string()` 插入；`to_display_index`/`to_logical_index`；§7.3 五处同步点；§7.4 绘制；`set_preedit` 实现 |
| `src/theme.rs` | `InputTheme`（`:339`）加 `preedit_underline`（可选，回退文字色 60%） |
| `examples/ime.rs` | 注入式 preedit 截图用例 |
| `docs/MACOS_PORTING.md` | `:101` 表格与 `:234-238` 的"已知语义差"改写为已消除 |
| `docs/API_GUIDE.md` | 新增 `Preedit` 与自定义控件接入 preedit 的说明 |
| `CHANGELOG.md` | 记录本次修复 |
