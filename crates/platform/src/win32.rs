//! Minimal Win32 adapter: the notification-area icon, its menu, balloon notices and the per-user
//! Run key. Every raw call lives in this module; the rest of the crate is safe Rust.
//!
//! Invariants for every function below: pointers come from owned buffers that outlive the call, the
//! window procedure is only reached while its `Arc<TrayShared>` is alive (the tray keeps the thread
//! joined before dropping it), and every string copied into a fixed Win32 buffer is bounded first.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW,
    RegOpenKeyExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_RESPECT_QUIET_TIME, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetWindowLongPtrW, LR_DEFAULTCOLOR, MF_STRING, MSG, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCCREATE, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};

use crate::notify::{Notice, Notifier};
use crate::tray::{Tray, TrayAction, TrayError, TrayLaunch, TrayState};

const TRAY_CLASS: &str = "PostinoDaemonTray";
const TRAY_UID: u32 = 1;
const TRAY_CALLBACK: u32 = WM_APP + 1;
const TASKBAR_CREATED: &str = "TaskbarCreated";
const MAX_REGISTRY_CHARS: usize = 16_384;

struct TrayShared {
    actions: SyncSender<TrayAction>,
    hwnd: AtomicIsize,
    icon: AtomicIsize,
    taskbar_created: AtomicU32,
    state: AtomicU8,
}

impl TrayShared {
    fn send(&self, action: TrayAction) {
        // A bounded queue: a full channel drops the click rather than growing without limit.
        let _ = self.actions.try_send(action);
    }
}

/// The real tray. Starting it creates a hidden top-level window on a dedicated thread; the window
/// receives the shell callback and the `TaskbarCreated` broadcast, so the icon can be re-added after
/// Explorer restarts.
pub struct Win32Tray {
    shared: Arc<TrayShared>,
    actions: std::sync::Mutex<Receiver<TrayAction>>,
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for Win32Tray {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Win32Tray").field("hwnd", &self.shared.hwnd.load(Ordering::SeqCst)).finish_non_exhaustive()
    }
}

impl Default for Win32Tray {
    fn default() -> Self {
        Self::new()
    }
}

impl Win32Tray {
    pub fn new() -> Self {
        let (actions, receiver) = std::sync::mpsc::sync_channel(32);
        Self {
            shared: Arc::new(TrayShared {
                actions,
                hwnd: AtomicIsize::new(0),
                icon: AtomicIsize::new(0),
                taskbar_created: AtomicU32::new(0),
                state: AtomicU8::new(TrayState::Quiet as u8),
            }),
            actions: std::sync::Mutex::new(receiver),
            thread: std::sync::Mutex::new(None),
        }
    }

    /// A notifier bound to this tray's icon, or `None` before the icon exists.
    pub fn notifier(&self) -> Option<Arc<dyn Notifier>> {
        let hwnd = self.shared.hwnd.load(Ordering::SeqCst);
        (hwnd != 0).then(|| Arc::new(Win32Notifier::new(hwnd)) as Arc<dyn Notifier>)
    }
}

impl Tray for Win32Tray {
    fn start(&self, launch: &TrayLaunch) -> Result<(), TrayError> {
        if self.shared.hwnd.load(Ordering::SeqCst) != 0 {
            return Ok(());
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let shared = Arc::clone(&self.shared);
        let launch = launch.clone();
        let thread = std::thread::Builder::new()
            .name("postino-tray".to_owned())
            .spawn(move || run_tray(shared, &launch, &ready_tx))
            .map_err(|error| TrayError::Unavailable(error.to_string()))?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {
                if let Ok(mut slot) = self.thread.lock() {
                    *slot = Some(thread);
                }
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(TrayError::Unavailable(error))
            }
            Err(_) => {
                // Do not join a thread that never acknowledged readiness: it may still be blocked.
                // Close its window if one already exists and detach the handle; the daemon serves on.
                let hwnd = self.shared.hwnd.load(Ordering::SeqCst);
                if hwnd != 0 {
                    // SAFETY: `hwnd`, when non-zero, was created by this tray's thread and is valid
                    // until that thread handles WM_CLOSE; posting is non-blocking.
                    unsafe {
                        PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
                    }
                }
                Err(TrayError::Unavailable("the tray window did not start in time".to_owned()))
            }
        }
    }

    fn set_state(&self, state: TrayState) -> Result<(), TrayError> {
        let hwnd = self.shared.hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return Err(TrayError::Stopped);
        }
        self.shared.state.store(state as u8, Ordering::SeqCst);
        let tip = state.tooltip();
        let mut data = notify_data(hwnd as HWND, null_mut(), NIF_TIP, TRAY_CALLBACK, tip);
        copy_wide(&mut data.szTip, tip);
        // SAFETY: `data` is a valid, fully initialised NOTIFYICONDATAW for the lifetime of the call
        // and `hwnd` was created by this tray and has not been destroyed.
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
            return Err(TrayError::Unavailable("the shell refused the icon update".to_owned()));
        }
        Ok(())
    }

    fn next_action(&self) -> Option<TrayAction> {
        self.actions.lock().ok()?.try_recv().ok()
    }

    fn stop(&self) {
        let hwnd = self.shared.hwnd.swap(0, Ordering::SeqCst);
        if hwnd != 0 {
            // SAFETY: `hwnd` belongs to the tray thread, which owns the message queue until joined.
            unsafe {
                PostMessageW(hwnd as HWND, WM_CLOSE, 0, 0);
            }
        }
        if let Ok(mut slot) = self.thread.lock()
            && let Some(thread) = slot.take()
        {
            let _ = thread.join();
        }
    }

    fn notifier(&self) -> Option<Arc<dyn Notifier>> {
        Win32Tray::notifier(self)
    }
}

impl Drop for Win32Tray {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_tray(shared: Arc<TrayShared>, _launch: &TrayLaunch, ready: &SyncSender<Result<(), String>>) {
    // SAFETY: a null module name asks for the current executable, which always exists.
    let instance = unsafe { GetModuleHandleW(null()) };
    let class_name = wide(TRAY_CLASS);
    let window_class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance, lpszClassName: class_name.as_ptr(), ..Default::default() };
    // SAFETY: `window_class` points at `class_name`, which outlives the call. A duplicate class in
    // the same process is not an error: the existing class has our procedure.
    let atom = unsafe { RegisterClassW(&window_class) };
    if atom == 0 && std::io::Error::last_os_error().raw_os_error().is_none() {
        let _ = ready.send(Err("the tray window class could not be registered".to_owned()));
        return;
    }
    // SAFETY: the class name is NUL-terminated, all handles may be null for a hidden top-level
    // window, and the shared pointer stays alive because this thread owns the `Arc` until it exits.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            Arc::as_ptr(&shared) as *const c_void,
        )
    };
    if hwnd.is_null() {
        let _ = ready.send(Err(format!("the tray window could not be created: {}", last_error())));
        // SAFETY: the class was registered by this process and no window uses it.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::UnregisterClassW(class_name.as_ptr(), instance);
        }
        return;
    }
    shared.hwnd.store(hwnd as isize, Ordering::SeqCst);
    let Some(icon) = create_icon() else {
        let _ = ready.send(Err("the tray icon could not be loaded".to_owned()));
        shared.hwnd.store(0, Ordering::SeqCst);
        // SAFETY: `hwnd` is a live window created above.
        unsafe {
            DestroyWindow(hwnd);
        }
        return;
    };
    shared.icon.store(icon as isize, Ordering::SeqCst);
    let mut data = notify_data(hwnd, icon, NIF_ICON | NIF_MESSAGE | NIF_TIP, TRAY_CALLBACK, TrayState::Quiet.tooltip());
    copy_wide(&mut data.szTip, TrayState::Quiet.tooltip());
    // SAFETY: `data` is a valid NOTIFYICONDATAW and `hwnd`/`icon` are live handles created above.
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) } == 0 {
        let _ = ready.send(Err(format!("the notification area refused the icon: {}", last_error())));
        shared.hwnd.store(0, Ordering::SeqCst);
        // SAFETY: both handles were created above and are owned here.
        unsafe {
            DestroyIcon(icon);
            DestroyWindow(hwnd);
        }
        return;
    }
    let taskbar = wide(TASKBAR_CREATED);
    // SAFETY: the string is NUL-terminated and outlives the call.
    let taskbar_message = unsafe { RegisterWindowMessageW(taskbar.as_ptr()) };
    shared.taskbar_created.store(taskbar_message, Ordering::SeqCst);
    let _ = ready.send(Ok(()));

    let mut message = MSG::default();
    loop {
        // SAFETY: `message` is writable and the window belongs to this thread.
        let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
        if result == 0 || result == -1 {
            break;
        }
        // SAFETY: `message` was filled by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    let data = notify_data(hwnd, icon, 0, TRAY_CALLBACK, "");
    // SAFETY: the icon is still present (or the call harmlessly fails after Explorer exited).
    unsafe {
        Shell_NotifyIconW(NIM_DELETE, &data);
        DestroyIcon(icon);
    }
    shared.icon.store(0, Ordering::SeqCst);
    shared.hwnd.store(0, Ordering::SeqCst);
    // SAFETY: window and class belong to this thread and no other window uses the class.
    unsafe {
        DestroyWindow(hwnd);
        windows_sys::Win32::UI::WindowsAndMessaging::UnregisterClassW(class_name.as_ptr(), instance);
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        // SAFETY: for WM_NCCREATE the system passes a live CREATESTRUCTW whose `lpCreateParams` is
        // the pointer given to CreateWindowExW. The owning thread keeps its Arc until the loop ends.
        let shared = unsafe { (*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::CREATESTRUCTW)).lpCreateParams } as *const TrayShared;
        // SAFETY: `hwnd` is the window being created and the value is only read by this procedure.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, shared as isize);
        }
        // SAFETY: default processing is required for window creation.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    // SAFETY: only this procedure writes GWLP_USERDATA, and it writes a pointer that outlives the
    // window procedure for every message after WM_NCCREATE.
    let shared = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const TrayShared;
    if !shared.is_null() {
        // SAFETY: non-null means WM_NCCREATE installed the tray-owned pointer.
        let shared = unsafe { &*shared };
        let taskbar = shared.taskbar_created.load(Ordering::SeqCst);
        if taskbar != 0 && message == taskbar {
            let icon = shared.icon.load(Ordering::SeqCst) as *mut c_void;
            let state = state_from_u8(shared.state.load(Ordering::SeqCst));
            if !icon.is_null() {
                let mut data = notify_data(hwnd, icon, NIF_ICON | NIF_MESSAGE | NIF_TIP, TRAY_CALLBACK, state.tooltip());
                copy_wide(&mut data.szTip, state.tooltip());
                // SAFETY: the icon handle is owned by the tray thread and remains valid.
                unsafe {
                    Shell_NotifyIconW(NIM_ADD, &data);
                }
            }
            return 0;
        }
        if message == TRAY_CALLBACK {
            let event = lparam as u32;
            match event {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => shared.send(TrayAction::Open),
                WM_RBUTTONUP => show_menu(hwnd, shared),
                _ => {}
            }
            return 0;
        }
        if message == WM_COMMAND {
            let command = (wparam & 0xffff) as u32;
            if let Some(action) = action_for_command(command) {
                shared.send(action);
            }
            return 0;
        }
        if message == WM_CLOSE {
            // SAFETY: the window is live and owned by this thread; WM_DESTROY posts the quit.
            unsafe {
                DestroyWindow(hwnd);
            }
            return 0;
        }
        if message == WM_DESTROY {
            // SAFETY: ends this thread's message loop.
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            }
            return 0;
        }
    }

    // SAFETY: unhandled messages go to the default procedure with the original arguments.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn show_menu(hwnd: HWND, shared: &TrayShared) {
    // SAFETY: no preconditions for a popup menu.
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    let entries = [TrayAction::Open, TrayAction::SyncNow, TrayAction::OpenOutbox, TrayAction::Quit];
    for (index, action) in entries.iter().enumerate() {
        let label = wide(action.label());
        // SAFETY: menu is live, the label is NUL-terminated and outlives the call.
        unsafe {
            AppendMenuW(menu, MF_STRING, index + 1, label.as_ptr());
        }
    }
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: `point` is writable.
    unsafe {
        GetCursorPos(&mut point);
        SetForegroundWindow(hwnd);
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, null());
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
    let _ = shared;
}

fn action_for_command(command: u32) -> Option<TrayAction> {
    match command {
        1 => Some(TrayAction::Open),
        2 => Some(TrayAction::SyncNow),
        3 => Some(TrayAction::OpenOutbox),
        4 => Some(TrayAction::Quit),
        _ => None,
    }
}

fn state_from_u8(value: u8) -> TrayState {
    match value {
        value if value == TrayState::Syncing as u8 => TrayState::Syncing,
        value if value == TrayState::Offline as u8 => TrayState::Offline,
        value if value == TrayState::Failed as u8 => TrayState::Failed,
        _ => TrayState::Quiet,
    }
}

fn notify_data(hwnd: HWND, icon: *mut c_void, flags: u32, callback: u32, _tip: &str) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: flags,
        uCallbackMessage: callback,
        hIcon: icon,
        ..Default::default()
    }
}

fn copy_wide(buffer: &mut [u16], text: &str) {
    let limit = buffer.len().saturating_sub(1);
    let mut index = 0;
    for unit in text.encode_utf16().take(limit) {
        if let Some(slot) = buffer.get_mut(index) {
            *slot = unit;
            index += 1;
        }
    }
    if let Some(slot) = buffer.get_mut(index) {
        *slot = 0;
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> String {
    std::io::Error::last_os_error().raw_os_error().map_or_else(|| "unknown error".to_owned(), |code| format!("Windows error {code}"))
}

fn create_icon() -> Option<*mut c_void> {
    const ICO: &[u8] = include_bytes!("../../../assets/app-icon/postino.ico");
    let image = icon_image(ICO, 32)?;
    // SAFETY: `image` points into a static, immutable byte array; the returned icon is owned here.
    let icon = unsafe { CreateIconFromResourceEx(image.as_ptr(), u32::try_from(image.len()).ok()?, 1, 0x0003_0000, 32, 32, LR_DEFAULTCOLOR) };
    (!icon.is_null()).then_some(icon)
}

/// Read one image from an ICO directory, preferring `preferred` pixels. Safe so the parser is tested
/// without a desktop.
fn icon_image(data: &[u8], preferred: u16) -> Option<&[u8]> {
    if data.len() < 6 || read_u16(data, 2)? != 1 {
        return None;
    }
    let count = usize::from(read_u16(data, 4)?);
    let mut best: Option<(u16, &[u8])> = None;
    for index in 0..count {
        let entry = 6usize.checked_add(index.checked_mul(16)?)?;
        if entry.checked_add(16)? > data.len() {
            break;
        }
        let width = data.get(entry).copied().map_or(256, |value| if value == 0 { 256 } else { u16::from(value) });
        let size = usize::try_from(read_u32(data, entry + 8)?).ok()?;
        let offset = usize::try_from(read_u32(data, entry + 12)?).ok()?;
        let end = offset.checked_add(size)?;
        if size == 0 || end > data.len() {
            continue;
        }
        let score = width.abs_diff(preferred);
        if best.is_none_or(|(best_score, _)| score < best_score) {
            best = Some((score, &data[offset..end]));
        }
    }
    best.map(|(_, image)| image)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([*bytes.first()?, *bytes.get(1)?, *bytes.get(2)?, *bytes.get(3)?]))
}

/// Balloon notices bound to the tray icon. No callback window is created here: the tray owns it.
pub struct Win32Notifier {
    hwnd: isize,
}

impl Win32Notifier {
    fn new(hwnd: isize) -> Self {
        Self { hwnd }
    }
}

impl Notifier for Win32Notifier {
    fn notify(&self, notice: &Notice) -> Result<(), crate::notify::NotifyError> {
        if self.hwnd == 0 {
            return Err(crate::notify::NotifyError::Unavailable("the tray icon is not present".to_owned()));
        }
        let mut data = notify_data(self.hwnd as HWND, null_mut(), NIF_INFO, TRAY_CALLBACK, "");
        copy_wide(&mut data.szInfoTitle, &notice.title);
        copy_wide(&mut data.szInfo, &notice.body);
        data.dwInfoFlags = NIIF_INFO | NIIF_RESPECT_QUIET_TIME;
        // SAFETY: `data` is valid for the call; the tray keeps the window alive.
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } == 0 {
            return Err(crate::notify::NotifyError::Unavailable("the shell refused the notification".to_owned()));
        }
        Ok(())
    }
}

/// A registry failure that keeps the numeric Win32 code, so a caller can tell `ERROR_ACCESS_DENIED`
/// from a genuine bug without matching English words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError {
    pub code: Option<u32>,
    pub message: String,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn registry_error(operation: &str, code: u32) -> RegistryError {
    RegistryError { code: Some(code), message: format!("{operation} failed with Windows error {code}") }
}

fn registry_failure(message: &str) -> RegistryError {
    RegistryError { code: None, message: message.to_owned() }
}

pub fn registry_read_string(key: &str, value: &str) -> Result<Option<String>, RegistryError> {
    if key.is_empty() || value.is_empty() || key.encode_utf16().count() > MAX_REGISTRY_CHARS || value.encode_utf16().count() > MAX_REGISTRY_CHARS {
        return Err(registry_failure("registry key or value name is too long"));
    }
    let key = wide(key);
    let value = wide(value);
    let mut buffer = vec![0u16; MAX_REGISTRY_CHARS];
    let mut bytes = u32::try_from(buffer.len().saturating_mul(2)).map_err(|_| registry_failure("registry buffer is too large"))?;
    // SAFETY: both strings are NUL-terminated, the output buffer is writable for `bytes` bytes and
    // the size pointer is valid. RRF_RT_REG_SZ restricts the result to a string.
    let result = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, null_mut(), buffer.as_mut_ptr().cast::<c_void>(), &mut bytes)
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if result != 0 {
        return Err(registry_error("reading autostart", result));
    }
    let units = usize::try_from(bytes).unwrap_or(0) / 2;
    let units = units.min(buffer.len());
    let end = buffer.iter().take(units).position(|unit| *unit == 0).unwrap_or(units);
    Ok(Some(String::from_utf16_lossy(buffer.get(..end).unwrap_or(&[]))))
}

pub fn registry_write_string(key: &str, value: &str, data: &str) -> Result<(), RegistryError> {
    if data.encode_utf16().count() > MAX_REGISTRY_CHARS {
        return Err(registry_failure("the autostart command is too long"));
    }
    let key_wide = wide(key);
    let value_wide = wide(value);
    let data_wide = wide(data);
    let mut handle: HKEY = null_mut();
    // SAFETY: the key path is NUL-terminated, the output handle is writable, and no security
    // attributes or class are needed for an HKCU key.
    let created = unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, key_wide.as_ptr(), 0, null(), 0, KEY_QUERY_VALUE | KEY_SET_VALUE, null(), &mut handle, null_mut())
    };
    if created != 0 {
        return Err(registry_error("opening the autostart key", created));
    }
    let bytes = u32::try_from(data_wide.len().saturating_mul(2)).map_err(|_| registry_failure("the autostart command is too long"))?;
    // SAFETY: `handle` is open, both names are NUL-terminated and `data_wide` is valid for `bytes`.
    let written = unsafe { RegSetValueExW(handle, value_wide.as_ptr(), 0, REG_SZ, data_wide.as_ptr().cast::<u8>(), bytes) };
    // SAFETY: `handle` was opened above and is closed exactly once.
    unsafe {
        RegCloseKey(handle);
    }
    if written != 0 {
        return Err(registry_error("writing the autostart value", written));
    }
    Ok(())
}

pub fn registry_delete_value(key: &str, value: &str) -> Result<(), RegistryError> {
    let key_wide = wide(key);
    let value_wide = wide(value);
    let mut handle: HKEY = null_mut();
    // SAFETY: the key path is NUL-terminated and the output handle is writable.
    let opened = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_wide.as_ptr(), 0, KEY_SET_VALUE, &mut handle) };
    if opened == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if opened != 0 {
        return Err(registry_error("opening the autostart key", opened));
    }
    // SAFETY: `handle` is open and the value name is NUL-terminated.
    let deleted = unsafe { RegDeleteValueW(handle, value_wide.as_ptr()) };
    // SAFETY: `handle` was opened above and is closed exactly once.
    unsafe {
        RegCloseKey(handle);
    }
    if deleted == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    if deleted != 0 {
        return Err(registry_error("deleting the autostart value", deleted));
    }
    Ok(())
}

#[cfg(test)]
pub fn registry_delete_tree(key: &str) -> Result<(), RegistryError> {
    let key_wide = wide(key);
    // SAFETY: the key path is NUL-terminated; deleting a missing test key is not an error.
    let deleted = unsafe { windows_sys::Win32::System::Registry::RegDeleteTreeW(HKEY_CURRENT_USER, key_wide.as_ptr()) };
    if deleted != 0 && deleted != ERROR_FILE_NOT_FOUND {
        return Err(registry_error("deleting the test key", deleted));
    }
    Ok(())
}

pub fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    let Some(source) = source.to_str() else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the source path is not valid UTF-16 text"));
    };
    let Some(destination) = destination.to_str() else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "the destination path is not valid UTF-16 text"));
    };
    let source = wide(source);
    let destination = wide(destination);
    // SAFETY: both paths are NUL-terminated and valid for the call; the flags replace the target
    // only after the source is durable, which is the atomic-replace contract used for daemon.json.
    let replaced = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_ico() -> Vec<u8> {
        let mut ico = Vec::new();
        ico.extend_from_slice(&[0, 0, 1, 0, 2, 0]);
        for (width, offset, size) in [(16u8, 38u32, 4u32), (32, 42, 6)] {
            ico.extend_from_slice(&[width, width, 0, 0]);
            ico.extend_from_slice(&1u16.to_le_bytes());
            ico.extend_from_slice(&32u16.to_le_bytes());
            ico.extend_from_slice(&size.to_le_bytes());
            ico.extend_from_slice(&offset.to_le_bytes());
        }
        ico.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        ico
    }

    #[test]
    fn icon_parser_prefers_requested_size_and_bounds_offsets() {
        let ico = tiny_ico();
        assert_eq!(icon_image(&ico, 32), Some(&[5u8, 6, 7, 8, 9, 10][..]));
        assert_eq!(icon_image(&ico, 16), Some(&[1u8, 2, 3, 4][..]));
        assert_eq!(icon_image(&ico, 256), Some(&[5u8, 6, 7, 8, 9, 10][..]));
        assert_eq!(icon_image(&ico[..4], 32), None);
        assert_eq!(icon_image(&[0, 0, 1, 0, 1, 0, 16, 16], 16), None);
    }

    #[test]
    fn bundled_icon_has_windows_frames() {
        let ico: &[u8] = include_bytes!("../../../assets/app-icon/postino.ico");
        assert!(ico.len() > 16 * 1024);
        for size in [16u16, 32, 48, 256] {
            assert!(icon_image(ico, size).is_some(), "missing {size}px frame");
        }
        assert!(icon_image(ico, 32).is_some_and(|image| image.starts_with(b"\x89PNG")));
    }

    #[test]
    fn win32_tray_probe_is_opt_in() {
        if std::env::var("POSTINO_WIN32_UI_TESTS").as_deref() != Ok("1") {
            return;
        }
        let tray = crate::Win32Tray::new();
        let launch = crate::tray::TrayLaunch { store: std::path::PathBuf::from(r"C:\Postino\tests\mail.db") };
        crate::tray::Tray::start(&tray, &launch).expect("the tray window and icon start");
        std::thread::sleep(std::time::Duration::from_millis(300));
        crate::tray::Tray::set_state(&tray, crate::tray::TrayState::Syncing).expect("icon state updates");
        if let Some(notifier) = crate::tray::Tray::notifier(&tray) {
            notifier.notify(&crate::notify::Notice::new("Postino", "Probe: no personal data.")).expect("balloon notice");
        }
        crate::tray::Tray::stop(&tray);
    }
}
