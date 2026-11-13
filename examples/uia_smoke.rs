//! 双进程真实 HWND 的 OS UIA 冒烟：验证 Win32 raw UIA provider 能被**独立进程**的
//! UI Automation 客户端看到。
//!
//! 用法：
//!   cargo run --example uia_smoke            # 客户端（父进程，默认）
//!   cargo run --example uia_smoke -- --host  # 宿主（子进程，被父进程拉起）
//!
//! 之所以要两个进程：provider 与 client 在同一 UI 线程里会互相读到自己的缓存，
//! 只有跨进程才真正经过 `WM_GETOBJECT` 与 OS 的 UIA 客户端栈。

#[cfg(not(windows))]
fn main() {
    eprintln!("uia_smoke: Windows only");
}

#[cfg(windows)]
fn main() {
    let is_host = std::env::args().any(|argument| argument == "--host");
    match if is_host { run_host() } else { run_client() } {
        Ok(()) => {}
        Err(error) => {
            eprintln!("uia_smoke FAILED: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
const HOST_TITLE: &str = "WindUI UIA smoke host";
#[cfg(windows)]
const STATIC_LABEL: &str = "Accessibility smoke";
#[cfg(windows)]
const DYNAMIC_INITIAL: &str = "Invoked 0";
#[cfg(windows)]
const DYNAMIC_AFTER: &str = "Invoked 1";
#[cfg(windows)]
const BUTTON_NAME: &str = "Invoke me";
#[cfg(windows)]
const HIDDEN_NAME: &str = "Hidden button";

/// 宿主：一个普通 WindUI 窗口，内容固定。Button 的回调只把动态标签改成 `Invoked 1`。
#[cfg(windows)]
fn run_host() -> Result<(), String> {
    use windui::prelude::*;

    let invoked = windui::signal::signal(false);
    let dynamic = invoked.map(|flag| {
        if *flag {
            DYNAMIC_AFTER.to_owned()
        } else {
            DYNAMIC_INITIAL.to_owned()
        }
    });

    let ui = Element::col()
        .fill()
        .padding(16)
        .spacing(12)
        .child(
            Element::label(STATIC_LABEL)
                .font_size(18.0)
                .width_match()
                .height(28),
        )
        .child(
            Element::label_signal(dynamic)
                .font_size(16.0)
                .width_match()
                .height(24),
        )
        .child(
            Element::row()
                .width_match()
                .height(40)
                .spacing(12)
                .child(Element::button(BUTTON_NAME).on_click(move |_| invoked.set(true)))
                .child(Element::button(HIDDEN_NAME).visible(false)),
        );

    App::new(HOST_TITLE, 420, 260).content(ui).run();
    Ok(())
}

#[cfg(windows)]
fn run_client() -> Result<(), String> {
    use std::time::Duration;

    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
        UIA_ButtonControlTypeId, UIA_InvokePatternId, UIA_TextControlTypeId,
        UIA_WindowControlTypeId,
    };

    // SAFETY: the COM apartment is initialized for this thread before any UIA object is created;
    // RPC_E_CHANGED_MODE only means another apartment mode is already active, which is harmless.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    let exe = std::env::current_exe().map_err(|error| format!("current_exe failed: {error}"))?;
    let mut host = std::process::Command::new(&exe)
        .arg("--host")
        .spawn()
        .map_err(|error| format!("host spawn failed: {error}"))?;

    let outcome = (|| -> Result<(), String> {
        let hwnd = wait_for_host_window(&mut host)?;
        // SAFETY: CUIAutomation is a documented in-proc coclass and the apartment is initialized.
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("CoCreateInstance(CUIAutomation) failed: {error}"))?;
        // SAFETY: `hwnd` is a live window owned by the host process.
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|error| format!("ElementFromHandle failed: {error}"))?;

        // (1) root is the Window element.
        let root_type = current_control_type(&root)?;
        if root_type != UIA_WindowControlTypeId.0 {
            return Err(format!(
                "root control type was {root_type}, expected Window"
            ));
        }

        // (2) the flattened semantic tree exposes Text and Button, and never the hidden button.
        let elements = descendants(&automation, &root)?;
        let mut names = Vec::new();
        let mut button: Option<IUIAutomationElement> = None;
        let mut saw_static = false;
        let mut saw_dynamic = false;
        for element in &elements {
            let name = current_name(element)?;
            let kind = current_control_type(element)?;
            names.push(format!("{name}({kind})"));
            if name == HIDDEN_NAME {
                return Err("hidden button was exposed to UIA".to_owned());
            }
            if kind == UIA_TextControlTypeId.0 && name == STATIC_LABEL {
                saw_static = true;
            }
            if kind == UIA_TextControlTypeId.0 && name == DYNAMIC_INITIAL {
                saw_dynamic = true;
            }
            if kind == UIA_ButtonControlTypeId.0 && name == BUTTON_NAME {
                button = Some(element.clone());
            }
        }
        if !saw_static {
            return Err(format!("static label missing; saw {names:?}"));
        }
        if !saw_dynamic {
            return Err(format!("initial dynamic label missing; saw {names:?}"));
        }
        let button = button.ok_or_else(|| format!("invoke button missing; saw {names:?}"))?;

        // (3) Button properties.
        // SAFETY: provider elements are only queried while the host window is alive.
        let enabled = unsafe { button.CurrentIsEnabled() }
            .map_err(|error| format!("CurrentIsEnabled failed: {error}"))?
            .as_bool();
        let focusable = unsafe { button.CurrentIsKeyboardFocusable() }
            .map_err(|error| format!("CurrentIsKeyboardFocusable failed: {error}"))?
            .as_bool();
        if !enabled {
            return Err("invoke button reported disabled".to_owned());
        }
        if !focusable {
            return Err("invoke button reported not keyboard focusable".to_owned());
        }

        // (4) BoundingRectangle is inside the real window rectangle (physical screen pixels).
        let window_rect = window_rect(hwnd)?;
        // SAFETY: same as above.
        let bounds = unsafe { button.CurrentBoundingRectangle() }
            .map_err(|error| format!("CurrentBoundingRectangle failed: {error}"))?;
        if bounds.left < window_rect.left
            || bounds.top < window_rect.top
            || bounds.right > window_rect.right
            || bounds.bottom > window_rect.bottom
        {
            return Err(format!(
                "button bounds {bounds:?} escaped window rect {window_rect:?}"
            ));
        }

        // (5) SetFocus is visible through the OS UIA client.
        // SAFETY: same as above.
        unsafe { button.SetFocus() }.map_err(|error| format!("SetFocus failed: {error}"))?;
        poll_until(Duration::from_secs(3), || {
            // SAFETY: same as above.
            unsafe { button.CurrentHasKeyboardFocus() }
                .map(|value| value.as_bool())
                .unwrap_or(false)
        })
        .map_err(|_| "HasKeyboardFocus stayed false after SetFocus".to_owned())?;

        // (6) Invoke through the real UIA pattern, then observe the dynamic label change.
        // SAFETY: the pattern id is the documented Invoke pattern.
        let pattern: IUIAutomationInvokePattern = unsafe {
            button.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        }
        .map_err(|error| format!("InvokePattern unavailable: {error}"))?;
        // SAFETY: the pattern was obtained from a live provider element.
        unsafe { pattern.Invoke() }.map_err(|error| format!("Invoke failed: {error}"))?;
        poll_until(Duration::from_secs(5), || {
            let Ok(elements) = descendants(&automation, &root) else {
                return false;
            };
            elements.iter().any(|element| {
                current_control_type(element) == Ok(UIA_TextControlTypeId.0)
                    && current_name(element).as_deref() == Ok(DYNAMIC_AFTER)
            })
        })
        .map_err(|_| format!("dynamic label never became {DYNAMIC_AFTER}"))?;
        println!(
            "uia_smoke OK: root=Window, {} descendant(s) [{}], Invoke produced {DYNAMIC_AFTER}",
            names.len(),
            names.join(" | ")
        );

        // (7) After the host dies, held elements must fail as unavailable, not hang or crash.
        let _ = host.kill();
        let _ = host.wait();
        std::thread::sleep(Duration::from_millis(500));
        // SAFETY: intentionally querying a stale element; a failure here is the expected outcome.
        let stale = unsafe { button.CurrentName() };
        match stale {
            Err(error) => println!("stale element reported: {error}"),
            Ok(name) => {
                return Err(format!(
                    "stale element still answered with {:?}",
                    name.to_string()
                ))
            }
        }
        Ok(())
    })();

    if outcome.is_err() {
        let _ = host.kill();
        let _ = host.wait();
    }
    outcome
}

#[cfg(windows)]
fn wait_for_host_window(
    host: &mut std::process::Child,
) -> Result<windows::Win32::Foundation::HWND, String> {
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowThreadProcessId};

    let host_pid = host.id();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(status) = host
            .try_wait()
            .map_err(|error| format!("host try_wait failed: {error}"))?
        {
            return Err(format!("host exited before showing a window: {status}"));
        }
        // SAFETY: a null class name matches any class; the title is a static wide literal.
        // `FindWindowW` reports "no such window" as `Err(HRESULT(0))`, which is a normal
        // not-found result here rather than a failure.
        if let Ok(hwnd) = unsafe { FindWindowW(None, w!("WindUI UIA smoke host")) } {
            if !hwnd.is_invalid() {
                let mut pid = 0_u32;
                // SAFETY: `hwnd` came from FindWindowW and `pid` is a valid out pointer.
                unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
                if pid == host_pid {
                    return Ok(hwnd);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("host window did not appear within 15s".to_owned())
}

#[cfg(windows)]
fn descendants(
    automation: &windows::Win32::UI::Accessibility::IUIAutomation,
    root: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Result<Vec<windows::Win32::UI::Accessibility::IUIAutomationElement>, String> {
    use windows::Win32::UI::Accessibility::TreeScope_Descendants;

    // SAFETY: `automation` is a live IUIAutomation and the condition is created per call.
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("CreateTrueCondition failed: {error}"))?;
    // SAFETY: `root` is a live element and the condition outlives the call.
    let found = unsafe { root.FindAll(TreeScope_Descendants, &condition) }
        .map_err(|error| format!("FindAll failed: {error}"))?;
    // SAFETY: `found` is a live element array.
    let count = unsafe { found.Length() }.map_err(|error| format!("Length failed: {error}"))?;
    let mut elements = Vec::new();
    for index in 0..count {
        // SAFETY: index is within the array length reported above.
        if let Ok(element) = unsafe { found.GetElement(index) } {
            elements.push(element);
        }
    }
    Ok(elements)
}

#[cfg(windows)]
fn current_name(
    element: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Result<String, String> {
    // SAFETY: element is a live provider element.
    unsafe { element.CurrentName() }
        .map(|value| value.to_string())
        .map_err(|error| format!("CurrentName failed: {error}"))
}

#[cfg(windows)]
fn current_control_type(
    element: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Result<i32, String> {
    // SAFETY: element is a live provider element.
    unsafe { element.CurrentControlType() }
        .map(|value| value.0)
        .map_err(|error| format!("CurrentControlType failed: {error}"))
}

#[cfg(windows)]
fn window_rect(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<windows::Win32::Foundation::RECT, String> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let mut rect = RECT::default();
    // SAFETY: `hwnd` is the live host window and `rect` is a valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut rect) }
        .map_err(|error| format!("GetWindowRect failed: {error}"))?;
    Ok(rect)
}

#[cfg(windows)]
fn poll_until(timeout: std::time::Duration, mut predicate: impl FnMut() -> bool) -> Result<(), ()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err(())
}
