//! windui — 轻量跨平台桌面 GUI 框架（Windows：Win32+DirectWrite；macOS：Cocoa+CoreText，开发中）。
//!
//! - 第三方使用指南（API 风格/规范/扩展）：`docs/API_GUIDE.md`
//! - 架构设计：`docs/DESIGN.md`；实施路线图：`docs/ROADMAP.md`

// 图形绘制 API 以标量坐标传参（x,y,w,h,radius,width,paint）是有意设计，放宽该 lint。
#![allow(clippy::too_many_arguments)]

pub mod anim;
pub mod app;
pub mod core;
pub mod event;
pub mod geometry;
pub mod icon;
pub mod platform;
pub mod render;
pub mod signal;
pub(crate) mod single_instance;
/// 单实例对外面:启动早期的闸门 [`claim_instance`]。模块其余部分是内部实现。
pub use single_instance::{claim_instance, InstanceRole};
pub mod spec;
pub mod style;
pub(crate) mod sync;
pub mod testing;
pub mod text;
pub mod theme;
pub mod ui;

pub mod prelude {
    pub use crate::app::{App, HotkeyHandle, ThemeHandle, Window};
    pub use crate::event::{
        window_state, CursorShape, Hotkey, HotkeyCtx, HotkeyOp, Key, MenuItem, Mods, Preedit,
        ToastKind, WindowState,
    };
    pub use crate::geometry::{Color, Insets, Point, Rect, Size};
    pub use crate::icon::{brand_icon, brand_icon_at, IconSource, WindowIcon};
    pub use crate::platform::{
        PickDialog, Renderer, Tray, TrayCtx, TrayHandle, TrayMenuItem, TrayOp,
    };
    pub use crate::render::image::{Fit, Image, ImageError, VisualState};
    pub use crate::render::{Gradient, PixmapTarget, RenderTarget};
    pub use crate::signal::{signal, Signal};
    pub use crate::spec::{Align, Axis, Dimension};
    pub use crate::style::{Brush, Edges, Role, Shadow, Style};
    pub use crate::sync::Sender;
    pub use crate::theme::{Intent, Len, TableTheme, Theme};
    // `TabItem` / `TabStyle` 是 `Element::tabs_items` 的参数类型，`ColorPickerOpts` 是
    // `Element::color_picker_opts` 的：构造器在 prelude 里，参数类型却要写一行深路径
    // import，那条路就没人走。`Hsva` 则是取色器对外的颜色模型，业务侧做色相运算时用得上。
    pub use crate::ui::containers::{TabItem, TabStyle};
    pub use crate::ui::{
        default_presets, CaretStyle, CheckMenuItem, ColorPickerOpts, CommitMode, DropdownItem,
        Element, Hsva, ImageContent, ImageView, Link, Para, RichColor, RichDoc, RowRequest,
        RowSource, SelectionScope, SortKey, SortOrder, SortStyle, SpanStyle, TextContent, Truncate,
        WindowButton, WindowButtonKind, ROW_CACHE_SEGMENTS, ROW_CHUNK, TABLE_ROW_H,
    };
}
