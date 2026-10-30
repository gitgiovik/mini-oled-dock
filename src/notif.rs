//! Windows notifications and "Do not disturb".
//!
//! Dedicated `notif` THREAD (COM MTA, `CoInitializeEx(COINIT_MULTITHREADED)`): NOT the shell thread
//! (the consent prompt and async waits must not stall the audio COM pump) and never the UI
//! thread. Cycle:
//!  1. `UserNotificationListener::Current()`; error -> access `Unavailable` (the thread stays alive
//!     for DND only).
//!  2. `GetAccessStatus()`: `Allowed` -> polling; `Unspecified` and `!config.notif_prompted` -> ONE
//!     `RequestAccessAsync()` (15 s wait), then `config::modify(notif_prompted = true)`; `Denied` ->
//!     `Denied`, no further listener calls.
//!  3. Try `NotificationChanged` once; if it fails (0x80070490 expected for an exe without a package)
//!     poll: every 2 s while the popup is closed (badge only), every 500 ms while it is open.
//!  4. Each read: `GetNotificationsAsync(NotificationKinds::Toast)` -> up to `MAX_ITEMS` entries
//!     converted into plain `NotifItem`s (app, title, text, time). Never WinRT objects outside the thread.
//!  5. DND: `ToastNotificationManager::GetDefault()?.NotificationMode()` on every polling cycle;
//!     writes go through `IQuietHoursSettings` (undocumented, best effort) in `toggle_dnd`.
//!
//!  On any change: update `STATE` (Mutex) and post `WM_DOCK_NOTIF` to the UI.
//!
//! ASYNC WITHOUT `join()`: `IAsyncOperation::join` can block forever (Waiter with INFINITE in
//! Drop). Instead `SetCompleted(AsyncOperationCompletedHandler)` sends on an mpsc channel and
//! we use `recv_timeout` (5 s; 15 s for `RequestAccessAsync`). Timeout = log + access `Unavailable`.
//!
//! REMOVAL: `RemoveNotification` / `ClearNotifications` need package identity and fail for the dock
//! (0x80070490); the fallback is the per-app notification history (`history_remove`,
//! `history_clear_apps`), which does answer for an unpackaged exe.
//!
//! UI -> thread commands (`remove`, `clear_all`, `toggle_dnd`, `refresh_now`, `set_popup_open`):
//! mpsc channel; `recv_timeout` on the channel doubles as the polling wait, so a command wakes
//! the thread immediately. Never blocking for the caller.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{interface, w, IUnknown, IUnknown_Vtbl, GUID, HRESULT, HSTRING, PCWSTR, PWSTR};
use windows::ApplicationModel::AppInfo;
use windows::Foundation::{Size, TypedEventHandler};
use windows::Storage::Streams::DataReader;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, SYSTEMTIME, WPARAM};
use windows::Win32::Globalization::{GetDateFormatEx, ENUM_DATE_FORMATS_FLAGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::{
    KnownNotificationBindings, NotificationKinds, ToastNotificationManager, ToastNotificationManagerForUser,
    ToastNotificationMode, UserNotification,
};
use windows_future::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncOperation};

use crate::icons::{self, IconBitmap};
use crate::log;
use crate::msg::WM_DOCK_NOTIF;
use crate::shell_thread::{self, ShellRequest};

/// Maximum number of notifications kept (the most recent ones).
pub const MAX_ITEMS: usize = 50;
/// Maximum length (characters) of title and text; longer text is truncated with "…".
pub const MAX_TEXT_CHARS: usize = 200;

/// Maximum wait for an async WinRT operation.
const ASYNC_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum wait for the consent prompt.
const ASYNC_TIMEOUT_PROMPT: Duration = Duration::from_secs(15);
/// Polling period with the popup closed / open.
const POLL_CLOSED: Duration = Duration::from_secs(2);
const POLL_OPEN: Duration = Duration::from_millis(500);

/// "Do not disturb" profiles (`IQuietHoursSettings` strings).
const PROFILE_UNRESTRICTED: &str = "Microsoft.QuietHoursProfile.Unrestricted";
const PROFILE_PRIORITY_ONLY: &str = "Microsoft.QuietHoursProfile.PriorityOnly";
/// CLSID of the class implementing `IQuietHoursSettings`.
const CLSID_QUIET_HOURS: GUID = GUID::from_u128(0xf53321fa_34f8_4b7f_b9a3_361877cb94cf);

/// `IQuietHoursSettings` (undocumented, method order from Rafael Rivera's MIT gist): only the first
/// two slots after `IUnknown` are needed.
#[allow(non_snake_case)]
mod quiet {
    use super::*;
    #[interface("6bff4732-81ec-4ffb-ae67-b6c1bc29631f")]
    pub unsafe trait IQuietHoursSettings: IUnknown {
        pub unsafe fn UserSelectedProfile(&self, out: *mut PWSTR) -> HRESULT;
        pub unsafe fn SetUserSelectedProfile(&self, id: PCWSTR) -> HRESULT;
    }
}
use quiet::IQuietHoursSettings;

/// A notification, as plain data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotifItem {
    /// `UserNotification::Id()`, for `remove`.
    pub id: u32,
    /// App name (`AppInfo().DisplayInfo().DisplayName()`), "" if unknown.
    pub app: String,
    /// First text element of the ToastGeneric binding.
    pub title: String,
    /// Subsequent text elements joined with " ".
    pub body: String,
    /// `CreationTime` as a UTC FILETIME (100 ns since 1601).
    pub time_ft: u64,
    /// Local time already formatted "HH:mm" (or "yesterday HH:mm", "Mon 12 Oct" if not today).
    pub time_text: String,
    /// App icon key for `notif::app_icon` (AUMID, or "name:<app>"); "" = none.
    pub icon_key: String,
    /// AppUserModelID of the app that sent it ("" if unknown): used to open the app on a click and
    /// to remove the toast through the per-app notification history.
    pub aumid: String,
}

/// State of access to the notification list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NotifAccess {
    /// Not yet determined (thread starting).
    #[default]
    Unknown,
    /// List available.
    Allowed,
    /// Denied by the user / the system.
    Denied,
    /// API not available (exe without package identity, error, timeout).
    Unavailable,
}

/// Snapshot for the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NotifState {
    pub access: NotifAccess,
    /// Most recent first. Empty if `access != Allowed`.
    pub items: Vec<NotifItem>,
    /// `Some(true)` = "Do not disturb" on; `None` = state not readable (toggle hidden).
    pub dnd: Option<bool>,
}

static STATE: Mutex<Option<NotifState>> = Mutex::new(None);

/// Commands UI -> `notif` thread.
enum Cmd {
    Stop,
    PopupOpen(bool),
    Remove(u32),
    ClearAll,
    ToggleDnd,
    Refresh,
}

/// Command channel (None = thread not started).
static CMD_TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
/// Thread handle and exit signal (for the join with timeout).
static THREAD: Mutex<Option<(JoinHandle<()>, Receiver<()>)>> = Mutex::new(None);
/// true after `stop`: the thread stops even if the `Stop` command has not been seen yet.
static STOPPED: AtomicBool = AtomicBool::new(false);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn send_cmd(c: Cmd) {
    if let Some(tx) = lock(&CMD_TX).as_ref() {
        // Channel closed = thread terminated: nothing to do.
        let _ = tx.send(c);
    }
}

/// Starts the `notif` thread. Does not block. `ui_hwnd` receives `WM_DOCK_NOTIF`. Called by `main` after
/// `shell_thread::start`. A spawn error is logged; `snapshot()` stays `Unknown`/empty.
pub fn start(ui_hwnd: HWND) {
    if lock(&CMD_TX).is_some() {
        log!("notif::start: already started");
        return;
    }
    STOPPED.store(false, Relaxed);
    let (tx, rx) = mpsc::channel::<Cmd>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let ui = ui_hwnd.0 as isize;
    let tx_thread = tx.clone();
    // The icon thread is spawned on demand by `ensure_icon`, possibly from the `notif` thread's
    // first cycle: publish the target window before that thread exists.
    ICON_UI.store(ui, Relaxed);
    let spawned = std::thread::Builder::new().name("notif".into()).spawn(move || {
        thread_main(ui, rx, tx_thread);
        let _ = done_tx.send(());
    });
    match spawned {
        Ok(j) => {
            *lock(&CMD_TX) = Some(tx);
            *lock(&THREAD) = Some((j, done_rx));
        }
        Err(e) => log!("notif::start: thread spawn failed: {e}"),
    }
}

/// true if the thread was started and not stopped.
pub fn is_started() -> bool {
    lock(&CMD_TX).is_some()
}

/// Stops the thread (channel closed + wake-up, join for at most 1 s; if it is stuck in a WinRT call
/// we proceed without joining). Idempotent. Called in `cleanup`.
pub fn stop() {
    STOPPED.store(true, Relaxed);
    send_cmd(Cmd::Stop);
    *lock(&CMD_TX) = None;
    // Channel closed: the icon thread exits after its current request, if any (not awaited).
    *lock(&ICON_TX) = None;
    let Some((j, done)) = lock(&THREAD).take() else { return };
    match done.recv_timeout(Duration::from_secs(1)) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
            let _ = j.join();
        }
        Err(RecvTimeoutError::Timeout) => log!("notif: no response within 1 s, proceeding without join"),
    }
}

/// Copy of the current state (short lock).
pub fn snapshot() -> NotifState {
    lock(&STATE).clone().unwrap_or_default()
}

/// Number of notifications (badge on the bell). 0 if the list is not available.
pub fn count() -> usize {
    lock(&STATE).as_ref().map_or(0, |s| s.items.len())
}

/// Notification popup open/closed: changes the polling rate; `true` also forces a read.
pub fn set_popup_open(open: bool) {
    send_cmd(Cmd::PopupOpen(open));
}

/// Removes a notification (`RemoveNotification(id)`, else through the app history), then re-reads.
pub fn remove(id: u32) {
    send_cmd(Cmd::Remove(id));
}

/// "Clear all" (`ClearNotifications()`, else app by app through the history), then re-reads.
pub fn clear_all() {
    send_cmd(Cmd::ClearAll);
}

/// Toggles "Do not disturb" via `IQuietHoursSettings` (profile `Microsoft.QuietHoursProfile.Unrestricted`
/// <-> `...PriorityOnly`). If the interface is not available: log + `shell_thread::request(Launch
/// "ms-settings:notifications")`. The new state arrives with the immediate re-read (`WM_DOCK_NOTIF` wParam 2).
pub fn toggle_dnd() {
    send_cmd(Cmd::ToggleDnd);
}

/// Immediate re-read (list + DND).
pub fn refresh_now() {
    send_cmd(Cmd::Refresh);
}

// ------------------------------------------------------------------ thread

/// State owned by the `notif` thread.
struct Worker {
    ui: isize,
    listener: Option<UserNotificationListener>,
    access: NotifAccess,
    popup_open: bool,
    /// Last published state.
    last: NotifState,
    /// Next cycle must convert every notification (set by Refresh/Remove/ClearAll/PopupOpen).
    force_full: bool,
    /// Cached conversion state (items by id, ToastGeneric HSTRING, last time key).
    rd: Reader,
    /// Cached toast manager used for the DND read; recreated after any error.
    mgr: Option<ToastNotificationManagerForUser>,
}

/// Local date/minute key `(y, m, d, h, min)` used to decide when time labels are stale.
type TimeKey = (u16, u16, u16, u16, u16);

/// Conversion cache owned by the `notif` thread.
#[derive(Default)]
struct Reader {
    /// Items of the last read, by `UserNotification::Id`.
    cache: HashMap<u32, NotifItem>,
    /// `KnownNotificationBindings::ToastGeneric()`, created once.
    binding: Option<HSTRING>,
    /// Local time key of the last time-label formatting.
    time_key: Option<TimeKey>,
}

fn thread_main(ui: isize, rx: Receiver<Cmd>, tx: Sender<Cmd>) {
    // SAFETY: initializes COM MTA for this thread; paired with CoUninitialize at the end.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        log!("notif: CoInitializeEx failed: {hr:?}");
    }
    log!("notif thread started (COM MTA)");
    *lock(&STATE) = Some(NotifState::default());

    let mut w = Worker {
        ui,
        listener: None,
        access: NotifAccess::Unknown,
        popup_open: false,
        last: NotifState::default(),
        force_full: false,
        rd: Reader::default(),
        mgr: None,
    };
    w.init_access(&tx);
    drop(tx);
    w.cycle();

    'outer: loop {
        if STOPPED.load(Relaxed) {
            break;
        }
        let timeout = if w.popup_open { POLL_OPEN } else { POLL_CLOSED };
        match rx.recv_timeout(timeout) {
            Ok(c) => {
                if !w.handle(c) {
                    break 'outer;
                }
                // Drain accumulated commands before re-reading.
                while let Ok(c) = rx.try_recv() {
                    if !w.handle(c) {
                        break 'outer;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        w.cycle();
    }
    w.listener = None;
    if hr.is_ok() {
        // SAFETY: paired with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    log!("notif thread terminated");
}

/// Waits for `op` to complete without ever blocking for long (no `join`/`get`): the `SetCompleted`
/// handler sends the status on a channel, then `GetResults` is called on the completed operation.
fn await_op<T: windows_core::RuntimeType + 'static>(op: &IAsyncOperation<T>, timeout: Duration) -> Result<T, String> {
    let (tx, rx) = mpsc::channel::<AsyncStatus>();
    let handler = AsyncOperationCompletedHandler::new(move |_op, status| {
        let _ = tx.send(status);
        Ok(())
    });
    op.SetCompleted(&handler).map_err(|e| format!("SetCompleted: {e}"))?;
    match rx.recv_timeout(timeout) {
        Ok(AsyncStatus::Completed) => op.GetResults().map_err(|e| format!("GetResults: {e}")),
        Ok(s) => Err(format!("operation not completed (status {})", s.0)),
        Err(_) => Err(format!("timeout of {} s", timeout.as_secs())),
    }
}

impl Worker {
    /// Module steps 1-3: listener, access, prompt (once), `NotificationChanged`.
    fn init_access(&mut self, tx: &Sender<Cmd>) {
        let listener = match UserNotificationListener::Current() {
            Ok(l) => l,
            Err(e) => {
                log!("notif: UserNotificationListener::Current failed: {e} -> Unavailable");
                self.access = NotifAccess::Unavailable;
                return;
            }
        };
        let mut status = match listener.GetAccessStatus() {
            Ok(s) => s,
            Err(e) => {
                log!("notif: GetAccessStatus failed: {e} -> Unavailable");
                self.access = NotifAccess::Unavailable;
                return;
            }
        };
        log!("notif: initial access status {}", status.0);
        if status == UserNotificationListenerAccessStatus::Unspecified {
            if crate::config::get().notif_prompted {
                log!("notif: access unspecified and prompt already shown -> Denied");
                self.access = NotifAccess::Denied;
                return;
            }
            log!("notif: requesting access (RequestAccessAsync)");
            let r = listener
                .RequestAccessAsync()
                .map_err(|e| format!("RequestAccessAsync: {e}"))
                .and_then(|op| await_op(&op, ASYNC_TIMEOUT_PROMPT));
            if let Err(e) = crate::config::modify(|c| c.notif_prompted = true) {
                log!("notif: saving notif_prompted failed: {e}");
            }
            match r {
                Ok(s) => {
                    log!("notif: access request result {}", s.0);
                    status = s;
                }
                Err(e) => {
                    log!("notif: access request failed: {e} -> Unavailable");
                    self.access = NotifAccess::Unavailable;
                    return;
                }
            }
        }
        if status == UserNotificationListenerAccessStatus::Allowed {
            self.access = NotifAccess::Allowed;
            let tx = tx.clone();
            let handler = TypedEventHandler::new(move |_l, _a| {
                let _ = tx.send(Cmd::Refresh);
                Ok(())
            });
            match listener.NotificationChanged(&handler) {
                Ok(_) => log!("notif: NotificationChanged registered (polling stays active anyway)"),
                Err(e) => log!("notif: NotificationChanged not available ({e}), polling only"),
            }
            self.listener = Some(listener);
        } else {
            log!("notif: access not granted ({}) -> Denied", status.0);
            self.access = NotifAccess::Denied;
        }
    }

    /// Executes a command; false = stop.
    fn handle(&mut self, c: Cmd) -> bool {
        match c {
            Cmd::Stop => return false,
            Cmd::PopupOpen(o) => {
                self.popup_open = o;
                self.force_full |= o;
            }
            Cmd::Refresh => self.force_full = true,
            Cmd::Remove(id) => {
                self.force_full = true;
                if let Some(l) = self.listener.as_ref() {
                    if let Err(e) = l.RemoveNotification(id) {
                        // Expected for an exe without package identity (0x80070490): the toast is
                        // removed through its app's notification history instead.
                        let item = self.last.items.iter().find(|it| it.id == id);
                        match item.map(history_remove) {
                            Some(Ok(())) => log!("notif: toast {id} removed through the app history"),
                            Some(Err(e2)) => log!("notif: RemoveNotification({id}) failed: {e}; history: {e2}"),
                            None => log!("notif: RemoveNotification({id}) failed: {e}"),
                        }
                    }
                }
            }
            Cmd::ClearAll => {
                self.force_full = true;
                if let Some(l) = self.listener.as_ref() {
                    if let Err(e) = l.ClearNotifications() {
                        // Same as above: clear the history of every app that has toasts listed.
                        log!("notif: ClearNotifications failed: {e}, clearing app by app");
                        history_clear_apps(&self.last.items);
                    }
                }
            }
            Cmd::ToggleDnd => toggle_dnd_now(),
        }
        true
    }

    /// One read (list + DND) and, if anything changed, publication.
    fn cycle(&mut self) {
        let mut cur = NotifState { access: self.access, items: Vec::new(), dnd: read_dnd(&mut self.mgr) };
        // In-place updated toasts keep their Id: with the popup open (or on request) re-convert all.
        let full = self.popup_open || std::mem::take(&mut self.force_full);
        if let Some(l) = self.listener.as_ref() {
            match read_items(l, &mut self.rd, full) {
                Ok(items) => {
                    cur.items = items;
                    cur.access = NotifAccess::Allowed;
                }
                Err(e) => {
                    self.rd.binding = None;
                    self.mgr = None;
                    if self.access == NotifAccess::Allowed {
                        log!("notif: list read failed: {e} -> Unavailable");
                    }
                    cur.access = NotifAccess::Unavailable;
                }
            }
            self.access = cur.access;
        }
        if cur == self.last {
            return;
        }
        let list_changed = cur.access != self.last.access || cur.items != self.last.items;
        if list_changed {
            prune_icons(&cur.items);
        }
        self.last = cur.clone();
        *lock(&STATE) = Some(cur);
        let hwnd = HWND(self.ui as *mut core::ffi::c_void);
        let wp = if list_changed { 1 } else { 2 };
        // SAFETY: PostMessageW is thread-safe; it only fails if the window no longer exists.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_DOCK_NOTIF, WPARAM(wp), LPARAM(0)) };
    }
}

/// DND active? Error = `None` (the cached manager is dropped and recreated next cycle).
fn read_dnd(mgr: &mut Option<ToastNotificationManagerForUser>) -> Option<bool> {
    if mgr.is_none() {
        *mgr = ToastNotificationManager::GetDefault().ok();
    }
    let r = mgr.as_ref()?.NotificationMode().ok().map(|mode| mode != ToastNotificationMode::Unrestricted);
    if r.is_none() {
        *mgr = None;
    }
    r
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= MAX_TEXT_CHARS {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX_TEXT_CHARS - 1).collect();
    t.push('…');
    t
}

/// Reads the toast notifications, newest first. Known ids are served from the cache unless `full`.
fn read_items(l: &UserNotificationListener, rd: &mut Reader, full: bool) -> Result<Vec<NotifItem>, String> {
    let op = l.GetNotificationsAsync(NotificationKinds::Toast).map_err(|e| format!("GetNotificationsAsync: {e}"))?;
    let view = await_op(&op, ASYNC_TIMEOUT)?;
    let n = view.Size().map_err(|e| format!("Size: {e}"))?;
    let mut raw: Vec<(u32, u64, UserNotification)> = Vec::with_capacity(n as usize);
    for i in 0..n {
        match view.GetAt(i) {
            Ok(u) => match (u.Id(), u.CreationTime()) {
                (Ok(id), Ok(t)) => raw.push((id, t.UniversalTime.max(0) as u64, u)),
                _ => log!("notif: entry {i} skipped: Id/CreationTime"),
            },
            Err(e) => log!("notif: entry {i} skipped: {e}"),
        }
    }
    raw.sort_by(|a, b| b.1.cmp(&a.1));
    raw.truncate(MAX_ITEMS);
    let mut old = std::mem::take(&mut rd.cache);
    let mut items = Vec::with_capacity(raw.len());
    for (id, ft, u) in raw {
        if !full {
            if let Some(it) = old.remove(&id) {
                items.push(it);
                continue;
            }
        }
        if rd.binding.is_none() {
            match KnownNotificationBindings::ToastGeneric() {
                Ok(b) => rd.binding = Some(b),
                Err(e) => {
                    log!("notif: ToastGeneric failed: {e}");
                    continue;
                }
            }
        }
        let Some(binding_name) = rd.binding.as_ref() else { continue };
        match convert_item(&u, id, ft, binding_name) {
            Ok(it) => items.push(it),
            Err(e) => {
                rd.binding = None;
                log!("notif: entry {id} skipped: {e}");
            }
        }
    }
    // Labels are recomputed for new items, or for all when the local minute/date changed.
    let now = now_ft();
    let key = local_time(now).map(|t| (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute));
    let stale = key != rd.time_key;
    rd.time_key = key;
    for it in &mut items {
        if stale || it.time_text.is_empty() {
            it.time_text = format_time(it.time_ft, now);
        }
    }
    rd.cache = items.iter().map(|it| (it.id, it.clone())).collect();
    Ok(items)
}

fn convert_item(u: &UserNotification, id: u32, time_ft: u64, binding_name: &HSTRING) -> Result<NotifItem, String> {
    let err = |what: &'static str| move |e: windows::core::Error| format!("{what}: {e}");
    let app = u
        .AppInfo()
        .and_then(|a| a.DisplayInfo())
        .and_then(|d| d.DisplayName())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let mut texts: Vec<String> = Vec::new();
    let binding =
        u.Notification().and_then(|n| n.Visual()).and_then(|v| v.GetBinding(binding_name)).map_err(err("binding"))?;
    let elems = binding.GetTextElements().map_err(err("GetTextElements"))?;
    let count = elems.Size().map_err(err("Size"))?;
    for i in 0..count {
        if let Ok(t) = elems.GetAt(i).and_then(|e| e.Text()) {
            texts.push(t.to_string());
        }
    }
    let title = texts.first().map(|s| truncate(s)).unwrap_or_default();
    let body = if texts.len() > 1 { truncate(&texts[1..].join(" ")) } else { String::new() };
    let info = u.AppInfo().ok();
    let aumid = info.as_ref().and_then(|i| i.AppUserModelId().ok()).map(|s| s.to_string()).unwrap_or_default();
    let icon_key = if !aumid.is_empty() {
        aumid.clone()
    } else if !app.is_empty() {
        format!("name:{app}")
    } else {
        String::new()
    };
    ensure_icon(&icon_key, &aumid, info);
    Ok(NotifItem { id, app: truncate(&app), title, body, time_ft, time_text: String::new(), icon_key, aumid })
}

// ------------------------------------------------------------------ removal through the app history
//
// `UserNotificationListener::RemoveNotification` / `ClearNotifications` need package identity and
// fail for the dock (0x80070490, like `NotificationChanged`). The per-app notification history
// (`ToastNotificationManager::History`, the overloads that take an AppUserModelID) does answer for
// an unpackaged exe, so it is used as the fallback.

/// Clears the notification history of every distinct app in `items`.
fn history_clear_apps(items: &[NotifItem]) {
    let history = match ToastNotificationManager::History() {
        Ok(h) => h,
        Err(e) => {
            log!("notif: ToastNotificationManager::History failed: {e}");
            return;
        }
    };
    let mut done: Vec<&str> = Vec::new();
    for it in items {
        let a = it.aumid.as_str();
        if a.is_empty() || done.iter().any(|d| d.eq_ignore_ascii_case(a)) {
            continue;
        }
        done.push(a);
        match history.ClearWithId(&HSTRING::from(a)) {
            Ok(()) => log!("notif: history of '{a}' cleared"),
            Err(e) => log!("notif: ClearWithId('{a}') failed: {e}"),
        }
    }
    let unknown = items.iter().filter(|it| it.aumid.is_empty()).count();
    if unknown > 0 {
        log!("notif: {unknown} toasts without an app id could not be cleared");
    }
}

/// Text elements of a toast in the history, from its XML content.
fn toast_texts(t: &windows::UI::Notifications::ToastNotification) -> Vec<String> {
    let Ok(nodes) = t.Content().and_then(|x| x.GetElementsByTagName(&HSTRING::from("text"))) else {
        return Vec::new();
    };
    let n = nodes.Length().unwrap_or(0);
    (0..n).filter_map(|i| nodes.Item(i).and_then(|node| node.InnerText()).ok()).map(|s| s.to_string()).collect()
}

/// Removes the history toast of `it.aumid` whose texts match `it` (same title and body as built by
/// `convert_item`). A toast without a tag cannot be addressed alone: it is removed by clearing the
/// app's history only when it is the app's last toast.
fn history_remove(it: &NotifItem) -> Result<(), String> {
    if it.aumid.is_empty() {
        return Err("no app id".into());
    }
    let h = ToastNotificationManager::History().map_err(|e| format!("History: {e}"))?;
    let aid = HSTRING::from(it.aumid.as_str());
    let list = h.GetHistoryWithId(&aid).map_err(|e| format!("GetHistoryWithId: {e}"))?;
    let n = list.Size().map_err(|e| format!("Size: {e}"))?;
    for i in 0..n {
        let Ok(t) = list.GetAt(i) else { continue };
        let texts = toast_texts(&t);
        let title = texts.first().map(|s| truncate(s)).unwrap_or_default();
        let body = if texts.len() > 1 { truncate(&texts[1..].join(" ")) } else { String::new() };
        if title != it.title || body != it.body {
            continue;
        }
        let tag = t.Tag().map_err(|e| format!("Tag: {e}"))?;
        if tag.is_empty() {
            if n == 1 {
                return h.ClearWithId(&aid).map_err(|e| format!("ClearWithId: {e}"));
            }
            return Err("toast without a tag".into());
        }
        let group = t.Group().map_err(|e| format!("Group: {e}"))?;
        return h.RemoveGroupedTagWithId(&tag, &group, &aid).map_err(|e| format!("RemoveGroupedTagWithId: {e}"));
    }
    Err("toast not found in the app history".into())
}

// ------------------------------------------------------------------ times

fn now_ft() -> u64 {
    // SAFETY: no preconditions, returns the current time.
    let ft = unsafe { GetSystemTimeAsFileTime() };
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// UTC FILETIME -> local date and time.
fn local_time(ft: u64) -> Option<SYSTEMTIME> {
    let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    let mut utc = SYSTEMTIME::default();
    // SAFETY: `f` and `utc` are valid for the duration of the call.
    if let Err(e) = unsafe { FileTimeToSystemTime(&f, &mut utc) } {
        log!("notif: FileTimeToSystemTime failed: {e}");
        return None;
    }
    let mut local = SYSTEMTIME::default();
    // SAFETY: null time zone = the current one; `utc` and `local` are valid.
    if let Err(e) = unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) } {
        log!("notif: SystemTimeToTzSpecificLocalTime failed: {e}");
        return None;
    }
    Some(local)
}

/// One day in 100 ns units.
const DAY_FT: u64 = 864_000_000_000;

/// "HH:mm" (today), "yesterday HH:mm", otherwise "ddd d MMM" (localized).
fn format_time(ft: u64, now: u64) -> String {
    let (Some(t), Some(today)) = (local_time(ft), local_time(now)) else { return String::new() };
    let ymd = |s: &SYSTEMTIME| (s.wYear, s.wMonth, s.wDay);
    let hm = format!("{:02}:{:02}", t.wHour, t.wMinute);
    if ymd(&t) == ymd(&today) {
        return hm;
    }
    if local_time(now.saturating_sub(DAY_FT)).is_some_and(|y| ymd(&y) == ymd(&t)) {
        return format!("{} {hm}", crate::i18n::tr("yesterday", "ieri"));
    }
    // Day and month names in the dock language (`i18n::locale`).
    let mut buf = [0u16; 48];
    // SAFETY: `t` and `buf` are valid for the duration of the call; the picture is a static NUL-terminated string.
    let n = unsafe {
        GetDateFormatEx(
            crate::i18n::locale(),
            ENUM_DATE_FORMATS_FLAGS(0),
            Some(&t),
            w!("ddd d MMM"),
            Some(&mut buf),
            PCWSTR::null(),
        )
    };
    if n > 1 {
        String::from_utf16_lossy(&buf[..(n as usize - 1).min(buf.len())])
    } else {
        format!("{:02}/{:02}", t.wDay, t.wMonth)
    }
}

// ------------------------------------------------------------------ Do not disturb

/// Performs the toggle; on any error opens the settings.
fn toggle_dnd_now() {
    if let Err(e) = try_toggle_dnd() {
        log!("notif: DND toggle failed: {e}, opening ms-settings:notifications");
        shell_thread::request(ShellRequest::Launch { target: "ms-settings:notifications".into(), args: None });
    }
}

fn try_toggle_dnd() -> Result<(), String> {
    // SAFETY: COM MTA initialized on the `notif` thread; CLSID and IID match.
    let q: IQuietHoursSettings = unsafe { CoCreateInstance(&CLSID_QUIET_HOURS, None, CLSCTX_ALL) }
        .map_err(|e| format!("CoCreateInstance: {e}"))?;
    let mut cur = PWSTR::null();
    // SAFETY: `cur` is writable; `q` is valid.
    unsafe { q.UserSelectedProfile(&mut cur) }.ok().map_err(|e| format!("UserSelectedProfile: {e}"))?;
    let profile = if cur.is_null() {
        String::new()
    } else {
        // SAFETY: NUL-terminated wide string allocated by the COM server.
        let s = unsafe { cur.to_string() }.unwrap_or_default();
        // SAFETY: allocated with CoTaskMemAlloc, not used after this line.
        unsafe { CoTaskMemFree(Some(cur.0 as *const core::ffi::c_void)) };
        s
    };
    let next = if profile == PROFILE_UNRESTRICTED { PROFILE_PRIORITY_ONLY } else { PROFILE_UNRESTRICTED };
    log!("notif: DND '{profile}' -> '{next}'");
    let w: Vec<u16> = next.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `w` is NUL-terminated and alive for the call.
    unsafe { q.SetUserSelectedProfile(PCWSTR(w.as_ptr())) }.ok().map_err(|e| format!("SetUserSelectedProfile: {e}"))
}

// ------------------------------------------------------------------ app icons

/// Side (px) of the icons kept in the cache (the UI scales them to the final size).
const ICON_PX: u32 = 64;

/// State of a cached app icon.
enum IconSlot {
    /// Request sent to the `notif-icons` thread.
    Pending,
    /// Not available (no source produced an image): not retried.
    Failed,
    Ready(Arc<IconBitmap>),
}

/// Cache by key (AUMID, or "name:<app name>" if the AUMID is missing).
static ICONS: Mutex<Option<HashMap<String, IconSlot>>> = Mutex::new(None);
/// Channel to the thread that loads icons.
static ICON_TX: Mutex<Option<Sender<IconReq>>> = Mutex::new(None);

struct IconReq {
    key: String,
    aumid: String,
    info: Option<AppInfo>,
}

/// Icon of the notification with key `key` (`NotifItem::icon_key`), if already loaded. Does not block.
pub fn app_icon(key: &str) -> Option<Arc<IconBitmap>> {
    match lock(&ICONS).as_ref()?.get(key)? {
        IconSlot::Ready(b) => Some(b.clone()),
        _ => None,
    }
}

/// Window handle (as isize) that receives `WM_DOCK_NOTIF` from the on-demand icon thread.
static ICON_UI: AtomicIsize = AtomicIsize::new(0);
/// Idle time after which the on-demand icon thread exits.
const ICON_IDLE: Duration = Duration::from_secs(30);

/// Drops Ready/Failed icon slots not referenced by `items` (Pending slots stay).
fn prune_icons(items: &[NotifItem]) {
    if let Some(map) = lock(&ICONS).as_mut() {
        map.retain(|k, slot| matches!(slot, IconSlot::Pending) || items.iter().any(|it| it.icon_key == *k));
    }
}

/// Queues the icon load for `key` if not already known; spawns the icon thread when it is not running.
fn ensure_icon(key: &str, aumid: &str, info: Option<AppInfo>) {
    if key.is_empty() {
        return;
    }
    {
        let mut g = lock(&ICONS);
        let map = g.get_or_insert_with(HashMap::new);
        if map.contains_key(key) {
            return;
        }
        map.insert(key.to_string(), IconSlot::Pending);
    }
    let req = IconReq { key: key.to_string(), aumid: aumid.to_string(), info };
    // The send happens under the ICON_TX lock: the exiting thread clears the sender under the same
    // lock, so a request can never be queued to a thread that already decided to leave.
    let sent = {
        let mut g = lock(&ICON_TX);
        if g.is_none() && !STOPPED.load(Relaxed) {
            let (tx, rx) = mpsc::channel::<IconReq>();
            let ui = ICON_UI.load(Relaxed);
            match std::thread::Builder::new().name("notif-icons".into()).spawn(move || icons_thread(ui, rx)) {
                Ok(_) => *g = Some(tx),
                Err(e) => log!("notif: icon thread spawn failed: {e}"),
            }
        }
        match g.as_ref() {
            Some(tx) => tx.send(req).is_ok(),
            None => false,
        }
    };
    if !sent {
        if let Some(map) = lock(&ICONS).as_mut() {
            map.remove(key);
        }
    }
}

/// Thread `notif-icons` (COM MTA), on demand: one request at a time; exits after `ICON_IDLE` of silence.
fn icons_thread(ui: isize, rx: Receiver<IconReq>) {
    // SAFETY: initializes COM MTA for this thread; paired with CoUninitialize at the end.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        log!("notif-icons: CoInitializeEx failed: {hr:?}");
    }
    loop {
        let req = match rx.recv_timeout(ICON_IDLE) {
            Ok(r) => r,
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                // Lock order: only ICON_TX is held here (never ICONS).
                let mut g = lock(&ICON_TX);
                match rx.try_recv() {
                    Ok(r) => r,
                    Err(_) => {
                        *g = None;
                        break;
                    }
                }
            }
        };
        if STOPPED.load(Relaxed) {
            break;
        }
        process_icon_req(ui, req);
    }
    if hr.is_ok() {
        // SAFETY: paired with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

fn process_icon_req(ui: isize, req: IconReq) {
    let slot = match load_app_icon(&req) {
        Some(b) => IconSlot::Ready(Arc::new(b)),
        None => {
            log!("notif-icons: no icon for '{}'", req.key);
            IconSlot::Failed
        }
    };
    let ready = matches!(slot, IconSlot::Ready(_));
    if let Some(map) = lock(&ICONS).as_mut() {
        map.insert(req.key, slot);
    }
    if ready {
        let hwnd = HWND(ui as *mut core::ffi::c_void);
        // SAFETY: PostMessageW is thread-safe; it only fails if the window no longer exists.
        let _ = unsafe { PostMessageW(Some(hwnd), WM_DOCK_NOTIF, WPARAM(1), LPARAM(0)) };
    }
}

/// AUMID -> AppsFolder catalog; then `AppDisplayInfo::GetLogo` decoded with WIC.
fn load_app_icon(req: &IconReq) -> Option<IconBitmap> {
    if !req.aumid.is_empty() {
        if let Some(b) = icons::appsfolder_icon(&req.aumid, 128) {
            return Some(icons::resize_area(&b, ICON_PX));
        }
    }
    let info = req.info.as_ref()?;
    match logo_bytes(info) {
        Ok(bytes) => icons::decode_image_bytes(&bytes).map(|b| icons::resize_area(&b, ICON_PX)),
        Err(e) => {
            log!("notif-icons: no app icon: {e}");
            None
        }
    }
}

/// Logo image bytes (WinRT stream read with `DataReader`, every wait with a timeout).
fn logo_bytes(info: &AppInfo) -> Result<Vec<u8>, String> {
    let reference = info
        .DisplayInfo()
        .and_then(|d| d.GetLogo(Size { Width: 64.0, Height: 64.0 }))
        .map_err(|e| {
            // windows-rs reports a null WinRT result as an error carrying S_OK: the app has no logo.
            if e.code().is_ok() {
                "app exposes no logo".to_string()
            } else {
                format!("GetLogo: {e}")
            }
        })?;
    let op = reference.OpenReadAsync().map_err(|e| format!("OpenReadAsync: {e}"))?;
    let stream = await_op(&op, ASYNC_TIMEOUT)?;
    let size = stream.Size().map_err(|e| format!("Size: {e}"))?;
    if size == 0 || size > 4 * 1024 * 1024 {
        return Err(format!("invalid stream size ({size})"));
    }
    let reader = DataReader::CreateDataReader(&stream).map_err(|e| format!("CreateDataReader: {e}"))?;
    let load = reader.LoadAsync(size as u32).map_err(|e| format!("LoadAsync: {e}"))?;
    let n = await_op(&load, ASYNC_TIMEOUT)?;
    let mut buf = vec![0u8; n as usize];
    reader.ReadBytes(&mut buf).map_err(|e| format!("ReadBytes: {e}"))?;
    Ok(buf)
}

// PREVIEW-HOOK (used by `preview.rs`): replaces the published state and icons with fake data.
pub(crate) fn preview_set_state(s: NotifState, icons: Vec<(String, Arc<IconBitmap>)>) {
    *lock(&STATE) = Some(s);
    let map: HashMap<String, IconSlot> = icons.into_iter().map(|(k, b)| (k, IconSlot::Ready(b))).collect();
    *lock(&ICONS) = Some(map);
}
