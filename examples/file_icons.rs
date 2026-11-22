//! 文件图标示例：`windui::platform::file_icon` 取系统文件管理器里的图标。
//!
//! 运行：  cargo run --example file_icons [路径 ...]
//! 截屏：  cargo run --example file_icons -- --screenshot artifacts/file_icons.png
//!
//! 不传路径时按平台列几类典型条目：程序、文件夹、几种扩展名的文件（在临时目录里现建）。
//! 每个条目显示 16 / 32 / 64 三档逻辑尺寸，取图时按 2 倍物理像素请求，200% 缩放下也清晰。
//! Linux 暂不支持（`file_icon` 恒返回 `None`），条目显示为空框。

use std::path::PathBuf;

use windui::platform::file_icon;
use windui::prelude::*;

#[path = "common/mod.rs"]
mod common;
use common::page_title;

/// 在临时目录里建几个空文件，让 Shell / Finder 给出按扩展名关联的类型图标。
fn temp_files() -> Vec<PathBuf> {
    let dir = std::env::temp_dir().join("windui-file-icons");
    let _ = std::fs::create_dir_all(&dir);
    ["说明.txt", "照片.png", "文档.pdf", "数据.zip"]
        .iter()
        .map(|name| {
            let p = dir.join(name);
            let _ = std::fs::write(&p, b"");
            p
        })
        .collect()
}

fn default_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        let root =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()));
        v.push(root.join("explorer.exe"));
        v.push(root.join("notepad.exe"));
        v.push(root.clone());
    }
    #[cfg(target_os = "macos")]
    {
        v.push("/System/Library/CoreServices/Finder.app".into());
        v.push("/System/Applications/Calculator.app".into());
        v.push("/Applications".into());
    }
    v.extend(temp_files());
    v
}

/// 一行：三档尺寸的图标 + 文件名。
fn row(path: &PathBuf) -> Element {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let mut r = Element::row().spacing(12);
    for side in [16u32, 32, 64] {
        let content = ImageContent::new(file_icon(path, side * 2)).fit(Fit::Contain);
        r = r.child(
            Element::image_content(content)
                .width(side as i32)
                .height(side as i32)
                .border_role(Role::Border, 1)
                .align(Align::Center),
        );
    }
    // 行内各子节点在交叉轴（纵向）上各自居中，与最高的 64 图标对齐。
    r.child(Element::label(name).weight(1.0).align(Align::Center))
}

fn main() {
    let args: Vec<PathBuf> = std::env::args()
        .skip(1)
        .take_while(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .collect();
    let paths = if args.is_empty() {
        default_paths()
    } else {
        args
    };

    let mut list = Element::col().spacing(10).width_match();
    for p in &paths {
        list = list.child(row(p));
    }
    let page = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(18)
        .spacing(12)
        .child(page_title("文件图标", "platform::file_icon · 16 / 32 / 64").height(34))
        .child(Element::scroll().fill().child(list));

    App::new("windui — 文件图标", 520, 560)
        .screenshot_from_args()
        .content(page)
        .run();
}
