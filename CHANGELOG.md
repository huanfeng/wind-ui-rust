# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added
- **表格行右键菜单 `Element::on_row_context_menu`**：右击数据行时按行下标现取现建菜单项并
  弹级联浮层，返回空 `Vec` 则不弹。三类表格（`table_sortable` / `table_sortable_server` /
  `table_selectable`）均支持——右键与首列复选框不争语义（复选框只吃左键），故不像整行双击
  激活那样把可选表格排除在外。菜单**每次右击重建**，`with_check` / `with_enabled` 才能反映
  右击当刻的数据；回调挂在行容器上，行内空白、自定义单元格、操作列上右击都能弹。
  表格行是控件内部构建的，应用侧拿不到行 `Element`，故这条接线只能由框架提供。
- **无 `ctx` 的延迟执行 `app::defer_blocking`**：把含阻塞式原生调用（文件对话框、
  `MessageBoxW` 等）的流程排到事件分发**完全返回**之后执行，是 `EventCtx::defer_blocking`
  的自由函数版本。菜单项动作是无参 `Fn()`（`MenuItem::run`），执行时虽已不在控件的
  `on_event` 里、却仍在平台消息回调栈内，直接同步弹原生模态框会与对话框自身的消息泵冲突；
  右键菜单里的"导出到文件…"这类项此前**无法表达**，只能靠把动作挪回工具栏按钮绕开。
  复用既有的 `DialogRequest::Custom` 通道交付（平台已在正确时机轮询它），
  `AppHandler::take_dialog_request` 的默认实现即取该队列，自定义 handler 覆盖时记得回退到它。
- **私用区回退字体 `text::register_private_use_font`**：注册一个 `.ttf` 后，文本里落在
  Unicode 私用区的码位改用它渲染，其余字符不受影响。图标字体（Font Awesome、Material
  Icons 等）的字形全部落在私用区，注册后即可把图标码位当普通文字放进任何 `label`/`button`，
  与文本同流布局、随字号缩放，无需另做图片资源。字体**不必安装到系统**——走 DirectWrite
  自建字体集加载文件，应用可以直接把 `.ttf` 随包分发。
  三段私用区（BMP `U+E000..=U+F8FF`、补充私用区 A/B `U+F0000..=U+10FFFD`）全部识别：
  图标集用哪一段并不统一，只判 BMP 会让用补充私用区的字体静默落回主字体、渲染成方框；
  而判据不能只看"是不是代理对"，否则 CJK 扩展 B 等生僻字会被误切到图标字体，同样变方框。
  注册表沿用 `render::image` 解码器的 thread-local 模式，须在 `App::run` 前调用；
  运行期替换用 `DWriteEngine::set_private_use_font`（它会一并清测量与基线缓存——
  私用区字符换了字形来源，宽度随之改变）。
- **拖拽重排列表 `Element::reorder_list`**：面向设置类应用的手动排序列表，每行前置拖动手柄，
  按住上下拖动即可调顺序，其余行平滑让位、被拖行浮起跟手，松手播回落动画后才提交，
  拖动中按 `Esc` 取消。手柄独立于行内容，故行里照常可放开关/下拉/输入框而不抢事件；
  让位按各行实际高度重新堆叠，支持带副标题的不等高表单行。
  默认 `CommitMode::Children` 直接重排子节点、**不重建行**，行内控件状态天然保留；
  数据驱动场景切 `CommitMode::Callback`，由应用在 `on_reorder` 回调里更新数据源。
  设计文档见 `docs/reorder-design.md`。
- **`Node::offset` / `Node::raised` 绘制层能力**：`offset` 是不参与 measure/arrange 的
  绘制/命中偏移，`raised` 把子节点提到同级最上层绘制并优先命中。二者供"视觉位移但布局不变"
  的场景使用（拖拽让位、后续的 FLIP 动画等）——直接改 `bounds` 会被任何一次 relayout 冲掉。
  变化纳入 `layout_signature`，故宿主自动升级整窗重绘，无需为其开特例分支。
- **数据驱动重排 `Element::reorder_list_signal`**：行由 `Signal<Vec<T>>` 生成，信号变化即整体
  重建，因此顺序的真相源在数据侧——`reorder_list` 的 `Children` 模式把顺序只存在节点树里，
  应用无法把顺序**推回**控件，「恢复默认」「重新载入配置」这类反向同步全部落空。
  重建能力做成非泛型的内部 `RowSource` 内嵌进 `ReorderList`（而非套一层 `DynList` 宿主：
  一个节点只能挂一个 widget），故控件保持非泛型、`on_reorder`/`commit_mode` 的 downcast 照旧。
  拖动中一律不重建（会打乱槽位快照与补间下标），积压的数据变更留到落定后补做；
  落定提交后**同帧**重建，不闪回旧顺序。
  手柄作为 `row_fn` 的第二个参数交还调用方安放：整行 `clickable()` 的列表**必须**把手柄放进
  `stack` 当同级覆盖层——`Clickable` 消费 `Down`/`Up`，手柄嵌在它内部时冒泡断在那里，
  列表收不到事件、拖动起不来。
- **`ReorderTheme` 主题覆盖层**：手柄常态/悬停色、拖动中行底色与投影、指示线色、手柄槽宽、
  拖动行圆角，全部 `Option` 回退 palette 并接入 TOML。

## [0.11.1] - 2026-08-11

### Fixed
- **`--no-default-features` 下编译失败**：`SvgSource` 及其 `impl` 没有随 `svg` feature 门控，
  而 `resolve` 内调用的 `Image::from_svg_bytes` 有门控——关掉 `svg` 就是 E0599「找不到
  `from_svg_bytes`」，且报错指向本 crate 内部，使用方无从下手。同一组合下 `SM_REMOTESESSION`
  的未使用导入（只在 `d2d` 下用到）一并门控。
  漏到发布版是因为 CI 只跑默认 feature：本仓的 example 全部跑在默认 feature 上，而唯一使用
  `default-features = false` 的消费者在另一个仓里，本仓看不见。已在 CI 补 `--no-default-features`
  的 clippy 门禁防回归。

### Changed
- **`fullshowcase` / `image` 两个 example 声明 `required-features = ["svg"]`**：它们用到
  `icon_svg` / `image_svg`，关掉 `svg` 时应被跳过而非编译失败。其余 example 仍自动发现。

## [0.11.0] - 2026-08-11

本版本补齐键盘可达性：浮层菜单弹不出也动不了、模态对话框圈不住焦点、Tab 会跑到视口外、
窗口按钮按空格没反应——这些键盘死角逐一修掉。另新增下拉式复选菜单，以及 SVG 按实际 DPI
现场光栅化（各缩放档位下描边 1:1 落像素）。

### Added
- **`CheckMenu` 下拉式复选菜单**（`Element::check_menu`）：外观同 `Dropdown`（当前项即入口），
  面板是菜单，项支持开关 / 动作 / 分隔线混排。默认点击即关（与右键菜单、单选下拉一致），
  `.stay_open()` 显式开启粘滞——留给「一次连改多个」的场景，如一组显示过滤。粘滞是整菜单
  开关不做逐项差异，同一面板里有的关有的不关，用户无法预期下一次点击会发生什么；动作项则
  无论如何都关闭。配套 `CheckMenuItem::on_change`（收到的是已生效的新值，与 `CheckBox` 的
  `on_toggle`「取代默认翻转」不同义，故不同名）。
- **`MenuItem::stay_open`**：菜单项点击执行后不关闭浮层，仅对 `MenuAction::Run` 有效
  （`SendKey` 是「把按键交给控件、菜单退场」的语义，与粘滞矛盾）。配套 `MenuRequest::rebuild`
  在粘滞项点击后原地刷新勾选态：沿 spawn 路径逐级换项并保留每级的 rect/scroll，不重跑
  `build_level`——后者会重新测量宽度、重做边界翻转，面板整个跳位置，而用户此刻正把指针停在
  上面准备点下一项。
- **浮层菜单完整键盘操作**：↑↓ 移动选项（跳过分隔线与禁用项、到头循环、自动滚进可视区）、
  Home/End 首末项、→← 进出子菜单、回车/空格执行、Tab 收起浮层。首次 ↑↓ 落在 checked 项而
  不是跳走一格——菜单刚弹出时没有高亮，直接跳下一项会让人不知道原来选中的是哪个。
- **对话框把键盘焦点圈在框内**：新增 `Widget::is_modal()`，`focusable_order` 改从最上层可见
  模态子树收集——此前遮罩后面那些鼠标点不到的控件，Tab 仍能停上去、空格仍能按下去。层级取
  「前序遍历中最后出现」而非「最深」，与 `hit_test` 的语义一致。另补焦点移交（同
  `<dialog>.showModal()`）：弹出落到框内首个可聚焦控件，关闭还给来处；嵌套 A→B 切换不覆盖
  来处，A 也关掉时才还给最初那个。
- **`Tree::scroll_into_view(id)`**：沿祖先链由内向外逐级对齐，每级只依赖当前帧的几何——内层
  滚完后目标项已落在内层视口内，外层只需把内层容器整个滚进来。

### Changed
- **SVG 按 DPI 现场光栅化**：`ImageContent` 改为保留矢量源，paint 期按 dst 的实际物理尺寸
  光栅化并按该尺寸缓存（着色结果一并入缓存）。写死光栅宽只在恰好等于该倍率的 DPI 下才是
  1:1，其余档位都要经一次双线性重采样，细描边被摊成两行灰边。`from_svg_bytes(_, None)` 即
  启用 DPI 感知，`Some(w)` 保持写死光栅宽的旧语义；`Element::image_svg` 一并改走 content
  路径（原先绕过 `ImageContent` 直接构造 `Image`，DPI 感知对它不生效）。
- **两个后端的 `draw_image` 补像素吸附**：物理尺寸与源图相差不足 1 像素时吸附为 1:1，落点
  取整到整数物理像素。d2d 尤其必要——该路径全程逻辑坐标，逻辑整数在 125%/150% 下会落到半
  物理像素上，LINEAR 插值照样把图标糊掉。
- **分发副作用收口为 `apply_dispatch_effects`**：`DispatchResult` 的十个副作用字段原先由指针
  与键盘两条路径各自手写消费，加字段时两边都不报错、漏接也没有任何征兆。改用无 `..` 兜底的
  解构，字段一增即 `E0027`，逼作者当场决定它归谁管——产出不是少了几行重复，而是把一类静默
  失败换成了编译错误。

### Fixed
- **下拉框按空格没反应**：宿主键盘路径消费了 `DispatchResult` 的 close / open_url / window_op
  / dialog / toast，唯独漏了 `menu` 与 `focus`，而指针路径两个都接。控件侧一直正确发出展开
  请求，是宿主收进结果后静默丢弃了。漏 `focus` 的后果更隐蔽：键盘路径上任何控件调
  `request_focus` 都无效。
- **点击控件外的空白不清焦点**：焦点归属此前完全由控件申报，点空白时没人上报、旧焦点原样
  留着，于是「取消高亮」只能作为「另一个控件接手」的副作用发生。改为宿主在每次 Down 上重新
  裁决（与 `activeElement` 的模型一致），判据取「命中节点是否落在焦点子树内」而非「本次有没有
  控件 `request_focus`」——后者会误清「点在焦点控件自己的内部子节点上」与「按下被上层可点击
  容器先消费」两种情况。
- **窗口按钮不支持空格/回车激活**：`WindowButton::focusable()` 返回 true 本是为了让标题栏拖动
  判定在按钮上让路，副作用是它一并进了 Tab 焦点环，而 `on_event` 只有 Pointer 分支——Tab 能停
  上去、按空格没反应，成了键盘死角。
- **焦点环只跟随键盘**：对话框的焦点移交此前无条件打开焦点环，纯鼠标用户会看到凭空冒出来的
  框。`:focus-visible` 的判据从来不是「这次聚焦是不是程序性的」，而是用户最近一次交互用的什么
  设备，故改为沿用当前状态——焦点本身照旧移进对话框，只是不画。
- **Tab 焦点跑到视口外**：滚出视口的节点只是被 `clip_children` 在绘制时裁掉、逻辑上仍可见，
  照样进焦点环，于是 Tab 几下焦点就到了看不见的地方，长列表里按空格会激活一个屏幕上根本没有
  的控件。改为 scroll-into-view 而非把视口外节点踢出焦点环——后者会让长列表下半截键盘不可达。
- **关闭浮层后的面板残影**：菜单画在控件树之上、不属于任何节点，而 render 的 overlay 判定问的
  是「本帧有没有浮层」——关闭帧已经没有了，此时若恰好存在一小块脏区就会走局部重绘，面板像素
  留在屏上直到下一次整窗重绘。四处关闭点（点面板外 / 尾随图标 / 命中叶子项 / Escape）收口为
  `close_menu` 统一升整窗。

## [0.10.0] - 2026-08-01

本版本集中修无边框窗口下的交互缺陷——滚动条被窗口缩放边框压住、弹出对话框后整窗拖不动，
另有文本控件选区与插入光标的渲染增强，以及非整数 DPI 下末字误换行的修复。

### Added
- **`Widget::tooltip()` 动态悬停提示**：控件可按当前指针位置自报提示文本，优先于节点上
  `.tooltip(..)` 设的静态文本，返回 `None` 则回退到静态文本（没有则不弹）。
  给自绘图表类控件用——整张图是一个节点，提示内容取决于指针落在哪个数据点上
  （日历热力图的哪一格、柱状图的哪一根），静态文本表达不了。控件在 `on_event` 里记下
  命中项、在 `tooltip()` 里据此返回文案即可，浮层的延时/跟随/边缘翻转仍由宿主统一处理。
  默认实现返回 `None`，既有控件不受影响。
- **`Element::max_height(px)` 限高**：只收窄节点占位，不削减滚动容器的 `content_h`
  ——限高的滚动区仍可滚到全部内容。
- **`Rect::scaled_out(scale)`**：左/上 `floor`、右/下 `ceil` 的物理化，契约为物理宽高不
  小于 `size × scale`、空矩形恒为空。`scaled()` 保持 `round` 语义，裁剪 mask 与相邻矩形
  仍无缝不重叠。

### Changed
- **插入光标改反色渲染**：光标条铺好后裁到光标矩形、用输入框底色把本行文字重画一遍，
  落在光标宽度内的字形笔画因此翻成底色（等同经典 XOR 插入符的观感）。光标与文字同色时
  压在笔画上会粘连、看不出落点，反色后不再沉进文字里。不走 difference 混合是因为 D2D
  后端的 `SetPrimitiveBlend` 只有 SourceOver/Copy/Min/Add，真反相需改走 `ID2D1Effect`
  离屏合成或每帧 GPU 读回。

### Fixed
- **无边框窗口弹出对话框后整窗拖不动**：模态遮罩全窗覆盖且自带背景，命中测试停在遮罩上，
  自绘标题栏因此拿不到 `HTCAPTION`。拖动区判定改走穿透遮罩的命中（新增
  `Widget::scrim_passthrough`，仅 `ModalScrim` 覆写），遮罩内的面板仍会在子遍历里先落定
  ——被面板压住的标题栏区域照旧不可拖。事件分发与交互控件判定不受影响：遮罩照常吞指针
  事件、照常屏蔽标题栏上的窗口按钮，模态语义不变。
- **选区高亮改按行盒全高铺底**：`TextInput` 原先上下各内缩 2px、`RichText` 按碎片自身高
  铺底，`p`/`{` 等下伸部露在高亮外，多行选中还在行与行之间留白缝。现在纵向一律取行盒
  ——混排字号同行顶底齐平、相邻行首尾相接。
- **滚动条避开窗口缩放边框**：无边框窗口把客户区右缘 8 逻辑 px 判为 `HTRIGHT`，贴边的
  滚动条整条压在缩放边框底下，看得见点不着。贴窗口右缘的滚动容器整体内缩 8px 与边框相接
  （不贴边的容器如对话框内滚动区保持原有紧凑外观），命中区由 10px 加宽至 16px 且两侧有界；
  滑块配色改取主题角色——原先写死的黑色半透明在深色主题下会连滑块一起隐没，轨道底衬默认不画。
- **非整数 DPI 下文字末字误换行**（125%/175%/225% 等档位）：`Rect::scaled()` 四条边各自
  `round`，取整方向不一致时物理宽会比 `w × scale` 略窄，据此反向换算出的排版宽度装不下原
  文本，DirectWrite/CoreText 便把本应单行的最后一个字挤到下一行。排版最大宽度改用
  `scaled_out()`，与 measure 的 `max_width × scale` 同源；定位仍走 `scaled()`。（#6）

## [0.9.0] - 2026-07-23

本版本新增 RichText 富文本控件与全局热键管线，并把文字属性收进 `TextStyle`——后者改动了
`TextEngine` / `Canvas` 两个 trait 的签名，自定义渲染后端需要跟随调整（见 Changed 的破坏性条目）。

### Added
- **`RichText` 富文本控件**（`Element::rich` / `rich_rc`）：段落 + 碎片（span）模型，配套能力如下。
  - **排版**：CJK 避头尾（闭合标点不落行首、开括类不孤悬行尾）、`Para::hanging` 悬挂缩进
    （编号义项续行对齐释义首字）、`Para::spacing_before` 按段覆盖段距。
  - **span 点击**：`Para::span_id` / `styled_id` 标注纯数据 id，回调经 `Element::on_span_click`
    挂在控件层——`RichDoc` 保持 `Clone` / 可比较 / 可缓存。悬停手型 + 同 id 跨行碎片一起提亮。
  - **划选复制**：碎片级选区（CJK 逐字、Latin 整词吸附、chip 整体）、选区高亮、`Ctrl+C` 复制选区、
    `Ctrl+Shift+C` 强制全文、`Ctrl+A` 全选，右键菜单按选区态给「复制 / 复制全部 / 全选」。
    跨块补换行、块内软换行按 CJK/Latin 边界补空格。
  - **双击选词 / 三击选段**：双击对 CJK 吞并同块内连续汉字碎片（至标点/空白/chip 边界止），
    三击选中命中碎片所在段落全部碎片（含软换行续行、不跨段），对齐浏览器习惯。
  - **折叠 Section**：可 `Tab` 聚焦，`↑↓` 在折叠头间移动、`Enter`/`Space` 翻转；展开/收起为
    卷帘高度动画（收拢中按目标状态完整排版，对外只占补间高度）。
  - **行数截断**：`Para::clamp(max_lines, expanded)` 未展开只排 N 行，行尾缀可点击的「… 展开」标记
    （不计入复制文本）。
  - **动态文档**：`Element::rich_rc(Signal<RichDoc>)` 整篇换文档，同步失效布局缓存与选区、复位悬停
    与键盘焦点下标。
  - `RichDoc::plain_text`（含 chip 与折叠区文字）与内建右键「复制全部」菜单，`Element::copy_menu(false)` 可关闭。
- **全局热键**：`App::hotkey` 注册全局热键、`App::start_hidden` 启动不显示窗口、
  `EventCtx::show_window` / `hide_window`，`WindowOp` 增 `Show` / `Hide`。回调只拿意图不拿句柄
  （`HotkeyCtx` 仅持 `Option<WindowOp>`），窗口操作在平台层释放借用后执行。注册失败不阻止启动。
  Windows 走 `RegisterHotKey` + `WM_HOTKEY`；macOS 待补。
- **热键运行期改绑**：`App::hotkey_rc` 返回 `HotkeyHandle`，`rebind(hotkey)` / `set_enabled(bool)`
  运行期即时生效（此前仅启动期一次性注册，改热键须重启）。改绑失败回滚重注册旧组合，
  `set_enabled(false)` 注销把组合归还系统。
- **主题运行期动态更新**：`ThemeHandle::update(|t| ...)` 局部改主题（换强调色/调字号一行完成，
  下一帧全树跟随）；新增 `Brush::RoleAlpha(Role, alpha)`、`Element::bg_role_alpha` 与
  `Role::InputBg` / `InputBorder`，把构建期取色改为角色延迟解析——徽章/chip/标签输入/对话框面板/
  表格编辑格换主题后自动跟随，不再停在旧主题色。
- **关闭即隐藏**：`App::hide_on_close()` 把 `ESC` 与标题栏关闭按钮转为隐藏窗口，退出留给托盘菜单
  （常驻托盘类应用的常见期望）。拦截器优先级高于它——`close_handler` 返回 `false` 时窗口既不关也不隐。
- **文字排版三项**：`Element::line_height(倍数)`（取倍数使行距随字号与 DPI 缩放）、
  `Element::max_width(px)`（测量前收窄可用宽，内容据此换行而非事后裁切）、
  `Element::border_edges(Edges)` 单边边框（页签下划线、分区底线不必再用 1px 色块拼）。
- **字体族**：`Element::font_family(name)` 指定字体族名（Windows/macOS 均生效）。字体未安装时静默回退系统默认，不报错也不 panic。
- **节点级焦点覆盖**：`Element::focusable(bool)` 控制 `Tab` 遍历是否纳入该节点（不改命中/拖动/`request_focus` 语义）。
- **胶囊式标签条**：`TabStyle::Pill` 与 `Element::tabs_pill`——accent 实底胶囊 + 白字滑动。
- **下拉项富信息**：`MenuItem` 新增 `subtitle` / `badge` / `trailing_icon`，展开态支持两行项与徽章胶囊，
  尾随图标点击独立于主项 action；收起态同步显示选中项徽章。新增 `DropdownItem` 与
  `Element::dropdown_items`，纯文本 `Vec<String>` 旧用法零改动。
- **表格整行双击激活**：`Element::on_row_activate`（释放 `Up` 时触发）。
- **无边框窗口圆角**：`frameless()` 窗口在 Win11 上显式声明 `DWMWA_WINDOW_CORNER_PREFERENCE`，与系统其余窗口一致；Win10 上 DWM 不识别该属性、返回错误码并被忽略，无需版本判断。macOS 由 AppKit 天然保持圆角。

### Changed
- **（破坏性）文字属性收进 `TextStyle`**：`TextEngine::measure` / `line_metrics` 与
  `Canvas::measure_text` / `draw_text` 改为接收 `&TextStyle`，字族/字号/字重/行高一并传递；
  原先的线程局部字重注入（`text::set_weight` / `current_weight`）随之删除——那让字重成了隐式全局
  状态，漏复位就会让后续无关文字跟着变粗。自定义 `TextEngine` / `Canvas` 实现需按新签名调整；
  控件调用方改为 `&TextStyle::of(style)`，比原先的散开参数更短。
- **（破坏性）`TrayCtx` 改意图队列**：不再持有 `hwnd`/`uid`，四个方法只累积 `TrayAction`，由平台层在
  释放借用后执行；macOS `TrayCtx` 同步改 `&mut self`，使两平台签名一致。
- **标签条重做为下划线式**：`TabButton` 逐节点 → 单个自绘 `TabBar`，选中项为整格宽指示条 + 贯穿基线，
  切换时横向滑动；去掉选中焦点框与悬停淡底，选中态加粗且按选中字重恒定测量以免布局抖动。
  整条为一个焦点节点、内部 `Left`/`Right` 移动，符合 tablist roving tabindex 约定。
- **chip 前景对比度**：默认前景按 WCAG AA 自适应——从 accent 向正文色插值直到对实际底色 ≥4.5:1
  （「同色淡底 + 同色前景」实测仅约 3:1）。
- **事件路径时间源**：新增 `EventCtx::now_ms` 作为事件回调中的推荐时间源。

### Fixed
- **托盘回调重入 UB**：`WM_TRAYICON` 在持有 `&mut WindowState` 期间跑用户回调，而回调经 `TrayCtx`
  直接调 `ShowWindow`/`DestroyWindow`、右键还调模态的 `TrackPopupMenu`，重入 `wnd_proc` 后再取一次
  `&mut WindowState` 即别名 UB；其中 `quit()` 的 `DestroyWindow` 会同步 drop 掉正在执行的闭包本身，
  属 use-after-free。改为意图队列后消除。顺带修正点托盘图标唤不起最小化窗口（`SW_SHOW` → `WindowOp::Show`）。
- **帧时钟在事件路径冻结**：`clock_ms()` 此前只在 render 前刷新，空闲不出帧期间停在上一帧，
  两次交互之间的静默期被整段计入时长判定（长按、双击、拖动速度均受影响）。`on_pointer`/`on_key`
  入口也同步帧时钟。
- **步进器点击即进快速加**：长按起点改由按下后首帧 paint 用刚刷新的帧时钟锚定，不再在事件路径读冻结时钟。
- **清屏色不随主题热切换**：未经 `App::bg` 显式固定时，`UiHost` 每帧跟随 `palette.bg`——修「切暗色主题后
  清屏/局部重绘仍是亮色底」。`theme()` 不再覆盖显式 `bg`（`.bg(c).theme(t)` 与反序同义）。
- **下拉徽章灰字灰底**：Neutral 意图徽章前景改用 `text_muted`。
- **最小化/最大化动画期左上角内容被拉伸**：flip-model 交换链下 `ResizeBuffers` 到重绘落地之间存在真空期，
  DWM 会采样旧尺寸缓冲并按 `DXGI_SCALING_STRETCH` 从左上角拉伸。非拖拽的最大化/还原改同步重绘
  （拖拽缩放中保持异步以免拖累手感）、跳过 `SIZE_MINIMIZED`、交换链 Scaling 改 `NONE`。
- **单实例转发失败被挡在门外**：首实例退出中或僵死时 `WM_COPYDATA` 同步发送会把二次实例一起挂住；
  改用 `SendMessageTimeoutW(SMTO_ABORTIFHUNG)` 探测送达失败并回退为正常启动新窗口。
- **表格多行单元格顶部对齐**：多行分支由 stack 改为 row + `cross(Center)`，同行折行撑高时单行文本格竖直居中。
- **富文本布局缓存每帧堆分配**：`ensure_layout` 命中判定改引用比较 + 零分配快路径，仅 miss 时构造 `LayoutKey`。

## [0.8.3] - 2026-07-13

### Added
- **表格单元格多行**：`Table` 单元格支持多行文本，新增 `cell_lines(n)` 配置显示行数。

### Fixed
- **表格多行单元格裁切**：多行单元格内容被错误裁切，修正行高与裁剪区计算。
- **`on_update` 相位的 toast 被丢弃**：在 `on_update` 阶段调用 `ctx.toast*` 发出的浮层不再被丢弃。
- **对话框复显开关瞬时落定**：对话框重新显示时开关状态瞬时正确落定；文本输入清除残留选区。
- **无边框窗口标题栏区域 toast 失效**：无边框窗口标题栏区域的 toast 被命中判定为客户区，修复其上 ✕ 关闭 / 右键菜单失效。

### Changed
- **toast 面板样式**：降低面板高度、移除强调色条，右键菜单置于 toast 之上。

## [0.8.2] - 2026-07-06

### Fixed
- **连续空格中光标无法移动**：`DWRITE_TEXT_METRICS::width` 不含尾随空白宽度，导致以
  空格结尾的子串测量宽度被折叠为同一值——文本框光标索引在连续空格中正确递增，但换算出的
  视觉 x 坐标不再前进，表现为"光标卡在第一个非空格字符处"。改用
  `widthIncludingTrailingWhitespace` 字段（`src/text/dwrite.rs`、`src/platform/win32/d2d.rs`）。
- **输入法组合态期间自绘光标位置错误**：拼音等未上屏组合期间，`TextInput`/`Stepper` 自绘的
  光标条停留在组合开始前的位置不动，与系统组合浮层里跟随合成进度前进的光标同时存在，视觉上
  像卡住。新增 `Widget::set_composing`，由平台层在 Windows 的
  `WM_IME_STARTCOMPOSITION`/`WM_IME_ENDCOMPOSITION`、macOS 的
  `setMarkedText`/`unmarkText`/`insertText:` 时通知焦点控件，组合期间跳过自绘光标绘制，
  交由系统浮层呈现。
- **输入法组合串字体与正文不一致**：Windows 合成串 `LOGFONTW.lfFaceName` 之前留空，系统常
  回退到陈旧的宋体；现显式指定为与正文渲染同族的 `Microsoft YaHei UI`。

## [0.8.1] - 2026-07-06

### Added
- **`PickDialog` 同步方法误用检测**：`pick_file`/`pick_files`/`pick_folder`/`pick_folders`/
  `save_file` 在控件事件回调（`on_click`/`on_event`）栈内被调用时，`debug_assert!` 报错
  （release 构建零开销剔除）——把"回调里别同步开模态对话框，OS 捕获来不及释放会导致鼠标
  失灵"这条只写在文档注释里的契约，变成 debug/测试阶段能捕获到的确定性失败，而不是留到运行时
  变成偶发的鼠标卡死。内部用线程局部 `EventDispatchGuard` 标记风险窗口（`on_pointer`/`on_key`/
  `on_drop_files` 分发期间），win32/macos 两个后端均已接入；`app.rs::on_drop_files` 同时补上了
  之前遗漏的 `dialog` 请求转发（`Element::on_drop` 回调里调用 `EventCtx::request_*` 之前会被
  静默丢弃）。

## [0.8.0] - 2026-07-06

### Added
- **`DialogRequest` + `EventCtx::request_pick_file`/`request_pick_files`/`request_pick_folder`/
  `request_pick_folders`/`request_save_file`/`defer_blocking`**：原生文件对话框不再在事件回调
  栈内同步弹出——按钮点击回调里直接调用 `PickDialog::pick_file()` 等阻塞方法时，OS 鼠标捕获的
  释放要等整条事件分发调用栈返回才生效，导致对话框存续期间主窗口仍持有 `SetCapture`，与对话框
  自己的消息泵抢鼠标输入，反复开关几次后捕获状态与 OS 实际状态错位，表现为鼠标彻底失灵。
  现改为把对话框请求（`PickDialog` + 结果延续回调，或 `defer_blocking` 逃生舱包一段任意阻塞式
  原生调用序列）经 `EventCtx`/`DispatchResult` 交给宿主，在事件分发**完全返回**、OS 捕获同步
  完毕之后才真正执行。`PickDialog` 本身的同步 API 仍保留（非 UI 回调场景可用），但**不要**在
  `on_click`/`on_event` 回调里直接调用。
- **表格自定义单元格渲染 `Element::cell_render`**：按 `(行下标, 列下标, 单元格文本)` 逐格询问，
  返回 `Some(Element)` 用自定义控件（徽章/彩色标签/图标等），`None` 回退默认文本。排序仍基于
  单元格文本（渲染与排序键解耦）；行下标语义同 `.actions`（客户端表格为原始行下标，服务端表格
  为页内显示下标）。适用于 `table_sortable` / `table_sortable_server` / `table_selectable`，
  可与 `.actions` 组合。fullshowcase 表格 tab 新增演示。
- **`Element::host_signal`**：信号驱动的响应式重建宿主。同 `list_signal` 的重建机制，但容器为
  普通列容器（非滚动）——子元素 `weight`/`fill` 能拿到确定高度，适合整体重建"结构随状态变化"
  的子树（如列集随类别切换的表格；滚动容器按无限高度测量会令表格正文高度崩塌）。

### Fixed
- 响应式广播（`dispatch_reactive_updates`）曾用广播快照的存活集**覆盖**注册列表，把广播期间
  动态重建子树新注册的响应式节点抹掉——`list_signal`/`host_signal` 重建出的响应式表头/正文
  永远收不到 `on_update`，表格在宿主重建后空白。现改为按批次迭代到收敛（新注册节点**同帧**
  收到回调，避免首帧空白），清理阶段基于真实列表 retain。

### Changed
- `DispatchResult` 不再 `derive(Clone)`（新增字段携带 `Box<dyn FnOnce()>`，不可 Clone；原实现
  从未实际克隆过该结构，纯类型层面的收紧）。

## [0.4.0] - 2026-06-26

### Added
- **Direct2D GPU 渲染后端（Windows，可选 opt-in）**：大窗口/多控件下软件光栅 paint-bound，新增
  Direct2D 后端把几何/渐变/裁剪/opacity/图片/阴影/文字光栅迁到 GPU。窗口级显式 opt-in
  `App::accelerated(true)`（示例 `--accelerated`），**默认仍软渲染**；与 tiny-skia 软路径并存。
  - 文字坚持走 **DirectWrite**（`DrawTextLayout`，系统字体缓存 + ClearType），与软路径字体/字重一致。
  - 阴影用 `ID2D1Shadow` GPU 高斯模糊，烘焙一次缓存成品避免每帧重模糊。
  - 自动回退软渲染（绝不 panic）：RDP 远程会话、无可用 GPU、设备创建失败、离屏截图。
  - 设备丢失检测 → 整体重建设备链 → 连续失败降级软后端；同 UI 线程多窗口共享设备链（避免 ×N 内存）。
  - 重对象（文字布局/画刷/位图/后备缓冲）全缓存复用，常驻内存从早期 190M 降到 ~70M。
- 渐变画刷（线性/径向）+ `Brush`（Solid/Gradient/Role）主题角色取色体系。
- `Theme::dark` 暗色预设 + `ThemeHandle` 运行期主题热切换（整树跟随刷新）。
- 浮层投影（box-shadow）+ 子树整体不透明度（离屏层合成）。
- 级联右键菜单（图标/分隔/快捷键/子菜单）+ `Element::on_context_menu`。
- `PickDialog`：系统原生文件/目录选择对话框。
- `Signal<T>`：`Copy` 句柄状态原语（运行时 arena 承载），全控件状态从 `Rc<Cell>`/`Rc<RefCell>` 迁入；
  `set` 自动产生局部脏区，新控件免手写 `mark_dirty`。
- 文字字重支持；半透明文字色。
- `App::min_size`：限制窗口最小客户区尺寸。
- 新增 `examples/ime.rs`（复刻中文输入法界面，暗/亮双主题）。

### Changed
- 控件状态原语统一为 `Signal<T>`，取代散落的 `Rc<Cell>`/`Rc<RefCell>`（API 基本不变，状态语义更一致）。
- 渲染接缝重构：`AppHandler::render` 改为面向 `RenderTarget`，软/GPU 两后端同形接入，软路径零回归。

### Performance
- 交互失效系统：hover/拖动/点击/打字走 ~1ms **局部重绘**（结构签名判定局部 vs 整窗），不再每次整窗重绘。
- DirectWrite 测量结果缓存，消除稳定文本每帧重复排版。
- 模糊阴影缓存（位置无关），修复阴影每帧重算导致的卡顿；新增 `WINDUI_PROF` 绘制热点计时。

### Fixed
- 窗口按钮与复选框的文字/悬停色未跟随主题。
- DPI 缩放下 win32 窗口显示异常（全窗重绘 scale 由 handler 提供）。
- 点击切换内容不刷新；标签条内边距、菜单尾随快捷键换行、分段选中反色、菜单高亮溢出等多处 UI 细节。

## [0.3.0] - 2026-06-23

### Added
- 多行 `TextInput`：滚动条、滚轮滚动、跨视口拖选。
- `Label` `max_lines` 行数限制 + Truncate 省略号（End/Start/Middle）。

### Fixed
- `ScrollWidget` 滚轮滚动到边界时冒泡给外层容器。

## [0.2.0] - 2026-06-23

### Added
- 跨线程 UI 更新：`App::channel::<Msg>(on_message) -> Sender<Msg>`（后台 `send` 事件驱动唤醒 UI、`on_message` 在 UI 线程写状态）+ `App::on_interval(dur, cb)` 定时回调。有更新才重绘、空闲零 CPU。
- 语义意图色（Intent）体系：Button / CheckBox 统一 `.intent()` / `.danger()` / `.neutral()` / `.accent(color)`；
  内置 primary/neutral/danger，`Custom(Color)` 为扩展点——单基色自动派生 hover/active + 对比自适应前景。
  Button 默认 Primary（现有代码零改动）；CheckBox 现有 `.danger()`/`.accent()` 收编进同一体系（API 不变）。
- CheckBox 受控点击拦截：`Element::checkbox(..).on_toggle(cb)`——设回调后点击/键盘激活不自动翻转
  绑定 state，交 app 决定是否翻转（可在翻转前弹确认、确认后再置真，渲染跟随 state，零闪烁）。
- `Color::lighten` / `darken` / `pick_fg`（对比自适应前景）颜色派生工具。
- 彩色 emoji 渲染：DirectWrite 字形经 `IDWriteFactory2::TranslateColorGlyphRun`
  拆成 COLR/CPAL 彩色层逐层着色（emoji、ZWJ 组合序列、肤色修饰均正确合成彩色），
  字体无彩色数据时自动回退原单色路径。新增 `examples/emoji.rs` 演示。

### Fixed
- 文本框无法输入 emoji：WM_CHAR 对补充平面字符（码点 > U+FFFF，如 emoji）
  分两条消息发来 UTF-16 代理对，原逻辑对单个代理项解码失败而丢弃。现正确
  暂存高代理项并与低代理项合成为单个 `char`，emoji 及 CJK 扩展区字符可正常输入。

## [0.1.0] - 2026-06-22

首个公开版本（Windows + macOS）。

### Added
- 核心框架：命令式 Builder API、retained 模式、DPI 感知、tiny-skia 渲染。
- 完整控件集（布局/文本/按钮/表单/容器/列表/图片/导航）、系统托盘、无边框窗口、触摸滚动、自动截屏。
- Windows 平台后端（Win32 + GDI + DirectWrite 文字）。
- macOS 平台后端（Cocoa/AppKit 窗口 + Core Text 文字 + NSPasteboard 剪贴板 + NSStatusItem 托盘）。
- 跨平台缝合层：渲染/控件/事件平台无关，平台仅实现「窗口+事件循环」与「文字引擎」两条缝。
- 开源配套：双许可（MIT OR Apache-2.0）、DCO、贡献指南、开发指南、issue/PR 模板、CI、发布工作流。

### Changed
- 依赖按 target 门控：`windows` 仅 Windows、`objc2` 系列仅 macOS。
- README 改为跨平台说明（中文主 + 英文副）。
- 依赖更新：`toml` 0.8 → 1.1；CI actions（checkout v7、action-gh-release v3）。
- **windows-rs 0.58 → 0.62 迁移**：`implement` 宏改由 `windows-core` 提供；可空句柄参数
  语义化为 `Option<T>`；`BOOL` 迁至 `windows::core`；COM 实现入参 `Option<&T>` → `Ref<'_, T>`。

[Unreleased]: https://github.com/huanfeng/wind-ui-rust/compare/v0.11.1...HEAD
[0.11.1]: https://github.com/huanfeng/wind-ui-rust/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.8.3...v0.9.0
[0.8.3]: https://github.com/huanfeng/wind-ui-rust/compare/v0.8.2...v0.8.3
[0.8.2]: https://github.com/huanfeng/wind-ui-rust/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/huanfeng/wind-ui-rust/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/huanfeng/wind-ui-rust/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/huanfeng/wind-ui-rust/releases/tag/v0.1.0
