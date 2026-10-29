//! Tray host: the notification-area icons of OTHER apps, hosted in the dock.
//! Code written from scratch; lightbar/ManagedShell/RetroBar (MIT) only as inspiration.
//!
//! THREAD
//!  * `tray-host`: HIDDEN top-level window of class `Shell_TrayWnd` (WS_POPUP, WS_EX_TOPMOST |
//!    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, never visible), property `DockTrayHost`,
//!    `ChangeWindowMessageFilterEx(WM_COPYDATA | WM_COMMAND | DockTrayYield, MSGFLT_ALLOW)`, child
//!    `TrayNotifyWnd`, copy of `TaskbandHWND` from Explorer. Receives `WM_COPYDATA` (dwData 1 = NIM_*,
//!    0 = AppBar forwarded to Explorer except GETTASKBARPOS, 3 = icon rect). 1 s timer (stays
//!    first in the Z-order unless `YIELDED`), 5 s timer (dead owners).
//!  * `tray-fwd`: queue (max 256) of NIM_* copies forwarded to Explorer with
//!    `SendMessageTimeoutW(SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT, 2000)`, only if `taskbar::is_hidden()`.
//!    Icon state lives in a `Mutex` with short locks (never held during SendMessage* or bitmap conversions). The UI
//!    reads `snapshot()` when `WM_DOCK_TRAY_CHANGED` arrives. UI calls (`click`, `hover`,
//!    `set_*`) do not block: `SendNotifyMessageW` / `PostMessageW`.
//!
//! RECOVERY: `yield_now()` (any thread, asynchronous) sends the window to the bottom and stops
//! repositioning; `stop()` destroys it. Both run BEFORE the restore's `TaskbarCreated` broadcast
//! (see `taskbar::restore_force`), so apps re-register their icons with Explorer.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{w, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_SUCCESS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::{SHAllocShared, SHFreeShared, SHLockShared, SHUnlockShared};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, ChangeWindowMessageFilterEx, CopyIcon, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyWindow, DispatchMessageW, EnumWindows, FindWindowExW, FindWindowW, GetMessageW, GetPropW, GetWindowLongW,
    GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible, KillTimer, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SendMessageTimeoutW, SendNotifyMessageW, SetPropW, SetTimer,
    SetWindowPos, ShowWindow, TranslateMessage, UnregisterClassW, ASFW_ANY, CS_DBLCLKS, GWL_STYLE, HICON, HWND_BOTTOM,
    HWND_TOPMOST, MSG, MSGFLT_ALLOW, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, WINDOWPOS, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WM_USER, WM_WINDOWPOSCHANGED, WNDCLASSEXW,
    WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP,
};

use crate::icons::{self, IconBitmap};
use crate::log;
use crate::msg::WM_DOCK_TRAY_CHANGED;

/// Window property that marks OUR `Shell_TrayWnd` (also read by the watchdog process and
/// by the `--restore` CLI through `taskbar::is_dock_tray_host`).
pub const PROP_DOCK_TRAY_HOST: PCWSTR = w!("DockTrayHost");
/// Registered message (`RegisterWindowMessageW`) that another dock process (watchdog/`--restore`)
/// sends to our host to make it yield: `yield_now()` + `DestroyWindow`.
pub const YIELD_MSG_NAME: PCWSTR = w!("DockTrayYield");

/// A tray icon to draw (only visible icons, not hidden by `config.tray_hidden_guids`, in
/// registration order).
#[derive(Clone, Debug)]
pub struct TrayView {
    /// Stable id, never reused (goes into `render::ItemId::Tray`).
    pub id: u64,
    /// Premultiplied bitmap ALREADY scaled to `set_icon_px` (None = conversion failed: do not draw).
    pub bitmap: Option<Arc<IconBitmap>>,
    /// Stable key for the `tray_hidden` config (see `item_key`).
    pub key: String,
    /// Short app name (the exe's FileDescription; fallback: first tooltip line, exe name).
    pub name: String,
    /// true if the user chose not to show the icon in the bar (`config.tray_hidden`).
    pub user_hidden: bool,
}

/// Dock geometry as seen by the apps (screen coordinates, physical px).
#[derive(Clone, Copy, Debug, Default)]
pub struct TrayGeometry {
    /// Rectangle of the SHOWN bar.
    pub bar: RECT,
    /// Rectangle of the tray area within the bar (for `ABM_GETTASKBARPOS` and for rect requests
    /// of icons not laid out yet).
    pub tray_area: RECT,
    /// `ABE_BOTTOM` (3) o `ABE_TOP` (1).
    pub edge: u32,
}

/// Kind of click forwarded to the owning app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayClick {
    Left,
    Right,
    Middle,
    /// Second left click within `GetDoubleClickTime` on the same icon (the first one was already
    /// sent as `Left`).
    Double,
}

// ------------------------------------------------------------------ wire format

/// Signature of `SHELLTRAYDATA` / rect requests.
const TRAY_SIGNATURE: u32 = 0x3475_3423;

const NIM_ADD: u32 = 0;
const NIM_MODIFY: u32 = 1;
const NIM_DELETE: u32 = 2;
const NIM_SETFOCUS: u32 = 3;
const NIM_SETVERSION: u32 = 4;

const NIF_MESSAGE: u32 = 0x01;
const NIF_ICON: u32 = 0x02;
const NIF_TIP: u32 = 0x04;
const NIF_STATE: u32 = 0x08;
const NIF_GUID: u32 = 0x20;
const NIS_HIDDEN: u32 = 0x1;

const NIN_SELECT: u32 = 0x400;

const ABM_QUERYPOS: u32 = 2;
const ABM_SETPOS: u32 = 3;
const ABM_GETTASKBARPOS: u32 = 5;

/// 32-bit-handle `NOTIFYICONDATA`, as it travels inside `WM_COPYDATA`.
/// Layout adapted from lightbar (MIT, Copyright (c) 2026 SauerExe), `src/tray.rs`; see THIRD_PARTY_NOTICES.md.
#[repr(C)]
#[derive(Clone, Copy)]
struct NotifyIconData32 {
    cb_size: u32,
    hwnd: u32,
    uid: u32,
    flags: u32,
    callback: u32,
    hicon: u32,
    tip: [u16; 128],
    state: u32,
    state_mask: u32,
    info: [u16; 256],
    version: u32,
    info_title: [u16; 64],
    info_flags: u32,
    guid: GUID,
    balloon_icon: u32,
}

/// `SHELLTRAYDATA`: header + `NotifyIconData32`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ShellTrayData {
    signature: u32,
    message: u32,
    nid: NotifyIconData32,
}

/// 32-bit-handle `APPBARDATA`.
/// Adapted from lightbar (MIT, Copyright (c) 2026 SauerExe), `src/bar.rs`; see THIRD_PARTY_NOTICES.md.
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

/// `APPBARMSGDATAV3` (64 bytes). Adapted from lightbar (MIT), like `AppBarData32`.
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

const _: () = assert!(
    std::mem::size_of::<NotifyIconData32>() == 956
        && std::mem::size_of::<ShellTrayData>() == 964
        && std::mem::offset_of!(NotifyIconData32, tip) == 24
        && std::mem::offset_of!(NotifyIconData32, info) == 288
        && std::mem::offset_of!(NotifyIconData32, version) == 800
        && std::mem::offset_of!(NotifyIconData32, guid) == 936
        && std::mem::size_of::<AppBarData32>() == 0x28
        && std::mem::size_of::<AppBarMsg>() == 0x40
        && std::mem::offset_of!(AppBarMsg, message) == 40
        && std::mem::offset_of!(AppBarMsg, shared) == 48
        && std::mem::offset_of!(AppBarMsg, pid) == 56
);

/// Copies at most `bytes.len()` bytes into a `T` struct; the rest stays zero (old senders send
/// shorter structs). `T` must be a `repr(C)` type made only of integers/integer arrays/GUIDs.
fn from_prefix<T: Copy>(bytes: &[u8]) -> T {
    // SAFETY: for the only `T`s used here (repr(C) structs of integers and GUIDs) the all-zero bit pattern
    // is a valid value.
    let mut v: T = unsafe { std::mem::zeroed() };
    let n = bytes.len().min(std::mem::size_of::<T>());
    // SAFETY: copies `n <= size_of::<T>()` bytes from `bytes` (at least `n` long) into `v`, which does not
    // overlap the slice; every bit pattern is valid for `T`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (&mut v as *mut T).cast::<u8>(), n) };
    v
}

/// Decodes a `WM_COPYDATA` with `dwData == 1`; `None` if too short or with a wrong signature.
fn parse_shell_tray(bytes: &[u8]) -> Option<ShellTrayData> {
    if bytes.len() < 8 + 24 {
        return None;
    }
    let d: ShellTrayData = from_prefix(bytes);
    (d.signature == TRAY_SIGNATURE).then_some(d)
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}

/// Sign-extends a 32-bit handle.
fn ext(h: u32) -> isize {
    h as i32 as isize
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

fn guid_is_zero(g: &GUID) -> bool {
    *g == GUID::zeroed()
}

/// Lowercase GUID without braces (format of `config.tray_hidden_guids`).
fn guid_string(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

/// UTF-16 string up to the first NUL (the buffer may not be terminated).
fn utf16_until_nul(buf: &[u16]) -> String {
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

// ------------------------------------------------------------------ state

struct TrayItem {
    id: u64,
    owner: isize,
    pid: u32,
    uid: u32,
    guid: Option<GUID>,
    callback: u32,
    version: u32,
    hidden: bool,
    tip: String,
    /// Native-size source (needed to rescale on DPI change).
    src: Option<Arc<IconBitmap>>,
    /// Version scaled to `ICON_PX`.
    bitmap: Option<Arc<IconBitmap>>,
    /// Owner's exe name (diagnostics and `tray_hidden` key).
    exe: String,
    /// Short app name (FileDescription), empty if unavailable.
    name: String,
}

struct TrayState {
    items: Vec<TrayItem>,
    next_id: u64,
    geom: TrayGeometry,
    rects: Vec<(u64, RECT)>,
}

const EMPTY_RECT: RECT = RECT { left: 0, top: 0, right: 0, bottom: 0 };

static TRAY: Mutex<TrayState> = Mutex::new(TrayState {
    items: Vec::new(),
    next_id: 1,
    geom: TrayGeometry { bar: EMPTY_RECT, tray_area: EMPTY_RECT, edge: 3 },
    rects: Vec::new(),
});

/// Tray area to show to the apps; if the UI has not reported it yet (empty rect) the bar is used.
fn area_rect(g: &TrayGeometry) -> RECT {
    let a = g.tray_area;
    if a.right > a.left && a.bottom > a.top {
        a
    } else {
        g.bar
    }
}

fn state() -> MutexGuard<'static, TrayState> {
    TRAY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Raw HWND of the host window (0 = absent).
static HOST_HWND: AtomicIsize = AtomicIsize::new(0);
/// Raw HWND of the dock's UI window.
static UI_HWND: AtomicIsize = AtomicIsize::new(0);
/// true from the creation of the host window until its destruction.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Permanent flag: the host has yielded to Explorer.
static YIELDED: AtomicBool = AtomicBool::new(false);
/// Side in physical px to scale the bitmaps to.
static ICON_PX: AtomicU32 = AtomicU32::new(16);
/// Id of the registered message `DockTrayYield` (0 = not registered yet).
static YIELD_MSG: AtomicU32 = AtomicU32::new(0);

/// Module threads.
type ThreadPair = (Option<JoinHandle<()>>, Option<JoinHandle<()>>);
static THREADS: Mutex<ThreadPair> = Mutex::new((None, None));

/// Forwarding queue towards Explorer: (wParam, original bytes).
static FWD_QUEUE: Mutex<VecDeque<(usize, Vec<u8>)>> = Mutex::new(VecDeque::new());
static FWD_CV: Condvar = Condvar::new();
static FWD_STOP: AtomicBool = AtomicBool::new(false);
static FWD_OVERFLOW_LOGGED: AtomicBool = AtomicBool::new(false);
const FWD_QUEUE_MAX: usize = 256;

/// Total cap on hosted tray icons and cap per owning process.
const MAX_ITEMS: usize = 256;
const MAX_ITEMS_PER_PID: usize = 32;
static CAP_LOGGED: AtomicBool = AtomicBool::new(false);

// Private messages of the host window.
const WM_HOST_RESIZE: u32 = WM_APP + 1;
const WM_HOST_RAISE: u32 = WM_APP + 2;
const WM_HOST_RESCALE: u32 = WM_APP + 3;

const TIMER_ZORDER: usize = 1;
const TIMER_DEAD: usize = 2;

fn yield_msg_id() -> u32 {
    let cur = YIELD_MSG.load(Relaxed);
    if cur != 0 {
        return cur;
    }
    // SAFETY: static NUL-terminated string literal.
    let id = unsafe { RegisterWindowMessageW(YIELD_MSG_NAME) };
    if id == 0 {
        log!("tray: RegisterWindowMessageW(DockTrayYield) failed: {:?}", windows::core::Error::from_thread());
    }
    YIELD_MSG.store(id, Relaxed);
    id
}

fn post_ui(kind: usize) {
    let ui = UI_HWND.load(Relaxed);
    if ui == 0 {
        return;
    }
    // SAFETY: PostMessageW is thread-safe; if the window no longer exists it merely fails.
    if let Err(e) = unsafe { PostMessageW(Some(hwnd_of(ui)), WM_DOCK_TRAY_CHANGED, WPARAM(kind), LPARAM(0)) } {
        log!("tray: PostMessageW(WM_DOCK_TRAY_CHANGED) failed: {e}");
    }
}

fn post_host(msg: u32) {
    let h = HOST_HWND.load(Relaxed);
    if h == 0 {
        return;
    }
    // SAFETY: PostMessageW is thread-safe; if the window is gone it merely fails.
    let _ = unsafe { PostMessageW(Some(hwnd_of(h)), msg, WPARAM(0), LPARAM(0)) };
}

// ------------------------------------------------------------------ pure NIM_* logic

/// What to do with the icon on a `NIM_ADD`/`NIM_MODIFY` with `NIF_ICON`.
enum IconUpdate {
    Keep,
    Clear,
    /// (source, scaled)
    Set(Arc<IconBitmap>, Arc<IconBitmap>),
}

struct NimResult {
    /// Return value of the `WM_COPYDATA` (true = 1).
    ret: bool,
    /// 0 nothing, 1 bitmap/tooltip only, 2 the set of visible icons changes.
    change: usize,
}

fn is_visible_item(t: &TrayItem) -> bool {
    !t.hidden && t.bitmap.is_some()
}

fn visible_ids(items: &[TrayItem]) -> Vec<u64> {
    items.iter().filter(|t| is_visible_item(t)).map(|t| t.id).collect()
}

/// Identity: GUID if present, otherwise `(hWnd, uID)`.
fn find_item(items: &[TrayItem], owner: isize, uid: u32, guid: Option<GUID>) -> Option<usize> {
    match guid {
        Some(g) => items.iter().position(|t| t.guid == Some(g)),
        None => items.iter().position(|t| t.owner == owner && t.uid == uid),
    }
}

/// Applies the fields indicated by the flags; returns true if the bitmap or tooltip changed.
fn apply_fields(t: &mut TrayItem, nid: &NotifyIconData32, icon: IconUpdate) -> bool {
    let mut changed = false;
    if nid.flags & NIF_MESSAGE != 0 {
        t.callback = nid.callback;
    }
    if nid.flags & NIF_STATE != 0 && nid.state_mask & NIS_HIDDEN != 0 {
        t.hidden = nid.state & NIS_HIDDEN != 0;
    }
    if nid.flags & NIF_TIP != 0 {
        let tip = utf16_until_nul(&nid.tip);
        if tip != t.tip {
            t.tip = tip;
            changed = true;
        }
    }
    match icon {
        IconUpdate::Keep => {}
        IconUpdate::Clear => {
            changed |= t.bitmap.is_some();
            t.src = None;
            t.bitmap = None;
        }
        IconUpdate::Set(src, scaled) => {
            t.src = Some(src);
            t.bitmap = Some(scaled);
            changed = true;
        }
    }
    changed
}

/// Core of `dwData == 1` (pure, tested). `owner_pid` = owner's pid if the window exists.
fn apply_nim(
    items: &mut Vec<TrayItem>,
    next_id: &mut u64,
    d: &ShellTrayData,
    owner_pid: Option<u32>,
    icon: IconUpdate,
    exe: &str,
) -> NimResult {
    let nid = &d.nid;
    let owner = ext(nid.hwnd);
    let guid = (nid.flags & NIF_GUID != 0 && !guid_is_zero(&nid.guid)).then_some(nid.guid);
    let before = visible_ids(items);
    let idx = find_item(items, owner, nid.uid, guid);
    let mut touched = false;
    let ret = match d.message {
        NIM_ADD => match (owner_pid, idx) {
            (None, _) => false,
            // Same GUID but different owner (app restarted without DELETE): replace the owner.
            (Some(pid), Some(i)) if guid.is_some() && items[i].owner != owner => {
                let t = &mut items[i];
                t.owner = owner;
                t.pid = pid;
                t.uid = nid.uid;
                t.exe = exe.to_string();
                touched = apply_fields(t, nid, icon);
                true
            }
            (Some(_), Some(_)) => false,
            // Caps: each icon holds two bitmaps in the elevated process; a sender looping over
            // different uIDs must not make memory grow without bound.
            (Some(pid), None)
                if items.len() >= MAX_ITEMS || items.iter().filter(|t| t.pid == pid).count() >= MAX_ITEMS_PER_PID =>
            {
                if !CAP_LOGGED.swap(true, Relaxed) {
                    log!("tray: icon limit reached ({MAX_ITEMS} total / {MAX_ITEMS_PER_PID} per process), NIM_ADD rejected");
                }
                false
            }
            (Some(pid), None) => {
                let mut t = TrayItem {
                    id: *next_id,
                    owner,
                    pid,
                    uid: nid.uid,
                    guid,
                    callback: 0,
                    version: 0,
                    hidden: false,
                    tip: String::new(),
                    src: None,
                    bitmap: None,
                    exe: exe.to_string(),
                    name: String::new(),
                };
                *next_id += 1;
                touched = apply_fields(&mut t, nid, icon);
                items.push(t);
                true
            }
        },
        NIM_MODIFY => match idx {
            Some(i) => {
                touched = apply_fields(&mut items[i], nid, icon);
                true
            }
            None => false,
        },
        NIM_DELETE => match idx {
            Some(i) => {
                items.remove(i);
                true
            }
            None => false,
        },
        NIM_SETFOCUS => true,
        NIM_SETVERSION => match idx {
            Some(i) if nid.version <= 4 => {
                items[i].version = nid.version;
                true
            }
            _ => false,
        },
        _ => idx.is_some(),
    };
    let change = if visible_ids(items) != before {
        2
    } else if touched {
        1
    } else {
        0
    };
    NimResult { ret, change }
}

// ------------------------------------------------------------------ callbacks to the apps

/// Encodes (wParam, lParam) for the icon version. Adapted from lightbar (MIT, `src/tray.rs`). In v4 the coordinates are signed 16
/// bit (the receiver sign-extends them again with `GET_X_LPARAM`).
fn encode(version: u32, uid: u32, ax: i32, ay: i32, ev: u32) -> (usize, isize) {
    if version >= 4 {
        let wp = (ax as u16 as usize) | ((ay as u16 as usize) << 16);
        let lp = ((ev & 0xFFFF) as isize) | (((uid & 0xFFFF) as isize) << 16);
        (wp, lp)
    } else {
        (uid as usize, ev as isize)
    }
}

/// Sequence of events sent for a click, as Explorer does.
fn click_events(kind: TrayClick, version: u32) -> Vec<u32> {
    let new_style = version >= 3;
    let mut v = match kind {
        TrayClick::Left => vec![WM_LBUTTONDOWN, WM_LBUTTONUP],
        TrayClick::Right => vec![WM_RBUTTONDOWN, WM_RBUTTONUP],
        TrayClick::Middle => vec![WM_MBUTTONDOWN, WM_MBUTTONUP],
        TrayClick::Double => vec![WM_LBUTTONDBLCLK, WM_LBUTTONUP],
    };
    if new_style {
        match kind {
            TrayClick::Left | TrayClick::Double => v.push(NIN_SELECT),
            TrayClick::Right => v.push(WM_CONTEXTMENU),
            TrayClick::Middle => {}
        }
    }
    v
}

struct Target {
    id: u64,
    owner: isize,
    pid: u32,
    uid: u32,
    callback: u32,
    version: u32,
    exe: String,
}

fn target_of(id: u64) -> Option<Target> {
    let st = state();
    st.items.iter().find(|t| t.id == id).map(|t| Target {
        id: t.id,
        owner: t.owner,
        pid: t.pid,
        uid: t.uid,
        callback: t.callback,
        version: t.version,
        exe: t.exe.clone(),
    })
}

/// true if the owning window still exists and belongs to the same registered process.
fn owner_alive(owner: isize, pid: u32) -> bool {
    let h = hwnd_of(owner);
    // SAFETY: no preconditions; returns false for an invalid handle.
    if !unsafe { IsWindow(Some(h)) }.as_bool() {
        return false;
    }
    window_pid(h) == pid
}

fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: `pid` is valid and writable; for an invalid handle the call fails leaving 0.
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

/// Removes icon `id` (dead owner) and notifies the UI if it was visible.
fn remove_dead(id: u64, exe: &str) {
    let was_visible = {
        let mut st = state();
        match st.items.iter().position(|t| t.id == id) {
            Some(i) => {
                let v = is_visible_item(&st.items[i]);
                st.items.remove(i);
                Some(v)
            }
            None => None,
        }
    };
    if let Some(v) = was_visible {
        log!("tray: owner of #{id} ({exe}) no longer valid, icon removed");
        if v {
            post_ui(2);
        }
    }
}

fn allow_foreground(pid: u32) {
    // SAFETY: no preconditions; `pid` is the icon owner's.
    if let Err(e) = unsafe { AllowSetForegroundWindow(pid) } {
        log!("tray: AllowSetForegroundWindow({pid}) failed: {e}, falling back to ASFW_ANY");
        // SAFETY: no preconditions.
        if let Err(e2) = unsafe { AllowSetForegroundWindow(ASFW_ANY) } {
            log!("tray: AllowSetForegroundWindow(ASFW_ANY) failed: {e2}");
        }
    }
}

/// Pure gating decision for any message the dock sends on behalf of a tray registration
/// (callback clicks, hover `WM_MOUSEMOVE`, `NIN_*`, `WM_CONTEXTMENU`).
///
/// UIPI: any medium-integrity process can register an icon whose owner HWND belongs to an elevated
/// process. Our elevated dock would then deliver attacker-chosen messages across the integrity
/// boundary. So owners above medium (or of unknown integrity) are refused, unless an administrator
/// opted in (`trust_elevated_tray_icons`, HKLM registry); even then, system messages (`callback < WM_USER`, e.g.
/// WM_COMMAND/WM_SYSCOMMAND) are never sent to them.
fn callback_permitted(owner_at_most_medium: bool, callback: u32, trust_elevated: bool) -> bool {
    owner_at_most_medium || (trust_elevated && callback >= WM_USER)
}

/// Admin-only opt-in: `HKLM\SOFTWARE\mini-oled-dock`, DWORD `TrustElevatedTrayIcons` (1 = trust).
/// Deliberately NOT in config.json, which any medium-integrity process can write (the gate would
/// fail open). HKLM\SOFTWARE is writable only by administrators.
const TRUST_KEY: PCWSTR = w!("SOFTWARE\\mini-oled-dock");
const TRUST_VALUE: PCWSTR = w!("TrustElevatedTrayIcons");
/// Maximum age of the cached registry value.
const TRUST_CACHE_TTL: Duration = Duration::from_secs(10);
/// Cached (read time, value) of the opt-in; `None` = never read or invalidated.
static TRUST_CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

/// Reads the opt-in from the 64-bit registry view. Default false on any error or if missing.
fn read_trust_elevated_tray_icons() -> bool {
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `data`/`size` are valid writable locals of the declared size; key/value names are
    // static NUL-terminated wide strings. RRF_RT_REG_DWORD guarantees at most 4 bytes are written.
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            TRUST_KEY,
            TRUST_VALUE,
            RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
            None,
            Some((&mut data as *mut u32).cast::<c_void>()),
            Some(&mut size),
        )
    };
    r == ERROR_SUCCESS && data == 1
}

/// Cached opt-in, re-read from the registry at most every `TRUST_CACHE_TTL` (or after
/// `invalidate_trust_cache`).
fn trust_elevated_tray_icons() -> bool {
    let mut c = TRUST_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((t, v)) = *c {
        if t.elapsed() < TRUST_CACHE_TTL {
            return v;
        }
    }
    let v = read_trust_elevated_tray_icons();
    if c.is_none_or(|(_, old)| old != v) {
        log!("tray: TrustElevatedTrayIcons (HKLM) = {v}");
    }
    *c = Some((Instant::now(), v));
    v
}

/// Forces the next check to re-read the registry (called on config reload).
pub fn invalidate_trust_cache() {
    let mut c = TRUST_CACHE.lock().unwrap_or_else(|p| p.into_inner());
    // Keep the last value (for change logging) but mark it as expired; if the clock cannot go
    // back that far, simply forget it.
    *c = match (*c, Instant::now().checked_sub(TRUST_CACHE_TTL)) {
        (Some((_, v)), Some(expired)) => Some((expired, v)),
        _ => None,
    };
}

/// Owners for which a blocked callback was already logged (log once per owner). Bounded.
static BLOCKED_LOGGED: Mutex<Vec<isize>> = Mutex::new(Vec::new());
const BLOCKED_LOGGED_CAP: usize = 256;

/// true if the dock may send messages on behalf of icon `t`: the owner HWND still belongs to the
/// registered pid and `callback_permitted` allows it. Logs once per owner when blocked.
fn callback_allowed(t: &Target) -> bool {
    if !owner_alive(t.owner, t.pid) {
        return false;
    }
    let trust = trust_elevated_tray_icons();
    if callback_permitted(pid_at_most_medium(t.pid), t.callback, trust) {
        return true;
    }
    let mut logged = BLOCKED_LOGGED.lock().unwrap_or_else(|p| p.into_inner());
    if !logged.contains(&t.owner) {
        if logged.len() >= BLOCKED_LOGGED_CAP {
            logged.clear();
        }
        logged.push(t.owner);
        log!(
            "tray: messages of icon #{} ({}, callback 0x{:X}) to owner 0x{:X} (pid {}) blocked: owner runs above medium integrity{}",
            t.id,
            t.exe,
            t.callback,
            t.owner,
            t.pid,
            if trust {
                " and the callback is a system message"
            } else {
                " (see HKLM\\SOFTWARE\\mini-oled-dock TrustElevatedTrayIcons)"
            }
        );
    }
    false
}

fn send_callback(t: &Target, ev: u32, anchor: POINT) {
    if !callback_allowed(t) {
        return;
    }
    let (wp, lp) = encode(t.version, t.uid, anchor.x, anchor.y, ev);
    // SAFETY: SendNotifyMessageW to another window does not block; if the window disappears it merely fails.
    if let Err(e) = unsafe { SendNotifyMessageW(hwnd_of(t.owner), t.callback, WPARAM(wp), LPARAM(lp)) } {
        log!("tray: SendNotifyMessageW(#{}, 0x{ev:X}) failed: {e}", t.id);
    }
}

// ------------------------------------------------------------------ public API

/// Starts the `tray-host` thread (creates the window, marker, UIPI filter, timers) and waits up to 2 s
/// for it to be ready. `ui_hwnd` receives `WM_DOCK_TRAY_CHANGED`. Called by `main` ONLY if
/// `taskbar::is_hidden()` and `config.show_tray`; on success `main` calls
/// `taskbar::announce_tray()`. Error = no tray in the dock (the icons stay with Explorer).
pub fn start(ui_hwnd: HWND) -> Result<(), String> {
    if RUNNING.load(Relaxed) {
        return Ok(());
    }
    YIELDED.store(false, Relaxed);
    FWD_STOP.store(false, Relaxed);
    FWD_OVERFLOW_LOGGED.store(false, Relaxed);
    UI_HWND.store(ui_hwnd.0 as isize, Relaxed);
    yield_msg_id();
    {
        let mut st = state();
        st.items.clear();
        st.rects.clear();
    }
    FWD_QUEUE.lock().unwrap_or_else(|e| e.into_inner()).clear();

    let fwd = std::thread::Builder::new()
        .name("tray-fwd".into())
        .stack_size(128 * 1024)
        .spawn(forward_main)
        .map_err(|e| format!("thread tray-fwd not started: {e}"))?;
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let host = match std::thread::Builder::new().name("tray-host".into()).spawn(move || host_main(tx)) {
        Ok(h) => h,
        Err(e) => {
            stop_forwarder(Some(fwd));
            return Err(format!("thread tray-host not started: {e}"));
        }
    };
    match rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => {
            let mut th = THREADS.lock().unwrap_or_else(|e| e.into_inner());
            *th = (Some(host), Some(fwd));
            log!("tray: host started");
            Ok(())
        }
        Ok(Err(e)) => {
            stop_forwarder(Some(fwd));
            let _ = host.join();
            Err(e)
        }
        Err(_) => {
            // The thread might complete later: ask it to close and give up.
            post_host(WM_CLOSE);
            stop_forwarder(Some(fwd));
            Err("tray host not ready within 2 s".into())
        }
    }
}

/// Stops the host thread: `WM_CLOSE` to the window, `DestroyWindow`, join for at most 1500 ms; on timeout
/// `yield_now()`. Idempotent. In `cleanup` BEFORE `taskbar::shutdown()`.
pub fn stop() {
    let (host, fwd) = {
        let mut th = THREADS.lock().unwrap_or_else(|e| e.into_inner());
        (th.0.take(), th.1.take())
    };
    post_host(WM_CLOSE);
    if let Some(h) = host {
        let t = Instant::now();
        while !h.is_finished() && t.elapsed() < Duration::from_millis(1500) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if h.is_finished() {
            if h.join().is_err() {
                log!("tray: tray-host thread terminated with a panic");
            }
        } else {
            log!("tray: host did not close within 1500 ms, yielding to Explorer");
            yield_now();
        }
    }
    stop_forwarder(fwd);
}

fn stop_forwarder(fwd: Option<JoinHandle<()>>) {
    FWD_STOP.store(true, Relaxed);
    FWD_CV.notify_all();
    if let Some(h) = fwd {
        let t = Instant::now();
        while !h.is_finished() && t.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(10));
        }
        if h.is_finished() {
            let _ = h.join();
        } else {
            log!("tray: tray-fwd thread busy, leaving it to the process");
        }
    }
}

/// true if the host thread is running (even if it yielded to Explorer): `stop` has something to stop.
pub fn is_started() -> bool {
    RUNNING.load(Relaxed)
}

/// true if the host window exists and is active (not yielded).
pub fn is_running() -> bool {
    RUNNING.load(Relaxed) && !YIELDED.load(Relaxed)
}

/// Stable key of an icon for `config.tray_hidden`: lowercase GUID without braces if present,
/// otherwise `<lowercase exe name>#<uID>` (unknown exe = "?").
fn item_key(t: &TrayItem) -> String {
    match &t.guid {
        Some(g) => guid_string(g),
        None => {
            let exe = if t.exe.is_empty() { "?".to_string() } else { t.exe.to_lowercase() };
            format!("{exe}#{}", t.uid)
        }
    }
}

/// Name to show for the icon: FileDescription, then first tooltip line, then exe without extension.
fn display_name(t: &TrayItem) -> String {
    if !t.name.trim().is_empty() {
        return t.name.trim().to_string();
    }
    if let Some(l) = t.tip.lines().next().map(str::trim).filter(|l| !l.is_empty()) {
        return l.to_string();
    }
    let stem = t.exe.rsplit_once('.').map_or(t.exe.as_str(), |(a, _)| a);
    if stem.is_empty() {
        "App".to_string()
    } else {
        stem.to_string()
    }
}

fn view_of(t: &TrayItem, user_hidden: bool) -> TrayView {
    TrayView { id: t.id, bitmap: t.bitmap.clone(), key: item_key(t), name: display_name(t), user_hidden }
}

fn user_hidden_in(cfg: &crate::config::Config, key: &str) -> bool {
    cfg.tray_hidden.iter().any(|h| h.eq_ignore_ascii_case(key))
}

/// All icons with a bitmap (excluding only the system ones in `tray_hidden_guids`), including those
/// hidden by the user (`user_hidden == true`). For the chevron popup.
pub fn snapshot_all() -> Vec<TrayView> {
    collect(true)
}

/// true if at least one icon that would otherwise be shown is hidden by the user (the bar shows the
/// "hidden icons" chevron for it). Same filters as `collect`; allocates (keys), so call it on state changes, not per frame.
pub fn any_user_hidden() -> bool {
    let cfg = crate::config::get();
    if cfg.tray_hidden.is_empty() {
        return false;
    }
    let st = state();
    st.items.iter().filter(|t| is_visible_item(t) && !guid_hidden(&cfg, t)).any(|t| user_hidden_in(&cfg, &item_key(t)))
}

fn guid_hidden(cfg: &crate::config::Config, t: &TrayItem) -> bool {
    match &t.guid {
        Some(g) => {
            let s = guid_string(g);
            cfg.tray_hidden_guids.iter().any(|h| h.eq_ignore_ascii_case(&s))
        }
        None => false,
    }
}

/// Builds the views; with `include_hidden == false` user-hidden icons are skipped BEFORE the
/// view is built (no bitmap Arc clone / String copies for them). No cap on the count.
fn collect(include_hidden: bool) -> Vec<TrayView> {
    let cfg = crate::config::get();
    let st = state();
    st.items
        .iter()
        .filter(|t| is_visible_item(t))
        .filter(|t| !guid_hidden(&cfg, t))
        .filter_map(|t| {
            let hidden = user_hidden_in(&cfg, &item_key(t));
            (include_hidden || !hidden).then(|| view_of(t, hidden))
        })
        .collect()
}

/// Candidate icons for the bar: `snapshot_all()` without those hidden by the user, in stable order.
/// Hidden ones are reachable only from the chevron popup (`snapshot_all`).
pub fn snapshot_visible() -> Vec<TrayView> {
    collect(false)
}

/// Same as `snapshot_visible()` (historical name, used by `ui`).
pub fn snapshot() -> Vec<TrayView> {
    snapshot_visible()
}

/// Sets whether icon `key` is hidden by the user: atomic config write and
/// `WM_DOCK_TRAY_CHANGED` to the UI (the bar updates immediately). Callable from the UI thread: a
/// small file write.
pub fn set_user_hidden(key: &str, hidden: bool) {
    let r = crate::config::modify(|c| {
        c.tray_hidden.retain(|h| !h.eq_ignore_ascii_case(key));
        if hidden {
            c.tray_hidden.push(key.to_string());
        }
    });
    if let Err(e) = r {
        log!("tray: saving tray_hidden failed: {e}");
        return;
    }
    post_ui(2);
}

/// Side (physical px) to scale the bitmaps to (`Metrics::px(16)`); on change the host thread rescales from the
/// sources and posts `WM_DOCK_TRAY_CHANGED` (1). Called by the UI at startup and on every DPI change.
pub fn set_icon_px(px: u32) {
    let px = px.max(1);
    if ICON_PX.swap(px, Relaxed) != px {
        post_host(WM_HOST_RESCALE);
    }
}

/// Forwards a click to the app owning icon `id` (encode for `uVersion`, `AllowSetForegroundWindow`,
/// `SendNotifyMessageW`). `anchor` = cursor point in physical screen coordinates. Dead owner =
/// icon removed + `WM_DOCK_TRAY_CHANGED` (2). Does not block.
pub fn click(id: u64, kind: TrayClick, anchor: POINT) {
    let Some(t) = target_of(id) else { return };
    if !owner_alive(t.owner, t.pid) {
        remove_dead(t.id, &t.exe);
        return;
    }
    if t.callback == 0 {
        log!("tray: icon #{id} ({}) has no callback message", t.exe);
        return;
    }
    if !callback_allowed(&t) {
        return;
    }
    allow_foreground(t.pid);
    let anchor = if kind == TrayClick::Middle { anchor } else { menu_anchor(anchor) };
    if kind != TrayClick::Middle {
        note_click(t.pid);
    }
    for ev in click_events(kind, t.version) {
        send_callback(&t, ev, anchor);
    }
}

// ------------------------------------------------------------------ app context menus

/// Duration (ms) after a click during which the app is assumed to be opening a menu/flyout.
const MENU_GRACE_MS: u64 = 1200;
/// Maximum duration (ms) for which an app popup window covering the bar counts as an open menu.
const MENU_WINDOW_MS: u64 = 15_000;

/// Last forwarded click: milliseconds since start (0 = never) and the app's pid.
static LAST_CLICK_MS: AtomicU64 = AtomicU64::new(0);
static LAST_CLICK_PID: AtomicU32 = AtomicU32::new(0);

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn note_click(pid: u32) {
    LAST_CLICK_PID.store(pid, Relaxed);
    LAST_CLICK_MS.store(now_ms(), Relaxed);
}

/// Point passed to the app as the menu anchor: with the bar at the top it is just BELOW the bar, so
/// menus (v4: `wParam`) open below and not over it; at the bottom it stays the cursor.
fn menu_anchor(p: POINT) -> POINT {
    let g = state().geom;
    if g.edge == 1 && g.bar.bottom > g.bar.top {
        POINT { x: p.x, y: g.bar.bottom + 1 }
    } else {
        p
    }
}

fn rects_overlap(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// true if a visible system menu window (`#32768`) exists.
fn system_menu_visible() -> bool {
    let mut prev: Option<HWND> = None;
    loop {
        // SAFETY: static class literal; `prev` is None or a window returned by the same enumeration.
        let Ok(h) = (unsafe { FindWindowExW(None, prev, w!("#32768"), PCWSTR::null()) }) else { return false };
        if h.0.is_null() {
            return false;
        }
        // SAFETY: no preconditions.
        if unsafe { IsWindowVisible(h) }.as_bool() {
            return true;
        }
        prev = Some(h);
    }
}

struct PopupScan {
    pid: u32,
    bar: RECT,
    found: bool,
}

unsafe extern "system" fn scan_popup_proc(h: HWND, lp: LPARAM) -> windows::core::BOOL {
    // SAFETY: `lp` is the pointer to `PopupScan` passed by `owner_popup_over_bar`, alive for the whole
    // synchronous enumeration.
    let scan = unsafe { &mut *(lp.0 as *mut PopupScan) };
    // SAFETY: no preconditions.
    if window_pid(h) == scan.pid && unsafe { IsWindowVisible(h) }.as_bool() {
        // SAFETY: no preconditions.
        let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
        let mut rc = RECT::default();
        // SAFETY: `rc` is writable; fails for an invalid handle.
        if style & WS_CAPTION.0 != WS_CAPTION.0
            && unsafe { GetWindowRect(h, &mut rc) }.is_ok()
            && rects_overlap(&rc, &scan.bar)
        {
            scan.found = true;
            return false.into();
        }
    }
    true.into()
}

/// true if process `pid` has a visible window without a title bar (menu/flyout) on the bar.
fn owner_popup_over_bar(pid: u32, bar: RECT) -> bool {
    let mut scan = PopupScan { pid, bar, found: false };
    // SAFETY: `scan` outlives the call (synchronous enumeration); the callback uses it only there.
    let _ = unsafe { EnumWindows(Some(scan_popup_proc), LPARAM(&mut scan as *mut PopupScan as isize)) };
    scan.found
}

/// true while a context menu (or flyout) of a tray app is open or about to open: the UI must
/// suspend the `HWND_TOPMOST` re-assertion and auto-hide, otherwise the dock covers the menu.
/// True (a) for ~1.2 s after a forwarded click, (b) as long as a visible system menu `#32768`
/// exists, (c) up to 15 s after the click while the app has a title-less window on the bar.
/// Cheap (a few Win32 calls): meant for the UI thread's 500 ms poll.
pub fn foreign_menu_active() -> bool {
    let last = LAST_CLICK_MS.load(Relaxed);
    let since = if last == 0 { u64::MAX } else { now_ms().saturating_sub(last) };
    if since < MENU_GRACE_MS {
        return true;
    }
    if system_menu_visible() {
        return true;
    }
    if since < MENU_WINDOW_MS {
        let bar = state().geom.bar;
        return owner_popup_over_bar(LAST_CLICK_PID.load(Relaxed), bar);
    }
    false
}

/// Mouse over icon `id` (`WM_MOUSEMOVE`; in v4 without NIF_SHOWTIP also `NIN_POPUPOPEN`).
/// The UI calls it at most once per entry onto the icon. Does not block.
pub fn hover(id: u64, anchor: POINT) {
    let Some(t) = target_of(id) else { return };
    if t.callback == 0 || !owner_alive(t.owner, t.pid) {
        return;
    }
    send_callback(&t, WM_MOUSEMOVE, anchor);
}

/// Dock geometry (shown bar, tray area, edge). Called by the UI on every redraw that changes
/// the geometry. The host window is resized to `tray_area` by the host thread.
pub fn set_geometry(g: TrayGeometry) {
    state().geom = g;
    post_host(WM_HOST_RESIZE);
}

/// SCREEN rectangles of the icons visible in the bar (for `Shell_NotifyIconGetRect`, dwData 3).
/// Overflow icons use the chevron's rect (the UI passes it for their ids).
pub fn set_icon_rects(rects: &[(u64, RECT)]) {
    let mut st = state();
    st.rects.clear();
    st.rects.extend_from_slice(rects);
}

/// Immediately brings the host back to the top (after `taskbar::hide_all` for an Explorer event/restart), if not
/// yielded. Does not block (private message to the host thread).
pub fn raise() {
    post_host(WM_HOST_RAISE);
}

/// Yields to Explorer: permanent `YIELDED` flag, `SetWindowPos(HWND_BOTTOM, SWP_ASYNCWINDOWPOS)`,
/// no more forwarding. Idempotent, callable from ANY thread (panic hook, WM_ENDSESSION,
/// `taskbar::restore_force`, `restored_externally`). No-op if the host does not exist in this process.
pub fn yield_now() {
    let raw = HOST_HWND.load(Relaxed);
    if raw == 0 {
        return;
    }
    YIELDED.store(true, Relaxed);
    // SAFETY: asynchronous variant: does not block the caller even if the host thread were hung; if the
    // window has already been destroyed the call merely fails.
    let r = unsafe {
        SetWindowPos(
            hwnd_of(raw),
            Some(HWND_BOTTOM),
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS,
        )
    };
    if let Err(e) = r {
        log!("tray: yield_now, SetWindowPos(BOTTOM) failed: {e}");
    }
}

/// For processes that do NOT own the host (watchdog, `--restore`): finds every window with
/// `PROP_DOCK_TRAY_HOST` and sends it `DockTrayYield` (`SendMessageTimeoutW`, `SMTO_ABORTIFHUNG`, 500 ms);
/// if it does not respond, `SetWindowPos(HWND_BOTTOM, SWP_ASYNCWINDOWPOS)` as best effort. Returns true if
/// at least one host was left unresponsive (the watchdog decides the policy).
pub fn yield_foreign_hosts() -> bool {
    let msg = yield_msg_id();
    // SAFETY: no preconditions.
    let me = unsafe { GetCurrentProcessId() };
    let mut unanswered = false;
    let mut prev: Option<HWND> = None;
    loop {
        // SAFETY: static class literal; `prev` is None or a window returned by the same
        // enumeration (if it disappears the search ends with an error).
        let Ok(h) = (unsafe { FindWindowExW(None, prev, w!("Shell_TrayWnd"), PCWSTR::null()) }) else { break };
        if h.0.is_null() {
            break;
        }
        prev = Some(h);
        // SAFETY: static property name; invalid handle = property absent.
        if unsafe { GetPropW(h, PROP_DOCK_TRAY_HOST) }.is_invalid() || window_pid(h) == me {
            continue;
        }
        let mut res = 0usize;
        // SAFETY: registered message without pointers; `res` valid; SMTO_ABORTIFHUNG avoids waiting on hung hosts.
        let r = unsafe { SendMessageTimeoutW(h, msg, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 500, Some(&mut res)) };
        if r.0 == 0 {
            log!("tray: host {:?} did not respond to DockTrayYield, sending it to the bottom", h.0);
            unanswered = true;
            // SAFETY: asynchronous variant, does not block.
            let _ = unsafe {
                SetWindowPos(
                    h,
                    Some(HWND_BOTTOM),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS,
                )
            };
        } else {
            log!("tray: host {:?} yielded", h.0);
        }
    }
    unanswered
}

// ------------------------------------------------------------------ host thread

fn host_main(ready: mpsc::Sender<Result<(), String>>) {
    match create_host() {
        Ok(()) => {
            let _ = ready.send(Ok(()));
            let mut msg = MSG::default();
            loop {
                // SAFETY: `msg` valid and writable; message loop of the thread that created the window.
                let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if r.0 <= 0 {
                    break;
                }
                // SAFETY: `msg` just filled by GetMessageW.
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            HOST_HWND.store(0, Relaxed);
            RUNNING.store(false, Relaxed);
            // SAFETY: classes registered by `create_host` with the same instance.
            unsafe {
                let inst = GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).ok();
                let _ = UnregisterClassW(w!("TrayNotifyWnd"), inst);
                let _ = UnregisterClassW(w!("Shell_TrayWnd"), inst);
            }
            let mut st = state();
            st.items.clear();
            st.rects.clear();
            log!("tray: host terminated");
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }
}

/// Registers the classes and creates `Shell_TrayWnd` + `TrayNotifyWnd`, marker, UIPI filters, timers.
fn create_host() -> Result<(), String> {
    // SAFETY: no preconditions.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    let inst = HINSTANCE(module.0);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(host_wnd_proc),
        hInstance: inst,
        lpszClassName: w!("Shell_TrayWnd"),
        ..Default::default()
    };
    // SAFETY: `wc` fully initialized; static class name literal.
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(format!("RegisterClassExW(Shell_TrayWnd): {:?}", windows::core::Error::from_thread()));
    }
    let wc_child = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(child_wnd_proc),
        hInstance: inst,
        lpszClassName: w!("TrayNotifyWnd"),
        ..Default::default()
    };
    // SAFETY: as above.
    if unsafe { RegisterClassExW(&wc_child) } == 0 {
        log!("tray: RegisterClassExW(TrayNotifyWnd) failed: {:?}", windows::core::Error::from_thread());
    }
    let g = state().geom;
    let (x, y) = (g.tray_area.left, g.tray_area.top);
    let (cw, ch) = ((g.tray_area.right - g.tray_area.left).max(1), (g.tray_area.bottom - g.tray_area.top).max(1));
    // SAFETY: class registered above; static PCWSTR literals; the window is never shown.
    let host = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("Shell_TrayWnd"),
            w!(""),
            WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            x,
            y,
            cw,
            ch,
            None,
            None,
            Some(inst),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW(Shell_TrayWnd): {e}"))?;

    // Marker: lets the watchdog/`--restore` avoid mistaking it for Explorer's taskbar.
    // SAFETY: static name; the value is only a marker, never dereferenced.
    if let Err(e) = unsafe { SetPropW(host, PROP_DOCK_TRAY_HOST, Some(HANDLE(std::ptr::dangling_mut::<c_void>()))) } {
        // SAFETY: window just created by this thread.
        let _ = unsafe { DestroyWindow(host) };
        return Err(format!("SetPropW(DockTrayHost): {e}"));
    }
    // SAFETY: `host` is the window just created by this thread; no optional struct.
    if let Err(e) = unsafe { ChangeWindowMessageFilterEx(host, WM_COPYDATA, MSGFLT_ALLOW, None) } {
        log!(
            "tray: WARNING ChangeWindowMessageFilterEx(WM_COPYDATA) failed: {e} (UIPI will block the icons of medium-integrity apps!)"
        );
    }
    // SAFETY: as above.
    if let Err(e) = unsafe { ChangeWindowMessageFilterEx(host, WM_COMMAND, MSGFLT_ALLOW, None) } {
        log!("tray: ChangeWindowMessageFilterEx(WM_COMMAND) failed: {e}");
    }
    let ym = yield_msg_id();
    if ym != 0 {
        // SAFETY: as above.
        if let Err(e) = unsafe { ChangeWindowMessageFilterEx(host, ym, MSGFLT_ALLOW, None) } {
            log!("tray: ChangeWindowMessageFilterEx(DockTrayYield) failed: {e}");
        }
    }
    // Child TrayNotifyWnd: some apps look for it to compute popup positions.
    // SAFETY: class registered above; `host` valid; child window never visible.
    if let Err(e) = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("TrayNotifyWnd"),
            w!(""),
            WS_CHILD | WS_CLIPSIBLINGS,
            0,
            0,
            cw,
            ch,
            Some(host),
            None,
            Some(inst),
            None,
        )
    } {
        log!("tray: CreateWindowExW(TrayNotifyWnd) failed: {e}");
    }
    copy_taskband(host);
    // SAFETY: `host` valid; timers handled by `host_wnd_proc` on the same thread.
    unsafe {
        if SetTimer(Some(host), TIMER_ZORDER, 1000, None) == 0 {
            log!("tray: SetTimer(1 s) failed: {:?}", windows::core::Error::from_thread());
        }
        if SetTimer(Some(host), TIMER_DEAD, 5000, None) == 0 {
            log!("tray: SetTimer(5 s) failed: {:?}", windows::core::Error::from_thread());
        }
    }
    HOST_HWND.store(host.0 as isize, Relaxed);
    RUNNING.store(true, Relaxed);
    // SAFETY: valid window; no SWP_SHOWWINDOW: it stays invisible.
    if let Err(e) =
        unsafe { SetWindowPos(host, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) }
    {
        log!("tray: initial SetWindowPos(TOPMOST) failed: {e}");
    }
    Ok(())
}

/// First `Shell_TrayWnd` that does not belong to a dock host (i.e. Explorer's).
fn find_explorer_tray() -> Option<HWND> {
    let mut prev: Option<HWND> = None;
    loop {
        // SAFETY: static class literal; `prev` is None or a window returned by the same enumeration.
        let h = unsafe { FindWindowExW(None, prev, w!("Shell_TrayWnd"), PCWSTR::null()) }.ok()?;
        if h.0.is_null() {
            return None;
        }
        if !crate::taskbar::is_dock_tray_host(h) {
            return Some(h);
        }
        prev = Some(h);
    }
}

/// Copies `TaskbandHWND` from Explorer's `Shell_TrayWnd` (for `ITaskbarList3`); acceptable degradation if missing.
fn copy_taskband(host: HWND) {
    let Some(ex) = find_explorer_tray() else { return };
    // SAFETY: static property name; invalid handle = property absent.
    let v = unsafe { GetPropW(ex, w!("TaskbandHWND")) };
    if v.is_invalid() {
        log!("tray: TaskbandHWND missing on Explorer (no progress on the dock)");
        return;
    }
    // SAFETY: `host` is our window; the value is an opaque handle copied from Explorer.
    if let Err(e) = unsafe { SetPropW(host, w!("TaskbandHWND"), Some(v)) } {
        log!("tray: SetPropW(TaskbandHWND) failed: {e}");
    }
}

unsafe extern "system" fn child_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwards the parameters received from the system to the default procedure.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

unsafe extern "system" fn host_wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COPYDATA => on_copydata(wp, lp),
        WM_TIMER => {
            match wp.0 {
                TIMER_ZORDER => keep_first(hwnd),
                TIMER_DEAD => sweep_dead(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGED => {
            // SAFETY: for WM_WINDOWPOSCHANGED lParam points to a WINDOWPOS valid for the duration of the message.
            let flags = unsafe { (*(lp.0 as *const WINDOWPOS)).flags };
            if flags.0 & SWP_SHOWWINDOW.0 != 0 {
                // The host must never be visible.
                // SAFETY: own window, same thread.
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            }
            // SAFETY: forwards to the default procedure.
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_COMMAND => {
            forward_post(msg, wp, lp);
            LRESULT(0)
        }
        m if (WM_USER..WM_APP).contains(&m) => {
            forward_post(msg, wp, lp);
            LRESULT(0)
        }
        WM_HOST_RESIZE => {
            resize_host(hwnd);
            LRESULT(0)
        }
        WM_HOST_RAISE => {
            if !YIELDED.load(Relaxed) {
                copy_taskband(hwnd);
                set_topmost(hwnd);
            }
            LRESULT(0)
        }
        WM_HOST_RESCALE => {
            rescale_all();
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: own window, same thread.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: timers created by this thread on this window.
            unsafe {
                let _ = KillTimer(Some(hwnd), TIMER_ZORDER);
                let _ = KillTimer(Some(hwnd), TIMER_DEAD);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        m if m != 0 && m == YIELD_MSG.load(Relaxed) => {
            yield_now();
            // SAFETY: own window, same thread.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(1)
        }
        // SAFETY: forwards to the default procedure.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn set_topmost(hwnd: HWND) {
    // SAFETY: the window is invisible and ours: making it topmost covers nothing and does not show it.
    if let Err(e) =
        unsafe { SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE) }
    {
        log!("tray: SetWindowPos(TOPMOST) failed: {e}");
    }
}

/// 1 s timer: if we are not the first `Shell_TrayWnd` (and have not yielded) climb back up.
fn keep_first(hwnd: HWND) {
    if YIELDED.load(Relaxed) {
        return;
    }
    // SAFETY: static class literal.
    let first = unsafe { FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()) }.unwrap_or_default();
    if first != hwnd {
        set_topmost(hwnd);
    }
    // SAFETY: no preconditions.
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        // SAFETY: own window, same thread.
        let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
    }
}

/// 5 s timer: removes icons whose owner is dead.
fn sweep_dead() {
    let cands: Vec<(u64, isize, u32, String)> =
        state().items.iter().map(|t| (t.id, t.owner, t.pid, t.exe.clone())).collect();
    for (id, owner, pid, exe) in cands {
        if !owner_alive(owner, pid) {
            remove_dead(id, &exe);
        }
    }
}

fn resize_host(hwnd: HWND) {
    let g = area_rect(&state().geom);
    let (w, h) = ((g.right - g.left).max(1), (g.bottom - g.top).max(1));
    // SAFETY: own window, same thread; without SWP_SHOWWINDOW it stays hidden.
    if let Err(e) = unsafe { SetWindowPos(hwnd, None, g.left, g.top, w, h, SWP_NOZORDER | SWP_NOACTIVATE) } {
        log!("tray: SetWindowPos(resize) failed: {e}");
    }
}

/// Rescales all bitmaps from the sources (host thread, outside the lock).
fn rescale_all() {
    let px = ICON_PX.load(Relaxed);
    let srcs: Vec<(u64, Arc<IconBitmap>)> =
        state().items.iter().filter_map(|t| t.src.clone().map(|s| (t.id, s))).collect();
    let scaled: Vec<(u64, Arc<IconBitmap>, Arc<IconBitmap>)> = srcs
        .into_iter()
        .map(|(id, s)| {
            let b = Arc::new(icons::resize_area(&s, px));
            (id, s, b)
        })
        .collect();
    {
        let mut st = state();
        for (id, s, b) in scaled {
            if b.is_empty() {
                continue;
            }
            if let Some(t) = st.items.iter_mut().find(|t| t.id == id) {
                // Only if the source has not changed in the meantime.
                if t.src.as_ref().is_some_and(|cur| Arc::ptr_eq(cur, &s)) {
                    t.bitmap = Some(b);
                }
            }
        }
    }
    post_ui(1);
}

fn forward_post(msg: u32, wp: WPARAM, lp: LPARAM) {
    if let Some(ex) = find_explorer_tray() {
        // SAFETY: PostMessageW does not block; if Explorer disappears it merely fails.
        let _ = unsafe { PostMessageW(Some(ex), msg, wp, lp) };
    }
}

// ------------------------------------------------------------------ WM_COPYDATA

/// Bytes of the `WM_COPYDATA` (at most 64 KiB; empty if absent).
fn cds_bytes(cds: &COPYDATASTRUCT) -> &[u8] {
    if cds.lpData.is_null() || cds.cbData == 0 {
        return &[];
    }
    let n = (cds.cbData as usize).min(1 << 16);
    // SAFETY: by the WM_COPYDATA contract `lpData` points to `cbData` bytes valid for the whole duration
    // of the message; `n <= cbData`.
    unsafe { std::slice::from_raw_parts(cds.lpData as *const u8, n) }
}

fn on_copydata(wp: WPARAM, lp: LPARAM) -> LRESULT {
    if lp.0 == 0 {
        return LRESULT(0);
    }
    // SAFETY: WM_COPYDATA's lParam is a pointer to a COPYDATASTRUCT valid for the whole duration of the message.
    let cds = unsafe { &*(lp.0 as *const COPYDATASTRUCT) };
    let bytes = cds_bytes(cds);
    match cds.dwData {
        1 => handle_nim(wp, lp, bytes),
        0 => handle_appbar(wp, lp, bytes),
        3 => handle_icon_rect(bytes),
        _ => forward_sync(wp, lp),
    }
}

/// Synchronous forwarding (3 s timeout) of the original `WM_COPYDATA` to Explorer; 0 if absent.
fn forward_sync(wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(ex) = find_explorer_tray() else { return LRESULT(0) };
    let mut res = 0usize;
    // SAFETY: `lp` is the original COPYDATASTRUCT, valid for the whole duration of our handler; the
    // call marshals the data to Explorer; `res` valid.
    let r = unsafe {
        SendMessageTimeoutW(ex, WM_COPYDATA, wp, lp, SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT, 3000, Some(&mut res))
    };
    if r.0 == 0 {
        return LRESULT(0);
    }
    LRESULT(res as isize)
}

/// Full path of the exe of process `pid` (empty if it cannot be opened).
fn exe_path_of_pid(pid: u32) -> String {
    // SAFETY: no preconditions; if the process cannot be opened it returns Err.
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    let mut buf = [0u16; 520];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` is writable with `len` elements; `h` opened above.
    let r = unsafe { QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    // SAFETY: handle opened above, closed exactly once.
    let _ = unsafe { CloseHandle(h) };
    if r.is_err() {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())])
}

/// File name of the exe from a full path.
fn file_name_of(path: &str) -> String {
    path.rsplit('\\').next().unwrap_or("").to_string()
}

/// Version value `sub` inside block `buf`: pointer (into `buf`) and length.
fn ver_query(buf: &[u64], sub: &str) -> Option<(*const u16, usize)> {
    let wsub: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
    let mut p: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `buf` holds a valid version block (GetFileVersionInfoW succeeded); `p` and `len`
    // are writable; `wsub` is NUL-terminated.
    let ok = unsafe { VerQueryValueW(buf.as_ptr().cast::<c_void>(), PCWSTR(wsub.as_ptr()), &mut p, &mut len) };
    (ok.as_bool() && !p.is_null() && len > 0).then_some((p.cast::<u16>().cast_const(), len as usize))
}

/// `file_description` with a per-exe-path cache (many apps register together at login).
fn cached_file_description(path: &str) -> String {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(c) = cache.lock() {
        if let Some(v) = c.get(path) {
            return v.clone();
        }
    }
    let name = file_description(path);
    if let Ok(mut c) = cache.lock() {
        c.insert(path.to_string(), name.clone());
    }
    name
}

/// `FileDescription` from the exe's version resources (empty if absent or unreadable).
fn file_description(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let wpath: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wpath` is NUL-terminated and lives for the whole call.
    let size = unsafe { GetFileVersionInfoSizeW(PCWSTR(wpath.as_ptr()), None) };
    if size == 0 || size > (1 << 20) {
        return String::new();
    }
    // u64 buffer for alignment; `size` bytes are needed.
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `buf` has at least `size` writable bytes; `wpath` NUL-terminated.
    let r = unsafe { GetFileVersionInfoW(PCWSTR(wpath.as_ptr()), None, size, buf.as_mut_ptr().cast::<c_void>()) };
    if r.is_err() {
        return String::new();
    }
    let mut langs: Vec<String> = Vec::new();
    if let Some((p, len)) = ver_query(&buf, "\\VarFileInfo\\Translation") {
        if len >= 4 {
            // SAFETY: `p` points inside `buf` to at least `len >= 4` bytes (16-bit language and codepage).
            let (lang, cp) = unsafe { (std::ptr::read_unaligned(p), std::ptr::read_unaligned(p.add(1))) };
            langs.push(format!("{lang:04x}{cp:04x}"));
        }
    }
    langs.extend(["040904b0".to_string(), "040904e4".to_string(), "000004b0".to_string()]);
    for l in langs {
        if let Some((p, len)) = ver_query(&buf, &format!("\\StringFileInfo\\{l}\\FileDescription")) {
            // SAFETY: for a version string `len` is the number of UTF-16 characters inside `buf`.
            let sl = unsafe { std::slice::from_raw_parts(p, len) };
            let d = utf16_until_nul(sl).trim().to_string();
            if !d.is_empty() {
                return d;
            }
        }
    }
    String::new()
}

/// RID of the "medium" integrity level (`SECURITY_MANDATORY_MEDIUM_RID`).
const MEDIUM_RID: u32 = 0x2000;

/// true if process `pid` runs at integrity NOT higher than medium (or is this process).
/// Used so the elevated dock does not cross the UIPI boundary on behalf of a medium sender:
/// when in doubt (process cannot be opened, token unreadable) returns false.
fn pid_at_most_medium(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: no preconditions.
    if pid == unsafe { GetCurrentProcessId() } {
        return true;
    }
    // SAFETY: no preconditions; if the process cannot be opened it returns Err.
    let Ok(proc_h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut token = HANDLE::default();
    // SAFETY: `proc_h` opened above; `token` writable, closed below.
    let opened = unsafe { OpenProcessToken(proc_h, TOKEN_QUERY, &mut token) }.is_ok();
    // SAFETY: handle opened above, closed exactly once.
    let _ = unsafe { CloseHandle(proc_h) };
    if !opened {
        return false;
    }
    // TOKEN_MANDATORY_LABEL + SID: 64 bytes are enough (integrity SID = 1 sub-authority); u64
    // buffer for pointer alignment.
    let mut buf = [0u64; 16];
    let mut ret = 0u32;
    // SAFETY: `buf` writable for the declared 128 bytes; `token` valid.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            std::mem::size_of_val(&buf) as u32,
            &mut ret,
        )
    }
    .is_ok();
    // SAFETY: handle opened above and no longer used.
    let _ = unsafe { CloseHandle(token) };
    if !ok {
        return false;
    }
    // SAFETY: GetTokenInformation succeeded: `buf` starts with a TOKEN_MANDATORY_LABEL whose `Sid`
    // points inside `buf` itself, still alive; the SID is valid, so count and sub-authorities are readable.
    let rid = unsafe {
        let label = &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>();
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return false;
        }
        *GetSidSubAuthority(sid, u32::from(count) - 1)
    };
    rid <= MEDIUM_RID
}

/// Converts the sender's HICON: copy, bitmap, destruction of the copy (all outside the lock).
fn make_icon_update(hicon: u32) -> IconUpdate {
    if hicon == 0 {
        return IconUpdate::Clear;
    }
    let raw = HICON(ext(hicon) as *mut c_void);
    // SAFETY: the HICON is a session-global USER handle; the sender is blocked in SendMessage and
    // therefore keeps it alive; CopyIcon fails with Err if it is not valid.
    let copy = match unsafe { CopyIcon(raw) } {
        Ok(c) => c,
        Err(e) => {
            log!("tray: CopyIcon failed: {e}");
            return IconUpdate::Keep;
        }
    };
    let bmp = icons::hicon_to_bitmap(copy);
    // SAFETY: `copy` is our own copy, destroyed exactly once.
    let _ = unsafe { DestroyIcon(copy) };
    let Some(bmp) = bmp else {
        log!("tray: HICON conversion failed");
        return IconUpdate::Keep;
    };
    let px = ICON_PX.load(Relaxed);
    // Already display size with nothing to crop: share one allocation between `src` and `scaled`
    // (same result as `resize_area`, so `rescale_all` agrees).
    if icons::resize_is_identity(&bmp, px) {
        let one = Arc::new(bmp);
        return IconUpdate::Set(Arc::clone(&one), one);
    }
    let scaled = icons::resize_area(&bmp, px);
    if scaled.is_empty() {
        log!("tray: icon empty after resizing");
        return IconUpdate::Keep;
    }
    IconUpdate::Set(Arc::new(bmp), Arc::new(scaled))
}

fn handle_nim(wp: WPARAM, lp: LPARAM, bytes: &[u8]) -> LRESULT {
    // Host that has yielded but is still the first `Shell_TrayWnd` (asynchronous SetWindowPos not
    // applied yet, or stop() timed out): the NIM goes to Explorer, otherwise the icon would be lost.
    if YIELDED.load(Relaxed) {
        return forward_sync(wp, lp);
    }
    let Some(d) = parse_shell_tray(bytes) else {
        // Not one of our NIMs (signature/length): Explorer decides.
        return forward_sync(wp, lp);
    };
    let nid = &d.nid;
    let owner = ext(nid.hwnd);
    let pid = window_pid(hwnd_of(owner));
    let msg = d.message;
    let icon = if (msg == NIM_ADD || msg == NIM_MODIFY) && nid.flags & NIF_ICON != 0 {
        make_icon_update(nid.hicon)
    } else {
        IconUpdate::Keep
    };
    let exe_path = if msg == NIM_ADD && pid != 0 { exe_path_of_pid(pid) } else { String::new() };
    let exe = file_name_of(&exe_path);
    let res = {
        let mut st = state();
        let TrayState { items, next_id, .. } = &mut *st;
        apply_nim(items, next_id, &d, (pid != 0).then_some(pid), icon, &exe)
    };
    if msg == NIM_ADD && res.ret && !exe_path.is_empty() {
        // Short name for the chooser: reading version resources is disk I/O, so it stays outside
        // the host thread (the app is still blocked in its Shell_NotifyIcon). Reply immediately.
        let guid = (nid.flags & NIF_GUID != 0 && !guid_is_zero(&nid.guid)).then_some(nid.guid);
        let (uid, path) = (nid.uid, exe_path.clone());
        if let Err(e) = std::thread::Builder::new().name("tray-name".into()).stack_size(128 * 1024).spawn(move || {
            let name = cached_file_description(&path);
            if name.is_empty() {
                return;
            }
            let changed = {
                let mut st = state();
                match find_item(&st.items, owner, uid, guid) {
                    Some(i) if st.items[i].name != name => {
                        st.items[i].name = name;
                        true
                    }
                    _ => false,
                }
            };
            if changed {
                post_ui(1);
            }
        }) {
            log!("tray: app-name thread not started: {e}");
        }
    }
    if msg == NIM_ADD {
        let guid = if nid.flags & NIF_GUID != 0 { guid_string(&nid.guid) } else { "-".into() };
        log!(
            "tray: NIM_ADD exe={} guid={guid} tip=\"{}\" hwnd=0x{:X} uid={} ret={}",
            if exe.is_empty() { "?" } else { &exe },
            utf16_until_nul(&nid.tip),
            owner,
            nid.uid,
            res.ret
        );
    }
    enqueue_forward(wp.0, bytes);
    if res.change != 0 {
        post_ui(res.change);
    }
    LRESULT(res.ret as isize)
}

fn handle_icon_rect(bytes: &[u8]) -> LRESULT {
    if bytes.len() < 24 || rd_u32(bytes, 0) != TRAY_SIGNATURE {
        return LRESULT(0);
    }
    let which = rd_u32(bytes, 4);
    let owner = ext(rd_u32(bytes, 16));
    let uid = rd_u32(bytes, 20);
    let guid = if bytes.len() >= 40 {
        let mut d4 = [0u8; 8];
        d4.copy_from_slice(&bytes[32..40]);
        let g = GUID::from_values(rd_u32(bytes, 24), rd_u32(bytes, 28) as u16, (rd_u32(bytes, 28) >> 16) as u16, d4);
        (!guid_is_zero(&g)).then_some(g)
    } else {
        None
    };
    let st = state();
    let Some(i) = find_item(&st.items, owner, uid, guid) else { return LRESULT(0) };
    let id = st.items[i].id;
    let r = st.rects.iter().find(|(rid, _)| *rid == id).map(|(_, r)| *r).unwrap_or_else(|| area_rect(&st.geom));
    let (x, y) = if which == 2 { (r.right, r.bottom) } else { (r.left, r.top) };
    LRESULT((((y as u16 as u32) << 16) | (x as u16 as u32)) as i32 as isize)
}

fn handle_appbar(wp: WPARAM, lp: LPARAM, bytes: &[u8]) -> LRESULT {
    if bytes.len() < std::mem::size_of::<AppBarMsg>() {
        return forward_sync(wp, lp);
    }
    let m: AppBarMsg = from_prefix(bytes);
    // These three use `SHLockShared(m.shared, m.pid)`, i.e. they duplicate a handle from process `m.pid`
    // and write into it: from an elevated process this is accepted only if `m.pid` owns the
    // AppBar window (when given) and does not run above medium, otherwise a medium sender could
    // make us write into a section of an elevated process.
    if matches!(m.message, ABM_GETTASKBARPOS | ABM_QUERYPOS | ABM_SETPOS) {
        let wnd_pid = if wp.0 == 0 { None } else { Some(window_pid(HWND(wp.0 as *mut c_void))) };
        // SAFETY: no preconditions.
        let own = unsafe { GetCurrentProcessId() };
        if !appbar_block_allowed(m.pid, own, wnd_pid, pid_at_most_medium) {
            log!(
                "tray: AppBar {} rejected (block pid {}, window pid {} not verified)",
                m.message,
                m.pid,
                wnd_pid.map_or_else(|| "-".to_string(), |p| p.to_string())
            );
            return LRESULT(0);
        }
    }
    match m.message {
        ABM_GETTASKBARPOS => get_taskbar_pos(&m),
        ABM_QUERYPOS | ABM_SETPOS => forward_with_shared(wp, m),
        _ => forward_sync(wp, lp),
    }
}

/// Whether an AppBar shared block may be locked/written. `block_pid` is the process holding the
/// handle (`m.pid`), `own` this process, `wnd_pid` the owner of the AppBar window (`None` = no window).
/// shell32 allocates the block directly for the tray window's process, so for a real AppBar
/// `block_pid == own` and the window belongs to the app: in that case the app (window owner) is what
/// must not run above medium. A block living in another process keeps the original rule (it must be
/// the window owner's and at most medium). Pure: `medium` is injected for tests.
fn appbar_block_allowed(block_pid: u32, own: u32, wnd_pid: Option<u32>, medium: impl Fn(u32) -> bool) -> bool {
    match wnd_pid {
        None => medium(block_pid),
        Some(w) if block_pid == own => w != 0 && w != own && medium(w),
        Some(w) => w == block_pid && medium(block_pid),
    }
}

/// `ABM_GETTASKBARPOS`: answers with the dock's tray area and edge, in our own shared block.
fn get_taskbar_pos(m: &AppBarMsg) -> LRESULT {
    if m.shared == 0 {
        return LRESULT(0);
    }
    let g = state().geom;
    let a = area_rect(&g);
    // SAFETY: the shared block was allocated by the sender for our process (`m.pid`).
    let p = unsafe { SHLockShared(HANDLE(m.shared as usize as *mut c_void), m.pid) };
    if p.is_null() {
        log!("tray: GETTASKBARPOS, SHLockShared failed");
        return LRESULT(0);
    }
    let abd = p.cast::<AppBarData32>();
    // SAFETY: the block holds at least one 32-bit APPBARDATA (0x28 bytes); it is updated in place.
    unsafe {
        (*abd).rc = [a.left, a.top, a.right, a.bottom];
        (*abd).edge = g.edge;
    }
    // SAFETY: `p` obtained from SHLockShared above, released exactly once.
    if let Err(e) = unsafe { SHUnlockShared(p) } {
        log!("tray: SHUnlockShared failed: {e}");
    }
    LRESULT(1)
}

/// `ABM_QUERYPOS`/`ABM_SETPOS`: recreates the shared block for Explorer's PID and forwards.
fn forward_with_shared(wp: WPARAM, m: AppBarMsg) -> LRESULT {
    let Some(ex) = find_explorer_tray() else { return LRESULT(0) };
    let ex_pid = window_pid(ex);
    if ex_pid == 0 || m.shared == 0 {
        return LRESULT(0);
    }
    // SAFETY: block allocated by the sender for our process (`m.pid`).
    let old = unsafe { SHLockShared(HANDLE(m.shared as usize as *mut c_void), m.pid) };
    if old.is_null() {
        log!("tray: AppBar, SHLockShared(old) failed");
        return LRESULT(0);
    }
    // SAFETY: `old` points to at least one 32-bit APPBARDATA; the copy is destined for Explorer.
    let new_h = unsafe { SHAllocShared(Some(old.cast_const()), std::mem::size_of::<AppBarData32>() as u32, ex_pid) };
    if new_h.is_invalid() {
        log!("tray: AppBar, SHAllocShared failed");
        // SAFETY: `old` obtained from SHLockShared above.
        let _ = unsafe { SHUnlockShared(old) };
        return LRESULT(0);
    }
    let mut m2 = m;
    m2.shared = new_h.0 as usize as u64;
    m2.pid = ex_pid;
    let cds = COPYDATASTRUCT {
        dwData: 0,
        cbData: std::mem::size_of::<AppBarMsg>() as u32,
        lpData: (&mut m2 as *mut AppBarMsg).cast::<c_void>(),
    };
    let mut res = 0usize;
    // SAFETY: `cds` and `m2` live for the whole call, which marshals the data to Explorer.
    let sent = unsafe {
        SendMessageTimeoutW(
            ex,
            WM_COPYDATA,
            wp,
            LPARAM(&cds as *const COPYDATASTRUCT as isize),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            3000,
            Some(&mut res),
        )
    };
    let ok = sent.0 != 0;
    if ok {
        // SAFETY: block created above for Explorer's PID.
        let back = unsafe { SHLockShared(new_h, ex_pid) };
        if !back.is_null() {
            // SAFETY: both blocks contain a 32-bit APPBARDATA (0x28 bytes), non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(back.cast::<AppBarData32>().cast_const(), old.cast::<AppBarData32>(), 1)
            };
            // SAFETY: `back` obtained from SHLockShared above.
            let _ = unsafe { SHUnlockShared(back) };
        }
        // SAFETY: handle created by SHAllocShared for `ex_pid`, freed exactly once.
        let _ = unsafe { SHFreeShared(new_h, ex_pid) };
    } else {
        log!("tray: AppBar, forwarding to Explorer failed (block left in place, it may still be needed)");
    }
    // SAFETY: `old` obtained from SHLockShared above.
    let _ = unsafe { SHUnlockShared(old) };
    if ok {
        LRESULT(res as isize)
    } else {
        LRESULT(0)
    }
}

// ------------------------------------------------------------------ tray-fwd thread

fn enqueue_forward(wp: usize, bytes: &[u8]) {
    if !crate::taskbar::is_hidden() || YIELDED.load(Relaxed) || bytes.is_empty() {
        return;
    }
    {
        let mut q = FWD_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= FWD_QUEUE_MAX {
            q.pop_front();
            if !FWD_OVERFLOW_LOGGED.swap(true, Relaxed) {
                log!("tray: Explorer forwarding queue full ({FWD_QUEUE_MAX}), dropping the oldest messages");
            }
        }
        q.push_back((wp, bytes.to_vec()));
    }
    FWD_CV.notify_one();
}

fn forward_main() {
    loop {
        let item = {
            let mut q = FWD_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if FWD_STOP.load(Relaxed) {
                    return;
                }
                if let Some(it) = q.pop_front() {
                    break it;
                }
                q = match FWD_CV.wait_timeout(q, Duration::from_millis(500)) {
                    Ok((g, _)) => g,
                    Err(e) => e.into_inner().0,
                };
            }
        };
        if !crate::taskbar::is_hidden() || YIELDED.load(Relaxed) {
            continue;
        }
        let Some(ex) = find_explorer_tray() else { continue };
        let (wp, mut data) = item;
        let cds = COPYDATASTRUCT { dwData: 1, cbData: data.len() as u32, lpData: data.as_mut_ptr().cast::<c_void>() };
        let mut res = 0usize;
        // SAFETY: `cds` and `data` live for the whole call; the system copies the bytes to Explorer.
        let r = unsafe {
            SendMessageTimeoutW(
                ex,
                WM_COPYDATA,
                WPARAM(wp),
                LPARAM(&cds as *const COPYDATASTRUCT as isize),
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                2000,
                Some(&mut res),
            )
        };
        if r.0 == 0 {
            log!("tray: forwarding to Explorer failed or timed out");
        }
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevated_owner_gating() {
        // Medium (or lower) owners: always allowed, system callbacks included.
        assert!(callback_permitted(true, WM_USER + 1, false));
        assert!(callback_permitted(true, 0x0111, false));
        // Elevated / unknown owners: blocked by default.
        assert!(!callback_permitted(false, WM_USER + 1, false));
        assert!(!callback_permitted(false, 0x0111, false));
        // Opt-in: only private callbacks (>= WM_USER), never system messages.
        assert!(callback_permitted(false, WM_USER, true));
        assert!(callback_permitted(false, 0xC123, true));
        assert!(!callback_permitted(false, 0x0111, true));
        assert!(!callback_permitted(false, WM_USER - 1, true));
    }

    fn nid(hwnd: u32, uid: u32, flags: u32) -> ShellTrayData {
        let mut d: ShellTrayData = from_prefix(&[]);
        d.signature = TRAY_SIGNATURE;
        d.nid.hwnd = hwnd;
        d.nid.uid = uid;
        d.nid.flags = flags;
        d
    }

    fn bmp() -> IconUpdate {
        IconUpdate::Set(Arc::new(IconBitmap::new(16, 16)), Arc::new(IconBitmap::new(16, 16)))
    }

    #[test]
    fn layout_e_firma() {
        assert_eq!(std::mem::size_of::<ShellTrayData>(), 964);
        assert!(parse_shell_tray(&[0u8; 100]).is_none());
        let mut raw = vec![0u8; 100];
        raw[..4].copy_from_slice(&TRAY_SIGNATURE.to_le_bytes());
        assert!(parse_shell_tray(&raw).is_some());
        assert!(parse_shell_tray(&raw[..20]).is_none());
    }

    #[test]
    fn encode_per_version() {
        assert_eq!(encode(0, 7, 100, 200, 0x202), (7, 0x202));
        assert_eq!(encode(3, 7, 100, 200, NIN_SELECT), (7, NIN_SELECT as isize));
        let (wp, lp) = encode(4, 7, 100, 200, 0x202);
        assert_eq!(wp & 0xFFFF, 100);
        assert_eq!(wp >> 16, 200);
        assert_eq!(lp & 0xFFFF, 0x202);
        assert_eq!(lp >> 16, 7);
        // negative coordinates: signed 16 bit
        let (wp, _) = encode(4, 1, -5, -9, 0x200);
        assert_eq!((wp & 0xFFFF) as u16 as i16, -5);
        assert_eq!(((wp >> 16) & 0xFFFF) as u16 as i16, -9);
    }

    #[test]
    fn sequenze_clic() {
        assert_eq!(click_events(TrayClick::Left, 0), vec![WM_LBUTTONDOWN, WM_LBUTTONUP]);
        assert_eq!(click_events(TrayClick::Left, 4), vec![WM_LBUTTONDOWN, WM_LBUTTONUP, NIN_SELECT]);
        assert_eq!(click_events(TrayClick::Right, 3), vec![WM_RBUTTONDOWN, WM_RBUTTONUP, WM_CONTEXTMENU]);
        assert_eq!(click_events(TrayClick::Middle, 4), vec![WM_MBUTTONDOWN, WM_MBUTTONUP]);
        assert_eq!(click_events(TrayClick::Double, 0), vec![WM_LBUTTONDBLCLK, WM_LBUTTONUP]);
        assert_eq!(click_events(TrayClick::Double, 3), vec![WM_LBUTTONDBLCLK, WM_LBUTTONUP, NIN_SELECT]);
    }

    #[test]
    fn ciclo_nim() {
        let mut items = Vec::new();
        let mut next = 1u64;
        let mut d = nid(0x1234, 1, NIF_ICON | NIF_MESSAGE);
        d.nid.callback = 0x8001;
        d.message = NIM_ADD;
        let r = apply_nim(&mut items, &mut next, &d, Some(10), bmp(), "a.exe");
        assert!(r.ret);
        assert_eq!(r.change, 2);
        assert_eq!(items.len(), 1);
        // double ADD = 0
        let r = apply_nim(&mut items, &mut next, &d, Some(10), bmp(), "a.exe");
        assert!(!r.ret);
        // MODIFY of unknown icon = 0
        let mut m = nid(0x9999, 1, NIF_TIP);
        m.message = NIM_MODIFY;
        assert!(!apply_nim(&mut items, &mut next, &m, Some(10), IconUpdate::Keep, "").ret);
        // MODIFY of known icon: tooltip
        let mut m = nid(0x1234, 1, NIF_TIP);
        m.message = NIM_MODIFY;
        m.nid.tip[0] = b'A' as u16;
        let r = apply_nim(&mut items, &mut next, &m, Some(10), IconUpdate::Keep, "");
        assert!(r.ret);
        assert_eq!(r.change, 1);
        assert_eq!(items[0].tip, "A");
        // hide
        let mut m = nid(0x1234, 1, NIF_STATE);
        m.message = NIM_MODIFY;
        m.nid.state = NIS_HIDDEN;
        m.nid.state_mask = NIS_HIDDEN;
        assert_eq!(apply_nim(&mut items, &mut next, &m, Some(10), IconUpdate::Keep, "").change, 2);
        // SETVERSION
        let mut v = nid(0x1234, 1, 0);
        v.message = NIM_SETVERSION;
        v.nid.version = 4;
        assert!(apply_nim(&mut items, &mut next, &v, Some(10), IconUpdate::Keep, "").ret);
        assert_eq!(items[0].version, 4);
        v.nid.version = 5;
        assert!(!apply_nim(&mut items, &mut next, &v, Some(10), IconUpdate::Keep, "").ret);
        // SETFOCUS always 1
        let mut f = nid(0x7777, 9, 0);
        f.message = NIM_SETFOCUS;
        assert!(apply_nim(&mut items, &mut next, &f, None, IconUpdate::Keep, "").ret);
        // DELETE
        let mut x = nid(0x1234, 1, 0);
        x.message = NIM_DELETE;
        assert!(apply_nim(&mut items, &mut next, &x, Some(10), IconUpdate::Keep, "").ret);
        assert!(items.is_empty());
        assert!(!apply_nim(&mut items, &mut next, &x, Some(10), IconUpdate::Keep, "").ret);
    }

    #[test]
    fn add_con_owner_morto_e_guid() {
        let mut items = Vec::new();
        let mut next = 1u64;
        let mut d = nid(0x10, 1, NIF_ICON | NIF_GUID);
        d.nid.guid = GUID::from_u128(0x7820ae73_23e3_4229_82c1_e41cb67d5b9c);
        d.message = NIM_ADD;
        assert!(!apply_nim(&mut items, &mut next, &d, None, bmp(), "").ret);
        assert!(apply_nim(&mut items, &mut next, &d, Some(5), bmp(), "").ret);
        // same GUID, different hWnd: replaces the owner
        d.nid.hwnd = 0x20;
        assert!(apply_nim(&mut items, &mut next, &d, Some(6), bmp(), "").ret);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].owner, 0x20);
        assert_eq!(items[0].pid, 6);
        assert_eq!(guid_string(&items[0].guid.unwrap_or(GUID::zeroed())), "7820ae73-23e3-4229-82c1-e41cb67d5b9c");
    }

    #[test]
    fn appbar_block_gating() {
        const OWN: u32 = 100;
        let medium = |p: u32| p != 0 && p != 666; // 666 = elevated process
        // Real case: block allocated for us, window of a medium app -> allowed (was rejected).
        assert!(appbar_block_allowed(OWN, OWN, Some(8120), medium));
        // Block for us but window of an elevated app, dead window, or our own window -> rejected.
        assert!(!appbar_block_allowed(OWN, OWN, Some(666), medium));
        assert!(!appbar_block_allowed(OWN, OWN, Some(0), medium));
        assert!(!appbar_block_allowed(OWN, OWN, Some(OWN), |_| true));
        // Block in another process: must be the window owner's and at most medium (unchanged).
        assert!(appbar_block_allowed(8120, OWN, Some(8120), medium));
        assert!(!appbar_block_allowed(666, OWN, Some(8120), medium));
        assert!(!appbar_block_allowed(666, OWN, Some(666), medium));
        // No window: only the block's process is checked (unchanged).
        assert!(appbar_block_allowed(OWN, OWN, None, |p| p == OWN));
        assert!(!appbar_block_allowed(666, OWN, None, medium));
    }

    #[test]
    fn handle_a_32_bit_con_segno() {
        assert_eq!(ext(0xFFFF_FFFE), -2);
        assert_eq!(ext(0x1234), 0x1234);
    }
}
