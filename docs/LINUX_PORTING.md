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

计划分五阶段（仓库根 `IMPLEMENTATION_PLAN.md`）。**当前完成 Stage 1**：窗口与呈现。

> ⚠ 本阶段没有任何输入（指针 / 键盘）、没有标题栏（weston / KDE / sway 给服务端装饰，
> GNOME 不给）、没有剪贴板与输入法。因此**默认不启用**：只有 `WINDUI_BACKEND=wayland`
> 才走它，其余情况 Wayland 会话照旧经 XWayland 运行。

### 8.1 后端选择

编译期：`wayland` feature（默认开；依赖只声明在 Linux 的 target 段，别的平台开着也
不多编东西）。`--no-default-features` 得到纯 X11 后端。

运行期（`platform/linux/mod.rs` 的 `choose_backend`，有单测）：

| 条件 | 结果 |
|------|------|
| `WINDUI_BACKEND=wayland` | 试 Wayland；连不上（含缺 `xdg_wm_base` / `wl_shm` / v4+ `wl_compositor`）`eprintln` 提示后**回退 X11**——显式指定也不该让程序起不来 |
| `WINDUI_BACKEND=x11`、未设、或值认不出（记警告） | X11（Wayland 会话经 XWayland） |
| 点名 wayland 但编译时关了 feature | 提示后走 X11 |

**何时改为自动优先**：Stage 2–5 全部完成、并在 GNOME 真桌面验证过之后，把未设时的分支改成
「有 `WAYLAND_DISPLAY` / `WAYLAND_SOCKET` 就优先 Wayland、连不上回退 X11」。在那之前自动选上
一个没有输入的后端，会让 Wayland 桌面上的现有应用点不动。

### 8.2 依赖

`wayland-client` 0.31 + `wayland-protocols` 0.32（`client` 特性）。默认即纯 Rust 协议实现，
不链 libwayland，编译期不要 `-dev` 包。**不用 smithay-client-toolkit**：它带 calloop
事件循环与整套抽象，而我们已有自己的 `poll` 循环；与 X11 直接用 x11rb 同理。

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
- **缩放**：本阶段只认 `WINDUI_SCALE` 的整数部分（`set_buffer_scale`）；分数缩放与运行期
  跟随在 Stage 2。

### 8.4 协议做不到、只能文档化的

应用不能设窗口坐标（`centered` 无效，由合成器摆放）；不能查询是否被最小化
（`WindowState::minimized` 恒 false，`hide_on_minimize` 无从触发）；唤起已显示的窗口要
`xdg-activation-v1`（Stage 5）。全局热键见 `IMPLEMENTATION_PLAN.md` 末节：不实现，
兜底是桌面设置里把快捷键绑到 `应用 --参数`，经单实例转发送达。

### 8.5 实测数据（weston 13 headless + pixman，release）

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
WAYLAND_DISPLAY=wl-test cargo run --release --example about
WAYLAND_DISPLAY=wl-test LD_LIBRARY_PATH=$R ~/.local/weston/root/usr/bin/weston-screenshooter
```

`WAYLAND_DEBUG=1` 对纯 Rust 后端同样生效（打印每条收发的协议消息），排查时很有用；
**测 CPU 时别开**——打印本身让动画帧的 CPU 翻了十倍。headless 没有输入设备，关窗 /
最大化 / 隐藏这类路径用一个按 `on_interval` 脚本化调用 `ctx.*` 的临时示例驱动。
