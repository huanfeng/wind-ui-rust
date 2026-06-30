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
use crate::signal::Signal;
use crate::style::Style;
use crate::text::TextEngine;

pub struct DynList<T: Clone + 'static> {
    data: Signal<Vec<T>>,
    row_fn: Box<dyn Fn(T) -> super::Element>,
    last_version: u64,
}

impl<T: Clone + 'static> DynList<T> {
    pub fn new(data: Signal<Vec<T>>, row_fn: impl Fn(T) -> super::Element + 'static) -> Self {
        Self {
            last_version: data.version(),
            data,
            row_fn: Box::new(row_fn),
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

        // 按新数据重建子节点
        let items = self.data.get();
        for item in items {
            let el = (self.row_fn)(item);
            let child_id = el.build(tree);
            tree.add_child(self_id, child_id);
        }
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
