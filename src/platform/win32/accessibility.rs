//! WindUI 窗口的原生 UI Automation provider。
//!
//! provider 刻意做成无状态的 COM 外壳：只存 HWND 和带代际校验的语义目标。每次属性、导航、
//! 聚焦、点击请求都经下面的私有窗口消息，同步交给窗口所属的 UI 线程求值。这样 UIA 就不会
//! 变成第二棵树、一层缓存，或者跨线程持有 `Tree` 的一方。

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

/// WM_APP+3 留给同步的 UIA 桥接调用。WM_APP+1 是托盘消息，WM_APP+2 是跨线程唤醒消息。
pub(crate) const WM_APP_ACCESSIBILITY: u32 = WM_APP + 3;

#[derive(Clone, Copy, Debug)]
pub(crate) enum BridgeOperation {
    Snapshot,
    Action {
        id: AccessibilityNodeId,
        action: AccessibilityAction,
    },
}

/// 栈上的请求。指针只在同步的 `SendMessageW` 期间使用，指针和 WindUI 状态都不会逃出
/// UI 线程的消息处理函数。
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
    // SAFETY: 请求在本栈帧上，`SendMessageW` 同步返回，窗口过程不保存这个指针。HWND 来自
    // 存活的 provider；发送失败按元素不可用处理。
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

/// 「这里没有元素 / 不支持这个模式」：UIA 约定返回 `S_OK` 加空输出指针，而不是失败码。
///
/// windows-rs 的接口类型背后是 `NonNull`，`Ok` 里放不进空接口。`Error::empty()` 的错误码是
/// `S_OK`（windows-result 0.4），生成的 vtable 胶水走 `Err` 分支时不写输出指针，直接返回
/// 这个 `S_OK`，输出保持调用方预置的 NULL。返回 E_FAIL 的话，Navigate 走到末尾的兄弟、
/// 查询不支持的模式都会被客户端当成真正的错误。
fn not_present<T>() -> Result<T> {
    Err(Error::empty())
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
        // 名称为空时返回 VT_EMPTY，UIA 会退回宿主 provider（`UiaHostProviderFromHwnd`）给的
        // 值，根节点因此仍能读到窗口标题，而不是被空串盖掉。
        id if id == UIA_NamePropertyId.0 && node.name.is_empty() => VARIANT::default(),
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
        return not_present();
    }
    let (_, node) = node_for(state)?;
    if node.role != AccessibilityRole::Button
        || !node
            .supported_actions
            .contains(&AccessibilityAction::Invoke)
    {
        return not_present();
    }
    Ok(InvokeProvider(state).into())
}

fn fragment_for_neighbor(
    state: ProviderState,
    snapshot: &AccessibilitySnapshot,
    id: Option<AccessibilityNodeId>,
) -> Result<IRawElementProviderFragment> {
    match id {
        Some(id) => Ok(provider_for(state.hwnd, snapshot, id)),
        None => not_present(),
    }
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
                return not_present();
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

/// 把语义矩形（逻辑 DIP）换算成客户区物理像素。
///
/// 与 [`scaled_screen_rect`] 里的系统调用分开，100% / 150% 的换算就能脱离 HWND 单测。
/// 快照本身从不改写，始终是逻辑坐标，只有这一步投影是物理的。
fn scale_logical_rect(bounds: crate::geometry::Rect, scale: f64) -> (i32, i32, i32, i32) {
    (
        (f64::from(bounds.x) * scale).round() as i32,
        (f64::from(bounds.y) * scale).round() as i32,
        (f64::from(bounds.right()) * scale).round() as i32,
        (f64::from(bounds.bottom()) * scale).round() as i32,
    )
}

fn scaled_screen_rect(state: ProviderState, node: &AccessibilityNode) -> Result<UiaRect> {
    // SAFETY: `state.hwnd` 是创建本元素的 provider 的存活窗口句柄；调用只读窗口 DPI，
    // 失败返回 0，由后面的 `.max(96)` 夹到文档规定的 96 DPI 基线。
    let dpi = unsafe { GetDpiForWindow(state.hwnd) }.max(96);
    let scale = f64::from(dpi) / 96.0;
    let (x, y, right, bottom) = scale_logical_rect(node.bounds, scale);
    let mut origin = POINT { x, y };
    // SAFETY: origin 是栈上有效的 POINT，hwnd 是 provider 的存活窗口句柄。
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
    // SAFETY: SafeArrayCreateVector 分配一个 COM 持有的 VT_I4 数组，成功后所有权交给 UIA。
    // 每次写入都用有效下标和指向存活 i32 的指针。
    let array = unsafe { SafeArrayCreateVector(VT_I4, 0, values.len() as u32) };
    if array.is_null() {
        return Err(Error::from_thread());
    }
    for (index, value) in values.iter().enumerate() {
        let index = index as i32;
        // SAFETY: `array` 刚在上面分配、尚未交给 UIA，`index` 不超过 `values.len()`，
        // 两个参数在调用期间都指向本栈帧持有的存活存储。
        if unsafe { SafeArrayPutElement(array, &index, value as *const i32 as *const _) }.is_err() {
            // SAFETY: array 在上面分配，尚未交给 UIA。
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
    match id {
        Some(id) => Ok(provider_for(state.hwnd, snapshot, id)),
        None => not_present(),
    }
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
    match best {
        Some((_, id)) => Ok(provider_for(state.hwnd, snapshot, id)),
        None => not_present(),
    }
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
        // 非根 fragment 没有宿主 provider：S_OK + NULL。
        not_present()
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
            // 根节点返回 NULL，由 UIA 按宿主 HWND 生成运行时 id。
            ProviderTarget::Root => Ok(null_mut()),
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
            // SAFETY: 只有根 provider 走这个分支，构造 provider 前已确认 HWND 存活。调用只是
            // 把这个 HWND 包成 COM provider，不从 `self` 转移任何所有权。
            ProviderTarget::Root => unsafe {
                windows::Win32::UI::Accessibility::UiaHostProviderFromHwnd(self.0.hwnd)
            },
            ProviderTarget::Node(_) => not_present(),
        }
    }
}

#[allow(non_snake_case)]
impl IRawElementProviderFragment_Impl for RootProvider_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        neighbor(self.0, direction)
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // 根节点返回 NULL，由 UIA 按宿主 HWND 生成运行时 id。
        Ok(null_mut())
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
    // 只有运行时 id 的测试要读回 COM 数组，这个导入放在测试里，库构建中不会变成未使用。
    use windows::Win32::System::Ole::SafeArrayGetElement;

    #[test]
    fn runtime_id_contains_uia_marker_index_and_generation() {
        // `NodeId` 的字段对 arena 私有，所以从一次真实插入拿 id，断言无障碍 id 保留了
        // 下标与代际（不为测试单独加 `NodeId` 构造函数）。
        let mut tree = crate::core::Tree::new();
        let node_id = crate::ui::Element::button("probe").build(&mut tree);
        let id = AccessibilityNodeId::from_node_id(node_id);
        assert_eq!(id.index(), node_id.index());
        assert_eq!(id.generation(), node_id.generation());

        // 子元素的运行时 id 依次是 UIA 标记、arena 下标、代际。根节点不走这里（按宿主规则）。
        let array = runtime_id(id).expect("运行时 id 数组");
        assert!(!array.is_null());
        let read = |index: i32| -> i32 {
            let mut value = 0_i32;
            // SAFETY: `array` 刚由 `runtime_id` 分配，恰好三个 VT_I4 元素，这里传入的下标都在
            // 范围内，目标是本栈帧上存活的 i32。
            unsafe { SafeArrayGetElement(array, &index, &mut value as *mut i32 as *mut _) }
                .expect("运行时 id 下标在范围内");
            value
        };
        assert_eq!(read(0), UiaAppendRuntimeId as i32);
        assert_eq!(read(1), id.index() as i32);
        assert_eq!(read(2), id.generation() as i32);
        // SAFETY: 这个数组没交给过 UIA，所有权仍在这里，由这里释放。
        let _ = unsafe { SafeArrayDestroy(array) };
    }

    #[test]
    fn not_present_reports_s_ok_with_null_output() {
        // 「没有这个元素」必须是 S_OK：E_FAIL 会被客户端当成真正的错误。
        let error = not_present::<IRawElementProviderFragment>().unwrap_err();
        assert_eq!(HRESULT::from(error), HRESULT(0));
    }

    #[test]
    fn bounds_scale_is_dpi_correct_at_100_and_150_percent() {
        // `Rect::new(x, y, w, h)`：right = 40，bottom = 60。
        let logical = crate::geometry::Rect::new(10, 20, 30, 40);
        // 100%：投影是恒等变换。
        assert_eq!(scale_logical_rect(logical, 1.0), (10, 20, 40, 60));
        // 150%：每条边缩放后取整，宽高保持一致。
        let (x, y, right, bottom) = scale_logical_rect(logical, 1.5);
        assert_eq!((x, y, right, bottom), (15, 30, 60, 90));
        assert_eq!(right - x, 45, "150% 下宽度必须是 30 DIP 的 1.5 倍");
        assert_eq!(bottom - y, 60, "150% 下高度必须是 40 DIP 的 1.5 倍");
        // 小数边必须四舍五入而不是截断。
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
        // 与 `bridge_action` 里的兜底一致。
        let mapped = request
            .action_result
            .unwrap_or(AccessibilityActionResult::NodeUnavailable);
        assert!(matches!(mapped, AccessibilityActionResult::NodeUnavailable));
    }

    #[test]
    fn null_hwnd_query_reports_unavailable() {
        // 窗口已销毁的 provider 必须报元素不可用，绝不能 panic。
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
        // 来自另一个 arena 的 id 不能解析成功：校验带代际，另一棵树报不可用，而不是点到别的节点。
        let mut source = crate::core::Tree::new();
        let node_id = crate::ui::Element::button("probe").build(&mut source);
        let stale = AccessibilityNodeId::from_node_id(node_id);
        let empty = crate::core::Tree::new();
        assert!(matches!(
            empty.validate_accessibility_action(stale, AccessibilityAction::Invoke),
            AccessibilityActionResult::NodeUnavailable
        ));
    }

    /// 语义快照是拍平过的；下面两个辅助函数负责导航与命中测试，是快照的纯函数，所以不需要
    /// HWND 就能测（包着它们的 provider 方法才需要真实窗口）。
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
            .expect("快照必须有根");
        let deepest = snapshot
            .nodes
            .iter()
            .map(|node| depth(&snapshot, node.id))
            .max()
            .expect("快照必须有根");
        assert_eq!(root_depth, 0, "语义根没有祖先");
        assert!(deepest > root_depth, "嵌套的按钮必须比根更深");
    }
}
