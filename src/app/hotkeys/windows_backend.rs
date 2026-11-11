//! Global hotkeys on Windows via a low-level keyboard hook.
//!
//! `RegisterHotKey` cannot report key releases, which push-to-loop needs, so
//! this backend installs a `WH_KEYBOARD_LL` hook on a dedicated thread and
//! reports both presses and releases through the same channel the Linux
//! backends use.

use std::any::Any;
use std::collections::HashSet;
use std::sync::mpsc::SyncSender;
use std::sync::OnceLock;
use std::thread::{self, JoinHandle};

use parking_lot::Mutex;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use super::backend_runtime::{try_dispatch_hotkey, try_dispatch_hotkey_release, HotkeyBackend};
use super::error::{unsupported_key_for_backend, HotkeyError};
use super::parsing::{parse_hotkey_spec, HotkeyCode, HotkeyModifier, HotkeySpec};

const MOD_CTRL: u32 = 1;
const MOD_ALT: u32 = 2;
const MOD_SHIFT: u32 = 4;
const MOD_SUPER: u32 = 8;

struct Binding {
    id: String,
    modifiers: u32,
    vk: u32,
}

#[derive(Default)]
struct Shared {
    bindings: Vec<Binding>,
    sender: Option<SyncSender<String>>,
    pressed: HashSet<u32>,
}

fn shared() -> &'static Mutex<Shared> {
    static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| Mutex::new(Shared::default()))
}

pub struct WindowsBackend {
    thread: Mutex<Option<JoinHandle<()>>>,
    thread_id: Mutex<Option<u32>>,
}

impl WindowsBackend {
    pub fn new() -> Result<Self, HotkeyError> {
        Ok(Self {
            thread: Mutex::new(None),
            thread_id: Mutex::new(None),
        })
    }

    fn start_hook_thread(&self) -> Result<(), HotkeyError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        let handle = thread::Builder::new()
            .name("win-hotkey-hook".to_string())
            .spawn(move || hook_thread_main(ready_tx))
            .map_err(|error| HotkeyError::BackendUnavailable(error.to_string()))?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(thread_id)) => {
                *self.thread.lock() = Some(handle);
                *self.thread_id.lock() = Some(thread_id);
                Ok(())
            }
            Ok(Err(error)) => Err(HotkeyError::BackendUnavailable(error)),
            Err(_) => Err(HotkeyError::BackendUnavailable(
                "Timed out installing the Windows keyboard hook".to_string(),
            )),
        }
    }
}

impl HotkeyBackend for WindowsBackend {
    fn name(&self) -> &'static str {
        "windows"
    }

    fn validate_hotkey(&self, hotkey: &str) -> Result<(), HotkeyError> {
        let spec = parse_hotkey_spec(hotkey)?;
        virtual_key(&spec.key)?;
        Ok(())
    }

    fn register(&self, id: &str, hotkey: &str) -> Result<(), HotkeyError> {
        let spec = parse_hotkey_spec(hotkey)?;
        let binding = binding_from_spec(id, &spec)?;
        let mut shared = shared().lock();
        shared.bindings.retain(|existing| existing.id != id);
        shared.bindings.push(binding);
        Ok(())
    }

    fn unregister(&self, id: &str) -> Result<(), HotkeyError> {
        shared().lock().bindings.retain(|binding| binding.id != id);
        Ok(())
    }

    fn start_listener(&self, sender: SyncSender<String>) {
        {
            let mut shared = shared().lock();
            shared.sender = Some(sender);
        }
        if self.thread.lock().is_none() {
            if let Err(error) = self.start_hook_thread() {
                log::warn!("Windows hotkey hook unavailable: {error}");
            }
        }
    }

    fn shutdown(&self) {
        if let Some(thread_id) = self.thread_id.lock().take() {
            // SAFETY: posts a thread message to the hook thread; no pointers.
            unsafe {
                PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
        shared().lock().sender = None;
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn binding_from_spec(id: &str, spec: &HotkeySpec) -> Result<Binding, HotkeyError> {
    let mut modifiers = 0;
    for modifier in &spec.modifiers {
        modifiers |= match modifier {
            HotkeyModifier::Ctrl => MOD_CTRL,
            HotkeyModifier::Alt => MOD_ALT,
            HotkeyModifier::Shift => MOD_SHIFT,
            HotkeyModifier::Super => MOD_SUPER,
            // Windows has no distinct AltGr modifier.
            HotkeyModifier::AltGr => MOD_CTRL | MOD_ALT,
        };
    }
    Ok(Binding {
        id: id.to_string(),
        modifiers,
        vk: virtual_key(&spec.key)?,
    })
}

fn virtual_key(code: &HotkeyCode) -> Result<u32, HotkeyError> {
    let token = code.token();
    if let Some(letter) = token.strip_prefix("Key") {
        let bytes = letter.as_bytes();
        if bytes.len() == 1 && bytes[0].is_ascii_uppercase() {
            return Ok(bytes[0] as u32);
        }
    }
    if let Some(digit) = token.strip_prefix("Digit") {
        if let Ok(value) = digit.parse::<u32>() {
            return Ok(0x30 + value);
        }
    }
    if let Some(rest) = token.strip_prefix('F') {
        if let Ok(number) = rest.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Ok(0x70 + number - 1);
            }
        }
    }
    if let Some(rest) = token.strip_prefix("Numpad") {
        match rest {
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                if let Ok(value) = rest.parse::<u32>() {
                    return Ok(0x60 + value);
                }
            }
            "Add" => return Ok(0x6B),
            "Subtract" => return Ok(0x6D),
            "Multiply" => return Ok(0x6A),
            "Divide" => return Ok(0x6F),
            "Decimal" => return Ok(0x6E),
            "Enter" => return Ok(0x0D),
            _ => {}
        }
    }

    let vk = match token {
        "Space" => 0x20,
        "Tab" => 0x09,
        "Enter" => 0x0D,
        "Escape" => 0x1B,
        "Backspace" => 0x08,
        "CapsLock" => 0x14,
        "Delete" => 0x2E,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "End" => 0x23,
        "PrintScreen" => 0x2C,
        "ScrollLock" => 0x91,
        "Pause" => 0x13,
        "PageUp" => 0x21,
        "PageDown" => 0x22,
        "ArrowUp" => 0x26,
        "ArrowDown" => 0x28,
        "ArrowLeft" => 0x25,
        "ArrowRight" => 0x27,
        "Quote" => 0xDE,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "Semicolon" => 0xBA,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "Backquote" => 0xC0,
        _ => {
            return Err(unsupported_key_for_backend(
                "windows",
                format!("{token} is not supported on Windows"),
            ))
        }
    };
    Ok(vk)
}

fn current_modifiers() -> u32 {
    // SAFETY: queries global key state; no pointers or lifetimes involved.
    unsafe {
        let mut modifiers = 0;
        if GetAsyncKeyState(VK_CONTROL as i32) < 0 {
            modifiers |= MOD_CTRL;
        }
        if GetAsyncKeyState(VK_MENU as i32) < 0 {
            modifiers |= MOD_ALT;
        }
        if GetAsyncKeyState(VK_SHIFT as i32) < 0 {
            modifiers |= MOD_SHIFT;
        }
        if GetAsyncKeyState(VK_LWIN as i32) < 0 || GetAsyncKeyState(VK_RWIN as i32) < 0 {
            modifiers |= MOD_SUPER;
        }
        modifiers
    }
}

fn hook_thread_main(ready: std::sync::mpsc::Sender<Result<u32, String>>) {
    // SAFETY: installing a global low-level hook with no DLL and a static
    // callback; the hook is removed before the thread exits.
    let hook =
        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), std::ptr::null_mut(), 0) };
    if hook.is_null() {
        let _ = ready.send(Err(format!(
            "SetWindowsHookExW failed: {}",
            std::io::Error::last_os_error()
        )));
        return;
    }
    // SAFETY: called from the hook thread after installation.
    let thread_id = unsafe { GetCurrentThreadId() };
    let _ = ready.send(Ok(thread_id));

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: standard message pump; message is a valid destination.
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        // SAFETY: message dispatch requires the message from GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // SAFETY: removes the hook installed by this thread.
    unsafe {
        UnhookWindowsHookEx(hook);
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let pressed = wparam == WM_KEYDOWN as usize || wparam == WM_SYSKEYDOWN as usize;
        let released = wparam == WM_KEYUP as usize || wparam == WM_SYSKEYUP as usize;
        if pressed || released {
            // SAFETY: for a low-level keyboard hook, lparam points to a
            // KBDLLHOOKSTRUCT valid for the duration of this callback.
            let data = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            dispatch_key(data.vkCode, pressed, released);
        }
    }
    // SAFETY: forwards the event to the next hook in the chain.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

fn dispatch_key(vk: u32, pressed: bool, released: bool) {
    let mut shared = shared().lock();
    let Some(sender) = shared.sender.clone() else {
        return;
    };

    if pressed {
        if !shared.pressed.insert(vk) {
            return;
        }
        let modifiers = current_modifiers();
        let matches: Vec<String> = shared
            .bindings
            .iter()
            .filter(|binding| binding.vk == vk && binding.modifiers == modifiers)
            .map(|binding| binding.id.clone())
            .collect();
        for id in matches {
            try_dispatch_hotkey(&sender, id);
        }
    } else if released {
        shared.pressed.remove(&vk);
        // Modifiers often release first, so match on the key alone.
        let matches: Vec<String> = shared
            .bindings
            .iter()
            .filter(|binding| binding.vk == vk)
            .map(|binding| binding.id.clone())
            .collect();
        for id in matches {
            try_dispatch_hotkey_release(&sender, &id);
        }
    }
}
