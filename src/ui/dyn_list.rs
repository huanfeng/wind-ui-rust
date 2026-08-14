//! 响应式动态列表 widget（`Element::list_signal` 的内部驱动）。
//!
//! `DynList<T>` 挂载在滚动容器节点上，当绑定的 `Signal<Vec<T>>` 版本号变化时，
//! 在 `on_update`（layout 前回调）中清空旧子节点、按新数据重建新子节点。
//!
//! 外部只通过 `Element::list_signal` 使用，无需直接构造本类型。

use crate::core::{EventCtx, Widget};
use crate::event::Event;
use crate::geometry::{Rect, Size};
use crate::render::Canvas;
use crate::signal::{Signal, SignalScope};
use crate::style::Style;
use crate::text::TextEngine;

pub struct DynList<T: Clone + 'static> {
    data: Signal<Vec<T>>,
    row_fn: Box<dyn Fn(T) -> super::Element>,
    last_version: u64,
    /// 当前这批行在构建期创建的信号。下轮重建先整批回收，避免
    /// "`row_fn` 里 `signal(..)` → 每次数据变化多漏一批槽位"。
    rows: SignalScope,
}

impl<T: Clone + 'static> DynList<T> {
    pub fn new(data: Signal<Vec<T>>, row_fn: impl Fn(T) -> super::Element + 'static) -> Self {
        Self::with_scope(data, row_fn, SignalScope::new())
    }

    /// 接管首批行的信号作用域：`Element::list_signal` 在构建初始行时已开了一个作用域，
    /// 把它交给 widget，首批行才能在第一次重建时被一并回收（否则永久漏一代）。
    pub(super) fn with_scope(
        data: Signal<Vec<T>>,
        row_fn: impl Fn(T) -> super::Element + 'static,
        rows: SignalScope,
    ) -> Self {
        Self {
            last_version: data.version(),
            data,
            row_fn: Box::new(row_fn),
            rows,
        }
    }
}

impl<T: Clone + 'static> Widget for DynList<T> {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.data.version();
        if ver == self.last_version {
            return;
        }
        self.last_version = ver;

        let self_id = ctx.id();
        let items = self.data.get();
        // 先拆字段：`rows.collect` 要 `&mut self.rows`，闭包里还要读 `self.row_fn`。
        let Self { row_fn, rows, .. } = self;
        let tree = ctx.tree_mut();

        // 移除当前所有子节点（递归释放子树 arena slot）
        let old_children: Vec<_> = tree
            .get(self_id)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for child in old_children {
            tree.remove(child);
        }
        if let Some(n) = tree.get_mut(self_id) {
            n.children.clear();
        }
        // 旧行的节点已经没了，其构建期信号同刻回收——两者生命周期必须一致，否则
        // 要么漏槽位、要么让还活着的行读到已回收的信号。
        rows.dispose();

        // 按新数据重建子节点，新一批构建期信号归 `rows` 所有。
        rows.collect(|| {
            for item in items {
                let el = row_fn(item);
                let child_id = el.build(tree);
                tree.add_child(self_id, child_id);
            }
        });
    }

    // DynList 自身无视觉内容；背景/边框由容器 Style 处理。
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::ZERO
    }
    fn paint(
        &self,
        _bounds: Rect,
        _content: Rect,
        _focused: bool,
        _enabled: bool,
        _canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
    }
    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
}
