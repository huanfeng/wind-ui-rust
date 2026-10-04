# Linux 后端（X11 / Wayland）

本文给在 Linux 上接续开发的人：后端怎么分层、选了哪些依赖和为什么、哪些已经实测过、
哪些还没做，以及在没有桌面的机器上怎么验证。

> 当前状态：**基本可用**。窗口与事件循环、软件呈现、自带文字栈（fontconfig 选字）、
> HiDPI、光标、滚轮、键盘（含快捷键）、剪贴板、XIM 输入法（合成串内联绘制）、
> 无边框窗口（拖动 / 边缘缩放 / 双击最大化）、多窗口与模态、文件拖入、全局热键、
> 单实例转发、`--screenshot` 离屏截图都已落地。`cargo build` / `cargo test`（默认与
> `--features gpu` 两档）/ `cargo clippy` 在 Linux 上通过。
>
> 尚未实现：系统托盘、文件拖出、零窗口常驻（`App::run_resident`）、窗口模式下的
> GPU 后端。这些入口都存在且不 panic——空操作并记日志，API 形状与另两个平台一致，
> 下游不必按平台分支。
>
> **原生 Wayland 后端正在分阶段落地**（`wayland` feature，默认开），现状见 §8。

---

## 1. 分层

与 macOS 移植同一套缝（见 `MACOS_PORTING.md` §1）：上层只依赖 `crate::platform::*`
与 `crate::text::*`，`cfg` 分发集中在 `src/platform/mod.rs` 与 `src/text/mod.rs`。

| 缝 | Linux 实现 | 文件 |
|----|-----------|------|
| `platform::run` 等 | 运行期选后端（见 §8.1） | `src/platform/linux/mod.rs` |
| X11 | 事件循环、窗口、呈现、窗口操作 | `src/platform/linux/x11.rs` |
| Wayland | 事件循环、窗口、`wl_shm` 呈现（见 §8） | `src/platform/linux/wayland.rs` |
| 宿主簿记 | 与协议无关、两个后端共用：点击计数、定时器、帧配速、出帧、事件后意图 | `src/platform/linux/host.rs` |
| 键位 | 核心协议键盘映射表 → keysym → `Key` | `src/platform/linux/keys.rs` |
| 输入法 | XIM 客户端（合成串回调风格） | `src/platform/linux/ime.rs` |
| `Clipboard` | 独立线程拥有 `CLIPBOARD` 选区 | `src/platform/linux/clipboard.rs` |
| 文件拖入 | XDND v5 目标端 | `src/platform/linux/dnd.rs` |
| 全局热键 | 根窗口 `GrabKey` | `src/platform/linux/hotkey.rs` |
| 事件循环原语 | `poll(2)` + 唤醒 socketpair | `src/platform/linux/sys.rs` |
| `PlatformTextEngine` | fontconfig + ttf-parser + ab_glyph_rasterizer | `src/text/linux/` |

## 2. 依赖取舍：小依赖优先

| 依赖 | 用途 | 为什么是它 |
|------|------|-----------|
| `x11rb` 0.13 | X11 协议 | 纯 Rust 实现协议，不链接 libX11/libxcb，编译期不要任何 `-dev` 包。钉 0.13 是为了与 `xim` 共用连接类型 |
| `xim` 0.5 | XIM 输入法协议 | fcitx5 / ibus 都提供 XIM 前端；比 D-Bus 路线（要拉进 zbus 整套异步栈）轻得多 |
| `ttf-parser` | 字体解析 | 零依赖；读 cmap / hmtx / kern / 轮廓 |
| `ab_glyph_rasterizer` | 轮廓 → 覆盖度 | 零依赖、单文件，精确面积覆盖 |
| `rfd`（`xdg-portal` 特性） | 文件对话框 | 走 D-Bus 门户、无门户时回退 zenity；不用 `gtk3` 特性——那会把整套 GTK 拉进进程 |

**fontconfig 在运行期 `dlopen`**（`text/linux/fontconfig.rs`），不在编译期链接：构建机不必
装 `libfontconfig1-dev`，运行期拿不到时退回扫字体目录（`/usr/share/fonts` 等，按内置
偏好序挑字体，CJK 字体优先以求中西文同源）。`WINDUI_NO_FONTCONFIG=1` 可强制走兜底路径。

刻意**没有**用的：winit（依赖树大、事件循环所有权模型与本库的平台层形状不合）、
softbuffer（X11 下同样是 PutImage/SHM，多一层抽象）、cosmic-text / HarfBuzz / FreeType
（前者依赖树大且启动要扫全部系统字体，后两者是 C 库、要 `-dev` 包）。

## 3. 文字栈的取舍

写在明处，排查「为什么这段字不对」时先看这里：

- **不做复杂文字整形**：阿拉伯文连写、印度系文字重排、连字都没有，逐字从左到右排。
  中文 / 西文 / 日文 / 韩文 UI 文本不受影响。
- **无 hinting**：灰度抗锯齿 + 4 档水平亚像素相位（与 Core Text 同粒度），观感接近 macOS。
- **字距只读旧式 `kern` 表**，只放在 GPOS 里的字距不生效。
- **缺粗体 / 斜体字面时合成**：水平加粗（字号的 1/24，限 0.5–2px）/ 0.2 斜切。
- **彩色 emoji 不渲染**（CBDT/COLR 位图字体没有轮廓），缺字落回主字体的 `.notdef` 方框。
- 折行：西文按空白断、超长词硬折；CJK 逐字可断，只做一层避头尾（闭合标点不落行首、
  开启标点不落行尾）。见 `text/linux/layout.rs`。
- 测量与绘制走同一条排版路径、同一物理字号，纵向定位满足 `TextEngine::draw` 的契约
  （`text_block_contract` 那组测试在 Linux 上跑的就是这个引擎）。

字体文件 `mmap` 只读映射、永不卸载：映射页是文件后备的共享页，不计入私有内存。

字形位图按「字体 + 字形 + 物理字号 + 4 档相位 + 合成标志」缓存，两代轮换（每代 4096 条，
命中上一代即提升）：界面常用字形超过一代容量时，每帧会重光栅超出的那部分，峰值约
8192 张小位图 / 每个文字引擎。测量结果另有缓存（2048 条，DPI 变化时清空）。

## 4. 与 win32 / macOS 的行为差异

- **没有同步重入**。X 是异步协议，请求只写进发送缓冲、结果以事件形式在之后的循环里
  回来，铁律 6（OS 重入前释放借用）天然成立。仍沿用「分发 → `after_event` 收尾」结构，
  是为了与另两个平台的时序逐项对齐。
- **帧配速**：按 60Hz 定超时（X 上没有可等的垂直同步信号），与 win32 同一套
  `max(刷新率间隔, 控件自报的下次变化时刻)`；无动画时阻塞在 `poll`，空闲零 CPU。
- **HiDPI**：`WINDUI_SCALE` 环境变量 > X 资源库的 `Xft.dpi`（桌面环境的缩放设置最终都
  落在这里）> `GDK_SCALE` > 1.0。运行期改 DPI 不跟随。
- **无边框窗口**：拖动 / 缩放经 `_NET_WM_MOVERESIZE` 交给窗口管理器，**移出阈值才发**
  ——按下就发的话，WM 收到时松开可能已经发生，WM 停在移动态、把下一次点击当作结束移动
  吃掉，双击最大化永远凑不齐第二下。
- **全局热键**：一个组合抓四遍（叠加 CapsLock / NumLock 的变体），否则开着数字锁就不灵。
  Wayland 会话下经 XWayland 只在 X 客户端有焦点时生效——协议限制，要走
  xdg-desktop-portal 的 GlobalShortcuts。
- **剪贴板**：独立线程 + 独立连接专职应答 `SelectionRequest`，与窗口生命周期脱钩；
  不支持 INCR 分段传输（超大段文本读取为空）。
- **双击**：时限 400ms、漂移 4 逻辑像素，平台层自己折算（X 不报点击计数）。
- **触摸惯性**：未接（X 核心协议无触摸事件）；触控板两指滚动经滚轮按钮 4/5 到达。
- **输入法进程中途退出**：XIM 没有断线通知，按键仍会转给已不存在的输入法而得不到回应
  （表现为打不出字），需重启应用。连接建立前输入法没启动则整个模块不启用、按键走本地。
- **非拉丁布局**：旧式 Cyrillic / Greek keysym 已映射成字符；快捷键在这类布局下回退到同一
  物理键位上的拉丁字母（Ctrl+С 即 Ctrl+C），与 win32 按物理 VK 取码一致。其它旧式段
  （阿拉伯、希伯来、泰文等）未映射，需经输入法输入。
- **字体文件被原地改写**：字体以 mmap 只读映射，运行中被截断会触发 SIGBUS（包管理器
  升级字体一般是换新文件而非原地改写，不受影响）。

## 5. 实测数据

Xvfb（1280×800×24）+ openbox，release 构建，`about` 示例（620×556）：

| 缩放 | 私有内存（RssAnon） | 文件映射（RssFile） |
|------|--------------------|--------------------|
| 100% | 3.7 MB | 5.6 MB |
| 200%（`WINDUI_SCALE=2`） | 8.8 MB | 5.6 MB |

对照 Windows 同一示例 5.5 MB / 14.2 MB（口径不同：那边是 PrivateBytes）。

## 6. 在无桌面的机器上验证

截图回归（`--screenshot` 等，见 `AGENTS.md` §5）在 Linux 上直接可用，不需要 X 服务器。
真窗口验证需要一个 X 服务器，无 root 权限时可以只解包不安装：

```bash
mkdir -p ~/.local/xvfb && cd ~/.local/xvfb
apt-get download xvfb xserver-common libxfont2 libfontenc1 x11-xkb-utils \
                 xdotool libxdo3 x11-apps x11-utils xclip openbox libobrender32v5 \
                 libobt2v5 libstartup-notification0 libimlib2t64
for f in *.deb; do dpkg -x "$f" root; done
export LD_LIBRARY_PATH=$PWD/root/usr/lib/x86_64-linux-gnu PATH=$PWD/root/usr/bin:$PATH
export XDG_DATA_DIRS=$PWD/root/usr/share:/usr/share
```

⚠ Xvfb 硬编码从 `/usr/bin/xkbcomp` 编译键盘映射，解包到别处时键盘初始化失败
（`XKB: Failed to compile keymap`）。把二进制里那个 `/usr/bin` 字符串改成等长的
`/tmp/xkb`，再把 `xkbcomp` 软链到 `/tmp/xkb/` 下即可。

之后：`Xvfb :77 -screen 0 1280x800x24 &`、`DISPLAY=:77 openbox &`，用 `xdotool` 合成
点击 / 键入 / 拖动，`xwd -root` 抓屏（xwd → PNG 用一小段 Python 解码即可），`xclip`
验证剪贴板互通。

输入法与文件拖入没有现成的无头工具可用：`xim` crate 带服务端特性，写一个几十行的
测试 XIM 服务端（字母进合成串、空格提交）即可在 `XMODIFIERS=@im=<名字>` 下走完整协议；
XDND 同理写一个最小拖放源（发 Enter/Position/Drop、应答 `XdndSelection`）。两者都是
真协议往返，不是 mock。

## 7. 后续

按价值排序：

1. **系统托盘**：StatusNotifierItem（D-Bus）是 KDE / GNOME 扩展的主流；老式 XEmbed
   托盘可做纯 X11 实现但覆盖面在缩小。托盘菜单需要一个 override-redirect 弹出窗口
   复用本库的菜单渲染。
2. **MIT-SHM 呈现**：大窗口整窗帧下 `PutImage` 要把整帧过一遍 socket，SHM 可省掉这次拷贝。
3. **窗口模式 GPU**：`src/render/gpu/` 已在 Linux 编译通过，差的是从 X 窗口建 wgpu surface。
4. **Wayland 原生**：进行中，见 §8 与仓库根 `IMPLEMENTATION_PLAN.md`。
5. **文件拖出**（XDND 源端）、**零窗口常驻**、运行期 DPI / 主题跟随。

## 8. Wayland 原生后端（进行中）

计划分五阶段（仓库根 `IMPLEMENTATION_PLAN.md`）。Stage 1–4 已完成并经 GNOME 42 真桌面验证：
窗口与呈现；指针、键盘、光标、HiDPI、无边框拖动 / 缩放；剪贴板（文本）与文件拖入；输入法
（text-input-v3）。Stage 5（窗口装饰：服务端装饰协商 + 客户端标题栏、模态对话框登记）已实现，
待真桌面人工验证。

> ⚠ **默认不启用**：只有 `WINDUI_BACKEND=wayland` 才走它，其余情况 Wayland 会话照旧经
> XWayland 运行。改为自动优先的条件与剩余风险见 §8.1。

### 8.1 后端选择

编译期：`wayland` feature（默认开；依赖只声明在 Linux 的 target 段，别的平台开着也
不多编东西）。`--no-default-features` 得到纯 X11 后端。

运行期（`platform/linux/mod.rs` 的 `choose_backend`，有单测）：

| 条件 | 结果 |
|------|------|
| `WINDUI_BACKEND=wayland` | 试 Wayland；连不上（含缺 `xdg_wm_base` / `wl_shm` / v4+ `wl_compositor`）`eprintln` 提示后**回退 X11**——显式指定也不该让程序起不来 |
| `WINDUI_BACKEND=x11`、未设、或值认不出（记警告） | X11（Wayland 会话经 XWayland） |
| 点名 wayland 但编译时关了 feature | 提示后走 X11 |

**何时改为自动优先**（决定权在维护者与用户，代码只改 `choose_backend` 未设时的分支与其单测）：
把未设时改成「有 `WAYLAND_DISPLAY` / `WAYLAND_SOCKET` 就优先 Wayland、连不上回退 X11」。条件：

1. Stage 5 的人工清单在 GNOME（必须 CSD 的那一类）上通过，且至少一个给服务端装饰的桌面
   （KDE / Deepin Treeland / sway）上确认「不重复画标题栏」。
2. 下面的剩余风险逐条被接受或补上。

剩余风险（自动优先后，原本经 XWayland 正常工作、换到原生后端会变差或变化的）：

- **全局热键失效**：XWayland 下 `GrabKey` 至少在本程序有焦点时生效；原生 Wayland 下不注册
  （见 §8.4）。用了 `App::hotkey` 的应用需要文档化的兜底，或这类应用显式 `WINDUI_BACKEND=x11`。
- **剪贴板仅界面线程**：其它线程读写当空（XWayland 下任何线程可用）。
- **窗口坐标 / 居中、是否最小化**：协议不提供，`centered` 无效、`minimized` 恒 false。
- **CSD 外观**：自绘标题栏是方的、无阴影、缩放边在窗口内侧 6 像素（GNOME 原生应用在窗口外有
  不可见缩放边与圆角阴影），视觉与 GNOME 原生应用不一致；标题栏按钮不跟合成器能力
  （`wm_capabilities`）隐藏，最大化后按钮图标不变成「还原」。
- **唤起已显示的窗口**（单实例第二次启动、托盘）要 `xdg-activation-v1`，未实现：窗口已显示时
  不会被提到前台。
- **文件拖出、primary selection（中键粘贴）、系统托盘** 在两条路上都没有，不算回退。
- 默认后端一变，所有下游应用同时换路径：建议先在 CHANGELOG 里公告一个版本，留出
  `WINDUI_BACKEND=x11` 作为逃生口。

### 8.2 依赖

`wayland-client` 0.31 + `wayland-protocols` 0.32（`client` / `staging` / `unstable` 特性——
cursor-shape-v1 要后两者同开才生成，只多生成绑定代码）。默认即纯 Rust 协议实现，不链
libwayland，编译期不要 `-dev` 包。**不用 smithay-client-toolkit**：它带 calloop 事件循环与
整套抽象，而我们已有自己的 `poll` 循环；与 X11 直接用 x11rb 同理。

- `xkbcommon-dl` 0.4：运行期 `dlopen` libxkbcommon（同 fontconfig 做法）。加载不到时
  `eprintln` 一次、键盘不可用，指针照常。
- `wayland-cursor` 0.31：合成器没有 cursor-shape-v1 时读 XCursor 主题自己挂光标。
- `memfd_create` 走 glibc（2.27+）：更老的 glibc 进程起不来，那些系统本就没有可用的
  Wayland 桌面，不单独做 `syscall` 回退。

源码划分：`wayland/mod.rs`（连接、窗口、事件循环、出帧）、`events.rs`（协议事件分发、
指针 / 键盘翻译）、`shm.rs`、`scale.rs`、`input.rs`（按键重复、滚轮聚合，纯逻辑）、
`xkb.rs`、`cursor.rs`、`data.rs`（剪贴板与文件拖入）。按键翻译（`host::translate_key`）与无边框边缘命中
（`host::edge_direction`）与 X11 共用，快捷键 / 单击 Alt / 空格双发的口径两边一致；唯一差异是
快捷键码取**当前**布局的基础层（X11 取第一布局），两个拉丁布局并存（`de,us`）时 Z / Y 位置
的快捷键跟着当前布局走。

### 8.3 实现要点

- **事件循环**：`dispatch_pending` → 唤醒管道 → 定时器 → 出帧 → `flush` →
  `prepare_read` → 连接 fd 与唤醒管道一起进 `poll` → `read`。`prepare_read` 返回
  `None` 说明队列里已有事件，不睡直接回去分发。
- **呈现**：宿主画进 `Pixmap`，脏区换成 XRGB8888 写进 `wl_shm` 缓冲。缓冲以 memfd 为底、
  **用 `pwrite` 写而不在本进程 mmap**——合成器映射同一个 fd，本进程私有内存里只有
  `Pixmap` 一份（`RssShmem` 为 0）。每窗最多两块缓冲；合成器没 `release` 的那块不写，
  拿不到空闲块时脏区留着等 `release`。每块记着「自己上次写入以来别的帧改过哪里」，
  复用时补写这部分（`ShmSlots`，纯簿记、有单测）。尺寸变了只重建空闲的那块，忙的那块
  等它 `release` 后再按新尺寸重建。
- **帧配速**：有动画的窗口**在本轮真出了帧时**附 `wl_surface.frame`，回调到了、且到了
  控件自报的下次变化时刻才出下一帧。没有像素变化的动画帧也照样请求回调并提交（否则没有
  回调可等、退化成按下限空转）；但没出帧的轮次不请求——否则报 500ms 截止的光标闪烁会在
  每个回调之后立刻再要一个，退化成按刷新率空提交。**不再叠
  `host::FRAME_MS` 的 16ms 下限**——回调本身按显示器节拍来，叠上去会让「回调到了但离
  16ms 还差一点」错过一个垂直同步；只留 4ms 防失控下限。窗口被遮住 / 最小化时合成器
  不发回调，动画自然停下。无动画时不请求回调，阻塞在 `poll`。
- **隐藏 / 再显示**：销毁 `xdg_toplevel` + `xdg_surface`、卸下并销毁缓冲；显示时重建角色
  对象。旧角色在队列里残留的事件按对象身份过滤掉（拿新角色 ack 旧 serial 是协议错误）。
  「挂空缓冲取消映射 → 再做一次无缓冲首提交」按协议也能重新映射，但 **weston 13 对第二
  次首提交不回 configure**，窗口再也出不来；换一套角色对象在各家合成器上都成立。
- **尺寸**：合成器 configure 给了尺寸就服从（最大化实测 1280×800 铺满）；给 0×0 表示
  「客户端自定」，回到最后一次非最大化尺寸（mutter 还原时就是这么给的）。
- **缩放**（`scale.rs`，有单测）：优先级 `WINDUI_SCALE`（可为分数）> `fractional-scale-v1`
  的首选缩放 > `wl_surface.preferred_buffer_scale`（v6）> 表面所在输出 `wl_output.scale` 的
  最大值 > 1。分数值要 viewporter 才能按分数画（缓冲 = `round(逻辑 × s)`，`viewport` 目标 =
  逻辑尺寸，buffer_scale = 1）；没有 viewporter 就近取整走 `set_buffer_scale`。整数值一律走
  buffer_scale。buffer_scale / viewport 目标只在「挂新尺寸缓冲」的那次提交里改，免得旧缓冲
  配上新缩放（尺寸不整除是协议错误）。运行期缩放变化（换显示器、改设置）→ `set_scale` +
  整窗重画；首帧按「唯一一块屏的 scale」预猜，少一次重排。
- **指针**：坐标 = `round(表面坐标 × 缩放)`。滚轮按 `wl_pointer.frame` 聚合：有
  `axis_value120`（v8）/ `axis_discrete` 就只认格数，否则连续量按 10 单位一格换算、零头
  留给下一帧（触控板）。横向滚轮丢弃（框架没有横向滚动事件，同 X11 / win32）。按着按钮
  收到 `leave` = 隐式抓取被合成器收走（开始移动窗口等）→ `on_capture_lost`。
- **键盘**：keymap fd 按文件 `pread` 读出来交给 xkbcommon（v7 起只许 MAP_PRIVATE 映射，
  干脆不映射）；修饰键状态只经 `wl_keyboard.modifiers` 同步。**按键重复客户端自己做**
  （`input::KeyRepeat`，有单测）：速率 / 延迟跟 `repeat_info`（rate=0 不重复），只有
  `xkb_keymap_key_repeats` 为真的键才重复，松开那个键 / 失焦 / 换 keymap 即停，计时并进
  `poll` 超时，循环卡住后恢复只补一下不补一串。
- **光标**：有 `cursor-shape-v1` 就只报形状名；没有（GNOME 42）用 `wayland-cursor` 读
  `XCURSOR_THEME` / `XCURSOR_SIZE`（默认 `default` / 24），尺寸乘缩放向上取整、光标表面
  设 buffer_scale——取不超过缩放、且整除图像宽高的最大整数（主题挑到的档未必是缩放的
  整数倍，不整除是协议错误、连接直接断开）。
- **无边框窗口**：拖动区 / 缩放边（6 逻辑像素）按下后**移出阈值才**发
  `xdg_toplevel.move` / `resize`（带按下的 serial）。Wayland 上理由换了形式：move 一发合成器
  立即接管指针，配对的松开不再送给客户端，单击标题栏也会变成一次移动、双击的第二下凑不齐；
  按下的 serial 在按住期间一直有效，晚发不影响合成器认可。双击拖动区切最大化。拖动区右键
  弹的是框架自己的系统菜单（与另两个平台同一套，用户可拼自己的项），不用
  `show_window_menu`。`wm_capabilities`（v5）决定能否最大化 / 最小化，没收到按全支持。
- **剪贴板**（`data.rs`，对外仍是 `platform::Clipboard`，原生 Wayland 下转给它，否则走 X11
  的剪贴板线程）：复制 = `wl_data_source` 声明 `text/plain;charset=utf-8` / `UTF8_STRING` /
  `text/plain` / `TEXT` / `STRING`（后三个给 XWayland 里的 X 应用），`set_selection` 带最近一次
  输入事件（按键、按钮、键盘焦点进入）的 serial。`send` 给的 fd **非阻塞写**，写不完挂进事件
  循环的 `poll`，5 秒没进展就放弃（关 fd）——读取方慢或卡死都不拖住界面。粘贴 = 对当前选区
  offer `receive` 到 `pipe2` 管道、`flush`、限时 1.5 秒读到 EOF（64MB 上限）；MIME 优先
  UTF-8，大小写不敏感，只有 `STRING` 时按 Latin-1 解。
  **自家的选区绝不走管道**（应答要等事件循环分发，而事件循环正阻塞在读管道上，互等到
  超时）：每个 source 另声明私有 MIME `application/x-windui-source;token=…;id=…`（token 是
  进程启动时的随机值，不用 pid——沙箱里各应用 pid 常相同），选区 offer 带本进程标记、且那个
  source 还活着就直接取本地文本；标记在但 source 没了，是剪贴板管理器照抄的副本，照常读。
  `set_selection` 后发 `wl_display.sync`：合成器按序处理，选区事件先于 `done`；有焦点却到
  `done` 都没见到自家选区 = 判为被拒（wlroots 拒绝时不发 `cancelled`），**只放弃本地认定**
  （读剪贴板回到实际选区），source 留到下一次选区事件：来的是它自己 → 认回来（判错了），
  是别的 → 释放。失焦时发出的那次无从当场判定，照旧以它为准，等重获焦点合成器补发选区时
  再看（补发的仍是自家更早的 / 是别人的 → 那次被拒，释放）。这套记账是纯逻辑（`Owner`），有单测。
  补充两条：
  - 「有焦点、`done` 前没见到自家选区即判被拒」**依赖合成器在处理 `set_selection` 时同步
    回发选区事件**（wlroots、KWin 如此；mutter 走 owner-changed 信号，推断同步、未实测）。
    若某合成器是异步回发，刚被采用的复制会被暂时判为被拒：这段时间本应用粘贴读的是实际
    选区（经管道读别人的，或读空），自家选区事件一到就认回来；别的应用照常读到这次复制。
  - 同一次输入（serial 未变）里连着复制两回：上一个 source 仍是本地认定的当前选区、且它的
    `sync` 还没回来时**直接改写它的文本**，不发第二次 `set_selection`——weston 会把同 serial 的
    后一次当作「不比现有选区新」丢掉，改写后与 X11 / win32 的「后写者胜」一致。`sync` 回来后
    不再改写：改写不产生选区事件，剪贴板历史 / 合成器自带的剪贴板管理器那时多半已读走旧文本；
    所以隔了一阵、中间没有新输入的程序化复制照常发新请求（在 weston 上可能被丢，已知差异）。
  - 被拒、失焦时的复制都不带来选区事件，反复发生（后台 / 托盘应用定时复制）会无上限累积：
    每次 `sync` 回来，已 sync 却未确认的 source 只留**每个 serial 里最旧的一个 + 整体最新的
    一个**，其余当场释放，个数以用过的不同 serial 数为限。依据：同一 serial 的几次复制里被
    合成器采用的只可能是最早的（weston 丢弃同 serial 的后一次；wlroots 都采用，但后者取代前者、
    前者先收到 `cancelled`），所以每组活着的至多最旧的一个是真实选区。不同 serial 之间没有这个
    顺序（旧 serial 被拒后，一次新输入的新 serial 照样可能被采用），所以按 serial 分组——整体
    只留最旧会把夹在中间、真被采用的那次释放掉，清空全局剪贴板。它也可能正是异步回发的
    合成器上被误判的那个，留着才认得回来，之后另一次真被拒不会连带把它释放。
    最新的是失焦时的本地认定、或等认回的最近一次。
  一次性诊断（合成器没有数据设备、首次判为被拒、首次在非界面线程访问）走 `eprintln`，与
  「找不到 libxkbcommon」同一做法——没装 log 后端的应用也看得见；反复出现的只记日志。
  与 X11 的差异：**应用退出后复制的内容随之消失**（协议没有剪贴板管理器，除非桌面自带）；
  以及下面这条已知限制。
- **已知限制：剪贴板仅界面线程可用**（其它线程读写提示一次、当空处理；X11 有独立剪贴板线程，
  任何线程都能用）。有用户反馈再做，方案已定：其它线程把请求放进队列、`sys::wake()` 唤醒事件
  循环、带超时等结果；循环开头处理——写照常 `set_text`，读自家选区直接回本地文本，读别人的
  只做 `start_receive` + flush，**把管道读端交还请求线程**自己限时读（大段读取不卡界面）。
  约百行，集中在 `data.rs` 与 `run_loop` 开头。暂不做的理由是风险：
  (a) 界面线程正等着某个工作线程（`join` 之类），而它在读写剪贴板 → 互等到超时才返回空，
  比现状「立即返回空」是退化；(b) 事件循环已退出、或正在弹阻塞对话框（门户 / zenity）时，
  请求都要等满超时；(c) 现有唤醒管道每次唤醒都给所有窗口置重画，频繁的跨线程访问会带来整窗
  重绘，要避免得让 `WakePipe` 区分唤醒来源；(d) 写用的是界面线程记下的最近 serial，跨线程
  发起的复制可能被合成器拒收（与界面线程上的定时器复制同一情形）。
  **为何不能像 X11 那样另开连接、独立线程**：serial 按客户端（连接）分配、选区事件只发给有键盘
  焦点的客户端——另一条连接既没有有效 serial 可 `set_selection`，也收不到选区事件可读。
- **文件拖入**：`enter` 时 offer 含 `text/uri-list` 且没被模态挡住就 `accept` +
  `set_actions(copy, copy)`（只接受复制——接受移动的话文件管理器会删源文件）；拖动途中模态
  状态变了在 `motion` 里改口。`drop` 时只发 `receive`，管道读端**挂进事件循环的 `poll`**
  异步读（与 XDND 一样不冻界面），读到 EOF → `host::parse_uri_list`（与 XDND 同一份）→ 落点按
  窗口那时的缩放换成物理像素交给 `on_drop_files`（与 X11 同口径）。读到了路径且合成器选定了
  动作才 `finish`（否则是 `invalid_finish` 协议错误），然后销毁 offer；没读到、1.5 秒内没写完、
  读的期间目标窗口已关或被模态子窗挡住、或事件循环已退出，都只销毁，源端收到 `cancelled`。
  交付回调里弹阻塞对话框（门户 / zenity）时，同一轮里其余已读完的拖入等对话框关掉才交付
  （数据不丢；还没读完、期间又过了 1.5 秒的会被放弃）。
- **输入法**（`ime.rs` 收发协议，`text_input.rs` 是纯逻辑状态机、有单测）：`zwp_text_input_v3`
  绑 v1（光标矩形随 `commit` 生效；v2 改为随下一次表面提交生效，GNOME 42 只有 v1），每 seat
  一个。上层与 X11 / macOS 同一套：宿主报 `ime_caret` / `ime_text` / `ime_selection`，合成串经
  `set_ime_preedit` 由 `TextInput` 内联绘制，提交的文字按 `Key::Char` 逐字送进去。
  - **启停**：文本焦点（text-input 自己的 `enter`）在某窗口、宿主报了光标、且没被模态挡住 →
    `enable` + `set_content_type` + `set_cursor_rectangle` + `set_surrounding_text` + `commit`；
    否则 `disable` + `commit`。每次事件收尾对账，**状态真变了才发、才 `commit`**；窗口待重画时
    不对账、交给重画后的收尾（宿主报的光标取自上一帧 paint，先对账会发出「新周围文本 + 旧
    矩形」、重画后再发一次），打一个字一次 `commit`。`enter` / `disable` 之后状态全作废重发，
    `enable` 连同本地还没 `done` 的输入法改动一起作废；合成器还没处理到我们的 `enable` 就发出
    的一批（`done` 的 serial 小于 `enable` 那次 `commit` 的 serial）是给上一个框的，合成串与
    删除丢掉、提交串照收（免得丢字）。`leave` 时清掉本地合成串；合成中焦点被
    移到别的控件（程序改焦点、Tab）时先撤掉合成串——宿主记着合成串挂在哪个节点上，清的是
    原来那个框而不是新焦点。
  - **光标矩形**：宿主给物理像素，除以窗口缩放换成表面逻辑坐标（1.5 倍下与 1 倍同值，已验）。
  - **周围文本**：正文与选区（字符）换成字节；超过协议上限 4000 字节时截取光标附近一段、切在
    字符边界上（以光标为中心，靠近末尾时往前多取、用满上限）。（光标，选区）都没变、且期间
    没有按键时沿用上次的结果，不在指针移动等事件上反复复制正文；按键之后一律重算（Delete 键
    光标不动、正文变了）。程序经 Signal 改写正文而光标不动的，要等下一次按键或光标变化。
  - **`done` 批量应用**：清旧合成串 → 删周围文本（按上次送出的周围文本把字节换成字符；长度
    相对选区之外，有选区时 Left 收到开头删前面、右移过选区删后面：选区里的文字保留，选区
    本身收拢、光标停在其后）→ 提交串逐字 `Key::Char` → 设新合成串（光标字节换字符；两端
    -1 = 隐藏光标，放末尾；两端不同 = 高亮分句）。期间不对账，应用完交给重画后的那次收尾统一对账（那时光标才是新位置，周围文本与
    矩形一次 `commit`）。serial 与我们的 `commit` 次数不符：文本照常应用，状态请求压到对得上
    的 `done` 再发（协议原文）；`enable` / `disable` 不受此限。
  - **合成中**：输入法抓着键盘时合成器不把按键发给我们（Enter 确认、Esc 取消都由它消化），
    它放行的照常走本地，与 X11「输入法不要的才转回来」同效，无需另拦；合成中点击 → 先放弃
    合成（本地清掉 + `disable` / `enable` 让输入法丢掉），同 X11 的 `abort_composition`。
  - **换输入框与内容类型**：宿主经 `AppHandler::ime_field` 报焦点文本控件的身份（焦点
    `NodeId` 的槽位 + 代际）与 `ImeHints`（取自控件真实配置：`Widget::ime_hints`，`TextInput`
    按 `password` / `multiline` 修饰符）。身份变了（同一窗口里 Tab 到另一个框也算）→ `disable`
    + `commit` 再 `enable` + 全部状态 + `commit`，协议要求如此，输入法据此换上下文；同一控件
    内容类型变了只补发 `set_content_type`。密码框：用途 `password`、提示
    `sensitive_data | hidden_text`，**不发周围文本**；多行框：提示 `multiline`。
    后续可接（本次不做）：win32 对密码框 `ImmAssociateContextEx` 关 IME、macOS 的 secure
    input、X11 的 XIM 按焦点控件重建 / 重置 IC——都可以用同一个 `ime_field`。
  - **缺口**：选区方向（光标在头还是尾）宿主不给，一律报光标在尾。
  - 合成器没有 text-input-v3 → 没有输入法，stderr 提示一次。Mutter 从 GNOME 3.34 起实现了
    text-input-v3，GNOME 42 应有（本机未实测，见人工清单）。
- **窗口装饰**（`decor.rs` 协议与交互，`csd.rs` 几何是纯逻辑、有单测）：
  - 协商：有 `zxdg_decoration_manager_v1`（sway、KDE）时有边框窗口请求服务端装饰，合成器回
    `server_side` 就不画；回 `client_side`、或根本没有这个协议（Mutter / GNOME、weston）时，有边框
    窗口自己画标题栏。无边框窗口明确请求 `client_side`（免得被加边框）。协商结果与尺寸一起随
    `xdg_surface.configure` 生效；装饰对象先于 toplevel 销毁，隐藏再显示时重新协商。
  - 标题栏是宿主经 `AppHandler::decoration` 造的一个小宿主（标题居中 + 最小化 / 最大化 / 关闭
    三个 `window_button`，整条 `window_drag`；底 `Surface`、字 `Text`、失活转 `TextMuted`、底边
    `Divider`），与窗口共用主题源，运行期换主题跟随。高 33 逻辑像素。
  - **一张表面**：标题栏与内容画进同一块 `wl_shm` 缓冲，上 `bar` 行是标题栏，内容整体下移
    （`write_pixels` 带下移量）。平台层把内容区坐标统一减掉标题栏：应用看到的尺寸、指针、输入法
    光标矩形、拖入落点都按内容区计，感知不到标题栏；`configure` 给的尺寸与告诉合成器的 min / max
    约束都含标题栏（有标题栏时最小高度至少留出标题栏 + 1 行）。没用子表面（libdecor 的做法）：
    它能把阴影与不可见缩放边放到窗口外，但要多管一组表面；我们不画阴影，用不上。
  - **分数缩放取整**：标题栏物理高 =「整窗高换算 − 内容高换算」，而不是单独取整——两次独立
    取整之和会比整窗多 1 像素，缓冲与 viewport 目标差一行、整窗被重采样发糊。
  - **交互**：窗口按钮的操作转给窗口（关闭走窗口自己的关闭决策链，可被拒绝）；空白处按下移出
    阈值才 `move`（沿用 `DragGate`）、双击切最大化、右键 `show_window_menu`（合成器的窗口菜单是
    CSD 惯例；框架自己的系统菜单是窗口里的浮层，塞不进 33 像素高的标题栏——与无边框窗口用框架
    菜单的惯例在此分开）；缩放边在窗口内侧 6 像素一圈（含标题栏顶边），落在可交互控件上时让给
    控件、最大化 / 平铺时没有（GNOME 原生应用的缩放边在窗口外的透明阴影区，我们没有阴影区）；
    指针在标题栏上时光标归标题栏宿主；滚轮在标题栏上不下发；拖入落在标题栏上不收。
  - **重画**：标题栏单独一条通路——它的悬停 / 按下 / 淡入淡出只重画那 33 行（实测悬停动画的
    `damage_buffer` 全落在标题栏行内），不逼内容整窗重画；内容整窗重画时（换主题、改尺寸）
    标题栏一并重画；标题与激活态变化用不发通知的信号写入，只重画标题栏。
  - **模态**：子窗 `set_parent`；有 `xdg-dialog-v1`（KDE 6.1+ 等）时模态子窗另 `set_modal`，
    父窗隐藏期间显示的模态子窗在父窗再显示时补登记；没有该协议（GNOME 42、sway）时只有层级
    关系，父窗的输入（含标题栏）由我们自己挡。
  - **偏差**：标题栏方角、无阴影；没有边缘的斜向缩放光标（无边框窗口同样没有）；按钮不跟
    `wm_capabilities` 隐藏；最大化后按钮图标不变。
- **诊断开关**：`WINDUI_WAYLAND_DISABLE=viewporter,fractional-scale,cursor-shape,text-input,xdg-decoration,xdg-dialog`
  假装合成器没有这些协议，在新合成器上走一遍 GNOME 42 的回退路径（`xdg-decoration` 关掉即在
  sway 上走客户端标题栏）。
- **已知缺口**：启动后才出现的 `wl_seat`（启动时一个输入设备都没有）不会绑定，剪贴板与拖入
  也随之不可用（数据设备按 seat 建，只建启动时那一个）；文件拖出、primary selection（中键
  粘贴）未做；
  `wl_output.scale` 收到即生效，没等 `done`；阻塞对话框期间不回 ping；被遮挡的动画窗口
  每秒醒一次（下一条），没做退避。
- **frame 回调兜底**：等了 1 秒还没回（合成器扣住被遮挡窗口的回调、或对空提交不回）就当
  丢了，按普通定时继续出帧——代价是被遮挡的动画窗口每秒醒一次。回调按对象身份认，旧的
  晚到不作数。

### 8.4 协议做不到、只能文档化的

应用不能设窗口坐标（`centered` 无效，由合成器摆放）；不能查询是否被最小化
（`WindowState::minimized` 恒 false，`hide_on_minimize` 无从触发）；唤起已显示的窗口要
`xdg-activation-v1`（未实现）。全局热键见 `IMPLEMENTATION_PLAN.md` 末节：不实现，启动时
stderr 提示一次、`App::hotkey` 成为空操作；兜底是桌面设置里把快捷键绑到 `应用 --toggle` 这样
的命令，应用开 `App::single_instance`，第二次启动的 argv 转给运行中的实例（见 API_GUIDE §5）。

### 8.5 实测数据（weston 13 / sway 1.9 headless + pixman，release）

私有内存（`RssAnon`）与 X11 后端同示例、**同窗口尺寸**对比：

| 示例 / 缩放 | X11 | Wayland |
|-------------|-----|---------|
| `about` 620×556 @1x | 2668 KB | 2656 KB |
| `about` 1240×1112 @2x | 6840 KB | 6824 KB |
| `fullshowcase` 760×700 @1x | 7352 KB | 7344 KB |
| `fullshowcase` 1520×1400 @2x | 13588 KB | 13584 KB |

⚠ 2x 对比要用 2560×1600 的 Xvfb：1280×800 的屏上 openbox 会把窗口钳到屏幕大小，
X11 那边的 Pixmap 随之变小，看起来像 Wayland 多占了 1.5–4MB。

空闲 CPU：`about` 静止 10 秒 `utime+stime` 增量 0 tick（阻塞在 `poll`）；自动聚焦的输入框
（光标约 530ms 翻转一次、12×24 的局部脏区）10 秒 0 tick，6 秒内只提交 13 次。
不定进度条动画：约 40fps（headless weston 的 repaint 节拍），4 秒 2 tick，240 帧只建了
2 块缓冲。离屏 `--screenshot` 与 weston 抓屏裁出的窗口逐像素一致（620×556 全等）；
不定进度条跑过上百帧局部重绘后，进度条以外的像素仍与首帧全等、条内只有一段高亮
（双缓冲补写欠账正确）。

Stage 2（sway headless + 自写注入器，见 §8.7）：
- 键盘：点进输入框、End、`Ab1` 正确输入；Ctrl+A 后按住 `x` 1000ms（repeat_info 25 次/秒、
  延迟 600ms）得到恰好 11 个 x（1 + 400ms / 40ms），松开即停。
- 滚轮：3 格 → `axis_value120` 360 → 一次 `Wheel(-360)`，内容下移 144 像素。
- 光标：悬停输入框 `set_shape(text)`；关掉 cursor-shape 后走主题（2x 下加载 48px、光标表面
  buffer_scale 2、热点折半）。
- 缩放：运行期把输出改成 1.5 → 收到 `preferred_scale 180` → 建 viewport、缓冲 1140×1050、
  目标 760×700，文字锐利；1.5 下点击标签页命中正确；改 2 → buffer_scale 2、清 viewport 目标；
  改回 1 正常。关掉 viewporter / fractional-scale 后按 `wl_output.scale` / `preferred_buffer_scale`
  走整数路径。
- 无边框（`about`）：拖标题栏窗口移动、拖右下角尺寸变大、双击标题栏发出 `set_maximized`
  （sway 的 xdg_wm_base 只有 v2，没有 wm_capabilities，浮动窗口不理最大化——合成器行为）、
  右键标题栏弹框架系统菜单、点菜单「关闭」进程退出。
- 私有内存 `about` 620×556@1x：weston（无输入设备）2664 KB；sway 无输入设备 3000 KB、
  出现键鼠后 3348 KB——多出的约 350 KB 是 libxkbcommon 编译 keymap 的常驻结构。
  空闲 10 秒 0 tick（weston / sway / GNOME 42 三处）。

Stage 3（sway headless；外部一侧用 `wl-clipboard` 2.2.1 的 `wl-copy` / `wl-paste`——sway 有
`wlr-data-control`，它们不需要焦点；拖放源是自写的最小客户端，见 §8.7）：
- 复制：本应用复制中文 → `wl-paste` 原样读到，`-l` 列出 5 种文本 MIME + 私有标记；190 KB
  中文 + emoji 文本 `UTF8_STRING` / `text/plain` / `STRING` 三种读法都完整（FNV 校验一致）。
- 粘贴：`wl-copy` 206 KB 中文 → 本应用读到全文、校验一致，耗时 2ms；只有 `text/plain` 时回退
  读到；只有 `image/png` 时读空、不挂起。输入框里 Ctrl+V 粘外部中文、Ctrl+A Ctrl+C 再被
  `wl-paste` 读回，键盘通路的 serial 被合成器采用。
- 慢读取方：1.3 MB 选区，一个读取方晚 3 秒才读（收到全部 1308890 字节）、一个卡 8 秒（5 秒后
  放弃，收到 196608 字节）；期间点粘贴 0ms 返回（自家选区不走管道），界面照常响应；结束后
  fd 数回到基线。
- 拖入：含中文、空格、`file://localhost/`、注释行、CRLF、`https://` 的 uri-list → 两条本地路径
  正确解码、网址被跳过，源端依次收到 `target text/uri-list`、`action copy`、`finished`；只有
  `text/plain` 的拖动 → `target None`、源端 `cancelled`；uri-list 里只有网址 → 不 `finish`、
  源端 `cancelled`。1.5 分数缩放下，落在左右两区分界线两侧（逻辑 x=270 / 330，分界 ~300）的
  拖入分别命中左 / 右区——若漏乘或重复乘缩放，330 会落到左区、270 会落到右区。
- 一次点击里连着 `clipboard_set` 两回：协议上只发了 1 次 `set_selection`（`WAYLAND_DEBUG`
  计数），`wl-paste` 与本应用粘贴都读到后一次。
- 拖入异步读：源端放下后晚 1 秒才写 uri-list，放下后 200ms 注入的点击先被处理（探针先打出
  复制按钮的结果），数据到了再交付路径、源端收到 `finished`；晚 3 秒 → 1.5 秒时放弃，源端
  `cancelled`，没有交付。
- 未能自动化：「写入被合成器拒绝」的真实路径（sway 上键盘焦点进入的 serial 就够用，构造不出
  被拒），只有状态机单测；拖动途中模态状态改变。
- `about` 空闲 10 秒仍 0 tick；私有内存 3004 KB（无输入设备，§8.5 同条件 3000 KB）。

Stage 4（sway headless；输入法端是自写的最小 `zwp_input_method_v2` 客户端，从 FIFO 读
`preedit` / `commit` / `delete` / `send` 命令，把收到的 activate / surrounding_text / done 打出来）：
- 点进输入框 → 输入法收到 activate、`surrounding_text("" 0 0)`、content_type normal；打 `ab`
  后周围文本 `"ab" 2 2`；点按钮（非文本）→ deactivate。协议请求（`WAYLAND_DEBUG`）：没变化时
  不发，光标矩形 `(20,63,1,17)` 随打字右移。
- `preedit zhong 5 5` → 输入框里内联显示下划线的 `zhong`，光标在其后；`commit 中文` → 输入框
  变成 `ab中文`（Ctrl+A Ctrl+C 经 `wl-paste` 读回），周围文本 `"ab中文" 8 8`（字节）；
  `delete 3 0` → `ab中`。
- 合成中把焦点切到另一窗口 → text-input `leave`、本地合成串清掉；切回 → 重新 `enable`，新合成串
  正常显示；合成中点击输入框开头 → 合成串放弃、`disable` + `enable`、光标落到点击处。
- 同一窗口两个输入框间 Tab：输入法端依次收到 deactivate、activate（周围文本、内容类型
  重发）；Tab 到密码框：content_type 为 `HiddenText | SensitiveData` + `Password`，没有周围文本。
- 选中 `xyz` 里的 `y` 后输入法 `delete 1 1` → 剩 `y`（选区之外各删一个，选区里的文字保留）。
- 1.5 倍缩放：光标矩形与 1 倍完全相同（逻辑坐标），`preferred_scale 180` 生效。
- `WINDUI_WAYLAND_DISABLE=text-input`：stderr 一行提示，其余照常。`about` 空闲 10 秒 0 tick，
  私有内存 3012 KB。关窗时文本焦点在它上面就本地当作 `leave`；开 / 关窗之后给文本焦点窗口
  补一次对账（模态子窗开关会改变它能否输入）。
- 未能自动化：真实输入法（ibus / fcitx5）的候选窗位置与拼音流程；sway 1.9 不给输入法的弹出
  表面发 `text_input_rectangle`，矩形只能从请求参数核对。

Stage 5（sway headless，`WINDUI_WAYLAND_DISABLE=xdg-decoration` 走客户端标题栏；weston 13 没有
装饰协议，天然走客户端标题栏）：
- sway 默认：协商得 `server_side`，窗口 600×400、不画标题栏；关掉协议：窗口 600×433，上 33 行
  是标题栏（标题居中、三个按钮）。weston 抓屏同样有标题栏。
- 内容区：点「copy-small」（标题栏下方）正确触发；输入法光标矩形 y = 63 + 33 = 96（无标题栏时
  63）；拖入左右两区命中正确，拖到标题栏上源端得 `target None`、`cancelled`。
- 标题栏：拖动空白处窗口移动（过阈值后交给合成器）；拖右下角内侧缩放；双击发 `set_maximized`
  （sway 浮动窗口不理会最大化，合成器行为）；右键发 `show_window_menu`；最小化键发
  `set_minimized`；关闭键进程退出；悬停关闭键红底（截图）、在三个按钮间移动悬停跟着走、移开
  后清掉。
- 1.5 倍：缓冲 900×650（逻辑 600×433）；缩放到 629×424 后缓冲 944×636 = 整窗逻辑高换算，
  viewport 目标 629×424，一像素不差；标题文字锐利；标题栏下的按钮点击命中正确。
- 失活：聚焦别的窗口后标题区墨量 94621 → 70408、最深像素 45 → 99（转淡）；切回恢复。运行期
  切暗色主题，标题栏随之变暗（截图）。
- `file_drop` 空闲 10 秒 0 tick（服务端装饰 / 客户端标题栏两种都是）；私有内存 1884 KB →
  2024 KB（客户端标题栏多一个小宿主 + 一条 480×33 的像素图）。
- 未能自动化：GNOME 下的真实外观与交互、合成器真实的最大化 / 平铺、`xdg-dialog-v1`（sway 与
  weston 都没有）。

### 8.6 无桌面验证环境（weston headless，无 root）

```bash
mkdir -p ~/.local/weston && cd ~/.local/weston
apt-get download weston libweston-13-0 libseat1 libmtdev1t64 libwacom9 libwacom-common \
                 libgudev-1.0-0 libevdev2
# libinput10：索引里的版本可能已从镜像下架（404），到 pool 目录直接取现存的那版
curl -sO http://archive.ubuntu.com/ubuntu/pool/main/libi/libinput/libinput10_1.25.0-1ubuntu3.7_amd64.deb
for f in *.deb; do dpkg -x "$f" root; done
```

踩过的坑：

- **模块路径是编译期写死的**（`/usr/lib/x86_64-linux-gnu/libweston-13/…`）。用环境变量
  `WESTON_MODULE_MAP="headless-backend.so=<路径>;desktop-shell.so=<路径>;weston-desktop-shell=<路径>;…"`
  逐个重定向；`LD_LIBRARY_PATH` 还要包含 `…/x86_64-linux-gnu/weston`（`libexec_weston.so.0` 在那）。
- **默认渲染器是 no-op，抓不了屏**：`--renderer=pixman`。抓屏协议要 `--debug` 才开放，
  之后 `weston-screenshooter` 把整屏存成 PNG（到当前目录）。
- **300 秒无输入后输出休眠、不再重绘**，`weston-screenshooter` 从此一直挂着等——症状像
  客户端把合成器弄坏了，其实是 idle。配置里 `[core] idle-time=0`。
- 配置里 `[input-method] path=` 置空，否则反复拉起不存在的 `weston-keyboard` 刷屏。
- `XDG_RUNTIME_DIR` 沿用会话已有的即可（socket 建在那里）；没有就自建一个 0700 目录。

```ini
# ~/.local/weston/weston.ini
[core]
shell=desktop-shell.so
idle-time=0
[shell]
background-color=0xff303030
panel-position=none
[input-method]
path=
```

```bash
R=~/.local/weston/root/usr/lib/x86_64-linux-gnu
LD_LIBRARY_PATH=$R:$R/weston WESTON_MODULE_MAP="headless-backend.so=$R/libweston-13/headless-backend.so;desktop-shell.so=$R/weston/desktop-shell.so;weston-desktop-shell=$HOME/.local/weston/root/usr/libexec/weston-desktop-shell" \
  ~/.local/weston/root/usr/bin/weston --config=$HOME/.local/weston/weston.ini \
  --backend=headless --renderer=pixman --socket=wl-test --width=1280 --height=800 --debug &
# Wayland 后端须显式启用（见 §8.1），漏了这个变量跑的是 X11（没有 DISPLAY 时直接连不上）
WINDUI_BACKEND=wayland WAYLAND_DISPLAY=wl-test cargo run --release --example about
WAYLAND_DISPLAY=wl-test LD_LIBRARY_PATH=$R ~/.local/weston/root/usr/bin/weston-screenshooter
```

`WAYLAND_DEBUG=1`（同样要配 `WINDUI_BACKEND=wayland`）对纯 Rust 后端同样生效（打印每条收发的协议消息），排查时很有用；
**测 CPU 时别开**——打印本身让动画帧的 CPU 翻了十倍。headless 没有输入设备，关窗 /
最大化 / 隐藏这类路径用一个按 `on_interval` 脚本化调用 `ctx.*` 的临时示例驱动。

### 8.7 带输入注入的无桌面验证（sway headless，无 root）

weston headless 没有输入设备，Stage 2 起改用 sway：它有 virtual-pointer / virtual-keyboard、
screencopy（`grim` 抓屏）、fractional-scale-v1、cursor-shape-v1，运行期还能 `swaymsg` 改输出
缩放——正向路径一处全覆盖（回退路径用 `WINDUI_WAYLAND_DISABLE` 或 GNOME 42 验）。

```bash
mkdir -p ~/.local/sway && cd ~/.local/sway
apt-get download sway libwlroots12t64 grim libjson-c5 libseat1 libxcb-icccm4 libliftoff0 \
  libdisplay-info1 libxcb-res0 libxcb-render-util0 libxcb-xinput0 libxcb-composite0 libxcb-ewmh2
for f in *.deb; do dpkg -x "$f" root; done
# 运行：headless 后端 + pixman，窗口一律浮动（否则被平铺拉满、尺寸不可控）
cat > config <<'CFG'
output HEADLESS-1 resolution 1280x800 position 0 0 scale 1
for_window [app_id=".*"] floating enable
default_border none
default_floating_border none
CFG
LD_LIBRARY_PATH=$PWD/root/usr/lib/x86_64-linux-gnu:$HOME/.local/weston/root/usr/lib/x86_64-linux-gnu \
  WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman \
  env -u WAYLAND_DISPLAY -u DISPLAY root/usr/bin/sway -c config &
# socket 名由 sway 自己挑（wayland-1 等），看 $XDG_RUNTIME_DIR
```

踩过的坑：

- **`wlrctl` / `wtype` 每次调用新建、退出即销毁虚拟设备**，seat 的 capabilities 随之来回跳，
  客户端的 `wl_pointer` / `wl_keyboard` 被反复建拆，点击时机对不上。改为自写一个常驻注入器
  （几十行：wayland-client + wayland-protocols-wlr 的 virtual-pointer 绝对移动 / 按钮 / 滚轮，
  wayland-protocols-misc 的 virtual-keyboard + 用 xkbcommon 按默认 RMLVO 生成 keymap 上传），
  从 stdin 读 `move x y` / `click left` / `key x down` / `sleep ms` 之类的脚本。当时放在会话
  草稿目录，需要时按此重写。
- 剪贴板外部一侧：`apt-get download wl-clipboard` + `dpkg -x` 解到 `~/.local/wlclip`，
  `wl-copy` / `wl-paste` 走 `wlr-data-control`，不需要窗口也不需要焦点（weston headless 没有
  seat，用不了）。拖放源：自写的最小客户端（开一个小窗口，收到指针按下就 `start_drag`，
  MIME 与数据从参数 / 环境变量取，把 source 的各个事件打印出来），注入器在它上面按下、
  移到目标窗口、松开。注入器每次运行都从 0 计时，相隔很近的两次运行各点一下会被判成双击。
- virtual-pointer 的绝对坐标按 `extent` 映射到输出**逻辑**坐标；传物理像素 + 物理 extent
  在任何缩放下都落在同一个像素上。
- sway 1.9 的 `xdg_wm_base` 只有 v2：没有 `wm_capabilities`、浮动窗口忽略最大化请求。
  最大化的正向结果要在 weston（无输入，靠脚本化 `ctx.toggle_maximize` 验）或真桌面看。
- `swaymsg output HEADLESS-1 scale 1.5` 即可验运行期换 DPI；`grim` 抓的是物理像素整屏。

**GNOME 42（192.168.5.55）上抓屏**：`org.gnome.Shell.Screenshot` 对非白名单调用方返回
AccessDenied；门户 `Screenshot`（`interactive: false`）能出图，但**每次都会在对方桌面弹一个
「Share this screenshot」确认框**且不会自己消失——远程无人值守时别用，或事后
`systemctl --user restart xdg-desktop-portal-gnome` 收掉。屏保熄屏时抓到的是全黑：先
`org.gnome.ScreenSaver.SetActive false`。

