//! System tray icon for the Windows build.
//!
//! The icon has its own hidden window. A message-only window cannot own a
//! menu, and a hidden main window stops painting, so Show Lantern asks
//! Windows to restore the real window directly.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{mpsc, Mutex};

pub enum Event {
    Show,
    Quit,
}

static SENDER: Mutex<Option<mpsc::Sender<Event>>> = Mutex::new(None);
static HWND_SLOT: AtomicIsize = AtomicIsize::new(0);
/// Explorer broadcasts this when it restarts. Every tray icon has to be added again.
#[cfg(windows)]
static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// True for Explorer's "taskbar is back" message. 0 means it was never registered.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_taskbar_created(msg: u32, registered: u32) -> bool {
    registered != 0 && msg == registered
}

/// Starts the tray icon. On Windows this is a real notification-area icon.
/// Elsewhere the receiver never produces events.
pub fn start() -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    #[cfg(windows)]
    {
        if let Ok(mut slot) = SENDER.lock() {
            *slot = Some(tx);
        }
        std::thread::spawn(|| unsafe { run() });
    }
    #[cfg(not(windows))]
    {
        drop(tx);
    }
    rx
}

/// Removes the icon when the process is leaving.
pub fn shutdown() {
    #[cfg(windows)]
    unsafe {
        let hwnd = HWND_SLOT.load(Ordering::Acquire) as winapi::shared::windef::HWND;
        if !hwnd.is_null() {
            winapi::um::winuser::PostMessageW(hwnd, winapi::um::winuser::WM_CLOSE, 0, 0);
        }
    }
}

#[cfg(windows)]
unsafe fn run() {
    use std::ptr::null_mut;
    use winapi::shared::minwindef::{LPARAM, LRESULT, UINT, WPARAM};
    use winapi::shared::windef::{HWND, POINT};
    use winapi::um::libloaderapi::GetModuleHandleW;
    use winapi::um::shellapi::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use winapi::um::winuser::{
        CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
        GetCursorPos, GetMessageW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassExW,
        SetForegroundWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION, MF_STRING,
        TPM_NONOTIFY, TPM_RETURNCMD, WM_CLOSE, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
        WM_NULL, WM_RBUTTONUP, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP, WNDCLASSEXW,
    };

    const WM_TRAY: UINT = winapi::um::winuser::WM_USER + 20;
    const SHOW_ID: usize = 1;
    const QUIT_ID: usize = 2;

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: UINT, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            let click = lparam as UINT;
            if click == WM_LBUTTONUP || click == WM_LBUTTONDBLCLK {
                reveal();
                notify(Event::Show);
                return 0;
            }
            if click == WM_RBUTTONUP {
                let mut point = POINT { x: 0, y: 0 };
                GetCursorPos(&mut point);
                let menu = CreatePopupMenu();
                if !menu.is_null() {
                    let show = wide("Show Lantern");
                    let quit = wide("Quit");
                    winapi::um::winuser::AppendMenuW(menu, MF_STRING, SHOW_ID, show.as_ptr());
                    winapi::um::winuser::AppendMenuW(menu, MF_STRING, QUIT_ID, quit.as_ptr());
                    SetForegroundWindow(hwnd);
                    let picked = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_NONOTIFY,
                        point.x,
                        point.y,
                        0,
                        hwnd,
                        null_mut(),
                    ) as usize;
                    // Required so the menu actually returns the item that was clicked.
                    PostMessageW(hwnd, WM_NULL, 0, 0);
                    DestroyMenu(menu);
                    if picked == SHOW_ID {
                        reveal();
                        notify(Event::Show);
                    } else if picked == QUIT_ID {
                        notify(Event::Quit);
                    }
                }
                return 0;
            }
        }
        if is_taskbar_created(msg, TASKBAR_CREATED.load(Ordering::Acquire)) {
            // Explorer restarted (a crash or an update) and the icon is gone.
            // Without it a window hidden to the tray could never be shown again.
            add_icon(hwnd);
            return 0;
        }
        if msg == WM_CLOSE || msg == WM_DESTROY {
            remove_icon(hwnd);
            PostQuitMessage(0);
            return 0;
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    let class_name = wide("LanternTray");
    let hinstance = GetModuleHandleW(null_mut());
    let mut class: WNDCLASSEXW = std::mem::zeroed();
    class.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
    class.lpfnWndProc = Some(wndproc);
    class.hInstance = hinstance;
    class.lpszClassName = class_name.as_ptr();
    RegisterClassExW(&class);

    // A message-only window cannot own a menu, so Show Lantern never came back.
    // This one is a real window, just never shown.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class_name.as_ptr(),
        wide("LanternTray").as_ptr(),
        WS_POPUP,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        hinstance,
        null_mut(),
    );
    if hwnd.is_null() {
        return;
    }
    HWND_SLOT.store(hwnd as isize, Ordering::Release);

    let taskbar = wide("TaskbarCreated");
    TASKBAR_CREATED.store(winapi::um::winuser::RegisterWindowMessageW(taskbar.as_ptr()), Ordering::Release);
    add_icon(hwnd);

    let mut message = std::mem::zeroed();
    while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    HWND_SLOT.store(0, Ordering::Release);

    unsafe fn add_icon(hwnd: HWND) {
        let hinstance = GetModuleHandleW(null_mut());
        let mut icon = LoadIconW(hinstance, winapi::um::winuser::MAKEINTRESOURCEW(1));
        if icon.is_null() {
            icon = LoadIconW(null_mut(), IDI_APPLICATION);
        }
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = icon;
        let tip = wide("Lantern");
        for (index, unit) in tip.iter().take(data.szTip.len() - 1).enumerate() {
            data.szTip[index] = *unit;
        }
        Shell_NotifyIconW(NIM_ADD, &mut data);
    }

    unsafe fn remove_icon(hwnd: HWND) {
        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        Shell_NotifyIconW(NIM_DELETE, &mut data);
    }
}

#[cfg(windows)]
fn notify(event: Event) {
    if let Ok(slot) = SENDER.lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(event);
        }
    }
}

/// Brings the Lantern window back. The UI loop is asleep while the window is
/// hidden, so the tray thread has to ask Windows directly.
#[cfg(windows)]
pub fn reveal() {
    use winapi::shared::minwindef::{BOOL, LPARAM};
    use winapi::shared::windef::HWND;
    use winapi::um::processthreadsapi::GetCurrentProcessId;
    use winapi::um::winuser::{
        BringWindowToTop, EnumWindows, GetClassNameW, GetWindowTextW, GetWindowThreadProcessId,
        SetForegroundWindow, SetWindowPos, ShowWindow, HWND_TOP, SWP_NOMOVE, SWP_NOSIZE,
        SWP_SHOWWINDOW, SW_RESTORE, SW_SHOW,
    };

    struct Find {
        pid: u32,
        hwnd: HWND,
    }

    unsafe extern "system" fn each(hwnd: HWND, param: LPARAM) -> BOOL {
        let find = &mut *(param as *mut Find);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != find.pid {
            return 1;
        }
        let mut class = [0u16; 64];
        let class_len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        if class_len > 0 && String::from_utf16_lossy(&class[..class_len as usize]) == "LanternTray" {
            return 1;
        }
        let mut title = [0u16; 64];
        let title_len = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        if title_len > 0 && String::from_utf16_lossy(&title[..title_len as usize]) == "Lantern" {
            find.hwnd = hwnd;
            return 0;
        }
        1
    }

    unsafe {
        let mut find = Find {
            pid: GetCurrentProcessId(),
            hwnd: std::ptr::null_mut(),
        };
        EnumWindows(Some(each), &mut find as *mut Find as LPARAM);
        if find.hwnd.is_null() {
            return;
        }
        ShowWindow(find.hwnd, SW_RESTORE);
        ShowWindow(find.hwnd, SW_SHOW);
        SetWindowPos(
            find.hwnd,
            HWND_TOP,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        BringWindowToTop(find.hwnd);
        SetForegroundWindow(find.hwnd);
    }
}

#[cfg(not(windows))]
pub fn reveal() {}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn explorer_restart_message_is_recognized_only_once_registered() {
        assert!(super::is_taskbar_created(0xC0F1, 0xC0F1));
        assert!(!super::is_taskbar_created(0xC0F2, 0xC0F1));
        assert!(!super::is_taskbar_created(0, 0));
    }
}
