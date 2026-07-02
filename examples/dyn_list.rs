//! 响应式动态列表示例：演示 `Element::list_signal` 的排序与筛选。
//!
//! 运行：cargo run --release --example dyn_list
//!
//! 交互：
//! - 「按名称排序 / 按优先级排序」— 切换排序维度，列表行即时重排
//! - 「隐藏已完成 / 显示全部」— 过滤已完成任务，行即时增删
//!
//! 每次点击只需对 Signal<Vec<Task>> 调 `.set()`，框架自动清空旧子节点并重建新子节点，
//! 调用方不感知 reconciler 的存在。

use windui::prelude::*;

const BG: u32 = 0xEEF1F5;
const CARD: u32 = 0xFFFFFF;
const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;

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
    fn color(self) -> Color {
        match self {
            Priority::High => Color::hex(0xE17055),
            Priority::Medium => Color::hex(0xFDAA0D),
            Priority::Low => Color::hex(0x00B894),
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
    let name_color = if task.done {
        Color::hex(SUB)
    } else {
        Color::hex(FG)
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
                .bg(task.priority.color()),
        )
        .child(
            // 任务名
            Element::label(format!("{}{}", task.name, done_text))
                .font_size(14.0)
                .fg(name_color)
                .weight(1.0),
        )
        .child(
            // 优先级 badge：显式固定宽度，保证 measure/paint max_w 一致，避免换行抖动。
            Element::label(badge_text)
                .font_size(11.0)
                .fg(task.priority.color())
                .padding_xy(6, 2)
                .corner(4.0)
                .border(task.priority.color(), 1)
                .width(84),
        )
}

fn main() {
    // 两个 UI 状态信号
    let sort_by_name = signal(false);
    let hide_done = signal(false);

    // 视图数据信号：初始值 = 按优先级排序、显示全部
    let tasks = signal(compute(false, false));

    // 排序按钮
    let sort_btn = {
        Element::button("按优先级排序")
            .on_click(move |_| {
                let by_name = !sort_by_name.get();
                sort_by_name.set(by_name);
                tasks.set(compute(by_name, hide_done.get()));
            })
            .intent(Intent::Primary)
    };

    // 筛选按钮
    let filter_btn = {
        Element::button("隐藏已完成")
            .on_click(move |_| {
                let hide = !hide_done.get();
                hide_done.set(hide);
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
                .bg(Color::hex(CARD))
                .border(Color::hex(0xDDE1E7), 1)
                .corner(6.0)
        },
    );

    let ui = Element::col()
        .fill()
        .bg(Color::hex(BG))
        .padding(20)
        .spacing(12)
        .child(
            Element::label("动态任务列表")
                .font_size(22.0)
                .fg(Color::hex(FG))
                .height(32)
                .width_match(),
        )
        .child(
            Element::label("点击下方按钮排序或筛选，列表即时刷新（无整窗重建）")
                .font_size(13.0)
                .fg(Color::hex(SUB))
                .height(20)
                .width_match(),
        )
        .child(toolbar)
        .child(list.weight(1.0));

    App::new("windui — 响应式动态列表", 480, 560)
        .bg(Color::hex(BG))
        .content(ui)
        .run();
}
