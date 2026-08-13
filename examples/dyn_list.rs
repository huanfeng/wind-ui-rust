//! 响应式动态列表示例：演示 `Element::list_signal` 的排序与筛选，以及绑信号的按钮文案。
//!
//! 运行：cargo run --release --example dyn_list
//!
//! 交互：
//! - 「按名称排序 / 按优先级排序」— 切换排序维度，列表行即时重排
//! - 「隐藏已完成 / 显示全部」— 过滤已完成任务，行即时增删
//!
//! 两个按钮的文案说的都是"点下去会发生什么"，故每次点击后文案要翻转成相反的动作。
//! 这靠给 `Element::button` 传 `Signal<String>` 实现（见 `TextContent`）——文案改了，
//! 按钮宽度也跟着重新测量，无需重建控件。
//!
//! 列表本身则是每次点击对 Signal<Vec<Task>> 调 `.set()`，框架自动清空旧子节点并重建
//! 新子节点，调用方不感知 reconciler 的存在。

use windui::prelude::*;

/// 优先级（数值越小越紧急）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Priority {
    High,
    Medium,
    Low,
}

impl Priority {
    fn label(self) -> &'static str {
        match self {
            Priority::High => "高",
            Priority::Medium => "中",
            Priority::Low => "低",
        }
    }
    /// 优先级配色走主题语义角色，换主题（含明暗切换）自动跟随。
    fn role(self) -> Role {
        match self {
            Priority::High => Role::Danger,
            Priority::Medium => Role::Warning,
            Priority::Low => Role::Success,
        }
    }
}

#[derive(Clone)]
struct Task {
    name: &'static str,
    priority: Priority,
    done: bool,
}

/// 原始数据（固定，不修改）。
fn all_tasks() -> Vec<Task> {
    vec![
        Task {
            name: "修复登录崩溃",
            priority: Priority::High,
            done: false,
        },
        Task {
            name: "撰写发布说明",
            priority: Priority::Medium,
            done: true,
        },
        Task {
            name: "重构数据库层",
            priority: Priority::High,
            done: false,
        },
        Task {
            name: "更新依赖版本",
            priority: Priority::Low,
            done: true,
        },
        Task {
            name: "添加单元测试",
            priority: Priority::Medium,
            done: false,
        },
        Task {
            name: "性能分析报告",
            priority: Priority::Low,
            done: false,
        },
        Task {
            name: "安全审计排查",
            priority: Priority::High,
            done: false,
        },
        Task {
            name: "设计评审会议",
            priority: Priority::Medium,
            done: true,
        },
    ]
}

/// 根据当前排序/筛选状态重新计算视图 Vec。
fn compute(sort_by_name: bool, hide_done: bool) -> Vec<Task> {
    let mut tasks = all_tasks();
    if hide_done {
        tasks.retain(|t| !t.done);
    }
    if sort_by_name {
        tasks.sort_by_key(|t| t.name);
    } else {
        tasks.sort_by_key(|t| t.priority);
    }
    tasks
}

/// 单行任务卡片。
fn task_row(task: Task) -> Element {
    let name_role = if task.done {
        Role::TextMuted
    } else {
        Role::Text
    };
    let badge_text = format!("优先级：{}", task.priority.label());
    let done_text = if task.done { " ✓ 已完成" } else { "" };

    Element::row()
        .width_match()
        .height(48)
        .cross(Align::Center)
        .padding_xy(12, 0)
        .spacing(10)
        .child(
            // 优先级色块
            Element::col()
                .width(4)
                .height(28)
                .corner(2.0)
                .bg_role(task.priority.role()),
        )
        .child(
            // 任务名
            Element::label(format!("{}{}", task.name, done_text))
                .font_size(14.0)
                .fg_role(name_role)
                .weight(1.0),
        )
        .child(
            // 优先级 badge：显式固定宽度，保证 measure/paint max_w 一致，避免换行抖动。
            Element::label(badge_text)
                .font_size(11.0)
                .fg_role(task.priority.role())
                .padding_xy(6, 2)
                .corner(4.0)
                .border_role(task.priority.role(), 1)
                .width(84),
        )
}

fn main() {
    // 两个 UI 状态信号
    let sort_by_name = signal(false);
    let hide_done = signal(false);

    // 视图数据信号：初始值 = 按优先级排序、显示全部
    let tasks = signal(compute(false, false));

    // 两个按钮的文案信号：初始文案 = 当前状态下点一下会做的事。
    let sort_caption = signal(String::from("按名称排序"));
    let filter_caption = signal(String::from("隐藏已完成"));

    // 排序按钮：文案绑信号，翻转状态的同时把文案改成相反的动作。
    let sort_btn = {
        Element::button(sort_caption)
            .on_click(move |_| {
                let by_name = !sort_by_name.get();
                sort_by_name.set(by_name);
                sort_caption.set(String::from(if by_name {
                    "按优先级排序"
                } else {
                    "按名称排序"
                }));
                tasks.set(compute(by_name, hide_done.get()));
            })
            .intent(Intent::Primary)
    };

    // 筛选按钮：同上。文案长度不同（5 字 ↔ 4 字），按钮宽度随之重新测量。
    let filter_btn = {
        Element::button(filter_caption)
            .on_click(move |_| {
                let hide = !hide_done.get();
                hide_done.set(hide);
                filter_caption.set(String::from(if hide {
                    "显示全部"
                } else {
                    "隐藏已完成"
                }));
                tasks.set(compute(sort_by_name.get(), hide));
            })
            .intent(Intent::Neutral)
    };

    // 工具栏
    let toolbar = Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .spacing(8)
        .child(sort_btn)
        .child(filter_btn);

    // 响应式列表：数据变化时框架自动清空旧行、重建新行
    let list = Element::list_signal(
        tasks,
        |t: &Task| t.name, // key_fn（暂未做 diff 优化，用于未来接入）
        |t: Task| {
            task_row(t)
                .bg_role(Role::Surface)
                .border_role(Role::Border, 1)
                .corner(6.0)
        },
    );

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(20)
        .spacing(12)
        .child(
            Element::label("动态任务列表")
                .font_size(22.0)
                .fg_role(Role::Text)
                .height(32)
                .width_match(),
        )
        .child(
            Element::label("点击下方按钮排序或筛选，列表即时刷新（无整窗重建）")
                .font_size(13.0)
                .fg_role(Role::TextMuted)
                .height(20)
                .width_match(),
        )
        .child(toolbar)
        .child(list.weight(1.0));

    App::new("windui — 响应式动态列表", 480, 560)
        .screenshot_from_args()
        .content(ui)
        .run();
}
