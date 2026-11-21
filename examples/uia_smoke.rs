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

    // SAFETY: 创建任何 UIA 对象之前先为本线程初始化 COM 套间；RPC_E_CHANGED_MODE 只表示
    // 已有别的套间模式在用，无害。
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    let exe = std::env::current_exe().map_err(|error| format!("current_exe failed: {error}"))?;
    let mut host = std::process::Command::new(&exe)
        .arg("--host")
        .spawn()
        .map_err(|error| format!("host spawn failed: {error}"))?;

    let outcome = (|| -> Result<(), String> {
        let hwnd = wait_for_host_window(&mut host)?;
        // SAFETY: CUIAutomation 是文档化的进程内 coclass，套间已初始化。
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("CoCreateInstance(CUIAutomation) failed: {error}"))?;
        // SAFETY: `hwnd` 是宿主进程的存活窗口。
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|error| format!("ElementFromHandle failed: {error}"))?;

        // (1) 根是 Window 元素。
        let root_type = current_control_type(&root)?;
        if root_type != UIA_WindowControlTypeId.0 {
            return Err(format!(
                "root control type was {root_type}, expected Window"
            ));
        }

        // (2) 拍平后的语义树露出 Text 与 Button，隐藏按钮绝不出现。
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

        // (3) 按钮属性。
        // SAFETY: 只在宿主窗口存活期间查询 provider 元素。
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

        // (4) BoundingRectangle 落在真实窗口矩形内（屏幕物理像素）。
        let window_rect = window_rect(hwnd)?;
        // SAFETY: 同上。
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

        // (5) SetFocus 的效果能经系统 UIA 客户端看到。
        // SAFETY: 同上。
        unsafe { button.SetFocus() }.map_err(|error| format!("SetFocus failed: {error}"))?;
        poll_until(Duration::from_secs(3), || {
            // SAFETY: 同上。
            unsafe { button.CurrentHasKeyboardFocus() }
                .map(|value| value.as_bool())
                .unwrap_or(false)
        })
        .map_err(|_| "HasKeyboardFocus stayed false after SetFocus".to_owned())?;

        // (6) 经真实的 UIA 模式调用 Invoke，再观察动态标签变化。
        // SAFETY: 模式 id 是文档化的 Invoke 模式。
        let pattern: IUIAutomationInvokePattern = unsafe {
            button.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        }
        .map_err(|error| format!("InvokePattern unavailable: {error}"))?;
        // SAFETY: 模式取自存活的 provider 元素。
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

        // (7) 宿主退出后，手里的旧元素必须报不可用，不能卡住或崩溃。
        let _ = host.kill();
        let _ = host.wait();
        std::thread::sleep(Duration::from_millis(500));
        // SAFETY: 故意查询已失效的元素，这里失败才是预期结果。
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
        // SAFETY: 类名传空匹配任意类，标题是静态宽字符串字面量。
        // `FindWindowW` 把「没有这个窗口」报成 `Err(HRESULT(0))`，这里是正常的未找到，不是失败。
        if let Ok(hwnd) = unsafe { FindWindowW(None, w!("WindUI UIA smoke host")) } {
            if !hwnd.is_invalid() {
                let mut pid = 0_u32;
                // SAFETY: `hwnd` 来自 FindWindowW，`pid` 是有效的输出指针。
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

    // SAFETY: `automation` 是存活的 IUIAutomation，条件每次调用现建。
    let condition = unsafe { automation.CreateTrueCondition() }
        .map_err(|error| format!("CreateTrueCondition failed: {error}"))?;
    // SAFETY: `root` 是存活元素，条件活过这次调用。
    let found = unsafe { root.FindAll(TreeScope_Descendants, &condition) }
        .map_err(|error| format!("FindAll failed: {error}"))?;
    // SAFETY: `found` 是存活的元素数组。
    let count = unsafe { found.Length() }.map_err(|error| format!("Length failed: {error}"))?;
    let mut elements = Vec::new();
    for index in 0..count {
        // SAFETY: 下标在上面报告的数组长度之内。
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
    // SAFETY: element 是存活的 provider 元素。
    unsafe { element.CurrentName() }
        .map(|value| value.to_string())
        .map_err(|error| format!("CurrentName failed: {error}"))
}

#[cfg(windows)]
fn current_control_type(
    element: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> Result<i32, String> {
    // SAFETY: element 是存活的 provider 元素。
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
    // SAFETY: `hwnd` 是存活的宿主窗口，`rect` 是有效的输出指针。
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
