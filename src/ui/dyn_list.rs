//! 响应式动态子树 widget。
//!
//! `DynList<T>`（`Element::host_signal` 的内部驱动）挂在容器节点上，当绑定的 `Signal<Vec<T>>`
//! 版本号变化时，在 `on_update`（layout 前回调）中清空旧子节点、按新数据重建新子节点。
//!
//! 外部只通过 `Element::host_signal` 使用，无需直接构造本类型。`Element::list_signal` 走同文件的
//! [`KeyedList`]：按键复用行节点，行上的过渡（增删、重排）才有身份可依。

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::core::{EventCtx, NodeId, Tree, Widget};
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

/// 按键复用行节点的响应式列表（`Element::list_signal` 的内部驱动）。
///
/// 与 [`DynList`] 的整批重建不同：数据变化时按 `key_fn` 比对新旧两批——键在、数据没变的行
/// **原样保留节点**（焦点、滚动、控件内部状态都留着），只按新顺序重排；键在但数据变了的行
/// 重建；新键建新行；消失的键删行。行节点因此有了身份，行上声明的过渡才有意义：
///
/// - 重排：行上的 `animate_layout` 从旧位置滑到新位置；
/// - 插入：行上的 `transition(..)` 从隐藏起播进场（首批行不播，与首帧不回放进场一致）；
/// - 删除：行上有 `transition(..)` 时先作为"残影"留在原处播离场，播完才真正移除
///   （残影不在数据里，点不到、拿不到焦点）；没有就立即移除。
///
/// 新数据里键有重复时退回整批重建（一个键对不上唯一的行）。
pub struct KeyedList<T: Clone + PartialEq + 'static, K: Eq + Hash + Clone + 'static> {
    data: Signal<Vec<T>>,
    key_fn: Box<dyn Fn(&T) -> K>,
    row_fn: Box<dyn Fn(T) -> super::Element>,
    last_version: u64,
    /// 当前子节点（含残影）按顺序一一对应。首批行在第一次 `on_update` 时才绑定节点 id
    /// （构建时还没有 id），见 `pending`。
    rows: Vec<Row<T, K>>,
    /// 首批行：构建期已建好元素，节点 id 等第一次 `on_update` 按子节点顺序认领。
    pending: Option<Vec<(K, T, SignalScope)>>,
}

struct Row<T, K> {
    key: K,
    item: T,
    node: NodeId,
    /// 本行构建期创建的信号：行被删除（或因数据变化重建）时随节点一起回收。
    scope: SignalScope,
    /// 已从数据中删除、正在播离场的残影。
    gone: bool,
}

impl<T: Clone + PartialEq + 'static, K: Eq + Hash + Clone + 'static> KeyedList<T, K> {
    pub(super) fn new(
        data: Signal<Vec<T>>,
        key_fn: impl Fn(&T) -> K + 'static,
        row_fn: impl Fn(T) -> super::Element + 'static,
        initial: Vec<(K, T, SignalScope)>,
    ) -> Self {
        Self {
            last_version: data.version(),
            data,
            key_fn: Box::new(key_fn),
            row_fn: Box::new(row_fn),
            rows: Vec::new(),
            pending: Some(initial),
        }
    }

    /// 建一行：元素在本行自己的作用域里构建，挂到列表节点下（顺序稍后统一重写）。
    fn build_row(&self, tree: &mut Tree, parent: NodeId, key: K, item: T) -> Row<T, K> {
        let mut scope = SignalScope::new();
        let el = scope.collect(|| (self.row_fn)(item.clone()));
        let node = el.build(tree);
        tree.add_child(parent, node);
        Row {
            key,
            item,
            node,
            scope,
            gone: false,
        }
    }

    fn drop_row(tree: &mut Tree, mut row: Row<T, K>) {
        tree.remove(row.node);
        // 节点没了，其构建期信号同刻回收——两者生命周期必须一致。
        row.scope.dispose();
    }

    /// 残影的离场播完了就真正移除。
    fn reap_ghosts(&mut self, tree: &mut Tree) -> bool {
        let mut reaped = false;
        let mut i = 0;
        while i < self.rows.len() {
            let r = &self.rows[i];
            if r.gone && !tree.get(r.node).is_some_and(|n| n.layout_visible()) {
                let row = self.rows.remove(i);
                Self::drop_row(tree, row);
                reaped = true;
            } else {
                i += 1;
            }
        }
        reaped
    }

    fn sync_children(&self, tree: &mut Tree, parent: NodeId) {
        let order: Vec<NodeId> = self.rows.iter().map(|r| r.node).collect();
        if let Some(n) = tree.get_mut(parent) {
            n.children = order;
        }
    }

    fn reconcile(&mut self, tree: &mut Tree, parent: NodeId, items: Vec<T>) {
        let keys: Vec<K> = items.iter().map(|t| (self.key_fn)(t)).collect();
        let unique = {
            let mut seen = HashSet::with_capacity(keys.len());
            keys.iter().all(|k| seen.insert(k))
        };
        if !unique {
            // 键对不上唯一的行：退回整批重建，不播任何过渡。
            for row in std::mem::take(&mut self.rows) {
                Self::drop_row(tree, row);
            }
            for (key, item) in keys.into_iter().zip(items) {
                let row = self.build_row(tree, parent, key, item);
                self.rows.push(row);
            }
            self.sync_children(tree, parent);
            return;
        }

        // 旧行按键索引；`old_order` 记旧顺序，供残影找落脚处。上一轮若因键重复整批重建过，
        // 旧行里会有同键的几行：只有一行能对上新数据，其余当场移除——直接 collect 进表会
        // 把它们静默丢掉，节点却还留在树上。
        let old_order: Vec<K> = self.rows.iter().map(|r| r.key.clone()).collect();
        let mut old: HashMap<K, Row<T, K>> = HashMap::with_capacity(self.rows.len());
        for row in std::mem::take(&mut self.rows) {
            if let Some(dup) = old.insert(row.key.clone(), row) {
                Self::drop_row(tree, dup);
            }
        }

        let mut next: Vec<Row<T, K>> = Vec::with_capacity(keys.len());
        for (key, item) in keys.into_iter().zip(items) {
            match old.remove(&key) {
                Some(mut row) if row.item == item => {
                    if row.gone {
                        // 播离场途中又加回来：就地复活，离场从当前进度掉头。
                        row.gone = false;
                        if let Some(n) = tree.get_mut(row.node) {
                            n.visible = true;
                        }
                    }
                    next.push(row);
                }
                Some(stale) => {
                    // 同一个键、数据变了：重建，但把过渡状态交给新节点——布局过渡的上次位置
                    // （挪了位置照样滑过去）与进出场进度（进场 / 离场播到一半的行从原进度
                    // 接着走，残影则掉头播回来）。
                    let flip = tree.get_mut(stale.node).and_then(|n| n.layout_anim.take());
                    let presence = tree.get_mut(stale.node).and_then(|n| n.presence.take());
                    Self::drop_row(tree, stale);
                    let row = self.build_row(tree, parent, key, item);
                    if let Some(n) = tree.get_mut(row.node) {
                        if n.layout_anim.is_some() && flip.is_some() {
                            n.layout_anim = flip;
                        }
                        if let (Some(new), Some(old)) = (n.presence.as_ref(), presence.as_ref()) {
                            new.adopt(old);
                        }
                    }
                    next.push(row);
                }
                None => {
                    let row = self.build_row(tree, parent, key, item);
                    // 运行中插入的行：从隐藏起播进场。
                    if let Some(p) = tree.get(row.node).and_then(|n| n.presence.as_ref()) {
                        p.rewind();
                    }
                    next.push(row);
                }
            }
        }

        // 被删的行：有过渡的转为残影留在原处播离场，没有的立即移除。残影挂在旧顺序里
        // 前一个仍在的行之后（连着的几个残影按旧顺序挂在同一处），没有就放最前——一趟走完
        // 旧顺序即可定位。锚点只认仍在的行：认残影的话连续删除会串成一条链，清空几千行时
        // 沿链展开就把栈撑爆了。
        let kept: HashSet<K> = next.iter().map(|r| r.key.clone()).collect();
        let mut front: Vec<Row<T, K>> = Vec::new();
        let mut after: HashMap<K, Vec<Row<T, K>>> = HashMap::new();
        let mut anchor: Option<K> = None;
        for key in &old_order {
            let Some(mut row) = old.remove(key) else {
                if kept.contains(key) {
                    anchor = Some(key.clone());
                }
                continue;
            };
            if !row.gone {
                row.gone = true;
                if let Some(n) = tree.get_mut(row.node) {
                    n.visible = false;
                }
            }
            if !tree.get(row.node).is_some_and(|n| n.layout_visible()) {
                Self::drop_row(tree, row);
                continue;
            }
            match &anchor {
                Some(a) => after.entry(a.clone()).or_default().push(row),
                None => front.push(row),
            }
        }
        let mut rows = Vec::with_capacity(next.len() + front.len());
        rows.extend(front);
        for row in next {
            let tail = after.remove(&row.key);
            rows.push(row);
            rows.extend(tail.into_iter().flatten());
        }
        self.rows = rows;
        self.sync_children(tree, parent);
    }
}

impl<T: Clone + PartialEq + 'static, K: Eq + Hash + Clone + 'static> Widget for KeyedList<T, K> {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let self_id = ctx.id();
        let claimed = self.pending.is_some();
        if let Some(initial) = self.pending.take() {
            // 首批行与子节点按构建顺序一一对应。
            let children = ctx
                .tree_mut()
                .get(self_id)
                .map(|n| n.children.clone())
                .unwrap_or_default();
            // 列表的子节点只能是行：构建后再 `.child(..)` 追加的会在第一次重排子节点时被
            // 挤出 children 却仍留在树上，这里先移除（debug 下直接报错提示误用）。
            debug_assert_eq!(
                children.len(),
                initial.len(),
                "list_signal 的子节点由数据生成，不支持再 .child(..) 追加"
            );
            for &extra in children.iter().skip(initial.len()) {
                ctx.tree_mut().remove(extra);
            }
            self.rows = initial
                .into_iter()
                .zip(children)
                .map(|((key, item, scope), node)| Row {
                    key,
                    item,
                    node,
                    scope,
                    gone: false,
                })
                .collect();
        }
        let tree = ctx.tree_mut();
        // 刚认领首批行时也重写一次子节点：多出的子节点已移除，children 里不能留着它的 id。
        let mut changed = self.reap_ghosts(tree) || claimed;
        let ver = self.data.version();
        if ver != self.last_version {
            self.last_version = ver;
            let items = self.data.get();
            self.reconcile(tree, self_id, items);
            changed = true;
        }
        if changed {
            self.sync_children(tree, self_id);
        }
    }

    // 自身无视觉内容；背景/边框由容器 Style 处理。
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
