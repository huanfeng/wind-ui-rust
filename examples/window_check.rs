//! 真机核对示例：窗口圆角 / DWM 边框（`WindowCorner`、`WindowOutline::Off`）与光标隐藏
//! （`CursorShape::Hidden`）。截图路径验不到这些，只能真窗口肉眼看。
//!
//! 运行：
//! ```text
//! window_check.exe                                   # 直角 + 无外框（默认）
//! window_check.exe --corner round --outline auto     # 对照：与此前行为一致
//! window_check.exe --corner small                    # 小圆角
//! ```
//! `--corner round|small|square`，`--outline off|auto`。圆角与外框都是**建窗时**定下的，
//! 要换组合就重新启动。
//!
//! # 看什么
//! 1. **圆角**：按 Win+← / Win+→（或拖到屏幕边）吸附到半屏，看贴着工作区的那几个角：
//!    `square` 应是直角、不露出桌面；`round` 应有圆角（对照组）。
//! 2. **亮边**：Win11 上窗口边缘有没有那道 1px 浅色线。`--outline off` 应没有；
//!    `--outline auto` 应有（对照组）。
//! 3. **光标隐藏（静止）**：点「3 秒后隐藏指针」，然后**手离开鼠标别动**。3 秒后指针
//!    应自己消失（此前要动一下鼠标才生效，且动完又出现）；再过 3 秒自动恢复。
//! 4. **光标隐藏（悬停）**：把指针移进下面那块深色区域，指针应消失；移出来应恢复。

use std::time::Duration;
use windui::prelude::*;

const TITLE_BG: u32 = 0x2D3436;
/// 定时器步长（毫秒）。倒计时按步数算，3 秒 = 30 步。
const TICK_MS: u64 = 100;
const STEPS: i32 = 30;

fn arg_value(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let corner_name = arg_value("--corner").unwrap_or_else(|| "square".into());
    let corner = match corner_name.as_str() {
        "round" => WindowCorner::Round,
        "small" => WindowCorner::Small,
        _ => WindowCorner::Square,
    };
    let outline_name = arg_value("--outline").unwrap_or_else(|| "off".into());
    let outline = if outline_name == "auto" {
        WindowOutline::Auto
    } else {
        WindowOutline::Off
    };

    // 倒计时：>0 表示还有多少步隐藏指针；`hide_left` 是隐藏后还要持续多少步。
    let countdown = signal(0i32);
    let hide_left = signal(0i32);
    let hidden = signal(false);
    let status = signal(String::from("空闲"));

    let title_bar = Element::row()
        .width_match()
        .height(36)
        .cross(Align::Stretch)
        .bg(Color::hex(TITLE_BG))
        .window_drag()
        .child(
            Element::label(format!(
                "   window_check — corner={corner_name} outline={outline_name}"
            ))
            .fg(Color::WHITE)
            .weight(1.0),
        )
        .child(Element::window_button(WindowButtonKind::Minimize).fg(Color::WHITE))
        .child(Element::window_button(WindowButtonKind::Maximize).fg(Color::WHITE))
        .child(Element::window_button(WindowButtonKind::Close).fg(Color::WHITE));

    let body = Element::col()
        .fill()
        .bg_role(Role::Surface)
        .padding(20)
        .spacing(10)
        .child(
            Element::label("1. 圆角 / 亮边：Win+← 吸附半屏，看贴边的角和窗口边缘")
                .font_size(13.0)
                .fg_role(Role::Text)
                .width_match(),
        )
        .child(
            Element::label("2. 静止隐藏：点下面按钮后，手离开鼠标，3 秒后指针应自己消失")
                .font_size(13.0)
                .fg_role(Role::Text)
                .width_match(),
        )
        .child(Element::button("3 秒后隐藏指针").on_click(move |_| {
            countdown.set(STEPS);
            status.set("倒计时中…手别动".into());
        }))
        .child(
            Element::label(status)
                .font_size(13.0)
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(
            Element::label("3. 悬停隐藏：指针进入下面深色区域应消失")
                .font_size(13.0)
                .fg_role(Role::Text)
                .width_match(),
        )
        .child(
            Element::col()
                .width_match()
                .weight(1.0)
                .bg(Color::hex(0x636E72))
                .cursor(CursorShape::Hidden)
                .child(
                    Element::label("指针在此区域应不可见")
                        .fg(Color::WHITE)
                        .padding(12),
                ),
        );

    // 隐藏期间盖一层全窗覆盖，光标形状取 Hidden。`clickable()` 让它参与命中，
    // 否则透明容器会被命中穿透、光标解析不到它。
    let cover = Element::col()
        .fill()
        .clickable()
        .cursor(CursorShape::Hidden)
        .visible_signal(hidden);

    let ui = Element::stack()
        .fill()
        .child(
            Element::col()
                .fill()
                .child(title_bar)
                .child(body.weight(1.0)),
        )
        .child(cover);

    App::new("windui — window_check", 560, 420)
        .icon(brand_icon())
        .frameless()
        .window_corner(corner)
        .window_outline(outline)
        .on_interval(Duration::from_millis(TICK_MS), move |_| {
            let c = countdown.get();
            if c > 0 {
                countdown.set(c - 1);
                if c == 1 {
                    hidden.set(true);
                    hide_left.set(STEPS);
                    status.set("指针应已隐藏（3 秒后恢复）".into());
                }
            } else if hide_left.get() > 0 {
                let l = hide_left.get() - 1;
                hide_left.set(l);
                if l == 0 {
                    hidden.set(false);
                    status.set("已恢复".into());
                }
            }
        })
        .screenshot_from_args()
        .content(ui)
        .run();
}
