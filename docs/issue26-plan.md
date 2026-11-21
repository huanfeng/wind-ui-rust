# Issue #26 后续规划：图片格式 / 文件拖出 / Linux GPU

> 来源：issue #26（2026-11-06，一位用户想用 windui 做 Windows + Linux 的文件统计分类工具）。
> 本文是调研结论与分期建议，**尚未开工**。各项排期以 `docs/ROADMAP.md` 为准。

## 1. 常用图片格式（可选 feature）

### 现状

- 内置 PNG（tiny-skia 原生解码）和 SVG（`svg` feature，默认开）。
- 扩展点已经有了：`ImageDecoder` trait（`probe` + `decode` → 非预乘 RGBA8）加 thread-local
  注册表，`from_bytes` / `from_file` / `Element::image*` 都按魔数自动分发（`src/render/image.rs`）。
  因此加格式**不改任何公共 API**，只是在默认注册表里按 cfg 多放几个解码器。

### 方案

每种格式一个 feature，默认全关；另设总开关 `image-formats` 一次全开。

| feature | 实现 | 新依赖 | 说明 |
|---|---|---|---|
| `jpeg` | `zune-jpeg` | 1 个，纯 Rust | 须自行读 EXIF Orientation 并旋转（解码库不处理，手机照片会横躺）。约 60 行，自写 |
| `gif` | `gif` | 1 个，纯 Rust | 只取首帧作静态图；动图另立项（要接动画帧时钟） |
| `webp` | `image-webp` | 1 个，纯 Rust | 有损 + 无损；动图同样只取首帧 |
| `bmp` | 自写 | 无 | BI_RGB 1/4/8/24/32 位 + BI_BITFIELDS；RLE 压缩不支持 |
| `ico` | 自写 | 无 | 容器格式，条目是 PNG 或 DIB（BMP 去文件头 + AND 掩码）。复用 PNG 与 `bmp` 的解码；按目标尺寸挑最合适的条目，配一个 `Image::from_ico_bytes(bytes, target)` |

- 不做 AVIF / HEIC / TIFF：依赖重（多数要 C 库），与小体积目标相悖。需要的用户仍可走 `register_decoder`。
- 不引入 `image` crate：它一次带进很多格式与依赖，粒度太粗。
- 验收：每个 feature 单独编译、单独测（魔数探测 + 解码一张最小样例图 + 尺寸 / 像素断言）；
  测出各 feature 对 release 二进制的体积增量，写进 API_GUIDE。

### 顺带：文件图标

issue 里要的「取 exe 图标」其实是**取文件图标**，这对文件管理类应用是刚需（按扩展名显示类型图标）。
建议加平台函数 `platform::file_icon(path, size) -> Option<Image>`：

- Windows：`IShellItemImageFactory::GetImage`（覆盖 exe 内嵌图标、文件类型图标、缩略图），
  HBITMAP → `GetDIBits` → `Image::from_rgba`。
- macOS：`NSWorkspace::iconForFile` → 按尺寸取 `NSBitmapImageRep` → RGBA。
- Linux：要先按 MIME 类型找到 XDG 图标主题里的图标名，再在主题目录里查找，图标主题规范实现量不小。
  第一版返回 `None`，另立项。

## 2. 文件拖出（macOS / Linux）

### 现状与契约

`platform::drag_files(paths, allow_move) -> DragEffect` 与
`drag_files_with(.., before_drop)`，Windows 已实现（OLE `DoDragDrop` + `CF_HDROP`），
macOS / Linux 是返回 `DragEffect::None` 的桩。契约要点（三个平台须一致）：

- **阻塞**：调用一直等到拖放结束，返回接收方执行的效果。调用方经 `ctx.defer_blocking` 调用，
  此时已不在事件回调栈里。
- **`before_drop`**：用户在接收方上松手、数据交出**之前**调一次，用来延迟生成文件；返回 false
  则取消。
- 接收方自己完成复制 / 移动，本方只提供路径。

### macOS

- `NSView::beginDraggingSessionWithItems:event:source:`。拖动条目用 `NSPasteboardItem`，类型为
  `public.file-url`；`before_drop` 对应 `NSPasteboardItemDataProvider` 的延迟提供回调，
  接收方真正读取数据时才触发。
- 实现 `NSDraggingSource`：`sourceOperationMaskForDraggingContext` 返回 copy（加 move），
  `draggingSession:endedAtPoint:operation:` 拿到最终效果。
- 需要触发拖动的鼠标事件：延迟闭包运行时 `NSApp.currentEvent` 仍是那次 `mouseDragged`，
  可以直接用（需验证）。
- **阻塞的实现**：AppKit 的拖动会话是异步的，调用后立即返回。要满足阻塞契约，需要在会话结束前
  自己跑一个嵌套的事件循环（`nextEventMatchingMask` + `sendEvent`）。性质上与 Win32
  `DoDragDrop` 自带消息泵相同，期间其它窗口照常重绘。
- 拖动预览图：第一版用系统的文件图标（`NSWorkspace::iconForFile`）。
- 风险：macOS 验证机没有辅助功能授权，合成不了真实拖动，只能请用户人工验证。

### Linux X11（XDND 源端）

目标端已有（`src/platform/linux/dnd.rs`，XDND v5）。源端要补：

1. 拥有 `XdndSelection`，抓住指针与键盘（Esc 取消）。
2. 指针移动时从根窗口逐层向下找第一个声明了 `XdndAware` 的窗口（处理 `XdndProxy`，跳过窗口
   管理器的外框），协商版本；进出目标时发 `XdndEnter` / `XdndLeave`，移动时发 `XdndPosition`，
   并读 `XdndStatus` 判断对方是否接受、更新光标。
3. 松手时，若对方接受就发 `XdndDrop`，否则发 `XdndLeave` 并取消。
4. 处理对 `text/uri-list` 的 `SelectionRequest`：首次请求时先调 `before_drop`，再写入属性。
5. 等 `XdndFinished`（v5 带最终动作），设超时兜底。拖到自己的窗口上时直接走本地放下路径。

**结构难点**：X11 后端执行延迟闭包时，整个 `X11` 结构正被可变借用（`after_event` 里调用
`req.run()`），`drag_files` 无法回调后端。方案：用一份 `Rc<RustConnection>` 放进 thread-local，
供拖动时取用；`drag_files` 自己跑嵌套的事件循环，只处理拖放相关的事件，其余事件存进待处理队列，
主循环先取完这个队列再从连接读新事件。代价是拖动期间本程序窗口不重绘（内容是静态的，可以接受）。

### Linux Wayland

- `wl_data_device.start_drag(source, origin, icon, serial)`：`serial` 必须是这次拖动所在的
  隐式抓取对应的那次按下事件的序列号，`origin` 是按下时所在的 surface。数据源提供
  `text/uri-list`，用 `set_actions` 声明 copy / move。
- 事件：`send(mime, fd)` 时写入 uri-list，首次触发前调 `before_drop`；`dnd_drop_performed` /
  `dnd_finished` / `cancelled`；`action` 事件给出最终效果。
- **阻塞的实现比 X11 干净**：把数据源建在**专用的 `EventQueue`** 上，嵌套循环只分发这个队列；
  主队列的事件照常留在队列里，事后由主循环处理，不需要自己维护待处理队列。所需的
  `wl_data_device`、serial、surface 由 Wayland 后端在每次按下时存进 thread-local（代理对象可克隆）。
- 拖动预览图（icon surface）第一版不画。

### 建议顺序

macOS → X11 → Wayland。默认后端是 X11，所以 X11 比 Wayland 优先；macOS 的实现量最小，先做。

## 3. Linux GPU 后端

### 现状

- 渲染器已经跨平台：`src/render/gpu/` 的图元管线、离屏渲染、设备丢失后的重建 / 降级都与平台
  无关，`cargo test --features gpu` 在 Linux 上就会跑 GPU 离屏测试。
- 窗口层没接：X11 / Wayland 后端遇到 `Renderer::Gpu` 都会打印提示并回退软件渲染。
- `WindowGpu`（`surface.rs`）按设计只接收平台层建好的 `wgpu::Surface`，本身零平台代码。

### 两个卡点

**1. 建 surface 需要 C 库的连接指针。** wgpu-hal 的 Vulkan 后端：
- XCB 需要 `xcb_connection_t*`（为空直接 `expect` panic），Xlib 需要 `Display*`；
- Wayland 需要 `wl_display*` 和 `wl_surface*`。

而我们为了不链接 C 库，X11 走 x11rb 的纯 Rust 实现 `RustConnection`，Wayland 走
wayland-client 的纯 Rust 后端，**两者都拿不出这些指针**。解法：

- X11：`gpu` feature 开启时，给 x11rb 打开 `dl-libxcb`，在运行期 dlopen libxcb，**另开一条**
  `XCBConnection`，专门用来建 surface。窗口 XID 在整个 X 服务器内全局有效，用别的连接建的窗口
  也能给它建 surface；主连接保持不动。
- Wayland：Wayland 对象只在建它的连接内有效，不能另开连接。`gpu` feature 开启时，要把
  wayland-client 整体切到 `client_system` + `dlopen` 后端（运行期加载 libwayland-client；
  Wayland 桌面上它总是存在），再用 `display_ptr()` / `ObjectId::as_ptr()` 取指针。
  这会改变整个 Wayland 后端的底层，回归面比 X11 大。
- 两条路都是运行期 dlopen，**编译期仍不需要任何 -dev 包**，符合 `LINUX_PORTING.md` 的取向。

**2. GPU 画文字需要 `GlyphSource`。** GPU 后端的文字像素由平台文字栈生成，只做上传与混合。
目前只有 macOS 的 Core Text 实现了它。Linux 自带的文字栈（ttf-parser + ab_glyph_rasterizer）
要补上 `raster_run`（整段光栅）；想用字形图集（glyph atlas），还要补 `shape_run` /
`raster_glyph`。这部分代码量不小，但纯计算、好测。

### 其它考量

- 仍是 opt-in：要开 `gpu` feature，运行期默认仍是 `Software`，与 Windows / macOS 一致。
- 内存：Vulkan 驱动本身会带来几十 MB 的常驻内存，与本项目「极低内存」的定位有冲突。值得做的
  前提是有真实场景能证明软件渲染在 Linux 上不够用（大窗口、高 DPI、频繁动画）。
- **验证环境**：现有 Linux 测试机都是虚拟机，没有直通显卡（一台是 virtio-gpu，一台是 bochs-drm），
  最多跑 virgl 或 lavapipe 软件 Vulkan：验证得了正确性，量不了性能。需要一台有真实显卡的 Linux 机器。

### 建议

先做一次**技术验证**：只做 X11 + Vulkan，`raster_run` 级别的 GlyphSource，在一台有真实显卡的机器上
对比连续动画时的 CPU 占用与内存，用数据决定要不要做完整版（含 Wayland 和字形图集）。

## 4. 建议总排期

| 顺序 | 项 | 规模 | 依赖 |
|---|---|---|---|
| 1 | 图片格式 feature（jpeg / gif / webp / bmp / ico） | 小 | 无 |
| 2 | `file_icon`（Windows + macOS） | 小 | 无（走系统 API 取位图） |
| 3 | 文件拖出 macOS | 中 | 需用户人工验证 |
| 4 | 文件拖出 X11 | 中偏大 | Linux 测试机（X11）可测 |
| 5 | 文件拖出 Wayland | 中 | Linux 测试机（GNOME Wayland）可测 |
| 6 | Linux GPU 技术验证 | 中 | 需要有真实显卡的 Linux 机器 |
