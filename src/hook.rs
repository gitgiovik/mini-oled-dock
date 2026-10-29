//! Low-level keyboard hook (WH_KEYBOARD_LL) for the Win key pressed "alone".
//!
//! Rules (see README for the limits):
//!  * Win down  -> stores `win_down`, `other=false`, SWALLOWS the event.
//!  * first other key down while `win_down` -> SWALLOWS that one too and injects
//!    with a SINGLE SendInput the pair [WIN down, key down] (extra-info = INJECT_TAG
//!    on both): the order is deterministic. Win+E, Win+D, Win+R... work.
//!    Repeats and the release of that key pass through normally.
//!  * Win up -> if `other==false` it swallows the event and posts WM_DOCK_TOGGLE
//!    (the real work is done by the wnd_proc), otherwise it lets the release through.
//!  * Double tap: a lone Win press starting within DOUBLE_TAP_MS of the previous lone release
//!    posts WM_DOCK_START_MENU instead of the second WM_DOCK_TOGGLE (the first tap already toggled).
//!  * Win auto-repeat: ignored (swallowed while the press is "pending").
//!  * Events injected by OTHER apps (LLKHF_INJECTED without our tag): ignored,
//!    the dock only responds to the physical Win key.
//!  * Left + right Win together: the second Win is treated as an "other key".
//!  * Stuck state (release lost on the secure desktop): if more than REPEAT_WINDOW_MS
//!    have passed since the last Win event, the first other key does NOT inject anything.
//!
//! The callback does NO I/O nor heavy work (LL hook timeout ~300 ms):
//! it only posts messages to the window, which logs and acts.
//!
//! The hook lives in a dedicated thread (`hook`) that only runs a message loop: the callback
//! is served by that thread, independently of rendering, timers and logging of the UI thread.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, VIRTUAL_KEY,
    VK_LWIN, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetMessageW, PeekMessageW, PostMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG,
    PM_NOREMOVE, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::msg::{WM_DOCK_DIAG, WM_DOCK_START_MENU, WM_DOCK_TOGGLE, WM_DOCK_WINKEY};
use crate::{hwnd, log};

/// Events for WM_DOCK_WINKEY (log only).
pub const EV_PRESSED: usize = 0;
pub const EV_ALONE: usize = 1;
pub const EV_COMBO: usize = 2;
pub const EV_SENDFAIL: usize = 3;
pub const EV_DOUBLE: usize = 4;

/// Marker in dwExtraInfo of the events we inject ("DOCK").
pub const INJECT_TAG: usize = 0x4B43_4F44;
/// Beyond this interval (ms) from the last Win event the "Win held" state is considered
/// stuck (e.g. Win+L: the release goes to the secure desktop). Auto-repeat keeps renewing it.
const REPEAT_WINDOW_MS: u32 = 1500;
/// Double tap: a lone Win press that starts within this interval (ms) of the previous lone Win
/// release posts WM_DOCK_START_MENU instead of WM_DOCK_TOGGLE.
pub const DOUBLE_TAP_MS: u32 = 300;

/// Thread message (UI -> hook): run the `reinstall` watchdog.
const WM_HOOK_REINSTALL: u32 = WM_APP + 10;

static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static WIN_OTHER: AtomicBool = AtomicBool::new(false);
static WIN_VK: AtomicU32 = AtomicU32::new(0);
static WIN_LAST_TIME: AtomicU32 = AtomicU32::new(0);
static HOOK_RAW: AtomicIsize = AtomicIsize::new(0);
/// Time (`kb.time`) of the last lone Win release that toggled the dock; 0 = none (or already
/// consumed by a double tap, so a third tap toggles again).
static LAST_ALONE_UP: AtomicU32 = AtomicU32::new(0);
/// true if the current Win press started within `DOUBLE_TAP_MS` of `LAST_ALONE_UP`.
static WIN_DOUBLE: AtomicBool = AtomicBool::new(false);
/// Experiment: if true a lone Win is swallowed but does NOT toggle the dock.
pub static NO_TOGGLE: AtomicBool = AtomicBool::new(false);
/// Opt-in diagnostics (`--diag`): if false `diag()` returns immediately.
pub static DIAG: AtomicBool = AtomicBool::new(false);

fn set_hook() -> Result<isize, String> {
    // SAFETY: no preconditions.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
    // SAFETY: the callback is `extern "system"` and lives for the whole process; thread id 0 = global.
    // Invariant: it must be called from the `hook` thread, which is also the one serving the callback.
    let h = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_keyboard_proc), Some(module.into()), 0) }
        .map_err(|e| format!("SetWindowsHookExW: {e}"))?;
    Ok(h.0 as isize)
}

/// Hook thread started: `tid` is used to post messages to it, `done` signals the end.
pub struct HookThread {
    tid: u32,
    done: Receiver<()>,
    join: JoinHandle<()>,
}

/// Starts the `hook` thread and waits for the installation result. Must be called AFTER
/// creating the dock window (the callback posts messages to `hwnd()`).
pub fn start() -> Result<HookThread, String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let join = std::thread::Builder::new()
        .name("hook".into())
        .spawn(move || thread_main(ready_tx, done_tx))
        .map_err(|e| format!("starting hook thread: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(tid)) => Ok(HookThread { tid, done: done_rx, join }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the hook thread ended before installing the hook".into()),
    }
}

/// Body of the `hook` thread: installs the hook and runs ONLY the message loop.
fn thread_main(ready: Sender<Result<u32, String>>, done: Sender<()>) {
    let mut msg = MSG::default();
    // SAFETY: `msg` is valid and writable. PM_NOREMOVE on an empty queue only serves to create
    // the thread message queue, so PostThreadMessageW from the UI thread cannot fail.
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    // Maximum priority for the hook thread: the LL callback must respond within
    // LowLevelHooksTimeout even with a game saturating the CPU.
    // SAFETY: GetCurrentThread returns an always-valid pseudo-handle.
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } {
        let _ = e; // not critical
    }
    match set_hook() {
        Ok(raw) => {
            HOOK_RAW.store(raw, Relaxed);
            log!("low-level keyboard hook installed (dedicated thread)");
            // SAFETY: no preconditions.
            let tid = unsafe { GetCurrentThreadId() };
            let _ = ready.send(Ok(tid));
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            let _ = done.send(());
            return;
        }
    }
    loop {
        // SAFETY: `msg` is valid and writable; filter None = all messages of the thread.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break; // WM_QUIT (0) or error (-1)
        }
        if msg.message == WM_HOOK_REINSTALL {
            reinstall();
            continue;
        }
        // SAFETY: `msg` just filled by GetMessageW (the thread has no windows: in practice
        // only thread messages arrive here, but forwarding is harmless).
        unsafe { DispatchMessageW(&msg) };
    }
    uninstall();
    let _ = done.send(());
}

/// Asks the hook thread to run the watchdog (called from the UI thread 2 s timer).
pub fn request_reinstall(t: &HookThread) {
    // SAFETY: PostThreadMessageW is thread-safe; if the thread has already ended it merely fails.
    let _ = unsafe { PostThreadMessageW(t.tid, WM_HOOK_REINSTALL, WPARAM(0), LPARAM(0)) };
}

/// Closes the hook thread: WM_QUIT, then waits (max 2 s) for it to remove the hook.
pub fn stop(t: HookThread) {
    // SAFETY: as above.
    let _ = unsafe { PostThreadMessageW(t.tid, WM_QUIT, WPARAM(0), LPARAM(0)) };
    match t.done.recv_timeout(Duration::from_secs(2)) {
        Ok(()) => {
            let _ = t.join.join();
        }
        Err(_) => log!("hook thread: no response within 2 s, proceeding without join"),
    }
}

/// Watchdog (run on the hook thread): Windows silently removes an LL hook that exceeds
/// LowLevelHooksTimeout several times. We ALWAYS reinstall (first the new one, then remove the
/// old one: no instant without a hook). The Win state is in a static, so it survives. It is
/// skipped if Win is held, so as not to touch the hook in the middle of a combination.
fn reinstall() {
    if HOOK_RAW.load(Relaxed) == 0 || WIN_DOWN.load(Relaxed) {
        return;
    }
    match set_hook() {
        Ok(new) => {
            let old = HOOK_RAW.swap(new, Relaxed);
            // SAFETY: `old` is an HHOOK of ours; if Windows had already removed it, this fails
            // with no effect and the error is intended (it is the case the watchdog covers).
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(old as *mut _)) };
        }
        Err(e) => log!("hook watchdog: reinstall failed: {e}"),
    }
}

fn uninstall() {
    let raw = HOOK_RAW.swap(0, Relaxed);
    if raw == 0 {
        return;
    }
    // SAFETY: `raw` is the HHOOK returned by SetWindowsHookExW, removed only once (swap),
    // by the same thread that installed it.
    match unsafe { UnhookWindowsHookEx(HHOOK(raw as *mut _)) } {
        Ok(()) => log!("keyboard hook removed"),
        Err(e) => log!("UnhookWindowsHookEx failed: {e}"),
    }
}

unsafe extern "system" fn ll_keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION in a WH_KEYBOARD_LL hook, lparam points to a
        // KBDLLHOOKSTRUCT valid for the whole duration of the callback.
        let kb = unsafe { &*(lp.0 as *const KBDLLHOOKSTRUCT) };
        let swallowed = handle_key(wp.0 as u32, kb);
        diag(wp.0 as u32, kb, swallowed);
        if swallowed {
            return LRESULT(1);
        }
    }
    // SAFETY: unchanged forwarding to the hook chain.
    unsafe { CallNextHookEx(None, code, wp, lp) }
}

/// Returns true if the event must be swallowed.
fn handle_key(msg: u32, kb: &KBDLLHOOKSTRUCT) -> bool {
    if kb.dwExtraInfo == INJECT_TAG {
        return false; // our own re-injected event
    }
    if kb.flags.0 & LLKHF_INJECTED.0 != 0 {
        return false; // injected by another app: not a physical key
    }
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !down && !up {
        return false;
    }
    let vk = kb.vkCode;
    let is_win = vk == u32::from(VK_LWIN.0) || vk == u32::from(VK_RWIN.0);

    if is_win {
        if down {
            let fresh = kb.time.wrapping_sub(WIN_LAST_TIME.load(Relaxed)) > REPEAT_WINDOW_MS;
            WIN_LAST_TIME.store(kb.time, Relaxed);
            if WIN_DOWN.load(Relaxed) && !fresh {
                if vk == WIN_VK.load(Relaxed) {
                    // auto-repeat: still "pending" -> swallow; already re-injected -> let through
                    return !WIN_OTHER.load(Relaxed);
                }
                // the other Win key while the first is held: it is an "other key"
                return other_key_down(kb);
            }
            WIN_DOWN.store(true, Relaxed);
            WIN_OTHER.store(false, Relaxed);
            WIN_VK.store(vk, Relaxed);
            let last = LAST_ALONE_UP.load(Relaxed);
            WIN_DOUBLE.store(last != 0 && kb.time.wrapping_sub(last) <= DOUBLE_TAP_MS, Relaxed);
            note(EV_PRESSED);
            return true;
        }
        if WIN_DOWN.load(Relaxed) && vk == WIN_VK.load(Relaxed) {
            WIN_DOWN.store(false, Relaxed);
            if !WIN_OTHER.load(Relaxed) {
                if WIN_DOUBLE.swap(false, Relaxed) {
                    // Second quick tap: Start menu; the next tap starts a fresh sequence.
                    LAST_ALONE_UP.store(0, Relaxed);
                    note(EV_DOUBLE);
                    post_start_menu();
                } else {
                    // `| 1`: a real timestamp of 0 must not read as "no previous tap".
                    LAST_ALONE_UP.store(kb.time | 1, Relaxed);
                    note(EV_ALONE);
                    post_toggle();
                }
                return true;
            }
            // Win used in a combination: it does not count as the first tap of a double tap.
            LAST_ALONE_UP.store(0, Relaxed);
            WIN_DOUBLE.store(false, Relaxed);
        }
        return false;
    }

    down && other_key_down(kb)
}

/// Key (other than the held Win) pressed. Returns true if it must be swallowed: in that case
/// it was re-injected inside the [WIN down, key down] pair.
fn other_key_down(kb: &KBDLLHOOKSTRUCT) -> bool {
    if !WIN_DOWN.load(Relaxed) || WIN_OTHER.load(Relaxed) {
        return false;
    }
    if kb.time.wrapping_sub(WIN_LAST_TIME.load(Relaxed)) > REPEAT_WINDOW_MS {
        // Win release lost (secure desktop): Win is no longer held, do not inject anything.
        WIN_DOWN.store(false, Relaxed);
        return false;
    }
    WIN_OTHER.store(true, Relaxed);
    if inject_win_and_key(WIN_VK.load(Relaxed), kb) {
        note(EV_COMBO);
        true
    } else {
        // SendInput failed: better to let at least the original key through.
        note(EV_SENDFAIL);
        false
    }
}

fn post(msg: u32, ev: usize) {
    let h = hwnd();
    if h.0.is_null() {
        return; // without this check PostMessageW(NULL) would post to the hook thread
    }
    // SAFETY: PostMessageW is thread-safe; the error is ignored (no I/O in the callback).
    let _ = unsafe { PostMessageW(Some(h), msg, WPARAM(ev), LPARAM(0)) };
}

fn note(ev: usize) {
    post(WM_DOCK_WINKEY, ev);
}

fn post_toggle() {
    if NO_TOGGLE.load(Relaxed) {
        return;
    }
    post(WM_DOCK_TOGGLE, 0);
}

fn post_start_menu() {
    if NO_TOGGLE.load(Relaxed) {
        return;
    }
    post(WM_DOCK_START_MENU, 0);
}

fn key_down_input(vk: u32, scan: u32, extended: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk as u16),
                wScan: scan as u16,
                dwFlags: if extended { KEYEVENTF_EXTENDEDKEY } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: INJECT_TAG,
            },
        },
    }
}

/// Injects [WIN down, key down] with ONE call: the two events enter the queue
/// contiguously and in order, regardless of when the callback returns.
/// Returns true if both were accepted.
fn inject_win_and_key(win_vk: u32, kb: &KBDLLHOOKSTRUCT) -> bool {
    let inputs = [
        key_down_input(win_vk, 0, true), // LWIN/RWIN are "extended" keys
        key_down_input(kb.vkCode, kb.scanCode, kb.flags.0 & LLKHF_EXTENDED.0 != 0),
    ];
    // SAFETY: slice of 2 initialized INPUTs; cbSize = size_of::<INPUT>() as required.
    // The injected events re-enter our callback with INJECT_TAG and pass through.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    sent == 2
}

/// DIAGNOSTICS (only with `--diag`): for every Win event (including those let through) posts WM_DOCK_DIAG.
/// wparam: bit0=down, bit1=up, bit2=injected, bit3=lower_il_injected, bit4=swallowed,
/// bit5=our tag, bit6=extended, bit7=rwin, bit8..=scancode. lparam: dwExtraInfo.
fn diag(msg: u32, kb: &KBDLLHOOKSTRUCT, swallowed: bool) {
    if !DIAG.load(Relaxed) {
        return;
    }
    let vk = kb.vkCode;
    if vk != u32::from(VK_LWIN.0) && vk != u32::from(VK_RWIN.0) {
        return;
    }
    let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    let mut w: usize = 0;
    if down {
        w |= 1
    }
    if up {
        w |= 2
    }
    if kb.flags.0 & LLKHF_INJECTED.0 != 0 {
        w |= 4
    }
    if kb.flags.0 & 0x02 != 0 {
        w |= 8
    } // LLKHF_LOWER_IL_INJECTED
    if swallowed {
        w |= 16
    }
    if kb.dwExtraInfo == INJECT_TAG {
        w |= 32
    }
    if kb.flags.0 & LLKHF_EXTENDED.0 != 0 {
        w |= 64
    }
    if vk == u32::from(VK_RWIN.0) {
        w |= 128
    }
    w |= (kb.scanCode as usize & 0xFFFF) << 8;
    if hwnd().0.is_null() {
        return;
    }
    // Latency: difference between now and the event timestamp (both GetTickCount, ms).
    // SAFETY: GetTickCount has no preconditions.
    let delta_ms = unsafe { GetTickCount() }.wrapping_sub(kb.time) & 0xFFFF;
    w |= (delta_ms as usize & 0xFFFF) << 24;
    // SAFETY: GetForegroundWindow has no preconditions; PostMessageW is thread-safe.
    let fg = unsafe { GetForegroundWindow() };
    let _ = unsafe { PostMessageW(Some(hwnd()), WM_DOCK_DIAG, WPARAM(w), LPARAM(fg.0 as isize)) };
}
