//! Windows tray icon.
//!
//! Linux exports a StatusNotifierItem over D-Bus; Windows has no session bus, so
//! the icon is a `Shell_NotifyIcon` entry owned by a hidden message-only window
//! on a dedicated thread. The actions it reports are the same
//! [`TrayAction`] values the D-Bus service emits, so the UI code does not branch.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, HWND_MESSAGE,
    IDI_APPLICATION, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WNDCLASSW,
};

use super::payload::{ItemKind, MenuItem};

/// Something the user did to the tray icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayAction {
    /// The icon was left-clicked.
    Activate,
    /// A menu row was clicked, identified by its `MenuItem::id`.
    MenuItem(i32),
}

const WM_TRAY: u32 = WM_APP + 1;
const WM_TRAY_TOOLTIP: u32 = WM_APP + 2;
const WM_TRAY_STOP: u32 = WM_APP + 3;
const ICON_ID: u32 = 1;
const CLASS_NAME: &str = "LinuxSoundboardTrayWindow";

type Handler = Arc<dyn Fn(TrayAction) + Send + Sync>;

struct Shared {
    handler: Option<Handler>,
    menu: Vec<MenuItem>,
    tooltip: String,
    hwnd: isize,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            handler: None,
            menu: Vec::new(),
            tooltip: String::new(),
            hwnd: 0,
        }
    }
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default()))
}

pub(crate) struct TrayService {
    hwnd: isize,
    live: bool,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TrayService {
    pub(crate) fn start(
        menu: Vec<MenuItem>,
        handler: impl Fn(TrayAction) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        {
            let mut shared = shared().lock();
            shared.handler = Some(Arc::new(handler));
            shared.menu = menu;
            shared.tooltip = "Linux Soundboard".to_string();
            shared.hwnd = 0;
        }

        let (ready_tx, ready_rx) = mpsc::channel::<Result<isize, String>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("win-tray".to_string())
            .spawn(move || tray_thread_main(ready_tx, thread_stop))
            .map_err(|error| error.to_string())?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                live: true,
                stop,
                thread: Mutex::new(Some(handle)),
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("Timed out creating the Windows tray icon".to_string()),
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live
    }

    pub(crate) fn set_menu(&self, items: Vec<MenuItem>) {
        shared().lock().menu = items;
    }

    pub(crate) fn set_tooltip(&self, description: &str) {
        shared().lock().tooltip = description.to_string();
        if self.hwnd != 0 {
            // SAFETY: the handle belongs to the tray thread's window and is
            // only read by it; posting a message is thread-safe.
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_TRAY_TOOLTIP, 0, 0);
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if self.hwnd != 0 {
            // SAFETY: as above, posting to our own window from another thread.
            unsafe {
                PostMessageW(self.hwnd as HWND, WM_TRAY_STOP, 0, 0);
            }
        }
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
        shared().lock().handler = None;
    }
}

fn tray_thread_main(ready: mpsc::Sender<Result<isize, String>>, stop: Arc<AtomicBool>) {
    // SAFETY: every handle used below is either null-by-design (the process
    // instance) or created in this function, and all of them are released before
    // it returns.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        if instance.is_null() {
            let _ = ready.send(Err("Could not read the module handle".to_string()));
            return;
        }

        let class_name = wide(CLASS_NAME);
        let window_name = wide("Linux Soundboard");
        let class = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: LoadIconW(std::ptr::null_mut(), IDI_APPLICATION),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&class) == 0 {
            let _ = ready.send(Err("Could not register the tray window class".to_string()));
            return;
        }

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err("Could not create the tray window".to_string()));
            return;
        }

        shared().lock().hwnd = hwnd as isize;
        if !add_icon(hwnd) {
            let _ = ready.send(Err("Could not add the tray icon".to_string()));
            let _ = DestroyWindow(hwnd);
            shared().lock().hwnd = 0;
            return;
        }
        let _ = ready.send(Ok(hwnd as isize));

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        remove_icon(hwnd);
        let _ = DestroyWindow(hwnd);
        shared().lock().hwnd = 0;
        stop.store(true, Ordering::Relaxed);
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn emit(action: TrayAction) {
    let handler = shared().lock().handler.clone();
    if let Some(handler) = handler {
        handler(action);
    }
}

/// SAFETY: `hwnd` must be the tray thread's window handle.
unsafe fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = std::mem::zeroed();
    data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = hwnd;
    data.uID = ICON_ID;
    data
}

fn copy_tooltip(data: &mut NOTIFYICONDATAW) {
    let tooltip = shared().lock().tooltip.clone();
    data.szTip.fill(0);
    let mut units: Vec<u16> = tooltip.encode_utf16().collect();
    units.truncate(data.szTip.len() - 1);
    data.szTip[..units.len()].copy_from_slice(&units);
}

/// SAFETY: `hwnd` must be the tray thread's window handle.
unsafe fn add_icon(hwnd: HWND) -> bool {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
    copy_tooltip(&mut data);
    Shell_NotifyIconW(NIM_ADD, &data) != 0
}

/// SAFETY: `hwnd` must be the tray thread's window handle.
unsafe fn remove_icon(hwnd: HWND) {
    let data = icon_data(hwnd);
    Shell_NotifyIconW(NIM_DELETE, &data);
}

/// SAFETY: `hwnd` must be the tray thread's window handle.
unsafe fn show_menu(hwnd: HWND) {
    let items = shared().lock().menu.clone();
    if items.is_empty() {
        return;
    }

    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }

    for item in &items {
        let separator = matches!(&item.kind, ItemKind::Separator);
        let (flags, label) = if separator {
            (MF_SEPARATOR, None)
        } else {
            let mut flags = MF_STRING;
            if let ItemKind::Checkmark(checked) = &item.kind {
                if *checked {
                    flags |= MF_CHECKED;
                }
            }
            if !item.enabled {
                flags |= MF_GRAYED;
            }
            (flags, Some(wide(&item.label)))
        };
        let pointer = label
            .as_ref()
            .map_or(std::ptr::null(), |label| label.as_ptr());
        AppendMenuW(menu, flags, item.id as usize, pointer);
    }

    let mut point = POINT { x: 0, y: 0 };
    if GetCursorPos(&mut point) == 0 {
        let _ = DestroyMenu(menu);
        return;
    }

    // A popup menu only dismisses itself when its owner is the foreground
    // window, and the WM_NULL afterwards clears the stale activation.
    SetForegroundWindow(hwnd);
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        point.x,
        point.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    let _ = DestroyMenu(menu);
    PostMessageW(hwnd, WM_NULL, 0, 0);

    if command != 0 {
        emit(TrayAction::MenuItem(command));
    }
}

/// SAFETY: called by the window manager for this thread's window only.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            match (lparam as u32) & 0xFFFF {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => emit(TrayAction::Activate),
                WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            0
        }
        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            if id != 0 {
                emit(TrayAction::MenuItem(id));
            }
            0
        }
        WM_TRAY_TOOLTIP => {
            let mut data = icon_data(hwnd);
            data.uFlags = NIF_TIP;
            copy_tooltip(&mut data);
            Shell_NotifyIconW(NIM_MODIFY, &data);
            0
        }
        WM_TRAY_STOP => {
            remove_icon(hwnd);
            let _ = DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}
