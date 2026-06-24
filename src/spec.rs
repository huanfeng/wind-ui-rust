//! 布局规格：尺寸意图、测量约束、轴向与对齐。单位为物理像素（i32）。

/// 尺寸意图。Builder 对外用逻辑 dp，构建时按 DPI 折算为 `Px`（Phase 1 scale=1）。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Dimension {
    /// 固定像素。
    Px(i32),
    /// 撑满父容器可用空间（match_parent）。
    Match,
    /// 包裹内容（wrap_content）。
    #[default]
    Wrap,
    /// 线性布局主轴权重，按比例瓜分剩余空间。
    Weight(f32),
}

impl Dimension {
    pub fn weight(&self) -> f32 {
        match self {
            Dimension::Weight(w) => *w,
            _ => 0.0,
        }
    }
    pub fn is_weight(&self) -> bool {
        matches!(self, Dimension::Weight(_))
    }
}

/// 测量模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureMode {
    /// 必须正好这个尺寸。
    Exact,
    /// 不超过这个尺寸（可更小）。
    AtMost,
    /// 无约束（按内容）。
    Unbounded,
}

/// 单轴测量约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeasureSpec {
    pub mode: MeasureMode,
    pub size: i32,
}

impl MeasureSpec {
    pub fn exactly(size: i32) -> Self {
        Self {
            mode: MeasureMode::Exact,
            size,
        }
    }
    pub fn at_most(size: i32) -> Self {
        Self {
            mode: MeasureMode::AtMost,
            size,
        }
    }
    pub fn unbounded() -> Self {
        Self {
            mode: MeasureMode::Unbounded,
            size: 0,
        }
    }
    /// 可用尺寸（Unbounded 视为无限大，用一个大数表达上界）。
    pub fn avail(&self) -> i32 {
        match self.mode {
            MeasureMode::Unbounded => i32::MAX / 4,
            _ => self.size,
        }
    }
    /// 把期望尺寸按约束收敛。
    pub fn resolve(&self, desired: i32) -> i32 {
        match self.mode {
            MeasureMode::Exact => self.size,
            MeasureMode::AtMost => desired.min(self.size).max(0),
            MeasureMode::Unbounded => desired.max(0),
        }
    }
}

/// 主轴方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// 交叉轴对齐方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    /// 拉伸填满交叉轴。
    Stretch,
}
