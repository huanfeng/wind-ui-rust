//! Native UI Automation provider for a WindUI HWND.
//!
//! The provider is intentionally a stateless COM façade.  It stores only the HWND and the
//! generation-checked semantic target; every property, navigation, focus, and invoke request
//! is synchronously evaluated by the owning UI thread through the private window message below.
//! This keeps UIA from becoming a second tree, cache, or cross-thread `Tree` owner.

#![deny(unsafe_op_in_unsafe_fn)]

use std::mem::ManuallyDrop;
use std::ptr::null_mut;

use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement};
use windows::Win32::System::Variant::{
    VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_BSTR, VT_I4,
};
use windows::Win32::UI::Accessibility::{
    IInvokeProvider, IInvokeProvider_Impl, IRawElementProviderFragment,
    IRawElementProviderFragmentRoot, IRawElementProviderFragmentRoot_Impl,
    IRawElementProviderFragment_Impl, IRawElementProviderSimple, IRawElementProviderSimple_Impl,
    NavigateDirection, NavigateDirection_FirstChild, NavigateDirection_LastChild,
    NavigateDirection_NextSibling, NavigateDirection_Parent, NavigateDirection_PreviousSibling,
    ProviderOptions, ProviderOptions_ServerSideProvider, UIA_ButtonControlTypeId,
    UIA_ControlTypePropertyId, UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId,
    UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId, UIA_NamePropertyId,
    UIA_TextControlTypeId, UIA_WindowControlTypeId, UiaAppendRuntimeId, UiaRect,
    UIA_E_ELEMENTNOTAVAILABLE, UIA_E_ELEMENTNOTENABLED, UIA_E_NOTSUPPORTED,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_APP};
use windows_core::{implement, ComObject, Error, IUnknown, Result, BSTR, HRESULT};

use crate::accessibility::{
    AccessibilityAction, AccessibilityActionResult, AccessibilityNode, AccessibilityNodeId,
    AccessibilityRole, AccessibilitySnapshot,
};

/// WM_APP+3 is reserved for synchronous UIA bridge calls.  WM_APP+1 is the tray message and
/// WM_APP+2 is the cross-thread wake message.
pub(crate) const WM_APP_ACCESSIBILITY: u32 = WM_APP + 3;

#[derive(Clone, Copy, Debug)]
pub(crate) enum BridgeOperation {
    Snapshot,
    Action {
        id: AccessibilityNodeId,
        action: AccessibilityAction,
    },
}

/// Stack-owned request.  The pointer is used only for the duration of synchronous SendMessageW;
/// no pointer or WindUI state escapes the UI-thread message handler.
pub(crate) struct BridgeRequest {
    pub(crate) operation: BridgeOperation,
    pub(crate) delivered: bool,
    pub(crate) snapshot: Option<AccessibilitySnapshot>,
    pub(crate) action_result: Option<AccessibilityActionResult>,
}

impl BridgeRequest {
    fn snapshot() -> Self {
        Self {
            operation: BridgeOperation::Snapshot,
            delivered: false,
            snapshot: None,
            action_result: None,
        }
    }

    fn action(id: AccessibilityNodeId, action: AccessibilityAction) -> Self {
        Self {
            operation: BridgeOperation::Action { id, action },
            delivered: false,
            snapshot: None,
            action_result: None,
        }
    }
}

pub(crate) fn bridge_snapshot(hwnd: HWND) -> Option<AccessibilitySnapshot> {
    let mut request = BridgeRequest::snapshot();
    if !send_request(hwnd, &mut request) {
        return None;
    }
    request.snapshot
}

pub(crate) fn bridge_action(
    hwnd: HWND,
    id: AccessibilityNodeId,
    action: AccessibilityAction,
) -> AccessibilityActionResult {
    let mut request = BridgeRequest::action(id, action);
    if !send_request(hwnd, &mut request) {
        return AccessibilityActionResult::NodeUnavailable;
    }
    request
        .action_result
        .unwrap_or(AccessibilityActionResult::NodeUnavailable)
}

fn send_request(hwnd: HWND, request: &mut BridgeRequest) -> bool {
    // SAFETY: request is stack-owned and SendMessageW is synchronous; the window procedure does
    // not retain the pointer. The HWND is supplied by UIA for a live provider and a failed send
    // is treated as an unavailable element.
    let result = unsafe {
        SendMessageW(
            hwnd,
            WM_APP_ACCESSIBILITY,
            Some(WPARAM(0)),
            Some(LPARAM(request as *mut BridgeRequest as isize)),
        )
    };
    result.0 != 0 && request.delivered
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderTarget {
    Root,
    Node(AccessibilityNodeId),
}

#[derive(Clone, Copy, Debug)]
struct ProviderState {
    hwnd: HWND,
    target: ProviderTarget,
}

#[implement(IRawElementProviderSimple, IRawElementProviderFragment)]
struct ElementProvider(ProviderState);

#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot
)]
struct RootProvider(ProviderState);

#[implement(IInvokeProvider)]
struct InvokeProvider(ProviderState);

pub(crate) fn root_provider(hwnd: HWND) -> IRawElementProviderSimple {
    ComObject::new(RootProvider(ProviderState {
        hwnd,
        target: ProviderTarget::Root,
    }))
    .into_interface()
}

fn root_fragment(hwnd: HWND) -> IRawElementProviderFragment {
    ComObject::new(RootProvider(ProviderState {
        hwnd,
        target: ProviderTarget::Root,
    }))
    .into_interface()
}

fn element_fragment(hwnd: HWND, id: AccessibilityNodeId) -> IRawElementProviderFragment {
    ComObject::new(ElementProvider(ProviderState {
        hwnd,
        target: ProviderTarget::Node(id),
    }))
    .into_interface()
}

fn provider_for(
    hwnd: HWND,
    snapshot: &AccessibilitySnapshot,
    id: AccessibilityNodeId,
) -> IRawElementProviderFragment {
    if snapshot.root == Some(id) {
        root_fragment(hwnd)
    } else {
        element_fragment(hwnd, id)
    }
}

/// The raw-provider ABI cannot express a null COM interface: the windows-rs interface types are
/// `NonNull`-backed, so `zeroed()` is invalid and aborts the process. UIA expects a fragment to
/// report "no such element" / "no such pattern" as a failed HRESULT, which its client side
/// treats as "nothing there".
fn no_element_error() -> Error {
    Error::from_hresult(HRESULT(0x8000_4005_u32 as i32))
}

fn uia_error(code: u32) -> Error {
    Error::from_hresult(HRESULT(code as i32))
}

fn unavailable<T>() -> Result<T> {
    Err(uia_error(UIA_E_ELEMENTNOTAVAILABLE))
}

fn unsupported<T>() -> Result<T> {
    Err(uia_error(UIA_E_NOTSUPPORTED))
}

fn disabled<T>() -> Result<T> {
    Err(uia_error(UIA_E_ELEMENTNOTENABLED))
}

fn snapshot_for(state: ProviderState) -> Result<AccessibilitySnapshot> {
    bridge_snapshot(state.hwnd).ok_or_else(|| uia_error(UIA_E_ELEMENTNOTAVAILABLE))
}

fn node_for(state: ProviderState) -> Result<(AccessibilitySnapshot, AccessibilityNode)> {
    let snapshot = snapshot_for(state)?;
    let id = match state.target {
        ProviderTarget::Root => snapshot
            .root
            .ok_or_else(|| uia_error(UIA_E_ELEMENTNOTAVAILABLE))?,
        ProviderTarget::Node(id) => id,
    };
    let node = snapshot
        .node(id)
        .cloned()
        .ok_or_else(|| uia_error(UIA_E_ELEMENTNOTAVAILABLE))?;
    Ok((snapshot, node))
}

fn action_result(result: AccessibilityActionResult) -> Result<()> {
    match result {
        AccessibilityActionResult::Performed => Ok(()),
        AccessibilityActionResult::Disabled => disabled(),
        AccessibilityActionResult::NodeUnavailable | AccessibilityActionResult::NotVisible => {
            unavailable()
        }
        AccessibilityActionResult::NotFocusable | AccessibilityActionResult::UnsupportedAction => {
            unsupported()
        }
    }
}

fn control_type(role: AccessibilityRole) -> i32 {
    match role {
        AccessibilityRole::Window => UIA_WindowControlTypeId.0,
        AccessibilityRole::Text => UIA_TextControlTypeId.0,
        AccessibilityRole::Button => UIA_ButtonControlTypeId.0,
    }
}

fn bstr_variant(value: &str) -> VARIANT {
    let value = BSTR::from(value);
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: ManuallyDrop::new(value),
                },
            }),
        },
    }
}

fn bool_variant(value: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    boolVal: value.into(),
                },
            }),
        },
    }
}

fn i32_variant(value: i32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_I4,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { lVal: value },
            }),
        },
    }
}

fn property_value(state: ProviderState, property_id: i32) -> Result<VARIANT> {
    let (_, node) = node_for(state)?;
    let value = match property_id {
        id if id == UIA_NamePropertyId.0 => bstr_variant(&node.name),
        id if id == UIA_ControlTypePropertyId.0 => i32_variant(control_type(node.role)),
        id if id == UIA_IsEnabledPropertyId.0 => bool_variant(node.enabled),
        id if id == UIA_IsKeyboardFocusablePropertyId.0 => bool_variant(node.focusable),
        id if id == UIA_HasKeyboardFocusPropertyId.0 => bool_variant(node.focused),
        _ => VARIANT::default(),
    };
    Ok(value)
}

fn pattern_provider(state: ProviderState, pattern_id: i32) -> Result<IUnknown> {
    if pattern_id != UIA_InvokePatternId.0 {
        return Err(no_element_error());
    }
    let (_, node) = node_for(state)?;
    if node.role != AccessibilityRole::Button
        || !node
            .supported_actions
            .contains(&AccessibilityAction::Invoke)
    {
        return Err(no_element_error());
    }
    Ok(InvokeProvider(state).into())
}

fn fragment_for_neighbor(
    state: ProviderState,
    snapshot: &AccessibilitySnapshot,
    id: Option<AccessibilityNodeId>,
) -> Result<IRawElementProviderFragment> {
    id.map(|id| provider_for(state.hwnd, snapshot, id))
        .ok_or_else(no_element_error)
}

#[allow(non_upper_case_globals)]
fn neighbor(
    state: ProviderState,
    direction: NavigateDirection,
) -> Result<IRawElementProviderFragment> {
    let (snapshot, node) = node_for(state)?;
    let id = match direction {
        NavigateDirection_Parent => node.parent,
        NavigateDirection_FirstChild => node.children.first().copied(),
        NavigateDirection_LastChild => node.children.last().copied(),
        NavigateDirection_NextSibling | NavigateDirection_PreviousSibling => {
            let Some(parent_id) = node.parent else {
                return Err(no_element_error());
            };
            let Some(parent) = snapshot.node(parent_id) else {
                return unavailable();
            };
            let Some(index) = parent
                .children
                .iter()
                .position(|candidate| *candidate == node.id)
            else {
                return unavailable();
            };
            if direction == NavigateDirection_NextSibling {
                parent.children.get(index + 1).copied()
            } else {
                index
                    .checked_sub(1)
                    .and_then(|i| parent.children.get(i).copied())
            }
        }
        _ => return unsupported(),
    };
    fragment_for_neighbor(state, &snapshot, id)
}

/// Scales a semantic (logical DIP) rectangle into physical client pixels.
///
/// Kept separate from the OS calls in [`scaled_screen_rect`] so the 100%/150% conversions are
/// unit-testable without an HWND. The snapshot itself is never rewritten: it stays in logical
/// coordinates and only this projection is physical.
fn scale_logical_rect(bounds: crate::geometry::Rect, scale: f64) -> (i32, i32, i32, i32) {
    (
        (f64::from(bounds.x) * scale).round() as i32,
        (f64::from(bounds.y) * scale).round() as i32,
        (f64::from(bounds.right()) * scale).round() as i32,
        (f64::from(bounds.bottom()) * scale).round() as i32,
    )
}

fn scaled_screen_rect(state: ProviderState, node: &AccessibilityNode) -> Result<UiaRect> {
    // SAFETY: `state.hwnd` is the live window handle of the provider that created this element;
    // the call only reads the window's DPI and a failure returns 0, which the following
    // `.max(96)` clamps to the documented 96-DPI baseline.
    let dpi = unsafe { GetDpiForWindow(state.hwnd) }.max(96);
    let scale = f64::from(dpi) / 96.0;
    let (x, y, right, bottom) = scale_logical_rect(node.bounds, scale);
    let mut origin = POINT { x, y };
    // SAFETY: origin is a valid stack POINT and hwnd is the provider's live window handle.
    if !unsafe { ClientToScreen(state.hwnd, &mut origin) }.as_bool() {
        return unavailable();
    }
    Ok(UiaRect {
        left: f64::from(origin.x),
        top: f64::from(origin.y),
        width: f64::from(right - x),
        height: f64::from(bottom - y),
    })
}

fn runtime_id(id: AccessibilityNodeId) -> Result<*mut SAFEARRAY> {
    let values = [
        UiaAppendRuntimeId as i32,
        id.index() as i32,
        id.generation() as i32,
    ];
    // SAFETY: SafeArrayCreateVector allocates a COM-owned VT_I4 array; ownership is transferred to
    // UIA on success. Every put uses a valid element index and pointer to a live i32.
    let array = unsafe { SafeArrayCreateVector(VT_I4, 0, values.len() as u32) };
    if array.is_null() {
        return Err(Error::from_thread());
    }
    for (index, value) in values.iter().enumerate() {
        let index = index as i32;
        // SAFETY: `array` was allocated immediately above and has not been transferred to UIA
        // yet, and `index` is bounded by `values.len()`, so both arguments refer to live storage
        // owned by this stack frame for the duration of the call.
        if unsafe { SafeArrayPutElement(array, &index, value as *const i32 as *const _) }.is_err() {
            // SAFETY: array was allocated above and has not been transferred to UIA.
            let _ = unsafe { SafeArrayDestroy(array) };
            return Err(Error::from_thread());
        }
    }
    Ok(array)
}

fn focused_fragment(
    state: ProviderState,
    snapshot: &AccessibilitySnapshot,
) -> Result<IRawElementProviderFragment> {
    let id = snapshot
        .nodes
        .iter()
        .find(|node| node.focused)
        .map(|node| node.id);
    id.map(|id| provider_for(state.hwnd, snapshot, id))
        .ok_or_else(no_element_error)
}

fn deepest_at_point(
    state: ProviderState,
    snapshot: &AccessibilitySnapshot,
    x: f64,
    y: f64,
) -> Result<IRawElementProviderFragment> {
    let mut best: Option<(usize, AccessibilityNodeId)> = None;
    for node in &snapshot.nodes {
        let rect = scaled_screen_rect(state, node)?;
        let contains = x >= rect.left
            && y >= rect.top
            && x < rect.left + rect.width
            && y < rect.top + rect.height;
        if !contains {
            continue;
        }
        let depth = depth(snapshot, node.id);
        if best.is_none_or(|(best_depth, _)| depth >= best_depth) {
            best = Some((depth, node.id));
        }
    }
    best.map(|(_, id)| provider_for(state.hwnd, snapshot, id))
        .ok_or_else(no_element_error)
}

fn depth(snapshot: &AccessibilitySnapshot, mut id: AccessibilityNodeId) -> usize {
    let mut depth = 0;
    while let Some(node) = snapshot.node(id) {
        let Some(parent) = node.parent else {
            break;
        };
        depth += 1;
        id = parent;
    }
    depth
}

#[allow(non_snake_case)]
impl IRawElementProviderSimple_Impl for ElementProvider_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(
        &self,
        patternid: windows::Win32::UI::Accessibility::UIA_PATTERN_ID,
    ) -> Result<IUnknown> {
        pattern_provider(self.0, patternid.0)
    }

    fn GetPropertyValue(
        &self,
        propertyid: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID,
    ) -> Result<VARIANT> {
        property_value(self.0, propertyid.0)
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        // A fragment (non-root) reports "no host provider" as a failure; the ABI cannot express
        // a null interface.
        Err(no_element_error())
    }
}

#[allow(non_snake_case)]
impl IRawElementProviderFragment_Impl for ElementProvider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        neighbor(self.0, direction)
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        match self.0.target {
            ProviderTarget::Node(id) => runtime_id(id),
            ProviderTarget::Root => Err(Error::from_hresult(HRESULT(0x80004001u32 as i32))),
        }
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let (_, node) = node_for(self.0)?;
        scaled_screen_rect(self.0, &node)
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        let id = match self.0.target {
            ProviderTarget::Node(id) => id,
            ProviderTarget::Root => return unsupported(),
        };
        action_result(bridge_action(
            self.0.hwnd,
            id,
            AccessibilityAction::SetFocus,
        ))
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(ComObject::new(RootProvider(ProviderState {
            hwnd: self.0.hwnd,
            target: ProviderTarget::Root,
        }))
        .into_interface())
    }
}

#[allow(non_snake_case)]
impl IRawElementProviderSimple_Impl for RootProvider_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(
        &self,
        patternid: windows::Win32::UI::Accessibility::UIA_PATTERN_ID,
    ) -> Result<IUnknown> {
        pattern_provider(self.0, patternid.0)
    }

    fn GetPropertyValue(
        &self,
        propertyid: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID,
    ) -> Result<VARIANT> {
        property_value(self.0, propertyid.0)
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        match self.0.target {
            // SAFETY: this arm runs only for the Root provider, and the HWND was validated as a
            // live window before the provider was constructed. The call merely wraps that HWND in
            // a COM provider; no ownership is transferred away from `self`.
            ProviderTarget::Root => unsafe {
                windows::Win32::UI::Accessibility::UiaHostProviderFromHwnd(self.0.hwnd)
            },
            ProviderTarget::Node(_) => Err(no_element_error()),
        }
    }
}

#[allow(non_snake_case)]
impl IRawElementProviderFragment_Impl for RootProvider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        neighbor(self.0, direction)
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        Err(Error::from_hresult(HRESULT(0x80004001u32 as i32)))
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        let (_, node) = node_for(self.0)?;
        scaled_screen_rect(self.0, &node)
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        unsupported()
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(ComObject::new(RootProvider(self.0)).into_interface())
    }
}

#[allow(non_snake_case)]
impl IRawElementProviderFragmentRoot_Impl for RootProvider_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let snapshot = snapshot_for(self.0)?;
        deepest_at_point(self.0, &snapshot, x, y)
    }

    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        let snapshot = snapshot_for(self.0)?;
        focused_fragment(self.0, &snapshot)
    }
}

#[allow(non_snake_case)]
impl IInvokeProvider_Impl for InvokeProvider_Impl {
    fn Invoke(&self) -> Result<()> {
        let id = match self.0.target {
            ProviderTarget::Node(id) => id,
            ProviderTarget::Root => return unsupported(),
        };
        action_result(bridge_action(self.0.hwnd, id, AccessibilityAction::Invoke))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the runtime-id test reads a COM array back, so this import stays out of the library
    // build (where it would be unused).
    use windows::Win32::System::Ole::SafeArrayGetElement;

    #[test]
    fn runtime_id_contains_uia_marker_index_and_generation() {
        // `NodeId` keeps its fields private to the arena, so obtain a real id
        // from a tree insert and assert the accessibility id preserves both
        // index and generation (no test-only `NodeId` constructor is added).
        let mut tree = crate::core::Tree::new();
        let node_id = crate::ui::Element::button("probe").build(&mut tree);
        let id = AccessibilityNodeId::from_node_id(node_id);
        assert_eq!(id.index(), node_id.index());
        assert_eq!(id.generation(), node_id.generation());

        // The child runtime id must carry the UIA marker followed by the arena index and the
        // generation, in that order. Root never reaches here (it uses the UIA host rules).
        let array = runtime_id(id).expect("runtime id array");
        assert!(!array.is_null());
        let read = |index: i32| -> i32 {
            let mut value = 0_i32;
            // SAFETY: `array` was allocated by `runtime_id` immediately above and owns exactly
            // three VT_I4 elements, so every index passed here is in range and the destination is
            // a live i32 owned by this stack frame.
            unsafe { SafeArrayGetElement(array, &index, &mut value as *mut i32 as *mut _) }
                .expect("runtime id element in range");
            value
        };
        assert_eq!(read(0), UiaAppendRuntimeId as i32);
        assert_eq!(read(1), id.index() as i32);
        assert_eq!(read(2), id.generation() as i32);
        // SAFETY: UIA never received this array, so ownership is still ours to release.
        let _ = unsafe { SafeArrayDestroy(array) };
    }

    #[test]
    fn bounds_scale_is_dpi_correct_at_100_and_150_percent() {
        // `Rect::new(x, y, w, h)` -> right = 40, bottom = 60.
        let logical = crate::geometry::Rect::new(10, 20, 30, 40);
        // 100%: the projection is the identity.
        assert_eq!(scale_logical_rect(logical, 1.0), (10, 20, 40, 60));
        // 150%: every edge scales and rounds, and width/height stay consistent.
        let (x, y, right, bottom) = scale_logical_rect(logical, 1.5);
        assert_eq!((x, y, right, bottom), (15, 30, 60, 90));
        assert_eq!(right - x, 45, "150% width must be 1.5x the 30 DIP width");
        assert_eq!(bottom - y, 60, "150% height must be 1.5x the 40 DIP height");
        // A fractional edge must round rather than truncate.
        let odd = crate::geometry::Rect::new(0, 0, 10, 0);
        assert_eq!(scale_logical_rect(odd, 1.5).2, 15);
    }

    #[test]
    fn logical_to_physical_formula_is_dpi_stable() {
        let logical = crate::geometry::Rect::new(10, 20, 30, 40);
        let scale = 1.5_f64;
        assert_eq!((f64::from(logical.x) * scale).round(), 15.0);
        assert_eq!((f64::from(logical.bottom()) * scale).round(), 90.0);
    }

    #[test]
    fn request_that_was_not_delivered_reports_unavailable() {
        let mut tree = crate::core::Tree::new();
        let node_id = crate::ui::Element::button("probe").build(&mut tree);
        let id = AccessibilityNodeId::from_node_id(node_id);
        let request = BridgeRequest::action(id, AccessibilityAction::Invoke);
        assert!(!request.delivered);
        assert!(request.action_result.is_none());
        // Mirrors the fallback in `bridge_action`.
        let mapped = request
            .action_result
            .unwrap_or(AccessibilityActionResult::NodeUnavailable);
        assert!(matches!(mapped, AccessibilityActionResult::NodeUnavailable));
    }

    #[test]
    fn null_hwnd_query_reports_unavailable() {
        // A provider whose window is gone must fail closed as element-unavailable, never panic.
        let hwnd = HWND(std::ptr::null_mut());
        assert!(bridge_snapshot(hwnd).is_none());
        let mut tree = crate::core::Tree::new();
        let node_id = crate::ui::Element::button("probe").build(&mut tree);
        let id = AccessibilityNodeId::from_node_id(node_id);
        assert!(matches!(
            bridge_action(hwnd, id, AccessibilityAction::Invoke),
            AccessibilityActionResult::NodeUnavailable
        ));
    }

    #[test]
    fn stale_generation_action_reports_unavailable() {
        // An id obtained from a different arena must not resolve: the validation is generation
        // checked, so the other tree reports it as unavailable rather than invoking a stranger.
        let mut source = crate::core::Tree::new();
        let node_id = crate::ui::Element::button("probe").build(&mut source);
        let stale = AccessibilityNodeId::from_node_id(node_id);
        let empty = crate::core::Tree::new();
        assert!(matches!(
            empty.validate_accessibility_action(stale, AccessibilityAction::Invoke),
            AccessibilityActionResult::NodeUnavailable
        ));
    }

    /// The semantic snapshot is flattened by `app::UiHost`; these two helpers drive navigation
    /// and hit-testing and are pure functions of that snapshot, so they are testable without an
    /// HWND (the provider methods that wrap them do need a live window).
    fn probe_snapshot() -> crate::accessibility::AccessibilitySnapshot {
        use crate::geometry::Size;
        use crate::text::NullTextEngine;
        let mut tree = crate::core::Tree::new();
        let root_id = crate::ui::Element::col()
            .child(crate::ui::Element::row().child(crate::ui::Element::button("deep")))
            .build(&mut tree);
        tree.root = Some(root_id);
        let mut text = NullTextEngine;
        tree.layout_root(Size::new(400, 240), &mut text);
        tree.accessibility_snapshot()
    }

    #[test]
    fn control_type_maps_the_three_semantic_roles() {
        use windows::Win32::UI::Accessibility::{
            UIA_ButtonControlTypeId, UIA_TextControlTypeId, UIA_WindowControlTypeId,
        };
        assert_eq!(
            control_type(AccessibilityRole::Window),
            UIA_WindowControlTypeId.0
        );
        assert_eq!(
            control_type(AccessibilityRole::Text),
            UIA_TextControlTypeId.0
        );
        assert_eq!(
            control_type(AccessibilityRole::Button),
            UIA_ButtonControlTypeId.0
        );
    }

    #[test]
    fn depth_follows_the_semantic_parent_chain() {
        let snapshot = probe_snapshot();
        let root_depth = snapshot
            .nodes
            .iter()
            .map(|node| depth(&snapshot, node.id))
            .min()
            .expect("snapshot has a root");
        let deepest = snapshot
            .nodes
            .iter()
            .map(|node| depth(&snapshot, node.id))
            .max()
            .expect("snapshot has a root");
        assert_eq!(root_depth, 0, "the semantic root has no ancestors");
        assert!(
            deepest > root_depth,
            "a nested Button must be deeper than the root"
        );
    }
}
