# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [0.12.0] - 2026-08-14

### Added
- **`testing` 模块：下游终于测得了收 `&mut EventCtx` 的回调**。`testing::run_with_ctx(f)`
  借一棵最小树的 ctx 跑 `f`，把它请求的副作用（toast、对话框、关窗、菜单、URL、窗口操作、
  重绘）按 `DispatchResult` 交回；`run_with_ctx_in(&mut tree, id, f)` 用调用方自己的树，
  回调改到树上的东西跑完仍可断言、节点几何也是布局后的真值。
  本版把回调普遍改成收 ctx（菜单动作、`App::channel` 的 on_message、`on_close_request`）之后，
  下游就再也造不出这个参数了——`EventCtx` 字段私有、`Tree::run_detached` 是 `pub(crate)`，
  于是"点这一项确实弹了 toast"这类断言只能退化成"把回调体抽成不收 ctx 的具名函数、再断言
  那个函数"：测的是抽出来的那一半，回调本身有没有接对反而没人管。这个退化在下游已经发生过
  两次，是本模块存在的直接原因。
  `run_detached` 保持 `pub(crate)` 不变：ctx 借的是宿主对控件树的可变访问，把内部入口直接
  放开等于敞开这条借用契约；包一层只暴露"跑一段闭包、收回副作用"这个受控形状。
- **表单行限行 `FormTheme::label_max_lines` / `desc_max_lines`**：设了就同时启用**末尾省略**
  与**悬浮看全文**，`None`（默认）保持原样按内容换行。
  设置页的说明文字长度常由后端数据决定：真实数据里有四分之一的说明超出左列单行宽度，
  不限行就会把行撑成三四行，同一列的行高从此参差，末尾还会被卡片边缘裁掉半截。而
  `setting_row_desc` 返回的是**拼好的容器**，调用方够不到内部那个 label——`.max_lines()`
  加在返回值上只会打到容器身上，这条路只能由主题这一侧给。
  截断与 tooltip 绑成一件事而非两个开关：截断意味着信息不完整，tooltip 是它唯一的兜底，
  拆开只会让人漏配后一半、把说明文字直接丢掉。短文本不会因此多出一个与可见文字相同的
  提示——`Tree::node_tooltip` 按 `Label::text_truncated()` 门控，没真截断就不弹。
  含换行的文本仍限行但**跳过 tooltip**：`Element::tooltip` 只收单行、多行会 `debug_assert`
  拦下，而这里的提示是库替调用方加的，不该由它引爆；调用方显式传多行才是该被拦的误用。
- **`Truncate` 补 `Debug`**：断言里 `assert_eq!(tr, Truncate::End)` 此前编译不过。

- **动态文案 `TextContent`：控件文案可直接绑 `Signal<String>`**。此前全库只有
  `Element::label_signal` 一条路径能让文字跟随状态，`button`/`link`/`badge`/`checkbox` 等
  的文案都是构建后不可变的 `impl Into<String>`——于是"切换类按钮"（播放/暂停、展开/收起、
  隐藏已完成/显示全部）这个最常见的需求根本做不了。`examples/dyn_list.rs` 的头部注释写着
  两个按钮会在「按名称排序 / 按优先级排序」之间切换，实现却是写死的固定文案，说明与代码
  对不上正是因为框架给不了。
  改法不是给每个控件再加一个 `_signal` 构造器，而是把动态性下沉到**参数类型**：所有单条
  文案参数从 `impl Into<String>` 放宽为 `impl Into<TextContent>`，`&str` / `String` /
  `Signal<String>` 走同一个构造器。规则因此收敛成一句话——**凡是接受一段文案的参数都可以
  传 `Signal<String>`**，不必记哪个控件有孪生构造器；`_signal` 后缀继续只用于参数类型确实
  不同的场景（`list_signal` 收 `Signal<Vec<T>>`）。选它而不选修饰符方案（`.text_signal(s)`）
  是因为绑错类型在**编译期**就过不去，不像修饰符只能靠 `debug_assert` 运行期喊停，而且
  `button("占位").text_signal(s)` 里的占位串是个永不显示的死参数。
  覆盖 `label` / `button` / `link` / `badge` / `badge_intent` / `checkbox` / `radio` /
  `nav_row` / `icon_button`（图标按钮绑信号即"图标随状态换"）。文案在每次 measure 时现取，
  所以换字会**重新测量宽度**而不是被旧尺寸裁掉：点击回调里写信号已由核心升级为
  `DamageReq::Layout` 级失效，整窗帧必先 `layout_root`。
- **表单脚手架 `Element::field` / `setting_row` / `setting_row_desc` / `card`**：「标签 + 控件」
  的一行和「标题 + 内容」的卡片此前在 6 个示例里各写了一遍，实现几乎逐字相同，只差行高与
  标签宽度——正是这个库自称的核心场景（做小工具）里重复最多的样板。
  `field` 是固定标签列 + 紧随其后的控件（表单感，控件左缘对齐成一条竖线），`setting_row`
  把控件贴到行右缘（设置页感），二者的差别只有这一点；两者都**定高**，一列行才对得齐。
  `setting_row_desc` 在标签下加一行弱化说明，是唯一**不定高**的一种——副标题长短不一，
  定高只会把它挤出去，故改由上下内边距撑开。副标题做成独立构造器而非 `Option` 参数：
  `Option<impl Into<String>>` 的 `None` 推断不出类型，退成 `Option<&str>` 又会让这一个参数
  破例不收 `String`，且占多数的无副标题行还得白写一个 `None`。
  行高/标签列宽/间距/字号一律走新的 `FormTheme` 与 `CardTheme` 覆盖层，**不进签名**：
  一个应用里的表单行必须整齐划一，逐行传尺寸只会让每处各写一个近似值。
  这几个构造器返回的是拼好的容器而非挂了 widget 的控件，故只接受容器/样式类修饰符，
  控件专属修饰符（`.intent()` / `.small()` / `.outline()`）须加在传入的 `control` 上——
  rustdoc 与 `docs/API_GUIDE.md` 均明确列出了可用与不可用的两类（`badge`/`chip`/`grid`/
  `dialog_panel` 同属此类，此前从签名上看不出来）。
- **`FormTheme` / `CardTheme` 主题覆盖层**：表单行的行高、标签列宽、水平间距、标签字号字重、
  副标题字号、带副标题行的上下内边距；卡片的圆角、内边距、元素间距、标题字号字重。
  全部 `Option` 回退 palette/metrics 并接入 TOML。卡片刻意**不含底色槽**——底色走
  `Role::Surface` 延迟解析才能在运行期换主题时跟着变，个别卡片换色链 `.bg_role(..)` 即可。
- **语义色角色补全 `Role::Success` / `Role::Warning`**：此前只有 `Danger` 一个语义色，
  优先级三色、状态圆点这类需求只能留硬编码。新增 `palette.success` / `palette.warning`
  两个色槽，亮暗两套各给取值，并同步扩展 `Intent::Success` / `Intent::Warning`——
  与 `Danger` 一样是「一个色槽、两条访问路径」（`Role` 供直接上色、`Intent` 供控件语义），
  不是第二套并行体系。取值刻意保证对表面 ≥ 3:1（有单元测试锁住）：语义色经常直接当**前景**
  用（状态文字、标签边框），而饱和亮黄在浅色表面上只有约 1.9:1，字一上去就糊。
- **三级弱化文字 `Role::TextSubtle`**：比 `TextMuted` 更淡但仍是可读正文（版权行、脚注、
  时间戳）。此前这类文字只能压成 `TextMuted`，正文的视觉层级从三档降成两档；借
  `TextDisabled` 会把可读内容说成不可交互，借 `Placeholder` 则暗示「待填写」，语义都不对。
  四档文字色的强弱顺序由单元测试锁住，防止后续调色把层级调反。
- **反色表面 `Role::SurfaceInverse` / `Role::OnSurfaceInverse`**：与当前主题**明暗相反**的
  实底条块及其前景（亮色主题下是深色横幅，暗色主题下翻成浅色）。取值即本主题的
  `text` / `bg`——正文色天生就是「在 bg 上对比最强的那一档」，用它当反色底能保证与页面其余
  部分同属一套色相。注意「不论主题都恒为深色」的标题栏是一个固定设计而非角色，
  仍应写死颜色（`examples/frameless.rs` / `light_titlebar.rs` 即刻意如此，已补注释说明）。
- **表格行右键菜单 `Element::on_row_context_menu`**：右击数据行时按行下标现取现建菜单项并
  弹级联浮层，返回空 `Vec` 则不弹。三类表格（`table_sortable` / `table_sortable_server` /
  `table_selectable`）均支持——右键与首列复选框不争语义（复选框只吃左键），故不像整行双击
  激活那样把可选表格排除在外。菜单**每次右击重建**，`check` / `enabled` 才能反映
  右击当刻的数据；回调挂在行容器上，行内空白、自定义单元格、操作列上右击都能弹。
  表格行是控件内部构建的，应用侧拿不到行 `Element`，故这条接线只能由框架提供。
- **菜单项里也能做阻塞式原生调用**：右键菜单里的"导出到文件…"这类项此前**无法表达**——
  动作是无参 `Fn()`，执行时虽已不在控件的 `on_event` 里、却仍在平台消息回调栈内，直接同步弹
  原生模态框会与对话框自身的消息泵冲突。现在菜单动作收 `&mut EventCtx`（见 Changed 段），
  写 `ctx.defer_blocking(f)` 即可把流程排到事件分发**完全返回**之后执行。
  本轮曾先落地一个无 `ctx` 的自由函数 `app::defer_blocking` 作为过渡，同版内即因根因被修掉而
  标记废弃；其配套的 `app::take_deferred` 保留：复用既有的 `DialogRequest::Custom` 通道交付
  （平台已在正确时机轮询它），`AppHandler::take_dialog_request` 的默认实现即取该队列，
  自定义 handler 覆盖时记得回退到它。
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
- **菜单项语义色 `MenuItem::danger()` / `MenuItem::intent(Intent)`**：`Danger` 把标签染成
  `palette.danger`，用于「删除 / 清空」这类不可逆项——菜单里所有项长得一样时，破坏性操作与
  「复制」只差一个词的距离。取色优先级：禁用 > intent > 悬停/勾选 > 常规（不可点的项不该还在
  喊危险；危险项被指向时更要保持红，而不是变成中性的强调色）。
- **`MenuTheme` 新增 `shadow_dy` / `shadow_blur` / `shadow_color`**：菜单浮层投影可按环境覆盖，
  `Option` 回退内置默认并接入 TOML（`#[serde(default)]`，旧 TOML 无需改动）。
- **信号槽位回收：`signal::SignalScope` + `Signal::dispose` / `try_get` / `try_with` /
  `is_alive` + `signal::stats`**。运行时 arena 此前只增不减（`free` 空闲链没有任何地方
  `push`、`generation` 从不自增），创建过的每个槽位随线程活到进程退出。
  所有权模型定为**两级**，而不是 leptos / floem 那样贯穿全库的隐式作用域树：作用域之外
  创建的信号**默认无主、永不回收**（`main` 里的应用状态活到退出就是正确语义，回收它们没有
  意义），只有 `SignalScope::collect(..)` 内创建的才归属该作用域、可整批回收。本库是保留
  模式的控件树而非响应式图，没有一棵现成的所有权树可挂；隐式回收还会让"谁杀了我的信号"
  不可追溯——菜单动作闭包、toast 回调、`App::channel` 的消息处理器都能合法地比控件节点
  活得久。显式 `collect` 把边界写在代码里。
  `Signal<T>` 仍是 `Copy`：回收的单位是**槽位**而非句柄，所以不需要给句柄加 `Drop`
  做引用计数（`Copy` 出去的副本互不知情，本来也做不到）。调用方不必回到"每个闭包前先
  clone 一遍"。
  `stats()` 返回 `{ live, free, capacity, peak }`；环境变量 `WINDUI_SIGNALS=1` 让活跃槽位
  每创下新高就往 stderr 打一行，健康的应用启动后即安静，泄漏表现为持续刷屏（值即报告
  步长，嫌吵调大；`0` 或不设即关闭）。
- **意图色的角色变体 `Intent::CustomRole(Role)` / `Element::accent_role`**：`Intent::Custom`
  收的是构建期给定的**定色**，运行期换主题不跟随，于是"想拿 palette 里内置意图之外的色槽
  当基色"这件事只能退回硬编码——`examples/multiline_demo.rs` 的 `const ACCENT = 0x4C8BF5`
  正是这么来的，它一字不差就是默认主题的 `palette.accent`，换暗色主题后按钮还是那个亮蓝。
  新变体把基色的解析推迟到 paint 期（口径同 `Brush::Role`：从当前线程活动主题取，
  故 `Role::InputBg` 这类落在覆盖层上的角色也解得出），派生规则与 `Custom` 完全共用。
  命名跟随既有成对约定 `fg` / `fg_role`，故是 `accent` / `accent_role`，Button 与 CheckBox
  通用；`badge_intent` 下 `CustomRole` 也与内置意图同路走 `bg_role_alpha` + `fg_role`。
  `examples/multiline_demo.rs` 已迁到 `.accent_role(Role::Accent)`，常量删除。

### Changed
- **`App::accelerated(bool)` 换成 `App::renderer(Renderer)`（破坏性）**。布尔开关只能表达
  "要不要试 GPU"，答不了"拿不到 GPU 该怎么办"——而这两件事需要分开：

  | 变体 | 行为 | 用途 |
  |---|---|---|
  | `Renderer::Auto` | GPU 优先，建不起来自动回退软光栅 | 发布给最终用户 |
  | `Renderer::Software`（默认） | 强制软光栅 | 内存敏感场景 |
  | `Renderer::Gpu` | 强制 GPU，拿不到就**报错终止** | 测试与排障 |

  `Gpu` 之所以报错而非回退，是因为它的用途就是"拿不到 GPU 要告诉我"。静默换一条路会让
  基于它的验证失去意义——两张软渲染的截图看起来当然一致。这也是它与 `Auto` 唯一的区别。
  迁移：`accelerated(true)` → `renderer(Renderer::Auto)`；`accelerated(false)` 即默认，可直接删去。
  命令行新增 `--renderer auto|software|gpu`，`--accelerated` 作为等价旧写法保留。
  **默认仍是软光栅**：GPU 路径的验证还在补齐（本版从零测试补到 13 条），默认切换留待后续版本。
- **`EventCtx::request_close` 改为走关闭决策链，新增 `force_close` 跳过它（破坏性）**。
  此前 `request_close()` 的语义是"应用已决定关闭"——直接落地，不问 `on_close_request`、
  也不先关最顶层对话框。问题在于**无边框窗口的 × 是自绘控件**
  （`Element::window_button(WindowButtonKind::Close)`），它走的正是这条路：于是
  `on_close_request` 拦得住 Alt+F4、ESC、系统 ×，唯独拦不住用户最常点的那个 ×。
  下游的报告是"改了内容点 × 直接丢失、按 Alt+F4 才提示"——守卫在 frameless 应用里
  基本等于没有。同一个"关闭窗口"的意图，来源不同却走两条路、守卫只覆盖一半，是设计漏洞。
  现在 `request_close()` = **请求**关闭（关顶层对话框 → 问 `on_close_request` →
  `hide_on_close`），与系统 × 完全同路；确实已经决定要关的场合改用 `force_close()`。
  取"默认过守卫、绕过要显式"这个方向，是因为绝大多数调用点是按钮点击（用户意图），
  而真正"应用已决定"的只有安装器要求退出、确认框里已选过"直接退出"这么几处——
  后者若不绕过会死锁：安装器等窗口关、窗口等用户回答。
  **迁移**：调用点若是"用户点了某个关闭按钮"，不动即可（自动获得守卫）；若是"程序已经
  决定关闭"，改成 `force_close()`。未设 `on_close_request` 的应用不受影响。
  `on_close_request` 回调内再调 `request_close()` 无效（宿主忽略以免自我递归）。
- **`setting_row_desc` 的说明文字改用 `Role::TextSubtle`**（原 `TextMuted`，视觉变更）。
  行标题已是正文档，说明再压一档才拉得开层次；`TextMuted` 是**次级正文**的档位，用在这里
  两行字仍显得同重。本版新增 `TextSubtle` 时讲的正是"版权行、脚注这类比 muted 更淡但仍
  可读"的场景，而库自己这处最典型的用例没跟上。四档文字色的强弱顺序由单测锁着。
- **`ui::DynLabel` 并入 `ui::Label`**（保留为 `#[deprecated]` 类型别名，`DynLabel::new(sig)`
  仍能编译）。它本是 `Label` 的逐行复制——换行、`max_lines` 裁剪、上百行的截断算法各存了
  两份，改一处得记得改两处；文案的动态性交给字段类型 `TextContent` 之后，这个孪生类型就
  没有存在理由了。附带好处：`.max_lines()` / `.truncate()` 不必再"先试 `Label` 再试
  `DynLabel`"两次 downcast，误用提示也只剩一条。`Element::label_signal` 不变。
- **`App::theme(t)` 现在**当场**把主题装进当前线程**，而不是等到 `run()`。一部分组合子
  （`Element::field` / `card` / `badge` / `chip` / `tag_field` / `dialog_panel`）在**构造期**
  就要读主题定尺寸和颜色；此前主题要到 `run()` 才装，这些构造器读到的一律是默认主题，
  自定义主题里的行高、圆角、徽章色会**静默失效**——编译通过、不报错、只是没生效。
  链式写法（`App::new(..).theme(t).content(build_ui())`）因此自动正确；若先把树建进变量
  再传，请把建树挪到 `.theme(t)` 之后（`examples/ime.rs` / `settings.rs` / `ime_settings.rs`
  已照此调整）。
- **`Role` 与 `Intent` 各新增变体（破坏性）**：下游对它们做穷举 `match` 的代码需补分支。
  `Role` 加了 `SurfaceInverse` / `OnSurfaceInverse` / `TextSubtle` / `Success` / `Warning`，
  `Intent` 加了 `Success` / `Warning`；`Palette` 相应新增五个色槽（`#[serde(default)]`，
  旧 TOML 无需改动）。两者本版同时加了 `#[non_exhaustive]`（见下文），**这是最后一次**
  因新增语义色而破坏下游。
- **`MenuItem` 新增 `pub intent: Option<Intent>` 字段（破坏性）**：下游用**字面量构造**或
  穷尽解构 `MenuItem` 的代码会 `E0063` / `E0027`。
  迁移：改用 `MenuItem::run` / `key` / `separator` / `submenu` 四个便捷构造加链式设置器
  （它们已收敛到共同底座）。本版同时给 `MenuItem` 加了 `#[non_exhaustive]`（见下文），
  字面量构造这条路已被封死，**这是最后一次**因新增字段而破坏下游。
- **菜单浮层投影默认值收敛**：偏移 6 → 3、模糊半径 18 → 9、黑色不透明度 43% → 22%，与
  `ReorderTheme` 既有的投影分量对齐（alpha 同为 56）。投影是用来把浮层从背景里托起来的，
  不该成为画面里最显眼的东西；大面积低对比渐变又正是远程桌面这类有损通道最先牺牲的部分。
  需要旧观感的用 `MenuTheme` 的三个投影字段覆盖。
- **toast 投影与菜单对齐，并可主题化**：toast 此前硬编码偏移 6 / 模糊 22 / 黑 35%，菜单收敛后
  它反而成了画面里更重的那个。两者都是浮在内容之上的临时面板，投影只负责把它们从背景里托起来，
  没有理由一个比另一个重。改为走新增的 `ToastTheme::shadow_dy` / `shadow_blur` / `shadow_color`
  （`Option` 回退，默认值与 `MenuTheme` 相同，接入 TOML）。淡入淡出仍按 alpha 缩放。
- **菜单分隔线自动规范化**：所有层级（根层与各级子菜单，弹出与刷新两条路径）在构建时统一
  去掉首尾分隔线、把相邻的折叠成一条。菜单项按条件生成、分组线无条件写下是最自然的写法，
  整组为空时旧行为会留下孤立的线。副作用：**有意**写在菜单首尾的分隔线现在会被移除。
- **`on_context_menu` 的构建器改收 `Fn`（原 `FnMut`），并交宿主兼任菜单重建器**：
  粘滞项（`MenuItem::stay_open`，即右键菜单里的复选开关）点击后菜单不关，此前
  `MenuRequest::rebuild` 恒为 `None`，勾选态刷不了——勾了也不变，看着像没生效。
  同一个构建器要留两份（弹出时建项、粘滞项点击后重建），`FnMut` 独占交不出第二份，
  故 `core::MenuFn` 由 `Box<dyn FnMut>` 改为 `Rc<dyn Fn>`。
  迁移：构建器捕获的可变状态改放 `Cell`/`RefCell`/`Signal`（本来也该如此——它每次右击都跑）。
- **`Signal<T>` 改为 `!Send` + `!Sync`（破坏性）**：此前它的字段全是 `Send`，于是自动实现了
  `Send`，而运行时存储是 thread_local——句柄 move 进别的线程后 `set()`，查不到 slot 就走空
  分支，不 panic 不报错、值直接丢掉，UI 侧毫无线索。这是从 `Rc<Cell<T>>` 迁到 `Signal` 时
  丢掉的编译期保护（`Rc` 是 `!Send`，同样的错误代码在旧模型下根本编译不过）。
  加零大小的 `PhantomData<*const ()>` 负标记补回，`Copy`/型变/8 字节布局均不受影响。
  迁移：跨线程更新状态走 `App::channel` 拿 `Sender<Msg>`（`Msg: Send`，可 move 进工作线程），
  在 UI 线程执行的 `on_message` 回调里写信号——回调本身不要求 `Send`，故能捕获信号。
  因此报错的下游代码本就在静默丢值，编译失败是把运行期的哑故障提前到了编译期。
  另：`set`/`update` 在 slot 缺失时补 `debug_assert`，与 `with`/`get` 的 panic 对齐，
  消除"读会炸、写静默"的分裂（当前 slot 永不回收，该分支不可达，是留给日后释放作用域的护栏）。
- **绑信号的构造统一 `_signal` 后缀**：`Element::label_rc` → `label_signal`、`rich_rc` → `rich_signal`、
  `dropdown_reactive` → `dropdown_signal`、`dropdown_items_reactive` → `dropdown_items_signal`。
  同一个概念此前有三套后缀：`_rc` 是 `Rc<Cell<T>>` 时代的残留，参数早已换成 `Signal<T>`（`Copy` 句柄），
  名字却还在暗示"要先包一层 `Rc`"，读文档的人因此绕开这些构造、退回手写 `Rc<RefCell<..>>`；
  `_reactive` 则与 `Element::reactive()`（把节点标记为响应式，供自定义控件用）撞概念，
  一个是"绑什么数据"、一个是"节点怎么刷新"，二者出现在同一份补全列表里无从分辨。
  既有的 `list_signal` / `host_signal` / `reorder_list_signal` 就是命名基准，本次向它们收敛。
  **迁移**：旧名保留为 `#[deprecated]` 转发别名（计划 0.13 移除），编译告警会逐处指出新名，
  照着改即可，签名与行为完全不变。
- **运行期句柄的获取与操作对齐**：`App::hotkey_rc` → `App::hotkey_handle`，与既有的 `App::theme_handle`
  同名式；`HotkeyHandle::rebind` → `HotkeyHandle::set`，与 `ThemeHandle::set` / `Signal::set` 同名式。
  改名的动因是 `_rc` 一个后缀被用出了两种意思——`label_rc` 是"绑信号"，`hotkey_rc` 是"返回句柄"，
  同一份 API 里同一个词指两件事，比起名不好更糟。句柄类现在统一是"`*_handle` 拿到、`set` 改值、
  `set_enabled` 启停"，学会一个即会用另一个。
  **迁移**：旧名保留为 `#[deprecated]` 转发别名（计划 0.13 移除）；`rebind(hk)` 直接换成 `set(hk)`，
  语义未变（下一次消息循环生效，失败回滚保留旧绑定）。
- **`MenuItem` builder 去掉 `with_` 前缀**：`with_icon`/`with_shortcut`/`with_check`/`with_subtitle`/
  `with_badge`/`with_trailing_icon`/`with_enabled` → `icon`/`shortcut`/`check`/`subtitle`/
  `badge`/`trailing_icon`/`enabled`。同类 builder 此前两套风格并存——`MenuItem` 全套 `with_*`，
  而 `DropdownItem` / `CheckMenuItem` 全套无前缀，混用同一个菜单时要来回切换习惯。
  取无前缀一侧是因为 Rust 生态里 `with_*` 通常表示"带着某配置构造"（如 `Vec::with_capacity`），
  用在链式设属性上是误导；`MenuItem::stay_open()` 本就没有前缀，也印证了这一侧才是本意。
  方法名与同名 `pub` 字段（`icon`/`enabled` 等）共存合法且无歧义：`self.icon` 取字段、
  `item.icon(..)` 调方法，`stay_open` 字段与方法早已如此共存。
  **迁移**：旧名保留为 `#[deprecated]` 转发别名（计划 0.13 移除），去掉 `with_` 前缀即可。
- **启用 / 可见两条轴的形态对称化（部分破坏性）**：这两条轴表达的是同一件事——"这个节点这一帧
  算不算数"，此前却各长各的。`enabled` 收 `Signal<bool>`、`disabled` 收 `bool`、`visible` 收 `bool`
  而没有信号版，于是"下一个该传什么"无从预测：想按信号显隐只能退回 `visible_when(move || s.get())`
  绕一圈，而 `enabled(true)` 这种最直觉的写法根本不存在。现在每条轴一律三形态、命名规则相同：

  | 轴 | 静态 | 信号 | 闭包 |
  |---|---|---|---|
  | 启用 | `enabled(bool)` | `enabled_signal(Signal<bool>)` | `enabled_when(\|\| ..)` |
  | 可见 | `visible(bool)` | `visible_signal(Signal<bool>)` | `visible_when(\|\| ..)` |

  三形态可叠加，取与；`disabled(bool)` 保留为 `enabled(!v)` 的取反便捷式（调用点常读作
  "这个按钮是禁用的"）。`_signal` 后缀沿用本版第一批定下的命名基准。
  **迁移（硬破坏，无法用 `#[deprecated]` 别名过渡——同名函数不能按参数类型重载）**：
  `enabled(sig)` → `enabled_signal(sig)`。旧调用点会在编译期报 `E0308`（expected `bool`,
  found `Signal<bool>`），不会静默改变行为。其余形态与旧代码兼容。
- **`Element::disabled(true)` 不再泄漏信号槽**：旧实现是 `self.enabled = Some(signal(false))`——
  信号槽的回收尚未实现，于是每一次**常量**禁用都在全局 arena 里占掉一个永不释放的槽位；
  在按帧重建子树的场景（表格行、`list_signal`）里这是随时间线性增长的泄漏。改为落在新增的
  `Node::enabled_static: bool` 上，与可见轴的 `Node::visible` 一一对应，不分配任何东西。
  `Node` 同时新增 `vis_signal` 字段承载 `visible_signal`；两个新字段对**用字面量构造 `Node`**
  的下游是 `E0063`（该结构体字段全 `pub` 且非 `#[non_exhaustive]`），正常经 `Element::build`
  的用法不受影响。
- **`Element::accordion` 的选中模型改用 `Option`（破坏性）**：`Signal<i32>` 以 `-1` 当"全部收起"
  的哨兵，`-2`、`-7` 这些值则是未定义区——类型允许、语义没有。它的文档还自称与
  `Element::tabs` 的 `Signal<usize>` "同构"，而两者类型根本不同。改为 `Signal<Option<usize>>`：
  `None` = 全收起，`Some(i)` = 展开第 i 个，非法状态直接不可表示。与 `tabs` 的差别也因此说得清了
  ——标签页恒有一页选中，手风琴可以全收起，正好差一个 `Option`。
  `ExpandState::Single { sel }`（`pub`）随之改为同样的类型。
  **迁移（硬破坏，同上，参数类型变化无法用别名过渡）**：`signal(-1)` → `signal(None)`，
  `signal(0i32)` → `signal(Some(0usize))`；读取处 `sel.get() == 0` → `sel.get() == Some(0)`。
- **排序状态从裸元组提升为命名类型 `SortKey`**：`(usize, SortOrder)` 这个"哪一列 + 什么方向"的
  概念在四处公开签名里重复出现却没有名字，谁是列、谁是方向全靠位置约定，读代码时要回签名里数。
  新增 `pub struct SortKey { pub column: usize, pub order: SortOrder }`（`Copy + Eq + Debug`，
  已进 prelude），便捷构造 `SortKey::asc(col)` / `SortKey::desc(col)` / `SortKey::new(col, ord)`。
  `table_sortable` / `table_sortable_server` / `table_selectable` 的 `sort` 参数与
  `table_sortable_server` 的 `on_sort` 回调参数一并从 `Option<(usize, SortOrder)>` 换成
  `Option<SortKey>`。
  **迁移（硬破坏，类型变化）**：`Some((0, SortOrder::Asc))` → `Some(SortKey::asc(0))`；
  解构处 `Some((col, ord))` → `Some(SortKey { column: col, order: ord })`。
- **`ListRow` / `TabItem` / `TabBar` 去掉 `with_` 前缀**：`ListRow::with_icon` /
  `TabItem::with_icon` → `icon_content`，`TabBar::with_style` → `style`。理由同上一条
  `MenuItem` 的收敛（`with_*` 在 Rust 生态里表示"带某配置构造"而非链式设属性）。
  两个图标方法没有跟着叫 `icon`：它们收的是 `ImageContent`（图片/SVG/RGBA），而 `MenuItem::icon`
  收的是 `impl Into<String>`（字形/emoji）——同名不同义比带个旧前缀更容易踩。`_content` 后缀
  与既有的 `Element::icon_content` 对齐，同一份 API 里"图标给的是图片内容"始终是这个词。
  `Dropdown::with_items` / `with_items_reactive` **不在此列**：它们是真正的"带配置构造"，用法正确。
  **迁移**：旧名保留为 `#[deprecated]` 转发别名（计划 0.13 移除）。
- **回调签名立法：`&mut EventCtx` 一律作第一参数（硬破坏）**：此前同一个库里 ctx 的位置各行
  其是——`on_reorder(|ctx, from, to|)` 在前，`on_span_click(|id, ctx|)` 在后，用户每碰一个新
  回调都得翻文档。ctx 是"环境/能力袋"而非数据，位置直觉同 `&mut self`；固定在首位后，读签名时
  后面的参数才是这个回调真正关心的数据。
  同名函数无法重载，**没有 deprecated 别名可过渡**，是编译期硬失败（`E0631` 闭包参数类型不符）。
  **迁移**：`on_span_click(|id, ctx| ..)` → `on_span_click(|ctx, id| ..)`；
  自定义控件直接持有 `rich::SpanClickFn` 的同步改为 `Box<dyn FnMut(&mut EventCtx, &str)>`。
- **菜单项动作补上 `&mut EventCtx`（硬破坏）**：`MenuItem::run` 的动作闭包过去是无参 `Fn()`，
  于是库里存在"这个回调能弹对话框、那个不能"的分层——菜单里想写"导出到文件…"就没有 `ctx`，
  只能绕道自由函数 `app::defer_blocking`，文档还要专门开一节讲"三个入口"。
  宿主执行菜单动作时其实握着 `&mut Tree` 与发起菜单的目标节点，缺的只是一条借出 `EventCtx`
  的通道；新增内部的 `Tree::run_detached(id, f)` 补上，副作用（对话框、toast、关窗、焦点、
  嵌套菜单）汇总成 `DispatchResult` 走宿主既有的 `apply_dispatch_effects` 消费——与指针/键盘
  分发同一条路径，日后给 `DispatchResult` 加字段不会独独漏掉菜单这一路。
  动作仍是 `Fn` 而非 `FnMut`：项会被克隆进浮层的每一级面板、粘滞项还要在原地重建后再执行
  同一份动作，独占可变借用无处安放；要改状态用 `Signal`（`Copy` + 内部可变，正为此而设）。
  受影响的签名：`MenuAction::Run(Rc<dyn Fn()>)` → `Run(MenuActionFn)`（即
  `Rc<dyn Fn(&mut EventCtx)>`）、`MenuItem::run`、`MenuItem::trailing_icon`（及其废弃别名
  `with_trailing_icon`）、`MenuItem::on_trailing_click` 字段、`DropdownItem::trailing_icon`、
  `CheckMenuItem::action`、`CheckMenuItem::on_change`（新签名 `Fn(&mut EventCtx, bool)`，
  ctx 在前、新值在后）。
  **迁移**：`MenuItem::run("x", || f(), false)` → `MenuItem::run("x", |_ctx| f(), false)`；
  `CheckMenuItem::on_change(|v| ..)` → `on_change(|_ctx, v| ..)`；其余同理补一个前置参数。
  需要阻塞式原生调用的地方，把 `windui::app::defer_blocking(f)` 换成 `ctx.defer_blocking(f)`。
- **自由函数 `app::defer_blocking` 标记 `#[deprecated(since = "0.12.0")]`**：它存在的唯一理由
  是"菜单项动作拿不到 `ctx`"，上一条已经补上，改用 `ctx.defer_blocking(f)`。托盘菜单项另有
  自己的 `TrayCtx`。`app::take_deferred` 保留不变——已经写下的老代码那条队列仍需被排空。
  `docs/API_GUIDE.md` §8.6 的"三个入口"随之收敛为两个。
- **`on_row_activate` 由 `Fn` 放宽为 `FnMut`**：一次性动作回调统一 `FnMut`（用户常需在闭包里
  改捕获的状态），只有需要留存多份/反复调用的闭包才用 `Fn`。内部类型
  `OnRowActivate` 相应从 `Rc<dyn Fn(..)>` 改为 `Rc<RefCell<dyn FnMut(..)>>`，与既有的
  `OnSort` 同款。对下游是**放宽**：原本能传的闭包全都还能传，无需迁移。
- **`TrayMenuItem::check` 的勾选态改收 `Signal<bool>`（硬破坏）**：这是全库最后一处在公共
  签名里要 `Rc<Cell<bool>>` 的地方——同一个 `impl` 块里紧挨着的 `TrayMenuItem::enabled`
  早在 0.4.1 就是 `Signal<bool>` 了，于是同一份托盘菜单里"勾选绑这个、灰显绑那个"要维护
  两份同义状态，`examples/tray.rs` 也因此成了全库唯一还写 `Rc::new(Cell::new(..))` 的示例。
  语义完全不变：勾选态仍是**菜单弹出时现读**，回调里自行翻转，框架不代改。
  线程约束成立：`Signal` 的存储是线程局部的，而 `Signal` 的 `!Send` 使 `Tray` → `App` 一路
  `!Send`，`App::run` 只能在建 `Tray` 的那个线程消费它并在那里建窗口；Win32 保证窗口消息
  只由建窗线程派发（`build_menu` / `run_item` 都在 `wnd_proc` 内），macOS 侧 `pop_menu`
  另有 `MainThreadMarker` 钉在主线程。这条不变量已加编译期护栏（`Tray` 一旦变成 `Send`
  即编译失败）。
  **迁移**：`Rc::new(Cell::new(true))` → `signal(true)`，传参去掉 `.clone()`，
  回调里的 `.get()` / `.set(..)` 写法不变；同一个信号可以直接同时喂给 `check` 和 `enabled`。
- **`Role` 与 `Intent` 加 `#[non_exhaustive]`（破坏性）**：本版给这两个枚举分别加了五个和
  两个变体（见上文），而**每加一个都是下游的破坏性变更**——语义色恰恰是最会持续演进的
  那类枚举，这个代价没有尽头。标注之后再补变体就只是新增。
  `#[non_exhaustive]` 只约束**下游 crate**，本 crate 内部的 `match` 仍须穷尽（忘了给新角色
  接上 `Role::resolve` 会当场编译失败，这正是想要的）。`Intent::Custom(Color)` 其实早已让
  "把所有 intent 一一列举"失去意义（基色是无穷的），标注只是把这一点写进类型。
  **迁移**：下游对 `Role` / `Intent` 做穷尽 `match` 的代码报 `E0004`（non-exhaustive patterns），
  补一条 `_ => ..` 兜底分支即可；构造与比较不受影响。
- **`MenuItem` 加 `#[non_exhaustive]`（破坏性）**：字段全 `pub` 且此前无标注，本版已因它
  破坏过两次（加 `intent` 字段导致 `E0063`、`on_trailing_click` 换类型）。菜单项的可选修饰
  只会越来越多，故把字面量构造这条路封住，日后加字段不再波及调用方。
  一并补上 `MenuItem::trailing_icon_display(icon)`：字段文档里"有尾随图标但 `on_trailing_click`
  为 `None`（纯展示）"是个受支持的状态，而唯一的设置器 `trailing_icon(icon, on_click)` 必须
  同时收回调——这个字段组合此前只有字面量写得出来，封住字面量就等于把它变成不可达。
  自查过全部 14 个字段，其余都已被四个构造器 + 链式设置器覆盖。
  **迁移**：下游 `MenuItem { .. }` 报 `E0639`（cannot create non-exhaustive struct using
  struct expression），改用 `MenuItem::run` / `key` / `separator` / `submenu` 四个便捷构造
  加链式设置器；穷尽解构 `let MenuItem { label, .. } = it` 需补 `..`。**读字段不受影响**。
- **`DropdownItem` 与 `CheckMenuItem` 一并加 `#[non_exhaustive]`（破坏性）**：它们与
  `MenuItem` 常出现在同一份菜单代码里，只封住其中一个反而是新的记忆负担——用户得记住
  "这个能写字面量、那个不能"。菜单项类型都会随需求长新字段和新变体，一次封齐。
  `CheckMenuItem` 是枚举，故枚举本身与 `Check` / `Action` 两个变体都标了（前者禁止下游
  穷尽 `match`，后者禁止变体的字面量构造）。
  自查过字段覆盖：`DropdownItem` 的四个字段都有设置器（`trailing_icon` 的回调不是
  `Option`，不存在 `MenuItem` 那种"纯展示"缺口）；`CheckMenuItem` 的
  `check` / `action` / `separator` 三个构造器加 `on_change` / `enabled` 两个设置器覆盖全部字段。
  **迁移**：改用构造器 + 链式设置器；对 `CheckMenuItem` 的穷尽 `match` 需补 `_ =>` 分支。
- **`Element::icon(path)` 改名 `icon_file(path)`**：同一个 `Element` 上并列着
  `icon(文件路径)` / `icon_bytes` / `icon_rgba` / `icon_svg` / `icon_content`，最短的名字
  给了**唯一会碰文件系统、唯一可能失败**的那个形态（路径写错即无图标），最容易被当成
  "通用图标入口"误用。改名后 `icon` 这个词在本库里只表示"图标这个概念"，不特指某种来源。
  **迁移**：旧名保留为 `#[deprecated]` 转发别名（计划 0.13 移除），改叫 `icon_file` 即可，
  签名与行为不变。注意与 `MenuItem::icon` / `DropdownItem::icon` 无关，那两个收的是字形。
- **`MenuTheme::shadow()` / `ToastTheme::shadow()` 改返回 `style::Shadow`（硬破坏）**：
  它们原返回 `(f32, f32, Color)` 三元组，而同名的 `ReorderTheme::shadow()` 早就返回
  `Shadow`——同名不同型，调用方拿到哪个全凭记忆，解构位置写反了还是编译得过（两个都是
  `f32`）。统一到 `Shadow` 而非反过来，是因为它同时是节点样式 `style.shadow` 的类型：
  浮层投影与普通节点投影从此可以互相搬。`dx` / `spread` 在这两个浮层上恒为 0
  （菜单/toast 只需"正下方托一层"），保留分量是为了共用同一个结构。
  **迁移**：`let (dy, blur, col) = th.menu.shadow();` → `let sh = th.menu.shadow();`，
  取 `sh.dy` / `sh.blur` / `sh.color`。`MenuTheme` / `ToastTheme` 的三个 `shadow_*` 字段
  与 TOML 键不变。
- **App 级回调收 `&mut EventCtx`（破坏性）**：`App::on_close_request`、`App::channel` 的
  `on_message`、`App::on_interval` 的回调三者都多一个 `ctx` 参数。
  `EventCtx` 是控件通往宿主能力的唯一通道（toast、对话框、剪贴板、焦点、窗口显隐、
  `tree_mut` 改控件树），而这三个回调恰恰处在最需要它的时机上——后台任务完成要弹一条轻提示、
  定时器到点要关窗、关闭请求要拦下来问一句"确认退出吗"。此前它们只能写信号，而 toast 是
  **宿主浮层**、关窗与对话框是**宿主能力**，都没有信号可绑：这些场景不是难写，是**表达不出来**，
  于是只能绕道已废弃的自由函数 `app::defer_blocking`，或干脆放弃。
  ctx 的 `self_id` 取**根节点**（这些回调不属于任何控件），后果写进了各自的 rustdoc：
  `ctx.bounds()` 是整个客户区、`mark_dirty()` 相当于整窗失效、`capture()` 无效
  （没有指针事件可捕获，请求被丢弃）。
  `on_close_request` **保留 `-> bool`**：它的返回值被平台在 `WM_CLOSE` /
  `windowShouldClose:` 里同步等待，改成任何"异步决定"的形状都骗不过这个约束。
  弹确认框的正确流程是「返回 `false` 挡下这一次 + 另起一条路把确认送回来」，两种写法
  （应用内 `Element::dialog`，或 `ctx.defer_blocking` + `App::channel` 回程）都带可运行示例
  写进了 `on_close_request` 的 rustdoc 与 `docs/API_GUIDE.md` §8.7。
  配套修好了副作用的交付时机：`on_message` / `on_interval` 的回调产生在**帧内**，而窗口操作、
  对话框请求、关窗意图此前只在指针/键盘路径被消费，于是"后台任务完成后 `ctx.request_close()`"
  要拖到用户下一次点鼠标才生效；win32 的帧路径（`WM_PAINT`）与 `WM_CLOSE` 取消分支现在也消费
  这几条意图。macOS 侧对应的消费点尚未补齐（toast、改树、焦点等不经平台的效果两端一致）。
  **迁移**：闭包各加一个参数即可，不用 ctx 就写 `|_ctx, msg|` / `|_ctx|`；行为不变。
  `App::single_instance` 的回调**刻意不动**（仍是 `FnMut(Vec<String>)`）：它不由主窗口驱动
  （Windows 上跑在独立 message-only 窗口的 `wndproc` 里、可能在任意嵌套消息泵中到达），
  够不着宿主状态；需要 ctx 的话在应用侧用 `App::channel` 转一道即可，rustdoc 给了写法与代价。

### Fixed
- **GPU 路径下非正方形矩形上的斜向线性渐变，轴角度不对**：线性画刷原先一律在单位空间
  `[0,1]²` 构造、再用 `SetTransform(diag(w,h))` 映射到目标矩形（好处是与位置/尺寸无关，
  一条缓存跨所有控件复用）。但非等比缩放**不保持垂直关系**：单位空间里轴 `(1,1)` 的等色线
  方向是 `(-1,1)`，被缩放后成了 `(-w,h)`，而正确的等色线应垂直于变换后的轴 `(w,h)`、
  即 `(-h,w)`——两者只在 `w == h` 时相等。
  于是 100×70 的矩形上画 `(0,0)→(1,1)` 的渐变，GPU 的轴角是 45° 而非正确的 35°：右上角与
  左下角都取到 t=0.5，软路径则分别是 0.671 / 0.329，最大通道差 43。
  修法只对**斜轴**渐变改用绝对坐标构造（缓存键随之带上绝对端点）；轴平行于坐标轴的渐变
  等色线也平行于另一条坐标轴，非等比缩放不改变正交关系，仍走原来的位置无关复用——绝大多数
  UI 渐变是水平或垂直的，那条性能路径零回归。修复后最大通道差降到 1。
- **鼠标在两个文本框之间点击后，旧框的光标竖条残留**：焦点转移时旧焦点收不到本次事件，
  脏区里只有被点中的那个控件，于是局部重绘只画了新框——新框的光标出现，旧框的光标仍留在
  后备缓冲里，要等下一次全窗刷新（改窗口大小、切主题等）才消失。
  三条焦点路径里只有这一条漏了：Tab 切换与点击空白清焦点都已置 `needs_full`，唯独"鼠标点到
  另一个可聚焦控件"没有。在 macOS 上实测发现，但成因与平台无关，Windows 同样复现。
  修法取整窗而非把旧节点矩形并进脏区：焦点环画在节点框外 1px，而此刻旧节点的 `focused` 已
  置 false，按它的脏区走会残留一圈——与点击空白清焦点那条分支同理。焦点转移是人手点击的
  频率，一帧整窗换取正确性划算。
- **GPU（Direct2D）路径下 `opacity` 子树里的文字整段消失**：ClearType 是子像素渲染，需要字形
  背后的真实底色才能做三通道混合；`PushLayer` 开的却是一张透明离屏层，层内无底色可依。D2D
  对此的应对不是降级成灰度，而是**整段文字不画**，且 `EndDraw` 不报错——于是软路径 551 个墨
  像素的一段字，GPU 路径上是 0 个，静默消失。
  改为按层深度推导抗锯齿模式：层内恒 GRAYSCALE、层外恒 ClearType。之所以不在 `draw_text` 里
  临时切换再恢复，是因为"恢复"只能写死成 ClearType 而非"进入前的模式"——半透明文字画在
  `opacity` 子树内部时（层套层），这个动作会把外层仍需的 GRAYSCALE 打回去，此后该层内所有
  文字一并消失。由状态推导就不存在"该恢复成什么"。
- **GPU 路径下半透明文字比软路径淡一半**：`ENABLE_COLOR_FONT` 路径（彩色 emoji）绕过 brush
  alpha，故 alpha 由合成层承载；但普通文字的 brush alpha 是**生效**的，两处各乘一次即双重
  削弱。实测半透明黑字峰值墨度 192，软路径 384，正好一半。toast 淡入淡出走的正是这条路
  （`scale_alpha` 把 alpha 乘进文字颜色），淡出时文字比应有的更淡。
  改为 alpha 全部交给合成层、brush 只管颜色——这是对彩色字形与普通字形都成立的唯一做法。
  彩色 emoji 的半透明行为保持不变，并补了回归测试锁定。
- **macOS 上装不下的多行文字整体居中溢出，与 Windows 不一致**：同一张表格，Windows 顶对齐、
  超出部分截断，macOS 却上下各露半行。三条绘制路径的纵向定位公式本该同源，Core Text 一侧
  少了 `.max(0)`——容器装不下时差值为负，文本便以容器中心为中心向上下对称溢出。
  根因不止于那一个缺失的钳制：`TextEngine::draw` 的契约当时只写「垂直居中」，**没规定装不下
  怎么办**。DirectWrite 实现的是「居中，但装不下顶对齐」，Core Text 实现的是字面意义的
  「居中」，两边都没违反文档。歧义本身就是缺陷，所以这次把契约写进 trait 文档并给出参考实现
  `block_offset_y`，d2d 的 `draw_text` 改为直接调用它，软硬两路不再各写各的。
  配套的 `text_block_contract` 是一组**跨引擎**测试：Windows 跑 DirectWrite、macOS 跑
  Core Text，同一份断言。判据取墨量而非逐像素比对——抗锯齿让边缘取值随引擎浮动，逐像素会把
  两个都正确的实现判成不一致，而「这片区域有没有字」是稳定的。`coretext.rs` 此前测试数为零。
- **软后端投影外缘的直角硬边**：阴影 pixmap 的模糊余量按 `2×半径` 留，而 3 趟 box-blur 每趟
  各扩散一个半径、总计 `3×`——尾部在 pixmap 边界被硬截断。边界处窗口收窄后按实际样本数重新
  归一，丢掉的又都是外侧接近 0 的样本，均值被抬高，于是截断处不是自然衰减到 0，而是留下一圈
  被放大的亮边。远程桌面强制走软后端（flip-model swapchain 在 RDP 不可用），有损压缩再把这道
  边糊成块。改留 `3×`，并加回归测试：沿射线向外扫，最后一个可见暗度必须已近乎归零。
- **D2D 后端投影在高 DPI 下被重采样**：阴影原按逻辑像素烘焙，再由 `SetTransform(scale)` 连同
  `DrawBitmap` 的 LINEAR 插值一起放大，125%/150% 下等于把一张低分辨率软阴影拉大——阴影是纯
  渐变，最经不起重采样。改为全程按物理像素烘焙、目标矩形换算回逻辑坐标，尺寸 1:1。
- **子菜单刷新后孤立分隔线复现、孙级菜单被静默关闭**：分隔线规范化最初只覆盖刷新路径的根层，
  而 `submenu` 字段存的始终是原始列表，故子级仍拿到未规范化的项；且 `spawn` 记的是规范化后的
  下标，按同一下标去原始列表取会错位——取到分隔线（`submenu` 为空）即走截断分支，把已展开的
  孙级菜单无声关掉。改为取子级时就地规范化。
- **重建子树时构建期信号永久累积**。三处会按数据变化整批重建子树的宿主——`list_signal` /
  `host_signal` 的 `DynList`、`reorder_list_signal` 的行源、可排序表格的表头与三类正文——
  都是"删掉旧子节点、按新数据重建"，但**只删节点不回收信号**：行构建期创建的每个信号
  （调用方在 `row_fn` 里现造的、或 `accordion_multi` 逐面板分配的）每重建一轮就多留一批。
  模块注释原本以"静态树可接受"为前提，而放宽后的动态文案 API（文案参数可收
  `Signal<String>`）让"在 `row_fn` 里造信号"变成了自然写法，这个前提已不成立。
  现在这三处各持有一个 `SignalScope`，回收与节点删除写在同一处（表格是 `clear_children`
  同时做两件事），节点与其构建期信号同生共死。回归测试反复重建 20 轮后断言活跃槽位数与
  arena 容量均不增长（改前是 73 vs 13）。
  `list_signal` / `host_signal` 的**首批**行也纳入作用域并交给 widget，否则首轮永久漏一代。
- **`Element::dropdown` / `with_items` 为静态选项表长期占用运行时槽位**：`Vec<String>` 被
  包进一个 `signal()` 只为凑出统一的读取路径，而这个信号既没有 owner 也没人回收。改为
  `Options::Static | Bound` 二选一（与 `TextContent` 同一套心智：动态性是**字段的类型**，
  不是控件的类型），静态表直接按值存，读取路径仍只有一条。响应式入口
  （`dropdown_signal` / `with_items_reactive`）行为不变。
- **图片占位框不跟随主题**：图片加载失败（或调用方本就没图）时画的占位框，底色与边框是
  模块级硬编码的淡灰 `#EEE`/`#CCC`——暗色主题下就是深色卡片中间一块近白方块。改走
  `Role::SurfaceAlt` + `Role::Border`，与卡片、表头这些"空态容器"同源，运行期换主题跟着变。
  取中性的表面角色而非 `Role::Danger`：占位分支的条件只是"没有可绘制的图层"，它同样覆盖
  "图还没来"，报成错误是过度解读。
- **菜单面板顶/底各 6px 的边带滚动后可点却看不见**：条目绘制裁剪到面板内缩
  `MENU_VPAD` 后的矩形，命中判据却看整个面板矩形。`scroll == 0` 时首项恰好从裁剪线起画，
  边带里没有行，问题不显；一旦滚动，挪进边带的行被裁掉不可见，却仍然可点——用户点到的是
  一个屏幕上不存在的项。现在绘制裁剪与命中判据（含尾随可点击图标）都取自同一个
  `MenuLevel::item_clip()`。落在边带上的点击照旧被菜单吞掉（不会误关菜单），只是不再命中项。
- **`Element::widget()` 会静默替换已有控件**：它的定位是给 `leaf` / 空容器挂自定义控件，
  但任何 `Element` 都能被它整个换掉——`Element::button("确定").widget(MyThing)` 会把按钮
  丢掉，不报错也没有任何迹象。现在 debug 下 `debug_assert` + `#[track_caller]` 当场指出误用
  （与相邻的 `clickable()` 一致）。库内组合构造器给自己刚建的节点挂控件走不受守卫的内部
  入口，故 `table_*` / `list_signal` / `scroll` 等构造不受影响；但它们返回的节点从此也被
  守卫覆盖——`Element::scroll().widget(..)` 会报错而不是悄悄毁掉滚动。
- **`examples/theming.rs` 与 `examples/tray.rs` 接不上截屏回归**：另外 27 个 example 都链了
  `App::screenshot_from_args()`，这两个漏了，于是统一的 `--screenshot` 命令对它们无效——
  偏偏 `theming.rs` 演示的正是主题与边框单位，最需要看图对比。已补上。
- **macOS：切走应用会让拖动态与输入法合成态永久悬挂**（以下三条均**未在 macOS 真机验证**，
  开发机为 Windows，只保证 `cargo check --target aarch64-apple-darwin` 通过）。
  `AppHandler::on_capture_lost` 在 macOS 后端从未被调用：Cocoa 有隐式捕获（`mouseDown:`
  之后的 `mouseDragged:`/`mouseUp:` 自动续派发给同一 view），所以确实不需要 win32 那样的
  `SetCapture`——但"按住不放时应用被切走、抬起事件从此不再送达"这件事一样会发生，
  reorder 列表、滑块会永远卡在拖动态。现在 `windowDidResignKey:` 通知上层收尾，门控条件
  （仅在上层自认持有捕获时才通知）与 win32 的 `WM_CAPTURECHANGED` 一致。
- **macOS：合成中途切走应用后文本框光标再也不闪**。控件在输入法组合态期间不画自绘光标，
  而 macOS 侧只有 `setMarkedText:`/`unmarkText`/`insertText:` 三条路会清合成态，切走应用
  时一条都不走（win32 有 `WM_IME_ENDCOMPOSITION` 兜底）。现在同一个 `windowDidResignKey:`
  里清合成态并 `discardMarkedText`。
- **macOS：触控板慢速滚动整格丢失**。滚轮增量 `as i32` 直接截断，而触控板的精确增量单次
  常不足 1 个框架单位，于是轻推毫无反应、动量尾段也提前断掉。改为累积亚像素残差后再取整
  （同 `app/fling.rs` 的 `pan_residual`），新手势起手时清残差。同时把"macOS 的惯性滚动
  由系统提供、`start_fling`/`cancel_fling` 刻意不实现"这条决策写进了 `AppHandler` 的
  trait 文档与后端注释——它此前看着像漏实现，很容易被后人照 win32 又移植一遍自研惯性，
  叠加成双倍速度。

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

[Unreleased]: https://github.com/huanfeng/wind-ui-rust/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.11.1...v0.12.0
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
