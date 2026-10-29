//! Top-level windows "to show", grouping by app, WinEventHook, activation.
//!
//! THREADING: EVERYTHING on the UI thread (thread-local state). The WinEvent callback (out-of-context)
//! is run by Windows on the thread that installed the hook (the UI thread, through its
//! message loop): it only does `PostMessageW(WM_DOCK_APPS_DIRTY)`; the UI then, after the 100 ms debounce
//! (`TIMER_APPS_DEBOUNCE`), calls `refresh`. No slow COM calls here: the identity
//! (AUMID) uses `SHGetPropertyStoreForWindow`, which is fast (it reads already published properties)
//! and must be called only once per HWND (result cached in the module). The UI thread has COM STA
//! initialized by `main` (see the main.rs docs) precisely for this reason.
//!
//! Program name and icon are NOT here: they are resolved by the shell thread (`shell_thread`).
//!
//! GROUPING KEY = window AUMID ?? package AUMID (`GetApplicationUserModelId`) ??
//! normalized lowercase exe path. `ApplicationFrameHost` windows without an AUMID: `hwnd:<n>`.
//!
//! IMPLEMENTATION DETAILS
//! - Identity cache per HWND (`State::cache`). The expensive part (OpenProcess, exe path,
//!   package AUMID, window properties) is done only once; the entry is however re-evaluated
//!   if the window AUMID was missing (apps may publish it after creation): every 3 s,
//!   or every 500 ms for an `ApplicationFrameHost` frame still unresolved. Entries for
//!   windows that disappeared from the enumeration are dropped on every `refresh` (no HWND reuse).
//! - `ApplicationFrameHost`: the frame process is the host, not the app. The child window
//!   `Windows.UI.Core.CoreWindow` (process of the real app) is looked up: exe and package AUMID
//!   are taken from there if the window property is missing. If nothing is found: key `hwnd:<n>`.
//! - MRU order: each window has an increasing `stamp` updated when it becomes the
//!   foreground window (read in `refresh`, or right after a click). `AppGroup::windows` is sorted by
//!   decreasing `stamp`. The multi-window cycle instead uses the stable order of appearance
//!   (`seq`), so each click moves to the next window without bouncing between the last two.
//! - No `RefCell` stays borrowed while calling APIs that may pump messages
//!   (`SendMessageTimeoutW`, `SetForegroundWindow`...): the Win32 calls are outside the `borrow`s.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, PROPERTYKEY, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, EnumWindows, FindWindowExW, GetAncestor, GetClassNameW,
    GetDesktopWindow, GetForegroundWindow, GetPropW, GetWindow, GetWindowLongW, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, PostMessageW, SendMessageTimeoutW, SetForegroundWindow, ShowWindowAsync,
    SwitchToThisWindow, ASFW_ANY, CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, GA_PARENT, GA_ROOT, GA_ROOTOWNER, GET_WINDOW_CMD, GWL_EXSTYLE,
    OBJID_WINDOW, SC_MINIMIZE, SC_RESTORE, SMTO_ABORTIFHUNG, SW_MINIMIZE, SW_RESTORE, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_CLOSE, WM_SYSCOMMAND, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};

use crate::log;
use crate::msg::WM_DOCK_APPS_DIRTY;

const GW_OWNER: GET_WINDOW_CMD = GET_WINDOW_CMD(4);

/// `PKEY_AppUserModel_ID` {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5. Defined here to avoid depending
/// on the `Win32_Storage_EnhancedStorage` feature (which exports thousands of constants).
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY =
    PROPERTYKEY { fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3), pid: 5 };

/// Re-check schedule (time since first resolution) for windows without a window-level AUMID; after
/// the last step the re-check keeps going at the last interval (see `recheck_due`).
const AUMID_RECHECK: [Duration; 4] =
    [Duration::from_secs(3), Duration::from_secs(10), Duration::from_secs(30), Duration::from_secs(120)];
/// Same, for an unresolved `ApplicationFrameHost` frame.
const AFH_RECHECK: [Duration; 6] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];
/// Timeout for synchronous messages to other apps' windows (minimize/restore).
const SYSCMD_TIMEOUT_MS: u32 = 300;

/// Identity of a window: which app it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowIdentity {
    pub pid: u32,
    /// Exe path (original case, from `QueryFullProcessImageNameW`).
    pub exe_path: String,
    /// AUMID of the window or, failing that, of the package.
    pub aumid: Option<String>,
    /// Grouping key (see the module docs).
    pub key: String,
}

/// A group = an app entry with at least one open window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppGroup {
    /// Grouping key.
    pub key: String,
    pub exe_path: String,
    pub aumid: Option<String>,
    /// HWNDs of the group's windows (as isize), in MRU order: the most recent first.
    pub windows: Vec<isize>,
    /// true if one of the group's windows is the foreground window.
    pub active: bool,
    /// Sequence number of first appearance (display order of non-pinned apps).
    pub first_seen: u64,
}

/// Outcome of `click_group`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickOutcome {
    /// No window for the key: the caller (UI) must launch the app.
    NoWindows,
    /// A window was activated (restored if minimized).
    Activated(isize),
    /// The group's foreground window was minimized.
    Minimized(isize),
}

/// Per-HWND cache entry.
struct Entry {
    ident: WindowIdentity,
    /// Process owning the window at resolution time (guard against HWND reuse).
    pid: u32,
    /// Stable order of appearance of the window (for the multi-window cycle).
    seq: u64,
    /// MRU stamp: higher = activated more recently.
    stamp: u64,
    /// true if the AUMID came from the window property (no longer needs re-evaluation).
    win_aumid_found: bool,
    /// true if it is an `ApplicationFrameHost` frame with the key fallen back to `hwnd:<n>`.
    afh_unresolved: bool,
    /// First resolution (backoff origin).
    first: Instant,
    /// Number of re-resolutions done so far (index into the backoff schedule).
    checks: u32,
}

#[derive(Default)]
struct State {
    groups: Vec<AppGroup>,
    next_group_seq: u64,
    next_win_seq: u64,
    stamp_counter: u64,
    /// Last tracked window seen in the foreground (0 = none).
    last_fg: isize,
    cache: HashMap<isize, Entry>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static HOOKS: RefCell<Vec<HWINEVENTHOOK>> = const { RefCell::new(Vec::new()) };
}

#[inline]
fn hw(h: isize) -> HWND {
    HWND(h as *mut c_void)
}

// ------------------------------------------------------------------ WinEventHook

/// Installs the `SetWinEventHook` hooks (OUTOFCONTEXT | SKIPOWNPROCESS) for
/// `EVENT_OBJECT_SHOW/HIDE/DESTROY/CREATE`, `EVENT_SYSTEM_FOREGROUND`, `EVENT_OBJECT_CLOAKED/UNCLOAKED`.
/// The callback posts `WM_DOCK_APPS_DIRTY` (wParam = event, lParam = HWND) to the dock window.
/// Called from the UI thread (needs its message loop). Idempotent.
pub fn install_winevent_hook() -> Result<(), String> {
    if HOOKS.with(|h| !h.borrow().is_empty()) {
        return Ok(());
    }
    // CREATE(0x8000)..HIDE(0x8003) = CREATE, DESTROY, SHOW, HIDE; CLOAKED/UNCLOAKED; FOREGROUND.
    let ranges: [(u32, u32, &str); 3] = [
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE, "create..hide"),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED, "cloaked..uncloaked"),
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, "foreground"),
    ];
    let mut installed: Vec<HWINEVENTHOOK> = Vec::new();
    for (min, max, name) in ranges {
        // SAFETY: `winevent_proc` is a 'static `extern "system"` function with the WINEVENTPROC signature;
        // without WINEVENT_INCONTEXT no module is needed (hmod = None). The current (UI) thread has a
        // message loop, a requirement for out-of-context delivery.
        let hook = unsafe {
            SetWinEventHook(min, max, None, Some(winevent_proc), 0, 0, WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS)
        };
        if hook.is_invalid() {
            let err = format!("SetWinEventHook({name}) failed: {:?}", windows::core::Error::from_thread());
            for h in installed {
                // SAFETY: handle just obtained from SetWinEventHook on this thread.
                let _ = unsafe { UnhookWinEvent(h) };
            }
            return Err(err);
        }
        installed.push(hook);
    }
    log!("apps: WinEventHooks installed ({})", installed.len());
    HOOKS.with(|h| *h.borrow_mut() = installed);
    Ok(())
}

/// Removes the hooks (`UnhookWinEvent`). Idempotent. UI thread.
pub fn remove_winevent_hook() {
    let hooks = HOOKS.with(|h| std::mem::take(&mut *h.borrow_mut()));
    if hooks.is_empty() {
        return;
    }
    for hook in hooks {
        // SAFETY: handle obtained from SetWinEventHook on this same thread, never released before
        // (the list was emptied with `take`).
        let _ = unsafe { UnhookWinEvent(hook) };
    }
    log!("apps: WinEventHooks removed");
}

/// WinEvent callback (UI thread, during the message loop). Filters child-control noise and
/// posts `WM_DOCK_APPS_DIRTY`. Does not touch `STATE`: no risk of reentrancy on a `RefCell`.
unsafe extern "system" fn winevent_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.0.is_null() {
        return;
    }
    if event != EVENT_SYSTEM_FOREGROUND {
        // Only events on the window itself (not on caret, cursor, accessible elements...).
        if id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
            return;
        }
        // Discard child windows: the parent of top-levels (and owned windows) is the desktop.
        // For DESTROY the parent may no longer be readable (null): in that case we post.
        // SAFETY: GetAncestor/GetDesktopWindow accept arbitrary HWNDs and have no side effects.
        let (parent, desktop) = unsafe { (GetAncestor(hwnd, GA_PARENT), GetDesktopWindow()) };
        if !parent.0.is_null() && parent != desktop {
            return;
        }
    }
    let ui = crate::hwnd();
    if ui.0.is_null() {
        return;
    }
    // SAFETY: PostMessageW is thread-safe; if the window no longer exists it just fails.
    let _ = unsafe { PostMessageW(Some(ui), WM_DOCK_APPS_DIRTY, WPARAM(event as usize), LPARAM(hwnd.0 as isize)) };
}

// ------------------------------------------------------------------ enumeration

/// Re-enumerates the windows (`EnumWindows` + `is_taskbar_window`), associates them with groups (creating the
/// new ones with progressive `first_seen`, removing those without windows) and updates `active` and
/// the MRU order. Returns true if anything changed (groups, windows, `active`).
/// `own` = the dock window, to exclude. UI thread.
pub fn refresh(own: HWND) -> bool {
    let z = enum_taskbar_windows(own); // top first
                                       // Resolve from the bottom towards the top: new windows get increasing `seq`/`stamp`, so
                                       // the highest in Z ends up being the most "recent".
    let mut live: Vec<isize> = Vec::with_capacity(z.len());
    for &h in z.iter().rev() {
        if ensure_entry(h, false).is_some() {
            live.push(h);
        }
    }
    let mut fg = foreground_target(&live);
    // A window that just came to the foreground (e.g. a UWP frame restored long after the dock
    // started) gets one forced re-check if its identity is still provisional.
    let fg_changed = STATE.with(|s| s.borrow().last_fg) != fg;
    if fg != 0 && fg_changed {
        let provisional =
            STATE.with(|s| s.borrow().cache.get(&fg).is_some_and(|e| e.afh_unresolved || !e.win_aumid_found));
        if provisional && ensure_entry(fg, true).is_none() {
            live.retain(|&h| h != fg);
            fg = 0;
        }
    }

    STATE.with(|s| {
        let st = &mut *s.borrow_mut();

        let live_set: HashSet<isize> = live.iter().copied().collect();
        st.cache.retain(|h, _| live_set.contains(h));

        if fg != st.last_fg {
            st.last_fg = fg;
            if fg != 0 {
                st.stamp_counter += 1;
                let c = st.stamp_counter;
                if let Some(e) = st.cache.get_mut(&fg) {
                    e.stamp = c;
                }
            }
        }

        struct Acc {
            key: String,
            exe_path: String,
            aumid: Option<String>,
            min_seq: u64,
            wins: Vec<(isize, u64)>,
        }
        let mut accs: Vec<Acc> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for &h in &live {
            let Some(e) = st.cache.get(&h) else { continue };
            let i = *index.entry(e.ident.key.clone()).or_insert_with(|| {
                accs.push(Acc {
                    key: e.ident.key.clone(),
                    exe_path: e.ident.exe_path.clone(),
                    aumid: e.ident.aumid.clone(),
                    min_seq: e.seq,
                    wins: Vec::new(),
                });
                accs.len() - 1
            });
            let a = &mut accs[i];
            // Group exe/AUMID from the oldest window: stable even if windows arrive
            // with a different exe but the same key (helper processes with the same AUMID).
            if e.seq < a.min_seq {
                a.min_seq = e.seq;
                a.exe_path = e.ident.exe_path.clone();
                a.aumid = e.ident.aumid.clone();
            }
            a.wins.push((h, e.stamp));
        }

        let old_first: HashMap<&str, u64> = st.groups.iter().map(|g| (g.key.as_str(), g.first_seen)).collect();
        // New groups: progressive first_seen in the order of appearance of their oldest window.
        let mut fresh: Vec<usize> =
            (0..accs.len()).filter(|&i| !old_first.contains_key(accs[i].key.as_str())).collect();
        fresh.sort_by_key(|&i| accs[i].min_seq);
        let mut assigned: HashMap<usize, u64> = HashMap::new();
        let mut next = st.next_group_seq;
        for i in fresh {
            assigned.insert(i, next);
            next += 1;
        }

        let mut new_groups: Vec<AppGroup> = accs
            .into_iter()
            .enumerate()
            .map(|(i, mut a)| {
                a.wins.sort_by(|x, y| y.1.cmp(&x.1)); // decreasing stamp = MRU
                let windows: Vec<isize> = a.wins.iter().map(|w| w.0).collect();
                let first_seen =
                    old_first.get(a.key.as_str()).copied().or_else(|| assigned.get(&i).copied()).unwrap_or(0);
                let active = fg != 0 && windows.contains(&fg);
                AppGroup { key: a.key, exe_path: a.exe_path, aumid: a.aumid, windows, active, first_seen }
            })
            .collect();
        new_groups.sort_by_key(|g| g.first_seen);

        st.next_group_seq = next;
        let changed = new_groups != st.groups;
        if changed {
            st.groups = new_groups;
        }
        changed
    })
}

/// Snapshot of the current groups, sorted by increasing `first_seen`. UI thread.
pub fn groups() -> Vec<AppGroup> {
    STATE.with(|s| s.borrow().groups.clone())
}

/// Ensures that `cache[h]` exists and is up to date (according to the re-evaluation rules).
/// `None` if the window is gone or the process cannot be opened (the entry is dropped).
/// `force` re-resolves an existing entry regardless of the schedule.
fn ensure_entry(h: isize, force: bool) -> Option<()> {
    let pid_now = window_pid(hw(h));
    if pid_now == 0 {
        STATE.with(|s| s.borrow_mut().cache.remove(&h));
        return None;
    }
    let needs = STATE.with(|s| match s.borrow().cache.get(&h) {
        Some(e) if e.pid == pid_now => {
            if e.afh_unresolved {
                force || recheck_due(&AFH_RECHECK, e)
            } else if !e.win_aumid_found {
                force || recheck_due(&AUMID_RECHECK, e)
            } else {
                false
            }
        }
        _ => true,
    });
    if !needs {
        return Some(());
    }
    // The expensive calls are outside any borrow.
    let Some(r) = resolve_identity(hw(h), pid_now) else {
        STATE.with(|s| s.borrow_mut().cache.remove(&h));
        return None;
    };
    STATE.with(|s| {
        let st = &mut *s.borrow_mut();
        let (seq, stamp, first, checks) = match st.cache.get(&h) {
            Some(e) if e.pid == pid_now => (e.seq, e.stamp, e.first, e.checks.saturating_add(1)),
            _ => {
                st.next_win_seq += 1;
                st.stamp_counter += 1;
                (st.next_win_seq, st.stamp_counter, Instant::now(), 0)
            }
        };
        st.cache.insert(
            h,
            Entry {
                ident: r.ident,
                pid: pid_now,
                seq,
                stamp,
                win_aumid_found: r.win_aumid_found,
                afh_unresolved: r.afh_unresolved,
                first,
                checks,
            },
        );
    });
    Some(())
}

/// True when the next re-check of `e` is due: follows `sched` (offsets from the first resolution),
/// then repeats every last interval for as long as the window lives.
fn recheck_due(sched: &[Duration], e: &Entry) -> bool {
    let Some(&last) = sched.last() else {
        return false;
    };
    let n = e.checks as usize;
    let due = match sched.get(n) {
        Some(&d) => d,
        None => {
            let extra = u32::try_from(n + 1 - sched.len()).unwrap_or(u32::MAX);
            last.saturating_add(last.saturating_mul(extra))
        }
    };
    e.first.elapsed() >= due
}

struct Resolved {
    /// The window process's own exe, before the helper -> root promotion (diagnostics).
    own_exe: String,
    ident: WindowIdentity,
    win_aumid_found: bool,
    afh_unresolved: bool,
}

fn window_pid(h: HWND) -> u32 {
    let mut pid = 0u32;
    // SAFETY: arbitrary HWND allowed; `pid` is a valid local u32 for writing.
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

/// Full identity resolution (without cache).
fn resolve_identity(hwnd: HWND, pid: u32) -> Option<Resolved> {
    let proc = OwnedProcess::open(pid)?;
    let exe_path = query_exe_path(proc.0)?;
    let win_aumid = window_aumid(hwnd);
    let is_afh = is_application_frame_host(&exe_path);

    // The "real" process of the app: for a UWP frame it is the one of the child CoreWindow.
    let mut pkg_aumid: Option<String> = None;
    let mut real_exe: Option<String> = None;
    if is_afh {
        if let Some(core) = find_core_window(hwnd) {
            let core_pid = window_pid(core);
            if core_pid != 0 && core_pid != pid {
                if let Some(p2) = OwnedProcess::open(core_pid) {
                    real_exe = query_exe_path(p2.0);
                    pkg_aumid = package_aumid(p2.0);
                }
            }
        }
    } else {
        pkg_aumid = package_aumid(proc.0);
    }

    // Helper process (e.g. steamwebhelper.exe) -> root of its install tree (steam.exe). Only for plain
    // Win32 apps: windows with an AUMID are already grouped and named through it.
    let own_exe = real_exe.clone().unwrap_or_else(|| exe_path.clone());
    let root_exe = if pkg_aumid.is_none() && !is_afh {
        Some(root_exe_of(pid, proc.0, &exe_path)).filter(|r| !r.eq_ignore_ascii_case(&exe_path))
    } else {
        None
    };
    let win_aumid_found = win_aumid.is_some();
    // A promoted helper window drops its AUMID (e.g. Steam's "Valve.Steam.Client") so that it groups,
    // is named and gets its icon from the root exe, like a pinned steam.exe.
    let aumid = if root_exe.is_some() { None } else { win_aumid.or(pkg_aumid) };
    let exe_path = root_exe.unwrap_or(exe_path);
    let (key, afh_unresolved) = match (&aumid, is_afh) {
        (Some(a), _) => (a.clone(), false),
        (None, true) => (format!("hwnd:{}", hwnd.0 as isize), true),
        (None, false) => (key_from_exe_path(&exe_path), false),
    };
    Some(Resolved {
        own_exe,
        ident: WindowIdentity { pid, exe_path: real_exe.unwrap_or(exe_path), aumid, key },
        win_aumid_found,
        afh_unresolved,
    })
}

// ------------------------------------------------------------------ helper -> root process

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(process: HANDLE, class: u32, info: *mut c_void, len: u32, ret_len: *mut u32) -> i32;
}

/// `PROCESS_BASIC_INFORMATION` (`ProcessBasicInformation` = class 0).
#[repr(C)]
struct ProcessBasicInformation {
    exit_status: i32,
    peb_base: *mut c_void,
    affinity_mask: usize,
    base_priority: i32,
    pid: usize,
    parent_pid: usize,
}

/// Maximum number of ancestors walked when looking for the root of an install tree.
const MAX_ROOT_DEPTH: usize = 8;
/// Bound on the per-pid root cache (cleared when exceeded: entries are cheap to recompute).
const ROOT_CACHE_MAX: usize = 256;

thread_local! {
    /// pid -> (creation time of the pid, root exe). The creation time guards against pid reuse.
    static ROOT_CACHE: RefCell<HashMap<u32, (u64, String)>> = RefCell::new(HashMap::new());
}

fn parent_pid(proc: HANDLE) -> Option<u32> {
    let mut pbi = ProcessBasicInformation {
        exit_status: 0,
        peb_base: std::ptr::null_mut(),
        affinity_mask: 0,
        base_priority: 0,
        pid: 0,
        parent_pid: 0,
    };
    let mut ret = 0u32;
    // SAFETY: `pbi` is a writable struct matching PROCESS_BASIC_INFORMATION with the declared size;
    // `proc` is a handle with PROCESS_QUERY_LIMITED_INFORMATION, enough for class 0.
    let st = unsafe {
        NtQueryInformationProcess(
            proc,
            0,
            (&mut pbi as *mut ProcessBasicInformation).cast::<c_void>(),
            std::mem::size_of::<ProcessBasicInformation>() as u32,
            &mut ret,
        )
    };
    if st < 0 {
        return None;
    }
    u32::try_from(pbi.parent_pid).ok().filter(|&p| p != 0)
}

/// Process creation time (100 ns ticks since 1601).
fn creation_time(proc: HANDLE) -> Option<u64> {
    let (mut c, mut e, mut k, mut u) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    // SAFETY: all four out-parameters are valid local FILETIMEs; `proc` is a valid process handle.
    unsafe { GetProcessTimes(proc, &mut c, &mut e, &mut k, &mut u) }.ok()?;
    Some((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime))
}

/// Lowercase directory segments of an exe path (`c:`, `program files (x86)`, `steam`...), without the file name.
fn dir_segments(exe: &str) -> Vec<String> {
    let lower = exe.replace('/', "\\").to_lowercase();
    let mut segs: Vec<String> = lower.split('\\').filter(|s| !s.is_empty()).map(str::to_string).collect();
    segs.pop();
    segs
}

/// A directory too generic to be an install root (drive root, Windows, Program Files, user profile...).
fn is_generic_dir(segs: &[String]) -> bool {
    if segs.len() < 2 {
        return true;
    }
    match segs[1].as_str() {
        "windows" => true,
        "program files" | "program files (x86)" | "programdata" => segs.len() == 2,
        // `c:\users`, `c:\users\<name>`, `...\appdata`, `...\appdata\<local|roaming>`, `...\appdata\local\programs`.
        "users" => {
            let seg = |i: usize| segs.get(i).map(String::as_str);
            segs.len() <= 3
                || (seg(3) == Some("appdata")
                    && (segs.len() <= 5 || (segs.len() == 6 && seg(4) == Some("local") && seg(5) == Some("programs"))))
        }
        _ => false,
    }
}

/// Directory names of game libraries kept inside a launcher's install tree (Steam `steamapps\common`,
/// GOG Galaxy and Ubisoft Connect `games`...): what lives below them is a separate app, not a helper.
const LIBRARY_DIRS: [&str; 4] = ["steamapps", "steamlibrary", "games", "common"];

/// Normalized (lowercase, backslashes) form of an exe path, for comparisons.
fn norm_path(exe: &str) -> String {
    exe.replace('/', "\\").to_lowercase()
}

/// True when `parent_exe` is the root of an install tree containing `child_exe` as a helper: either the
/// same exe (multi-process apps), or an exe in a strict subdirectory of the parent's one, which must not
/// be a generic location and must not pass through a game library. A different exe in the same directory
/// is a sibling app (Word started by Outlook), not a helper.
fn same_install_tree(parent_exe: &str, child_exe: &str) -> bool {
    let p = dir_segments(parent_exe);
    let c = dir_segments(child_exe);
    if is_generic_dir(&p) || c.len() < p.len() || c[..p.len()] != p[..] {
        return false;
    }
    if c.len() == p.len() {
        return norm_path(parent_exe) == norm_path(child_exe);
    }
    !c[p.len()..].iter().any(|s| LIBRARY_DIRS.contains(&s.as_str()))
}

/// Walks up the parent chain while the parent lives in the same install tree; returns the topmost
/// such exe (or `exe_path` itself). Cached per pid.
fn root_exe_of(pid: u32, proc: HANDLE, exe_path: &str) -> String {
    let Some(t0) = creation_time(proc) else {
        return exe_path.to_string();
    };
    if let Some(r) = ROOT_CACHE.with(|c| c.borrow().get(&pid).filter(|(t, _)| *t == t0).map(|(_, r)| r.clone())) {
        return r;
    }
    let mut cur_exe = exe_path.to_string();
    let mut cur_time = t0;
    let mut cur_pid = pid;
    let mut cur_handle = proc;
    let mut held: Option<OwnedProcess> = None;
    for _ in 0..MAX_ROOT_DEPTH {
        let Some(ppid) = parent_pid(cur_handle).filter(|&p| p != cur_pid) else { break };
        let Some(parent) = OwnedProcess::open(ppid) else { break };
        // The parent must be older than the child, otherwise the pid was reused.
        let Some(pt) = creation_time(parent.0).filter(|&pt| pt < cur_time) else { break };
        let Some(pexe) = query_exe_path(parent.0) else { break };
        if !same_install_tree(&pexe, &cur_exe) {
            break;
        }
        cur_exe = pexe;
        cur_time = pt;
        cur_pid = ppid;
        cur_handle = parent.0;
        held = Some(parent); // keeps the handle alive for the next step; the previous one is closed
    }
    drop(held);
    ROOT_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= ROOT_CACHE_MAX {
            c.clear();
        }
        c.insert(pid, (t0, cur_exe.clone()));
    });
    cur_exe
}

// ------------------------------------------------------------------ diagnostics (--list-apps)

/// One top-level app window with its resolved identity (for `--list-apps`).
pub struct ListedWindow {
    pub title: String,
    /// Exe of the process owning the window (before helper -> root promotion).
    pub own_exe: String,
    pub ident: WindowIdentity,
}

/// Enumerates the current top-level app windows with the same filters and identity resolution as the
/// dock. Needs COM on the calling thread. Touches nothing (no hooks, no taskbar).
pub fn list_windows() -> Vec<ListedWindow> {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    let mut out = Vec::new();
    for h in enum_taskbar_windows(HWND::default()) {
        let pid = window_pid(hw(h));
        if pid == 0 {
            continue;
        }
        let Some(r) = resolve_identity(hw(h), pid) else { continue };
        let mut buf = [0u16; 128];
        // SAFETY: `buf` is a writable buffer of the declared length; arbitrary HWND allowed.
        let n = unsafe { GetWindowTextW(hw(h), &mut buf) }.max(0) as usize;
        let title = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        out.push(ListedWindow { title, own_exe: r.own_exe, ident: r.ident });
    }
    out
}

/// Automatically closed process handle.
struct OwnedProcess(HANDLE);

impl OwnedProcess {
    fn open(pid: u32) -> Option<OwnedProcess> {
        // SAFETY: OpenProcess has no preconditions; the obtained handle is owned by `OwnedProcess`.
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok().map(OwnedProcess)
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // SAFETY: valid handle obtained from OpenProcess, closed exactly once here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn query_exe_path(proc: HANDLE) -> Option<String> {
    // Common case: stack buffer, no heap allocation.
    let mut small = [0u16; 512];
    let mut len = small.len() as u32;
    // SAFETY: `small` has `len` writable elements; `len` is in/out (in: capacity, out: length written
    // without NUL). `proc` is a handle with PROCESS_QUERY_LIMITED_INFORMATION.
    let r = unsafe { QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(small.as_mut_ptr()), &mut len) };
    if r.is_ok() {
        let s = String::from_utf16_lossy(&small[..(len as usize).min(small.len())]);
        return if s.is_empty() { None } else { Some(s) };
    }
    // Long paths: retry once with the maximum size.
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: same contract as above, with `buf` as the output buffer.
    let r = unsafe { QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) };
    if r.is_err() {
        return None;
    }
    let s = String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Package AUMID of the process. `None` if the process has no package identity
/// (`APPMODEL_ERROR_NO_APPLICATION`).
fn package_aumid(proc: HANDLE) -> Option<String> {
    let mut buf = [0u16; 256]; // an AUMID is at most 129 characters
    let mut len = buf.len() as u32;
    // SAFETY: `buf` has `len` writable WCHARs; `len` is in/out; `proc` is a valid handle.
    let rc = unsafe { GetApplicationUserModelId(proc, &mut len, Some(PWSTR(buf.as_mut_ptr()))) };
    if rc.0 != 0 {
        return None;
    }
    // `len` includes the terminating NUL.
    let n = (len as usize).saturating_sub(1).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..n]);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// AUMID published by the window (`PKEY_AppUserModel_ID`) through `SHGetPropertyStoreForWindow`.
/// Requires COM initialized on the calling thread.
fn window_aumid(hwnd: HWND) -> Option<String> {
    // SAFETY: arbitrary HWND allowed (fails if the window does not exist). The PROPVARIANT returned
    // by GetValue is owned by us: it must be freed with PropVariantClear (the type has no Drop).
    // The string allocated by PropVariantToStringAlloc must be freed with CoTaskMemFree.
    unsafe {
        let store: IPropertyStore = match SHGetPropertyStoreForWindow(hwnd) {
            Ok(s) => s,
            Err(e) => {
                use std::sync::atomic::{AtomicBool, Ordering};
                static WARNED: AtomicBool = AtomicBool::new(false);
                // CO_E_NOTINITIALIZED = 0x800401F0: report only once, it is a setup error.
                if e.code().0 as u32 == 0x800401F0 && !WARNED.swap(true, Ordering::Relaxed) {
                    log!("apps: SHGetPropertyStoreForWindow: COM not initialized on the UI thread");
                }
                return None;
            }
        };
        let mut pv = store.GetValue(&PKEY_APP_USER_MODEL_ID).ok()?;
        let out = match PropVariantToStringAlloc(&pv) {
            Ok(p) => {
                let s = p.to_string().ok();
                CoTaskMemFree(Some(p.0 as *const c_void));
                s
            }
            Err(_) => None,
        };
        let _ = PropVariantClear(&mut pv);
        out.filter(|s| !s.is_empty())
    }
}

fn is_application_frame_host(exe_path: &str) -> bool {
    exe_path.rsplit(['\\', '/']).next().is_some_and(|f| f.eq_ignore_ascii_case("applicationframehost.exe"))
}

/// Child window `Windows.UI.Core.CoreWindow` of an `ApplicationFrameHost` frame.
fn find_core_window(frame: HWND) -> Option<HWND> {
    // SAFETY: arbitrary HWND allowed; the strings are NUL-terminated literals ('static).
    unsafe { FindWindowExW(Some(frame), None, w!("Windows.UI.Core.CoreWindow"), PCWSTR::null()) }
        .ok()
        .filter(|h| !h.0.is_null())
}

/// Key from exe path: lowercase, `\` separators. (`C:/Foo/Bar.EXE` -> `c:\foo\bar.exe`).
/// Squirrel-style versioned folders (`app-1.0.9265`) become `app-*`, so the key
/// (and therefore a pin) survives app updates (Discord, Slack, etc.).
pub fn key_from_exe_path(path: &str) -> String {
    let lower = path.replace('/', "\\").to_lowercase();
    lower
        .split('\\')
        .map(|seg| match seg.strip_prefix("app-") {
            Some(v) if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') => "app-*",
            _ => seg,
        })
        .collect::<Vec<_>>()
        .join("\\")
}

/// Extracts the AUMID from a launch target `shell:AppsFolder\<AUMID>`; `None` if it does not have that form.
pub fn aumid_from_launch(launch: &str) -> Option<String> {
    const PREFIX: &str = "shell:appsfolder\\";
    if launch.len() > PREFIX.len()
        && launch.is_char_boundary(PREFIX.len())
        && launch[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
    {
        Some(launch[PREFIX.len()..].to_string())
    } else {
        None
    }
}

// ------------------------------------------------------------------ foreground / MRU

/// Which of the tracked windows `live` matches the foreground window? The foreground
/// may be a popup owned by a tracked window (we walk up to the root owner).
/// 0 if the foreground window is not tracked (desktop, Start menu, the dock itself...).
fn foreground_target(live: &[isize]) -> isize {
    // SAFETY: no preconditions; GetAncestor accepts arbitrary HWNDs.
    let (fg, root_owner, root) = unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return 0;
        }
        (fg, GetAncestor(fg, GA_ROOTOWNER), GetAncestor(fg, GA_ROOT))
    };
    for cand in [fg, root_owner, root] {
        let c = cand.0 as isize;
        if c != 0 && live.contains(&c) {
            return c;
        }
    }
    0
}

/// true if `hwnd` is the foreground window (or the root owner of the foreground window,
/// for owned dialogs/popups).
pub fn is_foreground(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    foreground_target(&[hwnd]) == hwnd
}

/// Records `h` as activated now (MRU) and as the last foreground window.
fn touch(h: isize) {
    STATE.with(|s| {
        let st = &mut *s.borrow_mut();
        st.stamp_counter += 1;
        let c = st.stamp_counter;
        if let Some(e) = st.cache.get_mut(&h) {
            e.stamp = c;
            st.last_fg = h;
        }
    });
}

// ------------------------------------------------------------------ window actions

/// Left click on a group: no windows -> `NoWindows`; one window: if foreground it
/// is minimized (`Minimized`), otherwise it is activated (`Activated`, `SW_RESTORE` if minimized, then
/// `AllowSetForegroundWindow` + `SetForegroundWindow`/`SwitchToThisWindow`); multiple windows: cycles
/// through them at every click (simple MRU). UI thread.
pub fn click_group(key: &str) -> ClickOutcome {
    // Windows of the group still alive, in MRU order, with their `seq` (stable order).
    let wins: Vec<(isize, u64)> = STATE.with(|s| {
        let st = s.borrow();
        st.groups
            .iter()
            .find(|g| g.key == key)
            .map(|g| g.windows.iter().map(|&h| (h, st.cache.get(&h).map(|e| e.seq).unwrap_or(0))).collect())
            .unwrap_or_default()
    });
    // SAFETY: IsWindow accepts arbitrary HWNDs.
    let wins: Vec<(isize, u64)> =
        wins.into_iter().filter(|&(h, _)| unsafe { IsWindow(Some(hw(h))) }.as_bool()).collect();
    if wins.is_empty() {
        return ClickOutcome::NoWindows;
    }

    let handles: Vec<isize> = wins.iter().map(|w| w.0).collect();
    let fg = foreground_target(&handles); // 0 if no window of the group is in the foreground

    if fg == 0 {
        // Inactive group: the most recent one.
        let target = wins[0].0;
        activate_window(target);
        return ClickOutcome::Activated(target);
    }
    if wins.len() == 1 {
        minimize_window(fg);
        return ClickOutcome::Minimized(fg);
    }
    // Active group with multiple windows: move to the next one in stable order of appearance.
    let mut by_seq = wins.clone();
    by_seq.sort_by_key(|w| w.1);
    let pos = by_seq.iter().position(|w| w.0 == fg).unwrap_or(0);
    let target = by_seq[(pos + 1) % by_seq.len()].0;
    activate_window(target);
    ClickOutcome::Activated(target)
}

/// Windows of group `key` in stable order of appearance (`seq`), for the window picker. UI thread.
pub fn windows_in_order(key: &str) -> Vec<isize> {
    STATE.with(|s| {
        let st = s.borrow();
        let mut v: Vec<(isize, u64)> = st
            .groups
            .iter()
            .find(|g| g.key == key)
            .map(|g| g.windows.iter().map(|&h| (h, st.cache.get(&h).map(|e| e.seq).unwrap_or(0))).collect())
            .unwrap_or_default();
        v.sort_by_key(|w| w.1);
        v.into_iter().map(|w| w.0).collect()
    })
}

/// Sends `WM_CLOSE` (PostMessageW) to all windows of the group. UI thread.
pub fn close_group(key: &str) {
    let wins: Vec<isize> =
        STATE.with(|s| s.borrow().groups.iter().find(|g| g.key == key).map(|g| g.windows.clone()).unwrap_or_default());
    log!("apps::close_group({key}): {} windows", wins.len());
    for h in wins {
        // SAFETY: PostMessageW accepts arbitrary HWNDs; it does not block and fails if the window is gone.
        let _ = unsafe { PostMessageW(Some(hw(h)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

/// Sends `WM_SYSCOMMAND` with a timeout (does not block the UI on hung apps). `true` if delivered.
fn sys_command(h: isize, cmd: u32) -> bool {
    let mut res = 0usize;
    // SAFETY: arbitrary HWND allowed; `res` is valid for writing; SMTO_ABORTIFHUNG +
    // a 300 ms timeout limit the wait if the target thread does not respond.
    let r = unsafe {
        SendMessageTimeoutW(
            hw(h),
            WM_SYSCOMMAND,
            WPARAM(cmd as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            SYSCMD_TIMEOUT_MS,
            Some(&mut res),
        )
    };
    r.0 != 0
}

fn minimize_window(h: isize) {
    if !sys_command(h, SC_MINIMIZE) {
        // SAFETY: ShowWindowAsync does not block (it queues the command to the owning thread).
        let _ = unsafe { ShowWindowAsync(hw(h), SW_MINIMIZE) };
    }
    log!("apps: minimize {h:#x}");
}

/// Brings `h` to the foreground; if minimized, restores it. Updates the MRU. UI thread (also used by
/// the window picker).
pub(crate) fn activate_window(h: isize) {
    let w = hw(h);
    // SAFETY: all the APIs accept arbitrary HWNDs and fail without UB if the window disappears.
    unsafe {
        if IsIconic(w).as_bool() && !sys_command(h, SC_RESTORE) {
            let _ = ShowWindowAsync(w, SW_RESTORE);
        }
        // Allows any process (including the target app) to take the foreground.
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let ok = SetForegroundWindow(w).as_bool();
        let _ = BringWindowToTop(w);
        if !ok || !is_foreground(h) {
            // Foreground lock: `SwitchToThisWindow` bypasses it and is
            // the same call used by Alt+Tab; it also restores if still minimized.
            SwitchToThisWindow(w, true);
        }
    }
    touch(h);
    log!("apps: activate {h:#x}");
}

// ------------------------------------------------------------------ filters

struct Ctx {
    own: HWND,
    out: Vec<isize>,
}

/// List of the HWNDs that pass `is_taskbar_window`, in Z order (top first).
pub fn enum_taskbar_windows(own: HWND) -> Vec<isize> {
    let mut ctx = Ctx { own, out: Vec::new() };
    // SAFETY: `ctx` lives until EnumWindows returns (synchronous, same thread); the callback
    // receives it through LPARAM and uses it only during the call.
    let _ = unsafe { EnumWindows(Some(enum_cb), LPARAM(&mut ctx as *mut Ctx as isize)) };
    ctx.out
}

unsafe extern "system" fn enum_cb(h: HWND, lp: LPARAM) -> BOOL {
    // SAFETY: lp is the `*mut Ctx` passed by `enum_taskbar_windows`, valid during the enumeration.
    let ctx = unsafe { &mut *(lp.0 as *mut Ctx) };
    if is_taskbar_window(h, ctx.own) {
        ctx.out.push(h.0 as isize);
    }
    BOOL(1)
}

/// Window filter: visible, not toolwindow, owner discarded except with
/// APPWINDOW, not cloaked, Progman/WorkerW classes discarded, dock window discarded, and
/// `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` must succeed.
/// In addition (ideas only, no code copied, from lightbar/ManagedShell/Seelen): discarding
/// `WS_EX_NOACTIVATE` windows without APPWINDOW, those marked `ITaskList_Deleted`, shell
/// top-level `CoreWindow`s (Start, search...) and Edge's `TabProxyWindow`s.
pub fn is_taskbar_window(h: HWND, own: HWND) -> bool {
    if h == own {
        return false;
    }
    // SAFETY: all the APIs below accept arbitrary HWNDs and fail without UB if the window disappears.
    unsafe {
        if !IsWindowVisible(h).as_bool() {
            return false;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        if ex & WS_EX_NOACTIVATE.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        // Owned windows (dialogs etc.) excluded except WS_EX_APPWINDOW, like Alt+Tab.
        let owner = GetWindow(h, GW_OWNER).unwrap_or_default();
        if !owner.0.is_null() && ex & WS_EX_APPWINDOW.0 == 0 {
            return false;
        }
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return false;
        }
        let mut cls = [0u16; 64];
        let n = GetClassNameW(h, &mut cls).max(0) as usize;
        let class = String::from_utf16_lossy(&cls[..n.min(cls.len())]);
        if matches!(
            class.as_str(),
            "Progman" | "WorkerW" | "Windows.UI.Core.CoreWindow" | "Windows.Internal.Shell.TabProxyWindow"
        ) {
            return false;
        }
        // Property set by Explorer on windows removed from the taskbar.
        if !GetPropW(h, w!("ITaskList_Deleted")).0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(p) => {
                let _ = CloseHandle(p);
            }
            Err(_) => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_normalization() {
        assert_eq!(key_from_exe_path("C:/Foo/Bar.EXE"), "c:\\foo\\bar.exe");
        assert_eq!(
            key_from_exe_path("C:\\Users\\x\\AppData\\Local\\Discord\\app-1.0.9265\\Discord.exe"),
            "c:\\users\\x\\appdata\\local\\discord\\app-*\\discord.exe"
        );
        assert_eq!(key_from_exe_path("C:\\app-store\\a.exe"), "c:\\app-store\\a.exe");
    }

    #[test]
    fn aumid_prefix() {
        assert_eq!(aumid_from_launch("shell:AppsFolder\\A.B_x!App").as_deref(), Some("A.B_x!App"));
        assert_eq!(aumid_from_launch("C:\\x.exe"), None);
        assert_eq!(aumid_from_launch("shell:AppsFolder\\"), None);
    }

    #[test]
    fn install_tree() {
        let steam = "C:\\Program Files (x86)\\Steam\\steam.exe";
        let web = "C:\\Program Files (x86)\\Steam\\bin\\cef\\cef.win64\\steamwebhelper.exe";
        assert!(same_install_tree(steam, web));
        assert!(same_install_tree(web, web));
        assert!(!same_install_tree("C:\\Windows\\explorer.exe", web));
        assert!(!same_install_tree("C:\\Windows\\System32\\cmd.exe", "C:\\Windows\\System32\\x\\a.exe"));
        assert!(!same_install_tree("C:\\launcher.exe", "C:\\Games\\a\\a.exe"));
        assert!(!same_install_tree("C:\\Program Files\\x.exe", "C:\\Program Files\\Foo\\y.exe"));
        assert!(!same_install_tree("C:\\Users\\me\\t.exe", "C:\\Users\\me\\Foo\\y.exe"));
        assert!(!same_install_tree("D:\\Other\\steam.exe", web));
    }

    #[test]
    fn install_tree_rejects_games_and_siblings() {
        let steam = "C:\\Program Files (x86)\\Steam\\steam.exe";
        let portal = "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Portal 2\\portal2.exe";
        assert!(!same_install_tree(steam, portal));
        let gog = "C:\\Program Files (x86)\\GOG Galaxy\\GalaxyClient.exe";
        assert!(!same_install_tree(gog, "C:\\Program Files (x86)\\GOG Galaxy\\Games\\Witcher\\bin\\w.exe"));
        let upc = "C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\upc.exe";
        assert!(!same_install_tree(upc, "C:\\Program Files (x86)\\Ubisoft\\Ubisoft Game Launcher\\games\\X\\x.exe"));
        let office = "C:\\Program Files\\Microsoft Office\\root\\Office16\\";
        assert!(!same_install_tree(&format!("{office}OUTLOOK.EXE"), &format!("{office}WINWORD.EXE")));
        assert!(same_install_tree(&format!("{office}OUTLOOK.EXE"), &format!("{office}outlook.exe")));
    }

    #[test]
    fn local_programs_is_generic() {
        let progs = "C:\\Users\\me\\AppData\\Local\\Programs";
        assert!(!same_install_tree(&format!("{progs}\\stub.exe"), &format!("{progs}\\Foo\\foo.exe")));
        assert!(same_install_tree(&format!("{progs}\\Foo\\foo.exe"), &format!("{progs}\\Foo\\bin\\h.exe")));
        assert!(!same_install_tree(
            "C:\\Users\\me\\AppData\\Roaming\\s.exe",
            "C:\\Users\\me\\AppData\\Roaming\\A\\a.exe"
        ));
    }

    #[test]
    fn afh_detection() {
        assert!(is_application_frame_host("C:\\Windows\\System32\\ApplicationFrameHost.exe"));
        assert!(!is_application_frame_host("C:\\x\\notepad.exe"));
    }
}
