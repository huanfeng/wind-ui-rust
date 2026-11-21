//! 真实控件树的平台无关无障碍投影。
//!
//! 这里刻意只做投影，不是第二套 UI 状态树：每个节点、每个值都现取自当前的 [`Tree`]。
//! 平台适配层查询快照，并把操作经正常事件路径送回树上执行。

use crate::core::{DispatchResult, NodeId, Tree};
use crate::event::{Key, KeyEvent};
use crate::geometry::Rect;

/// 首批支持的语义角色。没有角色的容器会被拍平。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityRole {
    /// 窗口根节点。
    Window,
    /// 静态文本（`label` 等）。
    Text,
    /// 可点击的按钮。
    Button,
}

/// 平台适配层可以送回真实控件树执行的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityAction {
    /// 把键盘焦点移到该节点。
    SetFocus,
    /// 激活该节点（按钮即点击），走与回车键相同的事件路径。
    Invoke,
}

/// 无障碍请求的平台无关结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityActionResult {
    /// 已执行。
    Performed,
    /// 节点已不存在（被删除，或槽位已换代）。
    NodeUnavailable,
    /// 节点或其祖先当前不可见。
    NotVisible,
    /// 节点或其祖先已禁用。
    Disabled,
    /// 节点不能获得焦点。
    NotFocusable,
    /// 节点不支持这个操作。
    UnsupportedAction,
}

/// 直接包着 WindUI 带代际校验的 arena 标识。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessibilityNodeId(NodeId);

impl AccessibilityNodeId {
    pub(crate) fn from_node_id(id: NodeId) -> Self {
        Self(id)
    }

    pub(crate) fn node_id(self) -> NodeId {
        self.0
    }

    /// arena 槽位下标。身份由它与 [`generation`](Self::generation) 共同确定。
    pub fn index(self) -> u32 {
        self.0.index()
    }

    /// arena 代际。槽位复用后代际变化，旧节点的标识随之失效。
    pub fn generation(self) -> u32 {
        self.0.generation()
    }
}

/// 当前控件树中一个可见的语义节点。
#[derive(Clone, Debug, PartialEq)]
pub struct AccessibilityNode {
    /// 节点标识。
    pub id: AccessibilityNodeId,
    /// 语义父节点（拍平后的，不是布局父节点）。根节点为 `None`。
    pub parent: Option<AccessibilityNodeId>,
    /// 语义子节点，按树序排列。
    pub children: Vec<AccessibilityNodeId>,
    /// 语义角色。
    pub role: AccessibilityRole,
    /// 当前文案（随信号变化）。没有文案时为空串。
    pub name: String,
    /// 补充说明：优先取悬停提示，其次取控件自己的说明。
    pub description: Option<String>,
    /// 窗口客户区内的逻辑坐标（DIP）。
    pub bounds: Rect,
    /// 自身及所有祖先都启用时为 `true`。
    pub enabled: bool,
    /// 能否获得键盘焦点。
    pub focusable: bool,
    /// 当前是否持有焦点。
    pub focused: bool,
    /// 文本是否被截断；不适用时为 `None`。
    pub truncated: Option<bool>,
    /// 该节点支持的操作。
    pub supported_actions: Vec<AccessibilityAction>,
}

/// 当前语义投影的只读有序快照。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccessibilitySnapshot {
    /// 根节点（窗口）。树为空或根不可见时为 `None`。
    pub root: Option<AccessibilityNodeId>,
    /// 全部节点，按先序遍历排列，根在第一个。
    pub nodes: Vec<AccessibilityNode>,
}

impl AccessibilitySnapshot {
    /// 按标识查节点。
    pub fn node(&self, id: AccessibilityNodeId) -> Option<&AccessibilityNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// 快照里是否有这个节点。
    pub fn contains(&self, id: AccessibilityNodeId) -> bool {
        self.node(id).is_some()
    }
}

impl Tree {
    /// 在当前真实控件树上校验一个操作，包括祖先的可见与启用状态。校验刻意放在树里，
    /// 平台适配层就不会不小心长出第二套有效性或操作规则。
    pub(crate) fn validate_accessibility_action(
        &self,
        id: AccessibilityNodeId,
        action: AccessibilityAction,
    ) -> AccessibilityActionResult {
        let node_id = id.node_id();
        let Some(node) = self.get(node_id) else {
            return AccessibilityActionResult::NodeUnavailable;
        };
        if !self.accessibility_visible(node_id) {
            return AccessibilityActionResult::NotVisible;
        }
        if !self.node_enabled(node_id) {
            return AccessibilityActionResult::Disabled;
        }
        match action {
            AccessibilityAction::SetFocus => {
                if self.focusable_order().contains(&node_id) {
                    AccessibilityActionResult::Performed
                } else {
                    AccessibilityActionResult::NotFocusable
                }
            }
            AccessibilityAction::Invoke => {
                if node.widget.accessibility_role() == Some(AccessibilityRole::Button) {
                    AccessibilityActionResult::Performed
                } else {
                    AccessibilityActionResult::UnsupportedAction
                }
            }
        }
    }

    /// 经现有的键盘事件路径执行一个已校验的操作。焦点、滚动与分发副作用的消费归
    /// `UiHost`；`Tree` 只负责带代际校验的目标和控件的正常事件路由。
    pub(crate) fn dispatch_accessibility_action(
        &mut self,
        id: AccessibilityNodeId,
        action: AccessibilityAction,
    ) -> (AccessibilityActionResult, crate::core::DispatchResult) {
        let result = self.validate_accessibility_action(id, action);
        if result != AccessibilityActionResult::Performed {
            return (result, DispatchResult::default());
        }
        match action {
            AccessibilityAction::SetFocus => (result, DispatchResult::default()),
            AccessibilityAction::Invoke => (
                result,
                self.dispatch_key(
                    KeyEvent {
                        key: Key::Enter,
                        pressed: true,
                        shift: false,
                        ctrl: false,
                        alt: false,
                        meta: false,
                    },
                    Some(id.node_id()),
                ),
            ),
        }
    }

    fn accessibility_visible(&self, id: NodeId) -> bool {
        let mut current = Some(id);
        while let Some(current_id) = current {
            let Some(node) = self.get(current_id) else {
                return false;
            };
            if !node.effective_visible() {
                return false;
            }
            current = node.parent;
        }
        true
    }

    /// 把真实的可见控件树投影成一份稳定的语义快照。
    ///
    /// 隐藏的子树不进快照。纯布局节点被拍平，`Row`/`Col`/`Stack` 的内部结构调整就不会
    /// 变成用户能感知的无障碍树抖动。
    pub fn accessibility_snapshot(&self) -> AccessibilitySnapshot {
        let mut snapshot = AccessibilitySnapshot::default();
        let Some(root) = self.root else {
            return snapshot;
        };
        let Some(root_node) = self.get(root) else {
            return snapshot;
        };
        if !root_node.effective_visible() {
            return snapshot;
        }

        let root_id = AccessibilityNodeId::from_node_id(root);
        snapshot.root = Some(root_id);
        snapshot.nodes.push(AccessibilityNode {
            id: root_id,
            parent: None,
            children: Vec::new(),
            role: AccessibilityRole::Window,
            name: root_node.widget.accessibility_name().unwrap_or_default(),
            description: root_node
                .tooltip
                .as_ref()
                .map(|tip| tip.resolve().into_owned())
                .or_else(|| root_node.widget.accessibility_description()),
            bounds: self.abs_bounds(root),
            enabled: root_node.own_enabled(),
            focusable: false,
            focused: root_node.focused,
            truncated: root_node.widget.text_truncated(),
            supported_actions: Vec::new(),
        });

        for &child in &root_node.children {
            self.collect_accessibility(child, 0, true, root_node.own_enabled(), &mut snapshot);
        }
        snapshot
    }

    /// `parent_slot` 是语义父节点在 `snapshot.nodes` 里的下标：直接按下标挂子节点，
    /// 不再逐个线性查找父节点（那样整棵树是 O(n²)）。
    fn collect_accessibility(
        &self,
        id: NodeId,
        parent_slot: usize,
        parent_visible: bool,
        parent_enabled: bool,
        snapshot: &mut AccessibilitySnapshot,
    ) {
        let Some(node) = self.get(id) else {
            return;
        };
        let visible = parent_visible && node.effective_visible();
        if !visible {
            return;
        }
        let enabled = parent_enabled && node.own_enabled();
        let semantic_role = node.widget.accessibility_role();
        let next_slot = if let Some(role) = semantic_role {
            let focusable = node.focusable.unwrap_or_else(|| node.widget.focusable());
            let mut actions = Vec::with_capacity(2);
            if focusable {
                actions.push(AccessibilityAction::SetFocus);
            }
            if role == AccessibilityRole::Button {
                actions.push(AccessibilityAction::Invoke);
            }
            let id = AccessibilityNodeId::from_node_id(id);
            let parent_id = snapshot.nodes[parent_slot].id;
            let slot = snapshot.nodes.len();
            snapshot.nodes.push(AccessibilityNode {
                id,
                parent: Some(parent_id),
                children: Vec::new(),
                role,
                name: node.widget.accessibility_name().unwrap_or_default(),
                description: node
                    .tooltip
                    .as_ref()
                    .map(|tip| tip.resolve().into_owned())
                    .or_else(|| node.widget.accessibility_description()),
                bounds: self.abs_bounds(id.node_id()),
                enabled,
                focusable,
                focused: node.focused,
                truncated: node.widget.text_truncated(),
                supported_actions: actions,
            });
            snapshot.nodes[parent_slot].children.push(id);
            slot
        } else {
            parent_slot
        };

        for &child in &node.children {
            self.collect_accessibility(child, next_slot, visible, enabled, snapshot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::signal::signal;
    use crate::text::{LineAwareTextEngine, NullTextEngine};
    use crate::ui::{Element, Truncate};

    fn snapshot_for(root: Element) -> AccessibilitySnapshot {
        let mut tree = Tree::new();
        let root_id = root.build(&mut tree);
        tree.root = Some(root_id);
        let mut text = NullTextEngine;
        tree.layout_root(Size::new(400, 240), &mut text);
        tree.accessibility_snapshot()
    }

    #[test]
    fn snapshot_flattens_layout_and_excludes_hidden_subtrees() {
        let snapshot = snapshot_for(
            Element::col()
                .child(Element::row().child(Element::button("Visible")))
                .child(
                    Element::stack()
                        .visible(false)
                        .child(Element::button("Hidden")),
                ),
        );
        assert_eq!(snapshot.nodes.len(), 2);
        let root = snapshot.node(snapshot.root.unwrap()).unwrap();
        assert_eq!(root.children.len(), 1);
        let button = snapshot.node(root.children[0]).unwrap();
        assert_eq!(button.role, AccessibilityRole::Button);
        assert_eq!(button.name, "Visible");
        assert_eq!(button.parent, snapshot.root);
    }

    #[test]
    fn dynamic_name_tracks_real_text_content() {
        let label = signal(String::from("Before"));
        let snapshot_before = snapshot_for(Element::col().child(Element::button(label)));
        assert_eq!(snapshot_before.nodes[1].name, "Before");
        label.set(String::from("After"));
        let snapshot_after = snapshot_for(Element::col().child(Element::button(label)));
        assert_eq!(snapshot_after.nodes[1].name, "After");
    }

    #[test]
    fn snapshot_uses_absolute_bounds_and_focus_state() {
        let clicks = signal(0);
        let root = Element::stack()
            .padding_xy(10, 12)
            .child(Element::button("Focus").on_click(move |_| clicks.set(clicks.get() + 1)));
        let mut tree = Tree::new();
        let root_id = root.build(&mut tree);
        tree.root = Some(root_id);
        let button_id = tree.get(root_id).unwrap().children[0];
        let mut text = NullTextEngine;
        tree.layout_root(Size::new(300, 180), &mut text);
        tree.set_focused(Some(button_id), None);
        let snapshot = tree.accessibility_snapshot();
        let button = snapshot
            .nodes
            .iter()
            .find(|node| node.role == AccessibilityRole::Button)
            .unwrap();
        assert_eq!(button.bounds, tree.abs_bounds(button_id));
        assert!(button.focusable);
        assert!(button.focused);
        assert!(button
            .supported_actions
            .contains(&AccessibilityAction::SetFocus));
        assert!(button
            .supported_actions
            .contains(&AccessibilityAction::Invoke));
    }

    #[test]
    fn label_truncation_is_projected_from_real_widget_state() {
        let mut tree = Tree::new();
        let root_id = Element::col()
            .child(
                Element::label("这是一段需要折成好几行才放得下的较长说明文字")
                    .width(60)
                    .max_lines(2)
                    .truncate(Truncate::End),
            )
            .build(&mut tree);
        tree.root = Some(root_id);
        let mut text = LineAwareTextEngine;
        tree.layout_root(Size::new(400, 240), &mut text);
        let snapshot = tree.accessibility_snapshot();
        let label = snapshot.node(snapshot.root.unwrap()).unwrap();
        assert_eq!(label.role, AccessibilityRole::Window);
        let text = snapshot
            .nodes
            .iter()
            .find(|node| node.role == AccessibilityRole::Text)
            .unwrap();
        assert_eq!(text.truncated, Some(true));
    }

    #[test]
    fn removed_node_generation_is_not_returned() {
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::button("Old"))
            .build(&mut tree);
        tree.root = Some(root);
        let old = tree.get(root).unwrap().children[0];
        tree.remove(old);
        let snapshot = tree.accessibility_snapshot();
        assert!(!snapshot.nodes.iter().any(|node| node.id.node_id() == old));
        assert!(tree.get(old).is_none());
    }

    #[test]
    fn accessibility_set_focus_updates_real_snapshot() {
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::button("Focus"))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 120), &mut NullTextEngine);
        let button = tree.get(root).unwrap().children[0];
        let id = AccessibilityNodeId::from_node_id(button);

        let (result, effects) =
            tree.dispatch_accessibility_action(id, AccessibilityAction::SetFocus);
        assert_eq!(result, AccessibilityActionResult::Performed);
        assert!(!effects.consumed);
        tree.set_focused(Some(button), None);
        assert!(tree.accessibility_snapshot().node(id).unwrap().focused);
    }

    #[test]
    fn accessibility_rejects_hidden_disabled_nonfocusable_and_stale_targets() {
        let show = signal(false);
        let enabled = signal(false);
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::button("Hidden").visible_signal(show))
            .child(Element::button("Disabled").enabled_signal(enabled))
            .child(Element::button("Not focusable").focusable(false))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(400, 180), &mut NullTextEngine);
        let children = tree.get(root).unwrap().children.clone();
        assert_eq!(
            tree.validate_accessibility_action(
                AccessibilityNodeId::from_node_id(children[0]),
                AccessibilityAction::Invoke,
            ),
            AccessibilityActionResult::NotVisible
        );
        assert_eq!(
            tree.validate_accessibility_action(
                AccessibilityNodeId::from_node_id(children[1]),
                AccessibilityAction::Invoke,
            ),
            AccessibilityActionResult::Disabled
        );
        assert_eq!(
            tree.validate_accessibility_action(
                AccessibilityNodeId::from_node_id(children[2]),
                AccessibilityAction::SetFocus,
            ),
            AccessibilityActionResult::NotFocusable
        );
        let stale = AccessibilityNodeId::from_node_id(children[0]);
        tree.remove(children[0]);
        assert_eq!(
            tree.validate_accessibility_action(stale, AccessibilityAction::Invoke),
            AccessibilityActionResult::NodeUnavailable
        );
    }

    #[test]
    fn accessibility_invoke_uses_button_event_path_exactly_once() {
        let clicks = signal(0);
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::button("Invoke").on_click(move |_| clicks.set(clicks.get() + 1)))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(300, 120), &mut NullTextEngine);
        let button = tree.get(root).unwrap().children[0];
        let (result, effects) = tree.dispatch_accessibility_action(
            AccessibilityNodeId::from_node_id(button),
            AccessibilityAction::Invoke,
        );
        assert_eq!(result, AccessibilityActionResult::Performed);
        assert!(effects.consumed);
        assert_eq!(clicks.get(), 1);
    }
}
