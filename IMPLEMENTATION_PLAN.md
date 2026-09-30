# Wayland 原生后端 实施计划

> 背景：#12。现 Linux 后端只有 X11，Wayland 会话经 XWayland 运行（HiDPI 发糊、
> 全局热键只在自家窗口有焦点时生效）。本计划新增原生 Wayland 平台层，X11 后端保留。
> 全部阶段完成后删除本文件，结论沉淀进 `docs/LINUX_PORTING.md`。

## 总体取舍

- **依赖（沿用 X11 那套「小依赖、编译期不要 -dev 包」取向）**
  - `wayland-client` 0.31 + `wayland-protocols` 0.32：默认走纯 Rust 协议实现，不链 libwayland。
  - `xkbcommon-dl` 0.4：运行期 `dlopen` libxkbcommon（任何 Wayland 桌面必有），同 fontconfig 做法。
  - `wayland-cursor` 0.31：仅作 `cursor-shape-v1` 不可用时的光标主题回退。
  - **不用 smithay-client-toolkit**：它带 calloop 事件循环与一整套抽象，而我们已有自己的
    `poll` 循环与宿主结构，与 X11 后端直接用 x11rb 同理。
- **后端选择**：运行期决定。**功能对齐前默认 X11**：只有 `WINDUI_BACKEND=wayland` 才试
  Wayland（连不上提示后回退 X11）。Stage 2–5 完成并在 GNOME 真桌面验证后，再改为有
  `WAYLAND_DISPLAY` 且连得上就自动优先 Wayland。Cargo feature `wayland`（Linux 默认开，可关以缩依赖）。
- **复用**：文字栈 `src/text/linux/`、软件渲染器、`keys.rs` 的 keysym→`Key` 表、离屏截图、
  单实例转发，全部原样复用。
- **协议做不到、只能文档化的**：应用不能设窗口坐标（居中/定位类 API 空操作）；不能查询/撤销最小化
  （唤起走 `xdg-activation-v1` 请求激活）。

## Stage 1: 基座——共享宿主抽取 + 最小可见窗口
**Goal**: 从 `x11.rs` 抽出与显示协议无关的宿主逻辑（点击计数、帧配速、多窗口表、`after_event`
收尾）到 `linux/host.rs`，X11 行为不变；新建 `wayland.rs` 跑通 registry → `xdg_toplevel` →
`wl_shm` 双缓冲呈现 → `frame` 回调配速 → 按脏区 `damage_buffer` → 关闭。
**Success Criteria**:
- X11 后端全部既有测试与真窗口验证不回归（抽取是纯搬移）。
- `fullshowcase` 在 headless weston 下出图、可缩放、关闭干净；空闲零 CPU（阻塞在 `poll`）。
- 同尺寸同 DPI 下私有内存不高于 X11 后端（3.7MB@100% 关于窗基准）。
**Tests**: 后端选择逻辑单测（环境变量 / 连接失败回退）；shm 缓冲池复用与尺寸变化单测；
weston headless 真协议往返（读回 buffer 做像素断言）。
**Status**: Complete（2026-09-24）。验证数据与环境搭法见 `docs/LINUX_PORTING.md` §8。偏差：
像素断言用 `weston-screenshooter` 抓屏对比离屏渲染，未写成 `cargo test` 里的自动化用例
（需要外部 weston 进程）；「可缩放」只验了合成器给定尺寸（最大化 / 还原），headless 无输入
设备、拖边缩放留到 Stage 2 与真桌面。后端默认不启用，须
`WINDUI_BACKEND=wayland` 显式指定（2026-09-24 定，理由见上「后端选择」）。

## Stage 2: 输入与缩放
**Goal**: `wl_seat` 指针（含 `axis_value120` 高精度滚轮、`frame` 聚合）、键盘（xkbcommon keymap
+ **客户端自行实现按键重复**——Wayland 服务端不发重复事件）、光标（`cursor-shape-v1`，回退
`wayland-cursor`）、HiDPI（`fractional-scale-v1` + `viewporter`，回退整数 `preferred_buffer_scale`，
**运行期换 DPI 跟随**）、无边框拖动/缩放（`xdg_toplevel.move/resize` 带 serial）、`WindowOp` 全集。
**Success Criteria**: 键鼠交互、快捷键、双击最大化、1.25/1.5/2x 缩放锐利；按键长按重复速率跟随
`repeat_info`；窗口在两块不同缩放的显示器间拖动时重新出图不糊。
**Tests**: 按键重复计时器单测（延迟/速率/松键即停/失焦即停）；分数缩放尺寸换算单测
（逻辑↔物理取整，避免 1px 缝）；headless weston 注入输入的往返测试。
**Stage 1 审查遗留（已处理）**：frame 回调按 `WlCallback` 身份复位；上一帧还没送上屏
（缓冲全忙）时不再为动画反复出帧；owner 重建角色时给从属窗口重设 parent；memfd 的 glibc
≥ 2.27 下限写进 `shm.rs` 与 LINUX_PORTING §8.2；frame 回调 1 秒兜底超时；阻塞对话框期间
不回 ping 记为已知缺口（`after_event` 注释，X11 同样现状）。
**Status**: In Progress（2026-09-24）——实现与 sway / weston headless 自动化验证完成（证据见
`docs/LINUX_PORTING.md` §8.5、§8.7）；**待 GNOME 42 真桌面人工验证**交互项（点击、打字、
长按重复、拖动 / 缩放无边框窗口、双击最大化、改缩放）后改 Complete。偏差：headless weston
无输入，注入改在 sway 上做（自写常驻注入器，理由见 §8.7）；「两块不同缩放的显示器间拖动」
只验了单输出运行期改缩放，双输出留给真桌面。拖动区右键用框架自己的系统菜单而非
`show_window_menu`（与另两个平台一致）。

## Stage 3: 剪贴板与文件拖入
**Goal**: `wl_data_device`：复制（`wl_data_source` 应答 `send`，写管道）、粘贴（读 offer 管道，不阻塞
事件循环）、文件拖入（`text/uri-list` → 落点路由到 `on_drop_files`，与 XDND 同一上层接口）。
**Success Criteria**: 与其它 Wayland 应用双向复制中文/大段文本；从文件管理器拖入多文件、含空格与中文路径。
**Tests**: uri-list 解析单测（百分号解码、`file://` 以外的跳过）；headless 下自写最小 data source 往返。
**Status**: In Progress（2026-09-30）——实现与 sway headless 自动化验证完成（证据见
`docs/LINUX_PORTING.md` §8.5）；**待 GNOME 42 真桌面人工验证**后改 Complete，清单：
1. gedit / 终端里复制中文，本应用 Ctrl+V 粘进输入框；反向：本应用复制，gedit 粘贴。
   若本应用复制后**自己**立刻粘贴得到旧内容（gedit 那边却是新的），是「有焦点、sync 回来前
   没见到自家选区即判被拒」这条判定在 mutter 上判错了（依赖合成器同步回发选区，未实测，见
   LINUX_PORTING §8.3「剪贴板」；判错只影响本地，片刻后自家选区事件到了即恢复）；
   判定首次生效时终端里会打一行 `[windui] 合成器没有采用这次剪贴板写入…`（stderr，示例也看得到）。
2. 大段文本（>1MB，比如 `seq 1 200000` 的输出）两个方向都完整。
3. 本应用复制后关掉本应用，再在 gedit 粘贴：GNOME 42 无剪贴板管理器时预期**贴不出**
   （Wayland 协议如此），记录实际表现。
4. X 应用（经 XWayland，如 `xterm`）与本应用互相复制。
5. 从 Nautilus 拖 1 个、多个文件到 `file_drop` 示例，含空格与中文路径；拖到窗口不同位置
   （示例里全窗接收，另可用分左右两区的探针）；拖网页链接 / 选中文字进来应显示禁止光标。
6. 拖入时 Nautilus 里源文件不被移走（我们只接受复制）。
偏差：剪贴板只能在界面线程读写（X11 有独立剪贴板线程）；只处理启动时绑定的那一个 seat；
「写入被合成器拒绝」的判定只有状态机单测，sway 上没能构造出被拒的场景（有键盘焦点时
enter 的 serial 就足够）。不做：文件拖出、primary selection（中键粘贴）。

## Stage 4: 输入法
**Goal**: `text-input-v3`：焦点进出 enable/disable、`set_cursor_rectangle` 让候选窗跟随光标、
`preedit_string` 接入现有合成串内联绘制、`commit_string` 提交、`done` 序号对账。
**Success Criteria**: fcitx5 与 ibus 下中文输入、候选窗位置、合成串内联显示正确。
**Tests**: 协议状态机单测（preedit/commit/done 的批量应用顺序）；**真桌面人工验证**
（headless 合成器无现成输入法，此项不声称自动化覆盖）。
**Status**: Not Started

## Stage 5: 装饰与桌面集成
**Goal**:
- `xdg-decoration` 协商；合成器不给服务端装饰（GNOME/Mutter）时**框架自绘客户端标题栏**：
  标题 / 最小化 / 最大化 / 关闭、边缘缩放、双击最大化、右键 `show_window_menu`，读 `theme::current()`。
- 多窗口与模态：`set_parent` + `xdg-dialog-v1`。
- 全局热键**不实现**（见下「已记录、暂不实现」）：Wayland 下 `register_hotkey` 记日志空操作，
  文档写明兜底用法。
**Success Criteria**: GNOME 下有边框窗口有标题栏且可拖可缩；KDE/sway 下用服务端装饰不重复画；
模态子窗置于父窗之上。
**Tests**: CSD 标题栏命中区单测（按钮 / 拖动区 / 缩放边）；装饰模式切换截图回归。
**Status**: Not Started

## 已记录、暂不实现：Wayland 全局热键（2026-09-24 决定，有用户反馈再做）
Wayland 协议刻意不让客户端抓全局按键，可选路线：
- **兜底（零代码，现在就能用）**：用户在桌面设置里把快捷键绑到 `myapp --toggle`，
  单实例转发（`single_instance` 已把第二次启动的 argv 转给运行中实例）送达应用。
  需在 API_GUIDE / LINUX_PORTING 写明此用法。
- **首选实现**：xdg-desktop-portal `GlobalShortcuts`（KDE 5.27+、GNOME 48+、Hyprland；sway 无）。
  不引 `zbus`/`ashpd`（体量大且异步），自写最小同步 D-Bus 客户端（EXTERNAL 认证 + 方法调用 +
  信号匹配 + 必要的序列化），其 fd 并入现有 `poll` 循环。首次使用会弹系统授权框。
- 排除：合成器私有协议（太碎）、读 `/dev/input`（需 input 组，等同键盘记录器）。

## 验证环境
- **本机（自动化）**：无 sudo，按 X11 那次的做法 `apt download` + `dpkg -x` 解包 weston 13，
  跑 headless 后端做真协议往返。
- **192.168.5.55（真桌面）**：Ubuntu 22.04，GNOME Shell 42.9 **Wayland 会话**。CSD 与输入法两项
  在这里人工验证。注意 Mutter 42 **没有** `fractional-scale-v1` / `cursor-shape-v1` /
  `xdg-dialog-v1` / `xdg-decoration`——恰好覆盖各项的**回退路径**（整数缩放、光标主题、
  无对话框提示、必须 CSD）；这些新协议的**正向路径**要靠 weston 或更新的桌面补验，
  不能以这台机器的结果声称已覆盖。
- 该机 glibc 2.35，本机编的二进制跑不了，用 cargo-zigbuild `--target x86_64-unknown-linux-gnu.2.35`。

## 不在本计划内
系统托盘（SNI 与 X11 共用，另立）、文件拖出、窗口模式 GPU、零窗口常驻。
