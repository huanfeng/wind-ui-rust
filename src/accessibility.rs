//! Platform-neutral accessibility projection for the real WindUI tree.
//!
//! This is deliberately a projection, not a second UI state tree: every node and every value
//! comes from the current [`Tree`]. Platform adapters can query the snapshot and route actions
//! back through the normal event path.

use crate::core::{DispatchResult, NodeId, Tree};
use crate::event::{Key, KeyEvent};
use crate::geometry::Rect;

/// The first supported semantic slice. Containers without a role are flattened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityRole {
    Window,
    Text,
    Button,
}

/// Actions that a platform adapter may route to the real tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityAction {
    SetFocus,
    Invoke,
}

/// Platform-neutral result for an accessibility request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessibilityActionResult {
    Performed,
    NodeUnavailable,
    NotVisible,
    Disabled,
    NotFocusable,
    UnsupportedAction,
}

/// Direct wrapper around WindUI's generation-checked arena identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessibilityNodeId(NodeId);

impl AccessibilityNodeId {
    pub(crate) fn from_node_id(id: NodeId) -> Self {
        Self(id)
    }

    pub(crate) fn node_id(self) -> NodeId {
        self.0
    }

    pub fn index(self) -> u32 {
        self.0.index()
    }

    pub fn generation(self) -> u32 {
        self.0.generation()
    }
}

/// One visible semantic node in the current WindUI tree.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessibilityNode {
    pub id: AccessibilityNodeId,
    pub parent: Option<AccessibilityNodeId>,
    pub children: Vec<AccessibilityNodeId>,
    pub role: AccessibilityRole,
    pub name: String,
    pub description: Option<String>,
    pub bounds: Rect,
    pub enabled: bool,
    pub focusable: bool,
    pub focused: bool,
    pub truncated: Option<bool>,
    pub supported_actions: Vec<AccessibilityAction>,
}

/// Immutable, ordered snapshot of the current semantic projection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccessibilitySnapshot {
    pub root: Option<AccessibilityNodeId>,
    pub nodes: Vec<AccessibilityNode>,
}

impl AccessibilitySnapshot {
    pub fn node(&self, id: AccessibilityNodeId) -> Option<&AccessibilityNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn contains(&self, id: AccessibilityNodeId) -> bool {
        self.node(id).is_some()
    }
}

impl Tree {
    /// Validate an action against the current real tree, including ancestor visibility and
    /// enabled state. This is intentionally kept in the tree so platform adapters cannot
    /// accidentally create a second validity or action policy.
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

    /// Execute an already validated action through the existing keyboard event path.
    /// `UiHost` owns focus, scrolling and dispatch-effect consumption; `Tree` only owns the
    /// generation-checked target and the normal widget event route.
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

    /// Projects the real visible tree into a stable semantic snapshot.
    ///
    /// Hidden subtrees are excluded. Pure layout nodes are flattened so changes in internal
    /// `Row`/`Col`/`Stack` structure do not become user-visible accessibility churn.
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
            self.collect_accessibility(
                child,
                root_id,
                true,
                root_node.own_enabled(),
                &mut snapshot,
            );
        }
        snapshot
    }

    fn collect_accessibility(
        &self,
        id: NodeId,
        semantic_parent: AccessibilityNodeId,
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
        let next_parent = if let Some(role) = semantic_role {
            let focusable = node.focusable.unwrap_or_else(|| node.widget.focusable());
            let mut actions = Vec::with_capacity(2);
            if focusable {
                actions.push(AccessibilityAction::SetFocus);
            }
            if role == AccessibilityRole::Button {
                actions.push(AccessibilityAction::Invoke);
            }
            let id = AccessibilityNodeId::from_node_id(id);
            snapshot.nodes.push(AccessibilityNode {
                id,
                parent: Some(semantic_parent),
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
            if let Some(parent) = snapshot.node_mut(semantic_parent) {
                parent.children.push(id);
            }
            id
        } else {
            semantic_parent
        };

        for &child in &node.children {
            self.collect_accessibility(child, next_parent, visible, enabled, snapshot);
        }
    }
}

impl AccessibilitySnapshot {
    fn node_mut(&mut self, id: AccessibilityNodeId) -> Option<&mut AccessibilityNode> {
        self.nodes.iter_mut().find(|node| node.id == id)
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
