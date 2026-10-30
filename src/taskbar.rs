//! Native Windows taskbar: hide / restore / self-heal / on-disk state.
//! The watchdog is in `watchdog.rs`.
//!
//! THREADING:
//!  * `init`, `install_hook`, `remove_hook`, `on_winevent`, `on_safety_timer`, `on_taskbar_created`,
//!    `shutdown`: UI thread ONLY (the WinEvent callback runs on the UI thread and posts
//!    `WM_DOCK_TASKBAR_EVENT`).
//!  * `restore()` and `restore_force()`: callable from ANY thread/process (panic hook,
//!    WM_ENDSESSION, watchdog, CLI `--restore`; Ctrl+C instead posts WM_CLOSE to the UI). They use only
//!    thread-safe Win32 APIs and do not touch thread-local state.
//!  * On-disk state: `%ProgramData%\mini-oled-dock\taskbar-state` (text: the numeric `ABS_*` value),
//!    admin-only folder, atomic write (`datadir::write_atomic`), read without following reparse
//!    points. Without a usable data folder the state cannot be saved and the taskbar is never hidden.
//!  * The dock does NOT register an AppBar and does NOT reserve a work area.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::{SHAppBarMessage, APPBARDATA};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, FindWindowExW, GetClassNameW, GetParent, GetPropW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, RegisterWindowMessageW, RemovePropW, SendMessageTimeoutW, SendNotifyMessageW,
    SetPropW, SetTimer, SetWindowPos, ShowWindowAsync, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_SHOW,
    HWND_BOTTOM,
    HWND_BROADCAST, HWND_NOTOPMOST, HWND_TOPMOST, MSGFLT_ALLOW, OBJID_WINDOW, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNA, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_COPYDATA,
};

use crate::config;
use crate::log;
use crate::msg::{TIMER_TRAY_REANNOUNCE, WM_DOCK_TASKBAR_EVENT};

/// Name of the window property that marks the taskbars hidden by us.
pub const PROP_DOCK_HIDDEN: windows::core::PCWSTR = w!("DockHidden");

/// true if this process hid the taskbar (and therefore must restore it).
static HIDDEN: AtomicBool = AtomicBool::new(false);
/// Set at the start of `restore`/`restore_force` and NEVER cleared: a process that has started
/// restoring must not hide the taskbar again (panic hook / safety timer race).
static RESTORING: AtomicBool = AtomicBool::new(false);
/// Id of the registered "TaskbarCreated" message (0 = not registered yet).
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);
/// Raw handle (isize) of the taskbar WinEvent hook (0 = no hook).
static WINEVENT_HOOK: AtomicIsize = AtomicIsize::new(0);
/// Instant (ms since process start, +1) of the last `TaskbarCreated` broadcast sent by us.
static LAST_BROADCAST_MS: AtomicU64 = AtomicU64::new(0);
/// Explorer's primary `Shell_TrayWnd` as last seen by `hide_all` (0 = unknown): its destruction means
/// Explorer is going away (`explorer_gone`).
static EXPLORER_TRAY: AtomicIsize = AtomicIsize::new(0);
/// `now_ms()` when `explorer_gone` stopped our tray host (0 = not paused): it is started again once the
/// new Explorer has registered its desktop (`on_reannounce_timer`).
static HOST_PAUSED_AT: AtomicU64 = AtomicU64::new(0);
/// Monotonic time reference of the process.
static START: OnceLock<Instant> = OnceLock::new();

// SHAppBarMessage constants (documented values; they avoid depending on the type of the crate's constants).
const ABM_GETSTATE: u32 = 4;
const ABM_SETSTATE: u32 = 10;
const ABS_AUTOHIDE: u32 = 1;
const ABS_ALWAYSONTOP: u32 = 2;
const ABS_MASK: u32 = ABS_AUTOHIDE | ABS_ALWAYSONTOP;

/// Quiet window during which our own `TaskbarCreated` broadcast is ignored.
const OWN_BROADCAST_WINDOW_MS: u64 = 2000;
/// Longest wait for a restarted Explorer's desktop before our tray host is started again anyway (ms).
const HOST_RESUME_MAX_MS: u64 = 60_000;
/// Maximum wait for `Shell_TrayWnd` at startup.
const WAIT_TRAY_MS: u64 = 30_000;

/// `<data root>\taskbar-state` (`None` without a usable data folder).
pub fn state_file_path() -> Option<PathBuf> {
    crate::datadir::state_path()
}

/// true if the state file exists (the name itself: a link is not followed).
pub fn state_file_exists() -> bool {
    state_file_path().is_some_and(|p| std::fs::symlink_metadata(p).is_ok())
}

/// Id of the `TaskbarCreated` message (`RegisterWindowMessageW`, registered by `init`).
/// The `main` wnd_proc compares it with `msg` (if != 0) to call `on_taskbar_created()`.
pub fn taskbar_created_msg() -> u32 {
    TASKBAR_CREATED_MSG.load(Relaxed)
}

// ------------------------------------------------------------------ utilities

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Id of `TaskbarCreated`, registering it if needed (works in any process).
fn ensure_taskbar_created_msg() -> u32 {
    let cur = TASKBAR_CREATED_MSG.load(Relaxed);
    if cur != 0 {
        return cur;
    }
    // SAFETY: static NUL-terminated literal string.
    let id = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if id == 0 {
        log!("RegisterWindowMessageW(TaskbarCreated) failed: {:?}", windows::core::Error::from_thread());
    }
    TASKBAR_CREATED_MSG.store(id, Relaxed);
    id
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is a writable buffer; the function truncates if too short.
    let n = unsafe { GetClassNameW(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

fn is_taskbar_class(name: &str) -> bool {
    name == "Shell_TrayWnd" || name == "Shell_SecondaryTrayWnd"
}

/// true if the window belongs to this process (never touch our own windows).
fn is_ours(h: HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: `pid` is valid and writable; `h` may also no longer be valid (the call fails).
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    // SAFETY: no preconditions.
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// All the native taskbars (main + secondary) that are not ours.
fn native_taskbars() -> Vec<HWND> {
    let mut out = Vec::new();
    for class in [w!("Shell_TrayWnd"), w!("Shell_SecondaryTrayWnd")] {
        let mut prev: Option<HWND> = None;
        loop {
            // SAFETY: `class` is a static literal; `prev` is None or a window already returned
            // by the same enumeration (if it disappears in the meantime the search ends with an error).
            let Ok(h) = (unsafe { FindWindowExW(None, prev, class, PCWSTR::null()) }) else { break };
            if h.0.is_null() {
                break;
            }
            if !is_dock_tray_host(h) {
                out.push(h);
            }
            prev = Some(h);
        }
    }
    out
}

fn is_visible(h: HWND) -> bool {
    // SAFETY: no preconditions; for an invalid window it returns false.
    unsafe { IsWindowVisible(h) }.as_bool()
}

fn has_dock_prop(h: HWND) -> bool {
    // SAFETY: static literal property name; invalid handle = property absent.
    !unsafe { GetPropW(h, PROP_DOCK_HIDDEN) }.is_invalid()
}

/// `APPBARDATA` with 32-bit handles, as it travels inside `WM_COPYDATA`.
#[repr(C)]
#[derive(Clone, Copy)]
struct AppBarData32 {
    cb_size: u32,
    hwnd: u32,
    callback: u32,
    edge: u32,
    rc: [i32; 4],
    lparam: i64,
}

/// `APPBARMSGDATAV3` (64 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct AppBarMsg {
    abd: AppBarData32,
    message: u32,
    _pad0: u32,
    shared: u64,
    pid: u32,
    _pad1: u32,
}

const _: () = assert!(std::mem::size_of::<AppBarData32>() == 0x28 && std::mem::size_of::<AppBarMsg>() == 0x40);

/// DIRECT send of an AppBar message without a shared block to Explorer's `Shell_TrayWnd`
/// (`hwnd`), so it does not depend on which `Shell_TrayWnd` is first in the Z-order (our tray
/// host is first). `Some(result)` if the message was delivered, `None` otherwise.
fn appbar_direct(msg: u32, hwnd: HWND, lparam: u32) -> Option<usize> {
    let mut m = AppBarMsg {
        abd: AppBarData32 {
            cb_size: 0x28,
            hwnd: hwnd.0 as usize as u32,
            callback: 0,
            edge: 0,
            rc: [0; 4],
            lparam: lparam as i64,
        },
        message: msg,
        _pad0: 0,
        shared: 0,
        pid: 0,
        _pad1: 0,
    };
    let cds = COPYDATASTRUCT {
        dwData: 0,
        cbData: std::mem::size_of::<AppBarMsg>() as u32,
        lpData: (&mut m as *mut AppBarMsg).cast::<c_void>(),
    };
    let mut res = 0usize;
    // SAFETY: `cds` and `m` live for the whole call, which copies the data to Explorer; `res` is
    // writable; SMTO_ABORTIFHUNG avoids waits on a hung Explorer.
    let r = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(hwnd.0 as usize),
            LPARAM(&cds as *const COPYDATASTRUCT as isize),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            2000,
            Some(&mut res),
        )
    };
    (r.0 != 0).then_some(res)
}

/// AppBar message to Explorer: first a direct send, then `SHAppBarMessage` (which goes through the tray
/// host, which forwards it). Returns the raw value.
fn appbar(msg: u32, hwnd: HWND, lparam: u32) -> usize {
    if let Some(v) = appbar_direct(msg, hwnd, lparam) {
        return v;
    }
    log!("taskbar: direct AppBar {msg} send failed, falling back to SHAppBarMessage");
    appbar_shell(msg, hwnd, lparam)
}

/// AppBar message through `SHAppBarMessage` (tray host -> Explorer): the fallback of `appbar()`
/// and `set_state()` when the direct send fails. Returns the raw value.
fn appbar_shell(msg: u32, hwnd: HWND, lparam: u32) -> usize {
    let mut d = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        hWnd: hwnd,
        lParam: LPARAM(lparam as isize),
        ..Default::default()
    };
    // SAFETY: `d` is initialized with the correct `cbSize` and stays valid for the whole call.
    unsafe { SHAppBarMessage(msg, &mut d) }
}

fn get_state(tray: HWND) -> u32 {
    (appbar(ABM_GETSTATE, tray, 0) as u32) & ABS_MASK
}

/// `ABM_SETSTATE`; true if the message reached Explorer. A delivered direct send is enough;
/// if it fails we fall back to `SHAppBarMessage` (which always returns TRUE, so it tells us nothing) and
/// confirm by re-reading the state with a direct send.
fn set_state(tray: HWND, state: u32) -> bool {
    let state = state & ABS_MASK;
    if appbar_direct(ABM_SETSTATE, tray, state).is_some() {
        return true;
    }
    log!("taskbar: direct ABM_SETSTATE send failed, falling back to SHAppBarMessage");
    appbar_shell(ABM_SETSTATE, tray, state);
    matches!(appbar_direct(ABM_GETSTATE, tray, 0), Some(v) if (v as u32) & ABS_MASK == state)
}

// ------------------------------------------------------------ state file

/// Parses a state file's content (decimal `ABS_*` value), masked to `ABS_MASK`. Pure.
fn parse_state(bytes: &[u8]) -> Option<u32> {
    let v = std::str::from_utf8(bytes).ok()?.trim().parse::<u32>().ok()?;
    Some(v & ABS_MASK)
}

/// Reads the saved state; `None` if the file is missing or unreadable/invalid.
fn read_state() -> Option<u32> {
    parse_state(&crate::datadir::read_file(&state_file_path()?, crate::datadir::STATE_MAX_BYTES, true).ok()?)
}

// ------------------------------------------------- legacy state (pre-ProgramData versions)

/// State file of the previous dock versions, `%LOCALAPPDATA%\Dock\taskbar-state`. If such a dock
/// exited uncleanly, Explorer keeps OUR autohide and only this file knows the original state.
/// It is only ever READ (medium-writable folder): never written or deleted by this process.
fn legacy_state_path() -> Option<PathBuf> {
    crate::datadir::legacy_dir().map(|d| d.join("taskbar-state"))
}

/// Marker in the admin-only data folder: the legacy state has been taken over (saved as our own
/// state or restored), so the stale legacy file is ignored from then on.
fn legacy_marker_path() -> Option<PathBuf> {
    state_file_path().map(|p| p.with_extension("legacy-done"))
}

/// The legacy state if it exists and has not been taken over yet. Without a writable data folder the
/// take-over could not be recorded, so it is ignored (it would otherwise override the user's
/// setting on every restore). Read without following reparse points, single link, size-capped;
/// the value is masked, and a medium process could set the same taskbar state itself anyway.
fn read_legacy_state() -> Option<u32> {
    if !crate::datadir::writable() || legacy_marker_path().is_none_or(|m| std::fs::symlink_metadata(m).is_ok()) {
        return None;
    }
    let bytes = crate::datadir::read_file(&legacy_state_path()?, crate::datadir::STATE_MAX_BYTES, true).ok()?;
    parse_state(&bytes)
}

/// Records that the legacy state has been taken over (the legacy file itself is left alone).
fn mark_legacy_consumed() {
    let Some(m) = legacy_marker_path() else { return };
    match crate::datadir::write_atomic(&m, b"1", crate::datadir::FileAcl::Inherit) {
        Ok(()) => log!(
            "taskbar: legacy state taken over; %LOCALAPPDATA%\\Dock\\taskbar-state is no longer used (delete it yourself)"
        ),
        Err(e) => log!("taskbar: recording the legacy state take-over failed: {e}"),
    }
}

/// Atomic write (temporary file + rename) in the admin-only data folder.
fn write_state(state: u32) -> Result<(), String> {
    if !crate::datadir::writable() {
        return Err("no writable data folder".into());
    }
    let dst = state_file_path().ok_or("no data folder")?;
    crate::datadir::write_atomic(&dst, state.to_string().as_bytes(), crate::datadir::FileAcl::Inherit)
}

/// saves the original state ONLY if the file is missing or unreadable.
/// Returns true if a valid state exists on file after the call.
/// `pre` is the state already read (and stabilized) by `init`; `None` = reads it here.
fn ensure_state_saved(pre: Option<u32>) -> bool {
    if read_state().is_some() {
        return true;
    }
    let list = native_taskbars();
    let Some(&tray) = list.first() else {
        log!("taskbar: no Shell_TrayWnd, state cannot be saved");
        return false;
    };
    // A previous dock version that died with the taskbar hidden left the true original state in the
    // legacy file: Explorer's current state is then OUR autohide and must not become the "original".
    let legacy = read_legacy_state();
    let mut state = match legacy {
        Some(s) => {
            log!("taskbar: using the original state saved by the previous dock version ({s})");
            s
        }
        None => pre.unwrap_or_else(|| get_state(tray)),
    };
    // Heuristic: autohide active + our mark = the autohide is the work of a previous dead run.
    if state & ABS_AUTOHIDE != 0 && list.iter().any(|&h| has_dock_prop(h)) {
        state = ABS_ALWAYSONTOP;
    }
    match write_state(state) {
        Ok(()) => {
            log!("taskbar: original state saved ({state})");
            if legacy.is_some() {
                mark_legacy_consumed();
            }
            true
        }
        Err(e) => {
            log!("taskbar: saving state failed: {e}");
            false
        }
    }
}

/// Two ABM_GETSTATE reads ~500 ms apart must match (max ~2 s in total);
/// if they do not stabilize the last one wins. Keeps the startup heartbeat going. `None` if the taskbar disappears.
fn stable_state(last_beat: &mut Instant) -> Option<u32> {
    let t = Instant::now();
    let mut prev = get_state(*native_taskbars().first()?);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        crate::watchdog::startup_heartbeat();
        *last_beat = Instant::now();
        let cur = get_state(*native_taskbars().first()?);
        if cur == prev || t.elapsed() >= Duration::from_millis(2000) {
            return Some(cur);
        }
        prev = cur;
    }
}

/// if a taskbar is invisible but marked by us (dead previous run), put it back in place.
fn self_heal() {
    let mut healed = false;
    for h in native_taskbars() {
        if !is_visible(h) && has_dock_prop(h) {
            if !healed {
                appbar(ABM_SETSTATE, h, ABS_ALWAYSONTOP);
                healed = true;
            }
            // SAFETY: Explorer window; the asynchronous call does not block.
            let _ = unsafe { ShowWindowAsync(h, SW_SHOWNA) };
        }
    }
    if healed {
        log!("taskbar: self-heal (taskbar hidden by a previous run)");
        std::thread::sleep(Duration::from_millis(300));
    }
}

// ------------------------------------------------------------------ API

/// Startup sequence, AFTER the watchdog has started and BEFORE the
/// dock window exists: waits for `Shell_TrayWnd` up to 30 s (may block: never from the message loop),
/// registers `TaskbarCreated`, self-heal, `ABM_GETSTATE` + saves state to file (if that fails it does
/// NOT hide), then `hide_all()`. Returns `Ok(true)` if the taskbar was hidden, `Ok(false)` if
/// it was chosen not to hide it (config/`--keep-taskbar`/state not saveable). Does NOT install the
/// WinEventHook (see `install_hook`).
pub fn init() -> Result<bool, String> {
    if crate::args().keep_taskbar || !config::effective_hide_native_taskbar() {
        return Ok(false);
    }
    let start = Instant::now();
    let mut last_beat = Instant::now();
    while native_taskbars().is_empty() {
        if start.elapsed().as_millis() as u64 >= WAIT_TRAY_MS {
            return Err("Shell_TrayWnd did not appear within 30 s".into());
        }
        // The watchdog must not mistake this wait for a hang. "Startup" heartbeat: keeps
        // the watchdog alive without making it switch to the short tolerance before TIMER_SECOND.
        if last_beat.elapsed() >= Duration::from_millis(500) {
            crate::watchdog::startup_heartbeat();
            last_beat = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    ensure_taskbar_created_msg();
    self_heal();
    // At logon Explorer may not have loaded the settings yet: ABM_GETSTATE read too
    // early would give a wrong state. Needed only if there is no saved state yet.
    let pre = if read_state().is_none() && read_legacy_state().is_none() { stable_state(&mut last_beat) } else { None };
    if !ensure_state_saved(pre) {
        log!("taskbar: original state not saveable, NOT hiding the taskbar");
        return Ok(false);
    }
    // From here on an exit must restore, even if hide_all failed halfway.
    HIDDEN.store(true, Relaxed);
    hide_all();
    Ok(true)
}

/// installs `SetWinEventHook(EVENT_OBJECT_SHOW..EVENT_OBJECT_CREATE)` (OUTOFCONTEXT |
/// SKIPOWNPROCESS) whose callback filters the taskbar classes (also on the parent) and posts
/// `WM_DOCK_TASKBAR_EVENT` to the dock window. Called from the UI thread AFTER the creation of the
/// window (the callback uses `crate::hwnd()`). No-op if `HIDDEN` is false.
pub fn install_hook() -> Result<(), String> {
    if !HIDDEN.load(Relaxed) || WINEVENT_HOOK.load(Relaxed) != 0 {
        return Ok(());
    }
    // Range CREATE(0x8000)..SHOW(0x8002): also includes DESTROY, discarded by the callback.
    // SAFETY: `winevent_proc` is a 'static `extern "system"` with the WINEVENTPROC signature; without
    // WINEVENT_INCONTEXT no module is needed. The UI thread has a message loop (out-of-context delivery).
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_SHOW,
            None,
            Some(winevent_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };
    if hook.is_invalid() {
        return Err(format!("SetWinEventHook failed: {:?}", windows::core::Error::from_thread()));
    }
    WINEVENT_HOOK.store(hook.0 as isize, Relaxed);
    log!("taskbar: WinEventHook installed");
    allow_taskbar_created();
    Ok(())
}

/// The dock runs elevated: without this exception UIPI would drop the `TaskbarCreated` broadcast
/// from Explorer (medium integrity) and `on_taskbar_created` would never fire.
fn allow_taskbar_created() {
    let ui = crate::hwnd();
    let msg = ensure_taskbar_created_msg();
    if ui.0.is_null() || msg == 0 {
        return;
    }
    // SAFETY: `ui` is the dock window (created by this thread); no optional structure.
    if let Err(e) = unsafe { ChangeWindowMessageFilterEx(ui, msg, MSGFLT_ALLOW, None) } {
        log!("taskbar: ChangeWindowMessageFilterEx(TaskbarCreated) failed: {e}");
    }
}

/// true if, while this process was hiding the taskbar, someone else (watchdog after missed
/// heartbeats, CLI `--restore` with the dock stuck) restored it: the state file is gone.
/// In that case it stops hiding it (`HIDDEN` = false): hiding it again would leave the taskbar
/// hidden with no state file and, possibly, no watchdog.
fn restored_externally() -> bool {
    if !HIDDEN.load(Relaxed) {
        return false;
    }
    if state_file_exists() {
        return false;
    }
    HIDDEN.store(false, Relaxed);
    // The tray host yields to Explorer: apps will re-register their icons with it.
    crate::tray_host::yield_now();
    log!("taskbar: state file gone (external restore), no longer hiding the taskbar");
    true
}

/// Removes the hook (`UnhookWinEvent`). Idempotent. UI thread.
pub fn remove_hook() {
    let raw = WINEVENT_HOOK.swap(0, Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: handle obtained from SetWinEventHook on this thread; the swap guarantees a single release.
    if !unsafe { UnhookWinEvent(HWINEVENTHOOK(raw as *mut c_void)) }.as_bool() {
        log!("taskbar: UnhookWinEvent failed");
    }
}

/// WinEvent callback (UI thread): filters the taskbar classes (also on the parent) and posts
/// `WM_DOCK_TASKBAR_EVENT` (wParam = event, lParam = HWND) to the dock window.
unsafe extern "system" fn winevent_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.0.is_null() || id_object != OBJID_WINDOW.0 {
        return;
    }
    if event == EVENT_OBJECT_DESTROY {
        if hwnd.0 as isize != EXPLORER_TRAY.load(Relaxed) {
            return;
        }
    } else if event != EVENT_OBJECT_SHOW && event != EVENT_OBJECT_CREATE {
        return;
    }
    // SAFETY: GetParent has no preconditions; for invalid windows it fails.
    let parent = unsafe { GetParent(hwnd) }.ok();
    let hit = event == EVENT_OBJECT_DESTROY
        || is_taskbar_class(&class_of(hwnd))
        || parent.is_some_and(|p| is_taskbar_class(&class_of(p)));
    if !hit {
        return;
    }
    let ui = crate::hwnd();
    if ui.0.is_null() {
        return;
    }
    // SAFETY: PostMessageW is thread-safe; if the window no longer exists it just fails.
    let _ = unsafe { PostMessageW(Some(ui), WM_DOCK_TASKBAR_EVENT, WPARAM(event as usize), LPARAM(hwnd.0 as isize)) };
}

/// for each `Shell_TrayWnd`/`Shell_SecondaryTrayWnd` that is not ours: `SetPropW(DockHidden)`,
/// `SHAppBarMessage(ABM_SETSTATE, ABS_AUTOHIDE)` (Explorer's HWND), `ShowWindowAsync(SW_HIDE)`,
/// `SetWindowPos(HWND_NOTOPMOST)` and `SetWindowPos(HWND_BOTTOM)` with
/// `SWP_NOACTIVATE|SWP_NOMOVE|SWP_NOSIZE|SWP_ASYNCWINDOWPOS`. Sets `HIDDEN`. UI thread.
pub fn hide_all() {
    // A process that has started restoring never hides again (re-checked before each action).
    if RESTORING.load(Relaxed) {
        return;
    }
    let list = native_taskbars();
    let Some(&first) = list.first() else { return };
    HIDDEN.store(true, Relaxed);
    if class_of(first) == "Shell_TrayWnd" {
        EXPLORER_TRAY.store(first.0 as isize, Relaxed);
    }
    for &h in &list {
        if RESTORING.load(Relaxed) {
            return;
        }
        // SAFETY: static name; the value 1 is only a marker, never dereferenced.
        if let Err(e) = unsafe { SetPropW(h, PROP_DOCK_HIDDEN, Some(HANDLE(std::ptr::dangling_mut::<c_void>()))) } {
            log!("taskbar: SetPropW failed: {e}");
        }
    }
    // Avoids repeating ABM_SETSTATE (and Explorer's AppBar recalculation) if autohide is already active.
    if RESTORING.load(Relaxed) {
        return;
    }
    if get_state(first) & ABS_AUTOHIDE == 0 {
        // Send without verification: we are on the UI thread here and the confirmation re-read is not needed.
        appbar(ABM_SETSTATE, first, ABS_AUTOHIDE);
    }
    let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS;
    for &h in &list {
        if RESTORING.load(Relaxed) {
            return;
        }
        // SAFETY: Explorer windows (another process): only asynchronous variants, which do not block
        // the UI thread if Explorer is busy.
        unsafe {
            let _ = ShowWindowAsync(h, SW_HIDE);
            if let Err(e) = SetWindowPos(h, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags) {
                log!("taskbar: SetWindowPos(NOTOPMOST) failed: {e}");
            }
            if let Err(e) = SetWindowPos(h, Some(HWND_BOTTOM), 0, 0, 0, 0, flags) {
                log!("taskbar: SetWindowPos(BOTTOM) failed: {e}");
            }
        }
    }
}

/// Callback for `WM_DOCK_TASKBAR_EVENT` (wParam = event, lParam = HWND): if `hwnd` (or its parent)
/// is of a native taskbar class and visible, redoes `hide_all()`. The creation of Explorer's primary
/// `Shell_TrayWnd` is an Explorer restart (`explorer_restarted`).
pub fn on_winevent(event: u32, hwnd: HWND) {
    if !HIDDEN.load(Relaxed) || hwnd.0.is_null() || restored_externally() {
        return;
    }
    if event == EVENT_OBJECT_DESTROY {
        if hwnd.0 as isize == EXPLORER_TRAY.swap(0, Relaxed) {
            explorer_gone();
        }
        return;
    }
    let class = class_of(hwnd);
    if event == EVENT_OBJECT_CREATE && class == "Shell_TrayWnd" && !is_dock_tray_host(hwnd) {
        explorer_restarted("new Shell_TrayWnd");
        return;
    }
    let target = if is_taskbar_class(&class) {
        hwnd
    } else {
        // SAFETY: GetParent has no preconditions; for invalid windows it fails.
        match unsafe { GetParent(hwnd) } {
            Ok(p) if is_taskbar_class(&class_of(p)) => p,
            _ => return,
        }
    };
    if is_dock_tray_host(target) || !is_visible(target) {
        return;
    }
    log!("taskbar: event 0x{event:X} on visible taskbar, hiding it again");
    hide_all();
}

/// 1 s safety timer (`TIMER_SECOND`): if a native taskbar is visible, redoes `hide_all()`.
/// No-op if `HIDDEN` is false or if the taskbar was restored from outside (see `restored_externally`).
pub fn on_safety_timer() {
    if !HIDDEN.load(Relaxed) || restored_externally() {
        return;
    }
    if native_taskbars().into_iter().any(is_visible) {
        hide_all();
    }
}

/// `TaskbarCreated` message (Explorer restarted): redoes self-heal + state saving (only if the
/// file is missing) + `hide_all()`. Ignores the broadcast if it is ours (sent by `restore_force()`
/// within the last 2 s). No-op if `HIDDEN` is false. If the state file is missing the restore was
/// done by another process (watchdog/CLI, whose broadcast is not recognizable as ours):
/// we stop hiding instead of re-saving the current state as "original".
pub fn on_taskbar_created() {
    if !HIDDEN.load(Relaxed) || restored_externally() {
        return;
    }
    let last = LAST_BROADCAST_MS.load(Relaxed);
    if last != 0 && now_ms().saturating_sub(last) < OWN_BROADCAST_WINDOW_MS {
        return;
    }
    explorer_restarted("TaskbarCreated");
}

/// Explorer's taskbar was destroyed (Explorer exited or crashed). Our tray host is a `Shell_TrayWnd` too:
/// if it exists while the new Explorer starts, Explorer never registers its desktop with ShellWindows
/// (every de-elevated launch then fails) nor broadcasts `TaskbarCreated`. So the host is stopped now
/// and started again by `on_reannounce_timer` once the new desktop is registered.
fn explorer_gone() {
    if !crate::tray_host::is_started() {
        return;
    }
    log!("taskbar: Explorer's taskbar destroyed, stopping our tray host until Explorer is back");
    let hook = crate::popup::suspend_mouse_hook();
    crate::tray_host::stop();
    crate::popup::resume_mouse_hook(hook);
    HOST_PAUSED_AT.store(now_ms(), Relaxed);
    arm_reannounce(1000);
}

/// (Re)arms the one-shot `TIMER_TRAY_REANNOUNCE` on the dock window.
fn arm_reannounce(ms: u32) {
    let ui = crate::hwnd();
    if !ui.0.is_null() {
        // SAFETY: `ui` is the dock window, created by this thread (UI thread).
        if unsafe { SetTimer(Some(ui), TIMER_TRAY_REANNOUNCE, ms, None) } == 0 {
            log!("taskbar: SetTimer(TIMER_TRAY_REANNOUNCE) failed: {:?}", windows::core::Error::from_thread());
        }
    }
}

/// Explorer restarted: self-heal, hide, and announce our tray again. Reached from `TaskbarCreated` and
/// from the creation of a new `Shell_TrayWnd` (`on_winevent`): while our tray host exists Explorer
/// does not broadcast `TaskbarCreated` on restart, so the window event is what actually fires.
fn explorer_restarted(why: &str) {
    log!("taskbar: Explorer restarted ({why}), redoing self-heal and hide");
    self_heal();
    if !ensure_state_saved(None) {
        log!("taskbar: state not saveable after the Explorer restart, not hiding");
        HIDDEN.store(false, Relaxed);
        return;
    }
    hide_all();
    if HOST_PAUSED_AT.load(Relaxed) != 0 {
        return; // our host comes back (and is announced) once the new desktop is registered
    }
    // Explorer's new Shell_TrayWnd is born topmost and would sit in front of ours: it is brought back to the
    // top and, after 300 ms, announced again so apps re-register their icons with us.
    crate::tray_host::raise();
    arm_reannounce(300);
}

/// "Normal" restore by the dock on exit (WM_DESTROY/clean exit, WM_ENDSESSION wParam != 0,
/// panic hook). Idempotent and guarded: if this process hid nothing (`HIDDEN` false) and
/// the state file does not exist, it does nothing. Otherwise it runs `restore_force()` and clears `HIDDEN`.
/// Callable from any thread.
pub fn restore() {
    RESTORING.store(true, Relaxed);
    let was_hidden = HIDDEN.swap(false, Relaxed);
    if !was_hidden && !state_file_exists() && read_legacy_state().is_none() {
        return;
    }
    restore_force();
}

/// UNCONDITIONAL restore (watchdog, CLI `--restore`): reads the state from the file (default
/// `ABS_ALWAYSONTOP`, NEVER autohide), `ABM_SETSTATE`, for each taskbar `RemovePropW` +
/// `ShowWindowAsync(SW_SHOWNA)` + `SetWindowPos(HWND_TOPMOST, ... ASYNCWINDOWPOS)`, then
/// deletes the state file and then `SendNotifyMessageW(HWND_BROADCAST, TaskbarCreated)`. Without a file and
/// without the `DockHidden` mark it does nothing; without `Shell_TrayWnd` it keeps the file and does nothing.
/// Callable from any thread and from a process other than the dock.
pub fn restore_force() {
    RESTORING.store(true, Relaxed);
    HIDDEN.store(false, Relaxed);
    let saved = read_state();
    // No state of ours: a previous dock version may have died leaving its (legacy) state behind.
    let legacy = if saved.is_none() { read_legacy_state() } else { None };
    let list = native_taskbars();
    // Guard: without a state file and without our mark there is nothing to restore (avoids
    // forcing ALWAYSONTOP over the user's autohide setting when the watchdog fires after
    // an exit that was already restored). Does not depend on visibility: the dock's asynchronous SW_SHOWNA
    // may not have been executed by Explorer yet. `hide_all` always marks BEFORE hiding.
    if saved.is_none() && legacy.is_none() && !list.iter().any(|&h| has_dock_prop(h)) {
        return;
    }
    let state = saved.or(legacy).unwrap_or(ABS_ALWAYSONTOP);
    let Some(&first) = list.first() else {
        // Explorer absent or restarting: ABM_SETSTATE impossible. The file stays, so the next
        // run (or another `--restore`) restores the right original state instead of saving as
        // "original" the autohide set by us and persisted by Explorer.
        log!("taskbar: no Shell_TrayWnd, restore postponed (state file kept)");
        return;
    };
    // The tray host (of this process and of other dock processes) yields BEFORE the `TaskbarCreated`
    // broadcast and before deleting the state file: so apps re-register their icons
    // with Explorer. `yield_now` is asynchronous and does not block.
    crate::tray_host::yield_now();
    crate::tray_host::yield_foreign_hosts();
    let delivered = set_state(first, state);
    let flags = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS;
    for &h in &list {
        // SAFETY: Explorer windows: RemovePropW and the asynchronous variants do not block the caller.
        unsafe {
            let _ = RemovePropW(h, PROP_DOCK_HIDDEN);
            let _ = ShowWindowAsync(h, SW_SHOWNA);
            if let Err(e) = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, flags) {
                log!("taskbar: SetWindowPos(TOPMOST) failed: {e}");
            }
        }
    }
    // Deletes the state file BEFORE the broadcast: a live dock handling TaskbarCreated sees
    // the file gone (`restored_externally`) and does not hide the taskbar again. If however ABM_SETSTATE
    // did not reach Explorer (session shutting down, hung) the file stays: the autohide set by
    // us has already been persisted by Explorer and without a file the next run would save it as "original".
    // This way the watchdog, the next run's self-heal or `--restore` complete the restore.
    if delivered && legacy.is_some() {
        mark_legacy_consumed();
    }
    if !delivered {
        log!("taskbar: ABM_SETSTATE not delivered, state file kept");
    } else if let Some(p) = state_file_path() {
        // Deletes the name itself (never a link target), inside the admin-only data folder.
        if let Err(e) = std::fs::remove_file(p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log!("taskbar: removing state file failed: {e}");
            }
        }
    }
    broadcast_taskbar_created();
    log!("taskbar: restored (state {state})");
}

/// Like `restore_force()`, but first waits up to `max` for a `Shell_TrayWnd` to exist (Explorer
/// restarting). Only for watchdog and CLI `--restore`: blocks, NEVER from the UI thread.
pub fn restore_force_waiting(max: Duration) {
    let t = Instant::now();
    while native_taskbars().is_empty() && t.elapsed() < max {
        std::thread::sleep(Duration::from_millis(200));
    }
    restore_force();
}

// ------------------------------------------------------- tray host

/// true if this process hid the native taskbar (and has not restored yet).
/// `main` starts the tray host only in that case.
pub fn is_hidden() -> bool {
    HIDDEN.load(Relaxed)
}

/// true for a `Shell_TrayWnd` created by the tray host of ANY dock process (property
/// `tray_host::PROP_DOCK_TRAY_HOST`) or by this process. Used by `native_taskbars()`,
/// `on_winevent` and by the tray host to tell Explorer's taskbar apart.
pub fn is_dock_tray_host(h: HWND) -> bool {
    // SAFETY: static literal property name; invalid handle = property absent.
    !unsafe { GetPropW(h, crate::tray_host::PROP_DOCK_TRAY_HOST) }.is_invalid() || is_ours(h)
}

/// Announces our tray: `LAST_BROADCAST_MS = now` + `SendNotifyMessageW(HWND_BROADCAST,
/// TaskbarCreated)`, so apps re-register their icons with the first `Shell_TrayWnd` (our
/// host) and our `on_taskbar_created` ignores the broadcast. Called by `main` after a successful
/// `tray_host::start` and by `on_reannounce_timer`.
pub fn announce_tray() {
    if broadcast_taskbar_created() {
        log!("taskbar: tray announced (TaskbarCreated)");
    }
}

/// `LAST_BROADCAST_MS = now` + non-blocking `SendNotifyMessageW(HWND_BROADCAST, TaskbarCreated)`,
/// shared by `restore_force` and `announce_tray`. true if the broadcast was sent; failures are logged.
fn broadcast_taskbar_created() -> bool {
    let msg = ensure_taskbar_created_msg();
    if msg == 0 {
        return false;
    }
    LAST_BROADCAST_MS.store(now_ms(), Relaxed);
    // SAFETY: non-blocking broadcast (SendNotifyMessage); no pointers in the parameters.
    if let Err(e) = unsafe { SendNotifyMessageW(HWND_BROADCAST, msg, WPARAM(0), LPARAM(0)) } {
        log!("taskbar: TaskbarCreated broadcast failed: {e}");
        return false;
    }
    true
}

/// `TIMER_TRAY_REANNOUNCE` (one-shot, already killed by `main`): second announcement after an Explorer
/// restart, if the taskbar is still hidden and the tray host is active.
pub fn on_reannounce_timer() {
    let paused = HOST_PAUSED_AT.load(Relaxed);
    if paused != 0 {
        // Waiting for the new Explorer: its desktop first, then our host (`HOST_RESUME_MAX_MS` at most).
        let waited = now_ms().saturating_sub(paused);
        if !crate::shell_thread::desktop_registered() && waited < HOST_RESUME_MAX_MS {
            arm_reannounce(1000);
            return;
        }
        HOST_PAUSED_AT.store(0, Relaxed);
        if !HIDDEN.load(Relaxed) {
            return;
        }
        log!("taskbar: Explorer back after {waited} ms, starting our tray host again");
        let hook = crate::popup::suspend_mouse_hook();
        let started = crate::tray_host::start(crate::hwnd());
        crate::popup::resume_mouse_hook(hook);
        match started {
            Ok(()) => announce_tray(),
            Err(e) => log!("taskbar: tray host not restarted (icons stay in Explorer's tray): {e}"),
        }
        return;
    }
    if HIDDEN.load(Relaxed) && crate::tray_host::is_running() {
        announce_tray();
    }
}

/// `remove_hook()` + `restore()`. UI thread, in `cleanup` (right away, before the rest).
pub fn shutdown() {
    remove_hook();
    restore();
}

/// CLI `--restore`: signals stop to the running instance (`watchdog::signal_stop_to_instance`),
/// waits up to 3 s for it to exit, then `restore_force()`. Returns the process exit code (0 ok).
pub fn run_restore_cli() -> i32 {
    let running = crate::watchdog::signal_stop_to_instance();
    if running {
        log!("--restore: instance running, exit request sent");
        // Wait for the instance to really exit: its single-instance mutex disappears with the process.
        // (Not the state file: an older dock keeps it in the legacy folder.) Then restore over a quiet
        // taskbar, so the exiting dock cannot re-hide it on our TaskbarCreated broadcast.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) && crate::watchdog::instance_running() {
            std::thread::sleep(Duration::from_millis(100));
        }
        if crate::watchdog::instance_running() {
            log!("--restore: the instance did not exit within 5 s, restoring anyway");
        }
        // A moment to let the process shutdown finish.
        std::thread::sleep(Duration::from_millis(200));
    } else {
        log!("--restore: no running instance");
    }
    restore_force_waiting(Duration::from_secs(5));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_parsing_masks_and_rejects() {
        assert_eq!(parse_state(b"2"), Some(ABS_ALWAYSONTOP));
        assert_eq!(parse_state(b" 3\r\n"), Some(ABS_AUTOHIDE | ABS_ALWAYSONTOP));
        // Unknown bits from a (medium-writable) legacy file are masked away.
        assert_eq!(parse_state(b"4294967295"), Some(ABS_MASK));
        assert_eq!(parse_state(b"autohide"), None);
        assert_eq!(parse_state(b""), None);
        assert_eq!(parse_state(&[0xFF, 0xFE]), None);
    }

    #[test]
    fn legacy_state_ignored_without_data_folder() {
        // Unit tests never initialize the data folder: nothing legacy is read or recorded.
        assert_eq!(read_legacy_state(), None);
        assert!(legacy_marker_path().is_none());
    }
}
