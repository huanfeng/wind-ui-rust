//! 文件拖出示例：按住条目拖到 Finder / 资源管理器 / 桌面上松手。
//!
//! 运行：  cargo run --example drag_out
//!
//! - 「现成的文件」：直接用 `platform::drag_files` 拖出，允许移动（同一卷上拖进 Finder 会移走）。
//! - 「放下时才生成」：`platform::drag_files_with`，路径先报给接收方，接收方要读取时才写出文件。
//!
//! 页面底部显示上一次拖动的结果与 `before_drop` 是否被调用。文件都建在临时目录的
//! `windui-drag-out` 下。Linux 暂未实现拖出，拖动后结果恒为 `None`。

use std::path::PathBuf;

use windui::core::{EventCtx, Widget};
use windui::event::{Event, MouseButton, PointerKind};
use windui::geometry::Point;
use windui::platform::{drag_files, drag_files_with};
use windui::prelude::*;

#[path = "common/mod.rs"]
mod common;
use common::page_title;

/// 指针移动超过这个距离（逻辑像素）才算开始拖动，免得单击被当成拖动。
const DRAG_THRESHOLD: i32 = 4;

/// 挂在一行上的拖动手柄：按下后移动过阈值就发起拖出。
struct DragOut {
    paths: Vec<PathBuf>,
    lazy: bool,
    status: Signal<String>,
    down_at: Option<Point>,
}

impl Widget for DragOut {
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else { return false };
        if p.button != MouseButton::Left && p.kind != PointerKind::Move {
            return false;
        }
        match p.kind {
            PointerKind::Down => {
                self.down_at = Some(p.pos);
                true
            }
            PointerKind::Up => {
                self.down_at = None;
                true
            }
            PointerKind::Move => {
                let Some(d) = self.down_at else { return false };
                if (p.pos.x - d.x).abs() + (p.pos.y - d.y).abs() < DRAG_THRESHOLD {
                    return true;
                }
                // 一次按下只发起一次：拖出阻塞到松手，会话吞掉的 Up 由平台层按捕获丢失补上。
                self.down_at = None;
                let (paths, lazy, status) = (self.paths.clone(), self.lazy, self.status);
                // 拖出阻塞到松手，必须排到事件派发返回之后再调。
                ctx.defer_blocking(move || {
                    let effect = if lazy {
                        let called = std::rc::Rc::new(std::cell::Cell::new(false));
                        let flag = called.clone();
                        let to_write = paths.clone();
                        let effect = drag_files_with(
                            &paths,
                            false,
                            Some(Box::new(move || {
                                flag.set(true);
                                to_write
                                    .iter()
                                    .all(|p| std::fs::write(p, "放下时才写出的内容\n").is_ok())
                            })),
                        );
                        format!("{effect:?}，before_drop 调用：{}", called.get())
                    } else {
                        format!("{:?}", drag_files(&paths, true))
                    };
                    status.set(format!("上次拖出结果：{effect}"));
                });
                true
            }
            _ => false,
        }
    }
}

fn draggable(
    title: &str,
    hint: &str,
    paths: Vec<PathBuf>,
    lazy: bool,
    status: Signal<String>,
) -> Element {
    Element::col()
        .padding(12)
        .spacing(4)
        .width_match()
        .bg_role(Role::SurfaceAlt)
        .border_role(Role::Border, 1)
        .corner(6.0)
        .child(Element::label(title.to_string()))
        .child(
            Element::label(hint.to_string())
                .font_size(12.0)
                .fg_role(Role::TextMuted),
        )
        .widget(DragOut {
            paths,
            lazy,
            status,
            down_at: None,
        })
}

fn main() {
    let dir = std::env::temp_dir().join("windui-drag-out");
    let _ = std::fs::create_dir_all(&dir);
    let ready: Vec<PathBuf> = ["拖出示例.txt", "拖出示例-2.txt"]
        .iter()
        .map(|n| {
            let p = dir.join(n);
            if !p.exists() {
                let _ = std::fs::write(&p, "windui 拖出示例\n");
            }
            p
        })
        .collect();
    let lazy = vec![dir.join("放下时生成.txt")];
    // 每次启动都删掉，好确认它确实是放下时才写出来的。
    let _ = std::fs::remove_file(&lazy[0]);

    let status = signal("还没有拖动过".to_string());
    let page = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(18)
        .spacing(12)
        .child(page_title("文件拖出", "platform::drag_files").height(34))
        .child(draggable(
            "现成的文件（2 个，允许移动）",
            "按住拖到 Finder / 资源管理器 / 桌面",
            ready,
            false,
            status,
        ))
        .child(draggable(
            "放下时才生成（1 个，只复制）",
            "拖动开始时文件还不存在",
            lazy,
            true,
            status,
        ))
        .child(Element::label(status).fg_role(Role::TextMuted))
        .child(
            Element::label(format!("文件位置：{}", dir.display()))
                .font_size(12.0)
                .fg_role(Role::TextMuted),
        );

    App::new("windui — 文件拖出", 480, 360)
        .screenshot_from_args()
        .content(page)
        .run();
}
