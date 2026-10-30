//! UI state: items, hover/animations, hit-testing, clicks, context menu, auto-hide.
//!
//! THREADING: UI thread ONLY. State is thread-local (`UI`) or atomic (visibility flags,
//! also read by the Ctrl+C handler only through `main::hwnd`). No blocking work: COM,
//! icons, audio and process launches go to the shell thread via `shell_thread::request`; results
//! come back as `WM_DOCK_SHELL_READY` / `WM_DOCK_VOLUME`.
//!
//! RESPONSIBILITIES
//!  * Builds the item list: pinned apps first (`config.pinned` order), then unpinned
//!    `apps::groups()` by `first_seen`. For every new key it sends
//!    `ShellRequest::ResolveApp` (name + icon at `BarSizes::icon_px`); on a DPI or icon size change it redoes the icon requests.
//!  * Every `redraw()`: builds a `render::Scene`, calls `render::layout`, recomputes the geometry
//!    (bar horizontally centered, width = `layout.width`), `render::resize` if the size
//!    changes, `render::paint`, `render::present`.
//!  * Hover: a plate (`render::Plate`) that animates over 120 ms (`TIMER_ANIM` timer, 15 ms,
//!    out-cubic easing, `KillTimer` when idle) towards the item under the cursor; pressed = alpha 50.
//!  * Mouse: `WM_MOUSEMOVE`/`WM_MOUSELEAVE` (TrackMouseEvent), `WM_SETCURSOR` (hand over items),
//!    `WM_LBUTTONDOWN/UP`, `WM_MBUTTONUP`, `WM_RBUTTONUP`, `WM_MOUSEWHEEL` (volume only: +-2%).
//!  * Drag & drop: left button down on an item + movement past `DRAG_THRESHOLD` starts a reorder inside
//!    the item's zone (apps / tray icons / system modules). The dragged item follows the cursor, the others slide
//!    (`TIMER_ANIM`); the drop persists `pinned` / `app_order` / `tray_order` / `modules_order`.
//!  * Click: apps (left, middle and right click), bell (Win+N via `SendInput` with
//!    `hook::INJECT_TAG` in `dwExtraInfo`), volume (toggle mute / `ms-settings:apps-volume`), clock
//!    (`ms-settings:dateandtime`), network (popup / right click `ms-settings:network`), metrics
//!    (Task Manager), Start button (fixed item at the left end, not draggable: left click = the dock's Start menu
//!    `popup_startmenu`, right click = quick-links popup, `popup_start`).
//!  * System monitor (`TIMER_SYSMON`, `sysmon_sync`): network and metrics modules, polled only while
//!    the bar is visible; a disabled module has no timer, no state and nothing loaded.
//!  * Themed context menu (`popup::open_app_menu`, command in `WM_DOCK_APP_MENU_CMD`): Pin/Unpin
//!    (`config::pin/unpin`), New window, Close all windows (`apps::close_group`).
//!    While the popup is open, auto-hide is suspended (`popup::is_open`).
//!  * Window picker (`picker`): the pointer resting `PICKER_HOVER_MS` on an app with 2+ windows
//!    (`TIMER_HOVER_PICKER`, one-shot, armed only then) opens live thumbnails of its windows; it
//!    also suspends auto-hide and closes on a click on the bar, another item, or the bar hiding.
//!  * DPI: `WM_DPICHANGED`/`WM_DISPLAYCHANGE` -> recompute metrics, fonts, icons, geometry.
//!  * Display: the bar sits on `config.monitor` (`dock_hmonitor`), the primary monitor when it is
//!    empty or that display is not connected; `WM_DISPLAYCHANGE` moves it when displays come and go.
//!    With `config.monitor == "pointer"` the Win key brings the bar to the display under the pointer
//!    (`follow_pointer`), and a double tap opens the dock's Start menu there (`open_start_menu`, `popup_startmenu`).
//!  * Auto-hide (screen edge, Win key, 2.5 s timeout); `config::effective_autohide()`
//!    false = always visible.
//!
//! REENTRANCY: `SetWindowPos`/`UpdateLayeredWindow` can re-enter (WM_DPICHANGED), so
//! no `borrow` of `UI` is held across calls that pump messages (`SetWindowPos`, `present`).

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering::Relaxed};

use windows::core::BOOL;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::AttachThreadInput;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, ReleaseCapture, SendInput, SetCapture, TrackMouseEvent, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, TME_LEAVE, TRACKMOUSEEVENT, VIRTUAL_KEY,
    VK_ESCAPE, VK_F15, VK_LBUTTON, VK_LWIN, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, DeregisterShellHookWindow, GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowThreadProcessId, InternalGetWindowText, IsIconic, IsWindow, IsWindowVisible, IsZoomed,
    KillTimer, LoadCursorW, RegisterShellHookWindow, RegisterWindowMessageW, SetCursor, SetForegroundWindow,
    SetWindowPos, ShowWindow, WindowFromPoint, EVENT_SYSTEM_FOREGROUND, GWL_STYLE, HTCLIENT, HWND_TOPMOST, IDC_HAND,
    MA_NOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONUP, WM_MOUSEACTIVATE, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SETTINGCHANGE, WM_TIMER, WS_CAPTION,
};

use crate::apps::{self, ClickOutcome};
use crate::audio::VolumeState;
use crate::config::{self, PinnedApp};
use crate::icons::IconBitmap;
use crate::log;
use crate::msg::{
    ID_MENU_CLOSE_ALL, ID_MENU_NEW_WINDOW, ID_MENU_PIN, ID_MENU_UNPIN, TIMER_ANIM, TIMER_APPS_DEBOUNCE, TIMER_CLOCK,
    TIMER_HOVER_PICKER, TIMER_POLL, TIMER_RECONCILE, TIMER_SYSMON, WM_DOCK_APPS_DIRTY, WM_DOCK_APP_MENU_CMD,
    WM_DOCK_SHELL_READY, WM_DOCK_VOLUME,
};
use crate::popup::{self, Anchor, AppMenuInfo, PopupKind};
use crate::render::{
    AppView, BarSizes, Indicator, ItemId, ItemRect, Layout, Lift, Metrics, Plate, Scene, SystemView, TrayIcon,
};
use crate::shell_thread::{self, ResolveApp, ShellEvent, ShellRequest};
use crate::tray_host::{self, TrayClick, TrayGeometry, TrayView};
use crate::{hook, hwnd, render};

// ---------------------------------------------------------------- constants

/// Time the dock stays visible after startup (ms).
const SHOW_MS_STARTUP: u64 = 3000;
/// Time the dock stays visible without the mouse over it (ms).
const SHOW_MS: u64 = 2500;
/// Dwell time at the screen edge before the dock appears (ms).
const EDGE_MS: u64 = 150;
/// Provisional bar width (logical px) before the first layout.
const DEFAULT_BAR_WIDTH: i32 = 400;
/// Duration of the hover plate's entry animation (ms).
const PLATE_IN_MS: u64 = 90;
/// Duration of the plate's exit fade (ms).
const PLATE_OUT_MS: u64 = 140;
/// Animation timer period (ms).
const ANIM_TICK_MS: u32 = 15;
/// Plate alpha when hovered / pressed.
const HOVER_ALPHA: u8 = 20;
const PRESS_ALPHA: u8 = 36;
/// Debounce after `WM_DOCK_APPS_DIRTY` (ms).
const DEBOUNCE_MS: u32 = 100;
/// Pointer resting on an app item with 2+ windows for this long opens the window picker (ms).
const PICKER_HOVER_MS: u32 = 400;
/// Periodic reconciliation of the window list (ms).
const RECONCILE_MS: u32 = 5000;
/// Timer for the bar width animation (local id, distinct from those in `msg`).
const TIMER_WIDTH: usize = 40;
/// After a popup closes, a foreground change to the fullscreen app within this
/// time (ms) is the automatic focus return and does not hide the dock.
const POPUP_CLOSE_GRACE_MS: u64 = 400;
/// Right after the dock/popup take the foreground over a fullscreen app, a foreground change back
/// to that app within this time (ms) is the app bouncing it back (or a stale event), not the user.
const FS_BOUNCE_GRACE_MS: u64 = 800;
/// How many times the foreground is re-taken after such a bounce, per show of the dock.
const FS_MAX_RETAKES: u32 = 2;
/// Duration of the width animation (ms, out-cubic).
const WIDTH_ANIM_MS: u64 = 160;
/// Timer of the attention blink (local id).
const TIMER_ATTN: usize = 41;
/// Timer of the show/hide slide+fade (local id).
const TIMER_VIS: usize = 42;
/// Blink half period (ms) and total blinking time before the indicator stays steady orange.
const ATTN_HALF_MS: u64 = 500;
const ATTN_BLINK_MS: u64 = 6000;
/// Show / hide animation durations (ms) and slide distance (logical px).
const SHOW_ANIM_MS: u64 = 140;
const HIDE_ANIM_MS: u64 = 100;
const SLIDE_PX: i32 = 10;
/// Shell hook notifications (wParam of the registered "SHELLHOOK" message).
const HSHELL_WINDOWACTIVATED: usize = 4;
const HSHELL_RUDEAPPACTIVATED: usize = 0x8004;
const HSHELL_FLASH: usize = 0x8006;
/// `WM_MOUSELEAVE` (defined here so as not to depend on the feature that exports it).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// `WM_CAPTURECHANGED`: the window lost the mouse capture.
const WM_CAPTURECHANGED: u32 = 0x0215;
/// Horizontal movement (logical px) after which a press becomes a drag.
const DRAG_THRESHOLD: i32 = 6;
/// Duration (ms) of the slide of the other items / of the drop settle (out-cubic).
const SLIDE_MS: u64 = 120;
/// Duration (ms) of the indicator width change when the foreground moves (out-cubic).
const ACT_ANIM_MS: u64 = 120;
/// Period of `TIMER_SYSMON` (ms): one CPU/RAM sample per tick.
const SYSMON_MS: u32 = 2000;
/// The network is queried every this many sysmon ticks (~6 s), the first tick included.
pub const NET_POLL_TICKS: u32 = 3;
/// N key (Win+N opens the notification center).
const VK_N: VIRTUAL_KEY = VIRTUAL_KEY(0x4E);

// ------------------------------------------------------------------- state

static VISIBLE: AtomicBool = AtomicBool::new(false);
static HIDE_AT: AtomicU64 = AtomicU64::new(0);
static EDGE_SINCE: AtomicU64 = AtomicU64::new(0);
static EDGE_ARMED: AtomicBool = AtomicBool::new(true);
/// true if the dock was shown over a fullscreen / borderless app.
static FS_SHOWN: AtomicBool = AtomicBool::new(false);
/// HWND (as isize) of the fullscreen app under the dock, to give it the foreground back.
static FS_PREV: AtomicIsize = AtomicIsize::new(0);
/// Tick (ms) of the last `force_foreground` call (0 = never).
static FG_TAKEN_AT: AtomicU64 = AtomicU64::new(0);
/// Foreground re-takes after a fullscreen bounce since the dock was shown.
static FS_RETAKES: AtomicU32 = AtomicU32::new(0);
/// Previous state of the left/right mouse buttons (falling-edge detection).
static CLICK_DOWN: AtomicBool = AtomicBool::new(false);
/// Id of the registered "SHELLHOOK" window message (0 = not registered).
static SHELLHOOK_MSG: AtomicU32 = AtomicU32::new(0);
/// Tick (ms) of the last new attention request: origin of the blink phase.
static ATTN_START: AtomicU64 = AtomicU64::new(0);

/// Geometry of the dock and of its display (physical pixels, per-monitor DPI).
#[derive(Clone, Copy, Debug)]
pub struct Geom {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Rectangle of the bar's display (`config.monitor`, primary by default).
    pub mon: RECT,
}

/// An app item (pinned and/or with windows) in the UI state.
#[derive(Clone, Debug, Default)]
struct Entry {
    /// Item key (the pin's key if pinned, otherwise the group's).
    key: String,
    /// Window group key to use with `apps::*` (may differ from `key` in case).
    group_key: String,
    /// Displayed name (config.pinned.name or the shell thread's reply); empty until it arrives.
    name: String,
    /// true if the name must not be overwritten by the shell thread (currently never: a pin's stored name
    /// is only a fallback).
    fixed_name: bool,
    /// Icon already at the current physical size; `None` = placeholder.
    icon: Option<IconBitmap>,
    /// Side (physical px) for which `ShellRequest::ResolveApp` was sent (0 = never).
    req_px: u32,
    pinned: bool,
    /// Launch target (pin, or derived from exe/AUMID).
    launch: Option<String>,
    exe_path: String,
    aumid: Option<String>,
    /// Custom icon of the pin (plain name inside `datadir::icons_dir()`).
    custom_icon: Option<String>,
    /// Group of current windows (empty = no windows).
    windows: Vec<isize>,
    active: bool,
    /// Indicator "foreground" level animation: `act_from` -> `act_to` (0 or 1) from tick `act_start`
    /// (0 = settled).
    act_from: f32,
    act_to: f32,
    act_start: u64,
}

/// Indicator level 0..=1 of the entry at instant `now` (out-cubic).
fn act_value(e: &Entry, now: u64) -> f32 {
    if e.act_start == 0 {
        return e.act_to;
    }
    let t = (now.saturating_sub(e.act_start) as f32 / ACT_ANIM_MS as f32).clamp(0.0, 1.0);
    e.act_from + (e.act_to - e.act_from) * (1.0 - (1.0 - t).powi(3))
}

/// Hover plate animation.
#[derive(Clone, Copy, Debug, Default)]
struct PlateAnim {
    from: RECT,
    to: RECT,
    /// Start tick (ms).
    start: u64,
    /// Start/end alpha (0..=255).
    from_alpha: u8,
    to_alpha: u8,
    /// Duration (ms) of this animation: shorter when entering than when leaving.
    dur: u64,
    running: bool,
}

/// Width animation of the pill (the bar stays centered).
#[derive(Clone, Copy, Debug, Default)]
struct WidthAnim {
    from: i32,
    to: i32,
    /// Start tick (ms).
    start: u64,
    running: bool,
}

/// Interpolated width (out-cubic) at instant `now`.
fn width_at(a: &WidthAnim, now: u64) -> i32 {
    let t = (now.saturating_sub(a.start) as f32 / WIDTH_ANIM_MS as f32).clamp(0.0, 1.0);
    lerp_i32(a.from, a.to, 1.0 - (1.0 - t).powi(3))
}

/// Shifts the layout by `off` px and widens it to `new_w`: needed when the window is wider than the
/// final layout (shrinking pill), so the items stay centered.
fn shift_layout(l: &mut Layout, off: i32, new_w: i32) {
    let mv = |r: &mut RECT| {
        r.left += off;
        r.right += off;
    };
    for it in &mut l.items {
        mv(&mut it.rect);
    }
    mv(&mut l.tray_area);
    if l.separator_x >= 0 {
        l.separator_x += off;
    }
    if l.separator2_x >= 0 {
        l.separator2_x += off;
    }
    l.width = new_w;
}

/// Show/hide animation: visibility level 0..=1 (alpha and slide) moving `from` -> `to`.
#[derive(Clone, Copy, Debug)]
struct VisAnim {
    from: f32,
    to: f32,
    start: u64,
    dur: u64,
    running: bool,
}

/// Current visibility level: ease-out-cubic when showing, ease-in when hiding.
fn vis_level(a: &VisAnim, now: u64) -> f32 {
    if !a.running {
        return a.to;
    }
    let t = (now.saturating_sub(a.start) as f32 / a.dur.max(1) as f32).clamp(0.0, 1.0);
    let e = if a.to >= a.from { 1.0 - (1.0 - t).powi(3) } else { t * t };
    a.from + (a.to - a.from) * e
}

#[derive(Default)]
struct UiState {
    /// Width of the displayed pill (physical px; 0 = never drawn).
    shown_w: i32,
    wanim: WidthAnim,
    /// Bar metrics: the monitor DPI.
    metrics: Metrics,
    /// Content sizes of the bar (`render::BarSizes::from_config`).
    sizes: BarSizes,
    /// Effective DPI of the bar's monitor (the popups use it).
    monitor_dpi: u32,
    entries: Vec<Entry>,
    layout: Layout,
    volume: VolumeState,
    hover: Option<ItemId>,
    pressed: Option<ItemId>,
    plate: PlateAnim,
    tracking_mouse: bool,
    /// Current tray icons (`tray_host::snapshot()`).
    tray: Vec<TrayView>,
    /// Cached `tray_host::any_user_hidden()` (recomputed in `refresh_tray_state`, not per frame).
    tray_overflow: bool,
    /// Pending notifications (bell badge).
    notif_count: usize,
    /// "Do Not Disturb" state (None = unreadable).
    notif_dnd: Option<bool>,
    /// Last left click on a tray icon: (id, tick ms, screen point), for double-click detection.
    last_tray_click: Option<(u64, u64, POINT)>,
    /// Last geometry/rects sent to the tray host (flat key of integers).
    last_tray_geo: Vec<i32>,
    /// Client x of the last left-button press, and whether the pressed item can be dragged.
    press_x: i32,
    press_drag: bool,
    /// Reorder in progress (or settling after the drop).
    drag: Option<DragState>,
    /// Network glyph state (last `ShellEvent::Network`); default while the module is off.
    net: crate::net::NetStatus,
    /// CPU / RAM usage shown by the metrics module.
    usage: crate::metrics::Usage,
    /// CPU sampler: exists only while the metrics module is on (`sysmon_sync`).
    sampler: Option<crate::metrics::Sampler>,
}

thread_local! {
    static GEOM: Cell<Option<Geom>> = const { Cell::new(None) };
    /// HMONITOR (as isize) the bar was last placed on, to log a display change once.
    static LAST_HMON: Cell<isize> = const { Cell::new(0) };
    /// "pointer" mode (`config::MONITOR_POINTER`): HMONITOR (as isize) the Win key last brought the
    /// bar to; 0 = not chosen yet (the display under the pointer is used).
    static FOLLOW_HMON: Cell<isize> = const { Cell::new(0) };
    static POLL_TICKS: Cell<u32> = const { Cell::new(0) };
    static VIS: Cell<VisAnim> = const { Cell::new(VisAnim { from: 0.0, to: 0.0, start: 0, dur: 1, running: false }) };
    /// Windows that asked for attention (FlashWindowEx) and were not activated yet.
    static ATTN: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    static UI: RefCell<UiState> = RefCell::new(UiState::default());
    /// Ids of the overflow tray icons in the last drawn layout (kept outside `UI` so it can be
    /// read from the popup without risking a double borrow).
    static TRAY_HIDDEN: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    /// true if some items have no ResolveApp request because the shell thread was not ready yet.
    static NEED_REQUESTS: Cell<bool> = const { Cell::new(false) };
    /// true if the apps debounce timer is armed.
    static DEBOUNCE_ARMED: Cell<bool> = const { Cell::new(false) };
    /// `config.names` at the last `rebuild_entries` (to redo the requests if it changes).
    static LAST_NAMES: RefCell<Option<std::collections::BTreeMap<String, String>>> = const { RefCell::new(None) };
    /// Wheel remainder (WHEEL_DELTA units) not yet translated into volume steps.
    static WHEEL_ACC: Cell<i32> = const { Cell::new(0) };
    /// true while `TIMER_SYSMON` is armed; ticks since it was armed.
    static SYSMON_ARMED: Cell<bool> = const { Cell::new(false) };
    /// Metrics flag the running `TIMER_SYSMON` period was computed for.
    static SYSMON_PERIOD_METRICS: Cell<bool> = const { Cell::new(false) };
    static SYSMON_TICKS: Cell<u32> = const { Cell::new(0) };
    /// true once a `NetworkQuery` was sent: the shell thread holds a `net::Querier` to release.
    static NET_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// true while the shell thread is expected to hold the audio controller (volume module on).
    static AUDIO_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

fn now_ms() -> u64 {
    // SAFETY: GetTickCount64 has no preconditions.
    unsafe { GetTickCount64() }
}

// ------------------------------------------------------------------ public API

/// Prepares the UI state BEFORE the window exists: DPI of the bar's display, provisional geometry
/// (centered bar, provisional width). Returns the `Geom` to create the window with.
/// Called by `main::run` after `config::init()`.
pub fn init() -> Result<Geom, String> {
    let dpi = dock_dpi();
    let sizes = BarSizes::from_config(&config::get());
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        st.metrics = Metrics { dpi };
        st.sizes = sizes;
        st.monitor_dpi = dpi;
    });
    let g = compute_geom(config_top(), Metrics { dpi }.px(DEFAULT_BAR_WIDTH), dpi, sizes)?;
    GEOM.with(|c| c.set(Some(g)));
    log!(
        "initial geometry: x={} y={} w={} h={} dpi={dpi} (monitor {}x{})",
        g.x,
        g.y,
        g.w,
        g.h,
        g.mon.right - g.mon.left,
        g.mon.bottom - g.mon.top
    );
    Ok(g)
}

/// After the window is created: `render::init`, initial items from the config, the UI's own timers
/// (`TIMER_POLL` 50 ms, `TIMER_RECONCILE` 5 s, `TIMER_CLOCK` aligned to the minute),
/// first `redraw`, and initial visibility of `SHOW_MS_STARTUP` ms (always visible if
/// `!config::effective_autohide()`). Also installs `apps::install_winevent_hook`.
pub fn attach(h: HWND) -> Result<(), String> {
    let (metrics, sizes) = UI.with(|u| {
        let st = u.borrow();
        (st.metrics, st.sizes)
    });
    render::init(metrics, sizes)?;
    if let Err(e) = apps::install_winevent_hook() {
        log!("ui::attach: apps WinEventHook not installed: {e}");
    }
    tray_host::set_icon_px((sizes.tray_px(metrics)).max(metrics.px(16)) as u32);
    refresh_tray_state();
    apps::refresh(h);
    rebuild_entries();
    set_timer(h, TIMER_POLL, 50);
    set_timer(h, TIMER_RECONCILE, RECONCILE_MS);
    arm_clock(h);
    // SAFETY: registering a window message by name has no preconditions.
    let shell_msg = unsafe { RegisterWindowMessageW(w!("SHELLHOOK")) };
    if shell_msg == 0 {
        log!("ui::attach: RegisterWindowMessage(SHELLHOOK) failed");
    } else {
        SHELLHOOK_MSG.store(shell_msg, Relaxed);
        // SAFETY: `h` is our live top-level window.
        if !unsafe { RegisterShellHookWindow(h) }.as_bool() {
            log!("ui::attach: RegisterShellHookWindow failed");
        }
    }
    let ms = if config::effective_autohide() { SHOW_MS_STARTUP } else { u64::MAX / 4 };
    show_dock("startup", ms);
    Ok(())
}

/// All window messages NOT handled directly by `main`. Returns `None` if the
/// message is of no interest (then `main` calls `DefWindowProcW`). Messages the UI must handle:
///  * `WM_TIMER` for `TIMER_POLL`, `TIMER_RECONCILE`, `TIMER_APPS_DEBOUNCE`, `TIMER_ANIM`, `TIMER_CLOCK`;
///  * `WM_DOCK_APPS_DIRTY` (starts the debounce), `WM_DOCK_SHELL_READY` (`shell_thread::drain_events`),
///    `WM_DOCK_VOLUME` (`audio::decode_volume_msg`);
///  * mouse (see the module doc), `WM_MOUSEACTIVATE` (-> `MA_NOACTIVATE`), `WM_DPICHANGED`,
///    `WM_DISPLAYCHANGE`, `WM_SETTINGCHANGE` (with "intl" -> `clock::invalidate_formats`).
pub fn handle_message(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if msg != 0 && msg == SHELLHOOK_MSG.load(Relaxed) {
        on_shell_hook(h, wp.0, lp.0);
        return Some(LRESULT(0));
    }
    match msg {
        WM_TIMER => on_timer(h, wp.0),
        WM_MOUSEACTIVATE => Some(LRESULT(MA_NOACTIVATE as isize)),
        WM_DOCK_APP_MENU_CMD => {
            on_app_menu_cmd(wp.0 as u32);
            Some(LRESULT(0))
        }
        WM_DOCK_APPS_DIRTY => {
            if wp.0 as u32 == EVENT_SYSTEM_FOREGROUND {
                on_shell_hook(h, HSHELL_WINDOWACTIVATED, lp.0);
                on_foreground_changed(HWND(lp.0 as *mut std::ffi::c_void));
            }
            if !DEBOUNCE_ARMED.with(|d| d.replace(true)) {
                set_timer(h, TIMER_APPS_DEBOUNCE, DEBOUNCE_MS);
            }
            Some(LRESULT(0))
        }
        WM_DOCK_SHELL_READY => {
            let mut changed = false;
            for ev in shell_thread::drain_events() {
                changed |= apply_shell_event(ev);
            }
            if changed {
                // New name/icon can change the width: the layout is recomputed even when the bar is hidden.
                redraw();
            }
            Some(LRESULT(0))
        }
        WM_DOCK_VOLUME => {
            let st = crate::audio::decode_volume_msg(wp.0, lp.0);
            UI.with(|u| u.borrow_mut().volume = st);
            crate::popup::on_volume(st);
            redraw_if_visible();
            Some(LRESULT(0))
        }
        WM_MOUSEMOVE => {
            on_mouse_move(h, wp, lp);
            Some(LRESULT(0))
        }
        WM_MOUSELEAVE => {
            on_mouse_leave();
            Some(LRESULT(0))
        }
        WM_CAPTURECHANGED => {
            on_capture_lost();
            Some(LRESULT(0))
        }
        WM_SETCURSOR => on_set_cursor(lp),
        WM_LBUTTONDOWN => {
            on_lbutton_down(h, lp);
            Some(LRESULT(0))
        }
        WM_LBUTTONUP => {
            on_lbutton_up(lp);
            Some(LRESULT(0))
        }
        WM_MBUTTONUP => {
            on_mbutton_up(lp);
            Some(LRESULT(0))
        }
        WM_RBUTTONUP => {
            on_rbutton_up(lp);
            Some(LRESULT(0))
        }
        WM_MOUSEWHEEL => {
            on_mouse_wheel(wp, lp);
            Some(LRESULT(0))
        }
        WM_DPICHANGED => {
            let dpi = (wp.0 & 0xFFFF) as u32;
            picker_cancel("display changed");
            apply_dpi(if dpi >= 96 { dpi } else { dock_dpi() });
            Some(LRESULT(0))
        }
        WM_DISPLAYCHANGE => {
            // Also a display plugged in or out: the bar moves to (or back to) its chosen display.
            picker_cancel("display changed");
            apply_dpi(dock_dpi());
            Some(LRESULT(0))
        }
        WM_SETTINGCHANGE => {
            on_setting_change(lp);
            None // DefWindowProc may do its own thing
        }
        _ => None,
    }
}

fn on_timer(h: HWND, id: usize) -> Option<LRESULT> {
    match id {
        TIMER_POLL => {
            if NEED_REQUESTS.with(|n| n.get()) && shell_thread::thread_id() != 0 {
                request_missing();
            }
            on_poll();
        }
        TIMER_RECONCILE => {
            refresh_apps();
            request_missing();
        }
        TIMER_APPS_DEBOUNCE => {
            kill_timer(h, TIMER_APPS_DEBOUNCE);
            DEBOUNCE_ARMED.with(|d| d.set(false));
            refresh_apps();
        }
        TIMER_ANIM => on_anim_tick(h),
        TIMER_WIDTH => on_width_tick(h),
        TIMER_ATTN => on_attn_tick(h),
        TIMER_VIS => on_vis_tick(h),
        TIMER_SYSMON => sysmon_tick(),
        TIMER_HOVER_PICKER => on_hover_picker_timer(h),
        TIMER_CLOCK => {
            if VISIBLE.load(Relaxed) && config::get().show_clock {
                // Re-armed one-shot: SetTimer on the same id replaces the previous interval.
                set_timer(h, TIMER_CLOCK, crate::clock::tick_ms());
                redraw();
            } else {
                // Hidden bar or clock module: no per-tick redraws; `arm_clock` re-arms the timer.
                kill_timer(h, TIMER_CLOCK);
            }
        }
        _ => return None,
    }
    Some(LRESULT(0))
}

/// Win key alone (`WM_DOCK_TOGGLE`): shows or hides the dock. In "pointer" mode the bar first moves
/// to the display under the pointer; if it was visible on another display it stays visible there.
pub fn toggle() {
    let moved = follow_pointer();
    if VISIBLE.load(Relaxed) && !moved {
        hide_dock("Win alone (toggle)");
    } else {
        show_dock(if moved { "Win alone (pointer display)" } else { "Win alone (toggle)" }, SHOW_MS);
    }
}

/// Lone Win tap (`WM_DOCK_TOGGLE`). With `config.win_opens_start` it opens / closes the Start menu
/// together with the bar (`start_with_bar`), otherwise it shows / hides the bar (`toggle`).
pub fn win_tap() {
    if config::get().win_opens_start {
        start_with_bar();
    } else {
        toggle();
    }
}

/// Second quick Win tap (`WM_DOCK_START_MENU`). With `config.win_opens_start` every tap is the same
/// toggle (`start_with_bar`), otherwise it opens the Start menu (`open_start_menu`).
pub fn win_double_tap() {
    if config::get().win_opens_start {
        start_with_bar();
    } else {
        open_start_menu();
    }
}

/// `win_opens_start`: the Start menu and the bar appear and disappear as ONE animation. A hidden bar
/// is shown and the Start menu is linked to its slide+fade (`popup::link_to_bar`: same level, same
/// frame, presented from `on_vis_tick`); with the Start menu open, the bar hides and the menu is linked
/// to the hide animation, then closes on its last frame. A tap during that hide reverses both. With
/// auto-hide off the bar stays and only the Start menu closes.
fn start_with_bar() {
    if popup::current() == Some(PopupKind::StartMenu) {
        if !VISIBLE.load(Relaxed) && popup::is_linked() {
            show_dock("Win (Start + bar, reversed)", SHOW_MS);
        } else if VISIBLE.load(Relaxed) && config::effective_autohide() {
            popup::link_to_bar();
            hide_dock("Win (Start + bar)");
        } else {
            popup::close("Win (Start)");
        }
        return;
    }
    toggle_start_menu("Win (Start + bar)");
}

/// Toggles the dock's Start menu on the bar, which in "pointer" mode is first brought to the display
/// under the pointer. A hidden bar is shown and an opening menu is linked to its slide+fade.
fn toggle_start_menu(reason: &str) {
    follow_pointer();
    let was_visible = VISIBLE.load(Relaxed);
    if !was_visible {
        show_dock(reason, SHOW_MS);
    }
    match anchor_for(ItemId::Start).or_else(bar_start_anchor) {
        Some(a) => popup::toggle(PopupKind::StartMenu, a),
        None => {
            log!("toggle_start_menu: the bar has no geometry yet");
            return;
        }
    }
    if !was_visible && popup::current() == Some(PopupKind::StartMenu) && popup::link_to_bar() {
        let (dy, alpha) = vis_offset(config_top());
        popup::follow_bar(dy, alpha, false);
    }
}

/// Win key tapped twice (`WM_DOCK_START_MENU`): opens (or closes) the dock's own Start menu
/// (`popup_startmenu`: all apps, search, pins) on the bar, which in "pointer" mode is first brought
/// to the display under the pointer. The first tap already toggled the bar, so it is shown again if
/// that tap hid it.
pub fn open_start_menu() {
    toggle_start_menu("Win double tap (Start menu)");
}

/// Anchor at the left end of the bar, for the Start popup when the Start button is hidden.
fn bar_start_anchor() -> Option<Anchor> {
    let g = GEOM.with(|g| g.get())?;
    let dpi = UI.with(|u| {
        let st = u.borrow();
        if st.monitor_dpi >= 96 {
            st.monitor_dpi
        } else {
            st.metrics.dpi
        }
    });
    Some(Anchor {
        item: RECT { left: g.x, top: g.y, right: g.x + g.h, bottom: g.y + g.h },
        bar: RECT { left: g.x, top: g.y, right: g.x + g.w, bottom: g.y + g.h },
        top: config_top(),
        dpi,
    })
}

/// true when the bar follows the pointer (`config.monitor == "pointer"`).
fn follows_pointer() -> bool {
    config::get().monitor.eq_ignore_ascii_case(config::MONITOR_POINTER)
}

/// Display under the mouse pointer (the nearest one if the pointer position cannot be read).
fn pointer_hmonitor() -> HMONITOR {
    let p = cursor_pos().unwrap_or_default();
    // SAFETY: MonitorFromPoint with MONITOR_DEFAULTTONEAREST has no preconditions and always returns a monitor.
    unsafe { MonitorFromPoint(p, MONITOR_DEFAULTTONEAREST) }
}

/// "pointer" mode: brings the bar to the display under the pointer (metrics, fonts, icons and
/// geometry follow that display's DPI). Returns true if the bar changed display; a bar visible on the
/// old display is hidden at once there, so the caller's `show_dock` slides it in on the new one.
fn follow_pointer() -> bool {
    if !follows_pointer() {
        return false;
    }
    let h = pointer_hmonitor();
    let prev = FOLLOW_HMON.with(|c| c.replace(h.0 as isize));
    if prev == h.0 as isize || GEOM.with(|g| g.get()).is_none() {
        return false;
    }
    // A popup or the picker anchored to the old position would be left behind.
    popup::close("bar moved to the pointer display");
    picker_cancel("bar moved to the pointer display");
    if VISIBLE.load(Relaxed) {
        hide("bar moved to the pointer display", true);
    }
    apply_dpi(dock_dpi());
    true
}

/// The config changed on disk (`config::poll_changed` returned true): rebuilds the items
/// (pins/names), rereads position/auto-hide and redraws.
pub fn on_config_changed() {
    log!("ui::on_config_changed: rebuilding items and geometry");
    // Sizes (or what makes them count: display mode, tray) changed: same path as a DPI change
    // (fonts, icons at the new size, geometry).
    let (mon, cur) = UI.with(|u| {
        let st = u.borrow();
        (st.monitor_dpi, st.sizes)
    });
    // A different display (`monitor`) may also have a different DPI.
    let dpi = dock_dpi();
    if mon >= 96 && (dpi != mon || BarSizes::from_config(&config::get()) != cur) {
        apply_dpi(dpi);
    }
    // Volume / notification modules switched: create or release their threads and COM objects.
    modules_sync();
    // `show_tray` switched: start or stop the tray host (stopped = no threads, no window).
    tray_sync();
    rebuild_entries();
    // `tray_hidden` may have been edited by hand: realign the bar icons.
    refresh_tray_state();
    popup::on_tray_changed();
    // The position (top/bottom) may have changed: the geometry is always recomputed.
    redraw();
    // Modules switched on/off: start/stop their polling and free their state.
    sysmon_sync();
    if VISIBLE.load(Relaxed) {
        // `show_seconds` / `show_clock` may have changed: re-align the clock timer.
        arm_clock(hwnd());
        if let Some(g) = GEOM.with(|g| g.get()) {
            show_topmost(hwnd(), &g);
        }
    }
    // Window previews switched off: no hover timer, picker hidden.
    if !config::get().window_previews {
        picker_cancel("window previews off");
        // Off = no timer and no window: destroy it (idempotent; `open` recreates it).
        crate::picker::shutdown();
    }
    // The bar may have moved or resized under an open popup (Settings): keep it attached to its item.
    if let Some(a) = popup::current().and_then(item_for_popup).and_then(anchor_for) {
        popup::reanchor(a);
    }
}

/// Starts or stops the notification thread and the audio controller to match `show_notifications` /
/// `show_volume` (a disabled module holds no thread, COM object or callback).
fn modules_sync() {
    let cfg = config::get();
    if cfg.show_notifications && !crate::notif::is_started() {
        crate::notif::start(hwnd());
    } else if !cfg.show_notifications && crate::notif::is_started() {
        let hook = popup::suspend_mouse_hook();
        crate::notif::stop();
        popup::resume_mouse_hook(hook);
    }
    let was = AUDIO_ACTIVE.with(|a| a.replace(cfg.show_volume));
    if cfg.show_volume && !was {
        shell_thread::request(ShellRequest::VolumeRefresh);
    } else if !cfg.show_volume {
        // Idempotent on the shell side (also covers a module that was already off at startup).
        shell_thread::request(ShellRequest::AudioRelease);
    }
}

/// Starts the tray host when `show_tray` is switched on (native taskbar hidden), stops it when switched
/// off; in both cases `TaskbarCreated` is announced so apps re-register their icons with whoever now owns
/// the tray (our host, or Explorer). Blocks the UI thread for at most ~2 s, only on a real switch.
fn tray_sync() {
    let on = config::get().show_tray;
    let started = tray_host::is_started();
    if on && !started && crate::taskbar::is_hidden() {
        // The wait below must not run under the popup's low-level mouse hook (it would stall the mouse).
        let hook = popup::suspend_mouse_hook();
        match tray_host::start(hwnd()) {
            Ok(()) => crate::taskbar::announce_tray(),
            Err(e) => log!("tray host not started: {e}"),
        }
        popup::resume_mouse_hook(hook);
    } else if !on && started {
        let hook = popup::suspend_mouse_hook();
        tray_host::stop();
        crate::taskbar::announce_tray();
        popup::resume_mouse_hook(hook);
        log!("tray: host stopped (show_tray off)");
    }
}

/// Rereads `tray_host::snapshot()` into the UI state.
fn refresh_tray_state() {
    let mut snap = tray_host::snapshot();
    let cfg = config::get();
    if !cfg.tray_order.is_empty() {
        // Stable sort: icons missing from `tray_order` keep their registration order, at the end.
        snap.sort_by_key(|t| cfg.tray_order.iter().position(|k| k.eq_ignore_ascii_case(&t.key)).unwrap_or(usize::MAX));
    }
    let overflow = tray_host::any_user_hidden();
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        st.tray = snap;
        st.tray_overflow = overflow;
    });
}

/// `WM_DOCK_TRAY_CHANGED` (wParam = 1 bitmap, 2 layout): rereads `tray_host::snapshot()` and redraws.
/// Called by `main`. With the bar hidden, a layout change still recomputes the geometry,
/// so apps (`Shell_NotifyIconGetRect`) find the rects of the "shown" position.
pub fn on_tray_changed(kind: usize) {
    refresh_tray_state();
    if VISIBLE.load(Relaxed) || kind == 2 {
        redraw();
    }
}

/// `WM_DOCK_NOTIF`: updates the bell's badge/DND state from `notif::snapshot()`. Called by `main`.
pub fn on_notif_changed() {
    let dnd = crate::notif::snapshot().dnd;
    let count = crate::notif::count();
    let changed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let changed = st.notif_count != count || st.notif_dnd != dnd;
        st.notif_count = count;
        st.notif_dnd = dnd;
        changed
    });
    if changed {
        redraw_if_visible();
    }
}

/// `WM_DOCK_POPUP_CLOSED`: the popup closed; the auto-hide countdown restarts and the persistent
/// plate disappears (the plate goes back to following the mouse). Called by `main`.
pub fn on_popup_closed() {
    HIDE_AT.store(now_ms() + SHOW_MS, Relaxed);
    redraw_if_visible();
}

/// Bar item that opens popup `kind` (persistent plate); the app menu has none.
fn item_for_popup(kind: PopupKind) -> Option<ItemId> {
    match kind {
        PopupKind::Notifications => Some(ItemId::Notifications),
        PopupKind::Audio => Some(ItemId::Volume),
        PopupKind::Calendar => Some(ItemId::Clock),
        PopupKind::Power | PopupKind::Settings => Some(ItemId::Power),
        PopupKind::TrayOverflow => Some(ItemId::TrayOverflow),
        PopupKind::Network => Some(ItemId::Network),
        PopupKind::Start | PopupKind::StartMenu => Some(ItemId::Start),
        PopupKind::AppMenu => None,
    }
}

/// `Anchor` for opening a popup from item `id` (screen coordinates).
fn anchor_for(id: ItemId) -> Option<Anchor> {
    let g = GEOM.with(|g| g.get())?;
    let (r, dpi) = UI.with(|u| {
        let st = u.borrow();
        (st.layout.rect_of(id), if st.monitor_dpi >= 96 { st.monitor_dpi } else { st.metrics.dpi })
    });
    let r = r?;
    Some(Anchor {
        item: RECT { left: g.x + r.left, top: g.y + r.top, right: g.x + r.right, bottom: g.y + r.bottom },
        bar: RECT { left: g.x, top: g.y, right: g.x + g.w, bottom: g.y + g.h },
        top: config_top(),
        dpi,
    })
}

/// Opens/closes popup `kind` from item `id`.
fn toggle_popup(kind: PopupKind, id: ItemId) {
    match anchor_for(id) {
        Some(a) => popup::toggle(kind, a),
        None => log!("toggle_popup({kind:?}): item {id:?} not in the layout"),
    }
}

/// Ids of the tray icons that ended up in overflow (behind the chevron) in the last drawn layout, in
/// bar order. Used by `popup_tray`. UI thread only.
pub fn tray_hidden_ids() -> Vec<u64> {
    TRAY_HIDDEN.with(|h| h.borrow().clone())
}

/// After a redraw: sends the tray host the bar, tray area and (screen) icon rects if they changed.
/// Overflow icons receive the chevron's rect.
fn sync_tray_geometry(layout: &Layout, g: &Geom, top: bool) {
    let off = |r: RECT| RECT { left: g.x + r.left, top: g.y + r.top, right: g.x + r.right, bottom: g.y + r.bottom };
    let bar = RECT { left: g.x, top: g.y, right: g.x + g.w, bottom: g.y + g.h };
    let area = off(layout.tray_area);
    let edge = if top { 1 } else { 3 };
    TRAY_HIDDEN.with(|h| h.borrow_mut().clone_from(&layout.tray_hidden));
    let chevron = if layout.has_chevron { layout.rect_of(ItemId::TrayOverflow).map(off) } else { None };
    let mut rects: Vec<(u64, RECT)> = Vec::new();
    for it in &layout.items {
        if let ItemId::Tray(id) = it.id {
            rects.push((id, off(it.rect)));
        }
    }
    if let Some(c) = chevron {
        for id in &layout.tray_hidden {
            rects.push((*id, c));
        }
    }
    let mut key: Vec<i32> =
        vec![bar.left, bar.top, bar.right, bar.bottom, area.left, area.top, area.right, area.bottom, edge as i32];
    for (id, r) in &rects {
        key.extend_from_slice(&[(*id & 0x7FFF_FFFF) as i32, (*id >> 31) as i32, r.left, r.top, r.right, r.bottom]);
    }
    let changed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        if st.last_tray_geo == key {
            false
        } else {
            st.last_tray_geo = key;
            true
        }
    });
    if changed {
        tray_host::set_geometry(TrayGeometry { bar, tray_area: area, edge });
        tray_host::set_icon_rects(&rects);
    }
}

/// true if the bar is visible.
pub fn is_visible() -> bool {
    VISIBLE.load(Relaxed)
}

/// Redraws the bar only if it is visible (the geometry is recomputed anyway on the next show).
fn redraw_if_visible() {
    if VISIBLE.load(Relaxed) {
        redraw();
    }
}

/// Redraws the bar (pipeline: scene -> layout -> geometry -> resize -> paint -> present).
pub fn redraw() {
    if GEOM.with(|g| g.get()).is_none() {
        return;
    }
    let clock = crate::clock::format_now();
    let cfg = config::get();
    let top = config_top();
    let (dpi, sizes) = UI.with(|u| {
        let st = u.borrow();
        (st.metrics.dpi, st.sizes)
    });
    let mi = match dock_monitor() {
        Ok(mi) => mi,
        Err(e) => {
            log!("redraw: {e}");
            return;
        }
    };
    let m = Metrics { dpi };
    let wa = mi.rcWork;
    let max_width = (wa.right - wa.left - 2 * m.px(render::EDGE_MARGIN)).max(0);
    let date = if cfg.show_date && !clock.date.is_empty() { Some(clock.date.as_str()) } else { None };
    let modules = render::visible_modules(&cfg);

    let now = now_ms();
    let attn: Vec<isize> = ATTN.with(|a| a.borrow().clone());
    let attn_on = attn_phase_on(now);
    // Scene, layout, resize and paint under a single short borrow (render does not re-enter the UI).
    let mut drag_lost = false;
    let layout = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let views: Vec<AppView<'_>> = st
            .entries
            .iter()
            .map(|e| AppView {
                name: &e.name,
                icon: e.icon.as_ref(),
                indicator: if e.windows.is_empty() {
                    Indicator::None
                } else if !attn.is_empty() && e.windows.iter().any(|w| attn.contains(w)) {
                    // Blink: orange bar in the "on" phase, nothing in the "off" phase.
                    if attn_on {
                        Indicator::Attention
                    } else {
                        Indicator::None
                    }
                } else if e.active {
                    Indicator::Active
                } else {
                    Indicator::Open
                },
                dimmed: e.windows.is_empty(),
                active_t: act_value(e, now),
            })
            .collect();
        let tray_icons: Vec<TrayIcon<'_>> =
            st.tray.iter().filter_map(|t| t.bitmap.as_deref().map(|b| TrayIcon { id: t.id, icon: b })).collect();
        let mut scene = Scene {
            metrics: m,
            sizes,
            apps: &views,
            tray: &tray_icons,
            overflow_enabled: popup::is_available(),
            force_chevron: st.tray_overflow, // chevron only if some icon is hidden
            bar_top: top,
            system: SystemView {
                volume: st.volume,
                show_volume_percent: cfg.show_volume_percent,
                time: &clock.time,
                date,
                bell_badge: st.notif_count > 0,
                bell_dnd: st.notif_dnd == Some(true),
                net: st.net,
                usage: st.usage,
            },
            plate: None,
            modules: &modules,
            start_button: cfg.show_start_button,
            lift: None,
            hover: st.hover.or_else(|| popup::current().and_then(item_for_popup)),
            max_width,
        };
        let mut layout = render::layout(&scene);
        // Width animation: the content snaps to the final layout right away, the pill
        // grows/shrinks centered. With the bar hidden (or on the first draw) there is no animation.
        let target_w = layout.width;
        let dw = if !VISIBLE.load(Relaxed) || st.shown_w <= 0 {
            st.wanim.running = false;
            target_w
        } else {
            let cur = if st.wanim.running { width_at(&st.wanim, now) } else { st.shown_w };
            if !(st.wanim.running && st.wanim.to == target_w) {
                if cur != target_w {
                    st.wanim = WidthAnim { from: cur, to: target_w, start: now, running: true };
                    set_timer(hwnd(), TIMER_WIDTH, ANIM_TICK_MS);
                } else {
                    st.wanim.running = false;
                }
            }
            if st.wanim.running {
                if now.saturating_sub(st.wanim.start) >= WIDTH_ANIM_MS {
                    st.wanim.running = false;
                    target_w
                } else {
                    width_at(&st.wanim, now)
                }
            } else {
                target_w
            }
        };
        st.shown_w = dw;
        // The window is as wide as the larger of pill and layout; the layout stays centered.
        let win_w = dw.max(target_w);
        if win_w != target_w {
            shift_layout(&mut layout, (win_w - target_w) / 2, win_w);
        }
        render::set_pill_width(if dw < win_w { dw } else { 0 });
        // Drag in progress: the zone being reordered gets its rectangles moved; if the items under the
        // drag changed (app closed, DPI change...) the drag is dropped.
        if let Some(d) = st.drag.as_mut() {
            if apply_drag(&mut layout, d, &st.entries, &st.tray, now) {
                scene.lift = Some(Lift { id: d.id, alpha: PRESS_ALPHA });
                scene.hover = Some(d.id);
            } else {
                drag_lost = true;
            }
        }
        if drag_lost {
            st.drag = None;
        }
        // Hover on an item that disappeared from the layout (or moved): the plate follows it.
        if let Some(id) = st.hover {
            if layout.rect_of(id).is_none() {
                st.hover = None;
                st.pressed = None;
            }
        }
        let target = plate_target(st.hover, st.pressed, &layout, m);
        retarget_plate(&mut st.plate, target, now);
        let (rect, alpha) = plate_now(&st.plate, now);
        if alpha > 0 {
            scene.plate = Some(Plate { rect, alpha });
        }
        if let Err(e) = render::resize(layout.width, layout.height) {
            log!("render::resize failed: {e}");
            return None;
        }
        render::paint(&scene, &layout);
        Some(layout)
    });
    if drag_lost {
        log!("drag: the items changed under the drag, cancelled");
        release_capture();
    }
    let Some(layout) = layout else { return };
    // Geometry: bar centered on the work area, width = layout.width.
    let mut g = match geom_from(&mi, top, layout.width, dpi, sizes) {
        Ok(g) => g,
        Err(e) => {
            log!("redraw: {e}");
            return;
        }
    };
    if g.w != layout.width {
        // Layout wider than the work area (degradation exhausted): the bar stays centered.
        g.x += (g.w - layout.width) / 2;
        g.w = layout.width;
    }
    GEOM.with(|c| c.set(Some(g)));
    // During the width animation the tray host is not updated on every frame: the final rect
    // arrives with the last frame (`running` = false).
    // Nor during a drag: the moved rectangles are not final (the drop redraws and syncs).
    if !UI.with(|u| u.borrow().wanim.running || u.borrow().drag.is_some()) {
        sync_tray_geometry(&layout, &g, top);
    }
    UI.with(|u| u.borrow_mut().layout = layout);
    // UpdateLayeredWindow sets position AND size: it also covers the width change.
    let (dy, alpha) = vis_offset(top);
    render::present_alpha(hwnd(), g.x, g.y + dy, alpha);
}

// ------------------------------------------------------------ show/hide animation

/// Vertical offset (px, towards the screen edge the bar sits on) and constant alpha for the
/// current animation level. Idle and visible = `(0, 255)`.
fn vis_offset(top: bool) -> (i32, u8) {
    let lvl = VIS.with(|v| vis_level(&v.get(), now_ms()));
    if lvl >= 1.0 {
        return (0, 255);
    }
    let dpi = UI.with(|u| u.borrow().metrics.dpi);
    let slide = Metrics { dpi }.px(SLIDE_PX) as f32;
    let dy = ((1.0 - lvl) * slide).round() as i32;
    (if top { -dy } else { dy }, (lvl * 255.0).round() as u8)
}

/// Starts (or reverses, from the current level) the slide+fade towards `to` (1 = shown, 0 = hidden).
fn start_vis_anim(to: f32, dur: u64) {
    let now = now_ms();
    let cur = VIS.with(|v| vis_level(&v.get(), now));
    VIS.with(|v| v.set(VisAnim { from: cur, to, start: now, dur, running: true }));
    set_timer(hwnd(), TIMER_VIS, ANIM_TICK_MS);
}

/// Stops the animation and jumps to the final level `to` (used for the instant hide).
fn stop_vis_anim(to: f32) {
    VIS.with(|v| v.set(VisAnim { from: to, to, start: 0, dur: 1, running: false }));
    kill_timer(hwnd(), TIMER_VIS);
}

/// Animation tick: re-presents the already painted DIB with the new offset/alpha (no repaint).
fn on_vis_tick(h: HWND) {
    let now = now_ms();
    let a = VIS.with(|v| v.get());
    let done = !a.running || now.saturating_sub(a.start) >= a.dur;
    if done {
        stop_vis_anim(a.to);
        if a.to <= 0.0 {
            // A popup linked to the hide (Win with the Start menu open) goes away on the same frame.
            if popup::is_linked() {
                popup::close("bar hidden (with Start)");
            }
            hide_window(h);
            return;
        }
    }
    let Some(g) = GEOM.with(|g| g.get()) else { return };
    let (dy, alpha) = vis_offset(config_top());
    render::present_alpha(h, g.x, g.y + dy, alpha);
    // Same level, same tick: a linked popup moves with the bar (released once the show settles).
    popup::follow_bar(dy, alpha, done);
}

// ------------------------------------------------------------ attention (FlashWindowEx)

/// true in the visible phase of the blink; steady (true) once the blinking time is over.
fn attn_phase_on(now: u64) -> bool {
    let el = now.saturating_sub(ATTN_START.load(Relaxed));
    el >= ATTN_BLINK_MS || (el / ATTN_HALF_MS).is_multiple_of(2)
}

/// Shell hook message: `what` = HSHELL_* code, `win` = window handle.
fn on_shell_hook(h: HWND, what: usize, win: isize) {
    match what {
        HSHELL_FLASH => {
            // SAFETY: no preconditions.
            if unsafe { GetForegroundWindow() }.0 as isize == win {
                return; // the active window needs no attention
            }
            let added = ATTN.with(|a| {
                let mut a = a.borrow_mut();
                if a.contains(&win) {
                    false
                } else {
                    a.push(win);
                    true
                }
            });
            if added {
                ATTN_START.store(now_ms(), Relaxed);
                set_timer(h, TIMER_ATTN, ATTN_HALF_MS as u32);
                redraw_if_visible();
            }
        }
        HSHELL_WINDOWACTIVATED | HSHELL_RUDEAPPACTIVATED => {
            let (removed, empty) = ATTN.with(|a| {
                let mut a = a.borrow_mut();
                let n = a.len();
                a.retain(|w| *w != win);
                (a.len() != n, a.is_empty())
            });
            if removed {
                if empty {
                    kill_timer(h, TIMER_ATTN);
                }
                redraw_if_visible();
            }
        }
        _ => {}
    }
}

/// Blink tick: drops destroyed windows, redraws, and stops once steady (or nothing flashes).
fn on_attn_tick(h: HWND) {
    let empty = ATTN.with(|a| {
        let mut a = a.borrow_mut();
        // SAFETY: IsWindow accepts any handle value.
        a.retain(|w| unsafe { IsWindow(Some(HWND(*w as *mut std::ffi::c_void))) }.as_bool());
        a.is_empty()
    });
    redraw_if_visible();
    if empty || now_ms().saturating_sub(ATTN_START.load(Relaxed)) >= ATTN_BLINK_MS {
        kill_timer(h, TIMER_ATTN);
    }
}

/// Destroys the UI resources (`render::destroy`; `main::cleanup` removes the apps WinEventHook).
pub fn shutdown() {
    kill_timer(hwnd(), TIMER_ANIM);
    kill_timer(hwnd(), TIMER_VIS);
    kill_timer(hwnd(), TIMER_ATTN);
    if SHELLHOOK_MSG.load(Relaxed) != 0 {
        // SAFETY: `hwnd()` is our window, registered in `attach`.
        let _ = unsafe { DeregisterShellHookWindow(hwnd()) };
    }
    render::destroy();
}

// ------------------------------------------------------------------ items

fn same_key(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Launch target from exe/AUMID: packages (AUMID with '!') via AppsFolder, the others via exe.
fn compute_launch(exe: &str, aumid: Option<&str>) -> Option<String> {
    if let Some(a) = aumid {
        if a.contains('!') {
            return Some(format!("shell:AppsFolder\\{a}"));
        }
    }
    if !exe.is_empty() {
        return Some(exe.to_string());
    }
    aumid.map(|a| format!("shell:AppsFolder\\{a}"))
}

/// Rebuilds `entries`: pins first (config order), then unpinned groups by `first_seen`.
/// Keeps name/icon/requests of already known items and sends requests for the new ones.
fn rebuild_entries() {
    let cfg = config::get();
    let groups = apps::groups();
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let old = std::mem::take(&mut st.entries);
        let mut used = vec![false; groups.len()];
        let mut out: Vec<Entry> = Vec::with_capacity(cfg.pinned.len() + groups.len());
        for p in &cfg.pinned {
            if out.iter().any(|e| same_key(&e.key, &p.key)) {
                continue;
            }
            let gi = groups.iter().enumerate().position(|(i, g)| !used[i] && same_key(&g.key, &p.key));
            let mut e = Entry {
                key: p.key.clone(),
                group_key: p.key.clone(),
                // The stored name is only a placeholder/fallback: the live resolver (and the `names`
                // overrides) replace it as soon as the shell thread answers.
                name: p.name.clone(),
                fixed_name: false,
                pinned: true,
                custom_icon: p.icon.clone(),
                ..Default::default()
            };
            let from_launch = apps::aumid_from_launch(&p.launch);
            if let Some(i) = gi {
                used[i] = true;
                let g = &groups[i];
                e.group_key = g.key.clone();
                e.exe_path = g.exe_path.clone();
                e.aumid = g.aumid.clone().or(from_launch);
                e.windows = g.windows.clone();
                e.active = g.active;
            } else {
                if from_launch.is_none() && !p.launch.to_ascii_lowercase().starts_with("shell:") {
                    e.exe_path = p.launch.clone();
                }
                e.aumid = from_launch;
            }
            e.launch = if p.launch.is_empty() {
                compute_launch(&e.exe_path, e.aumid.as_deref())
            } else {
                Some(p.launch.clone())
            };
            out.push(e);
        }
        // Unpinned apps: `app_order` first (its order), then the rest in first-seen order (stable sort).
        let mut rest: Vec<usize> = (0..groups.len()).filter(|i| !used[*i]).collect();
        if !cfg.app_order.is_empty() {
            rest.sort_by_key(|i| cfg.app_order.iter().position(|k| same_key(k, &groups[*i].key)).unwrap_or(usize::MAX));
        }
        for i in rest {
            let g = &groups[i];
            out.push(Entry {
                key: g.key.clone(),
                group_key: g.key.clone(),
                launch: compute_launch(&g.exe_path, g.aumid.as_deref()),
                exe_path: g.exe_path.clone(),
                aumid: g.aumid.clone(),
                windows: g.windows.clone(),
                active: g.active,
                ..Default::default()
            });
        }
        // Config reloaded live: if `names`, a pin's custom icon or fixed name change,
        // the item must be resolved again (meanwhile the old icon and name stay as placeholders).
        let names_changed = LAST_NAMES.with(|n| {
            let mut n = n.borrow_mut();
            let changed = n.as_ref().is_some_and(|prev| *prev != cfg.names);
            *n = Some(cfg.names.clone());
            changed
        });
        let now = now_ms();
        let animate = VISIBLE.load(Relaxed);
        let mut act_started = false;
        for e in &mut out {
            let target = if e.active && !e.windows.is_empty() { 1.0 } else { 0.0 };
            e.act_from = target;
            e.act_to = target;
            if let Some(o) = old.iter().find(|o| same_key(&o.key, &e.key)) {
                // Indicator level: keep the running animation, start one if the foreground changed.
                e.act_from = o.act_from;
                e.act_to = o.act_to;
                e.act_start = o.act_start;
                if o.act_to != target {
                    if animate {
                        e.act_from = act_value(o, now);
                        e.act_to = target;
                        e.act_start = now;
                        act_started = true;
                    } else {
                        e.act_from = target;
                        e.act_to = target;
                        e.act_start = 0;
                    }
                }
                // A fixed name removed from the config must not be kept: the shell thread's name replaces it.
                if !e.fixed_name && !o.fixed_name {
                    e.name = o.name.clone();
                }
                e.icon = o.icon.clone();
                let same_inputs = o.custom_icon == e.custom_icon && o.fixed_name == e.fixed_name;
                e.req_px = if same_inputs && !names_changed { o.req_px } else { 0 };
            }
        }
        // A pressed app follows its entry to the new index (or the press is dropped if it is gone), so
        // a release or a drag never hits the app that slid into the old index. A moved item also loses
        // its drag (the grab offset would be stale): the release stays a click.
        if let Some(ItemId::App(i)) = st.pressed {
            let ni = old.get(i).and_then(|o| out.iter().position(|e| same_key(&e.key, &o.key)));
            st.pressed = ni.map(ItemId::App);
            if ni != Some(i) {
                st.press_drag = false;
            }
        }
        st.entries = out;
        if act_started {
            set_timer(hwnd(), TIMER_ANIM, ANIM_TICK_MS);
        }
    });
    // An open window picker whose app gained or lost windows closes (no live re-layout).
    if let Some(k) = crate::picker::open_key() {
        let wins = UI.with(|u| u.borrow().entries.iter().find(|e| e.group_key == k).map(|e| e.windows.clone()));
        crate::picker::sync_windows(&k, wins.as_deref());
    }
    request_missing();
}

/// Sends `ResolveApp` for the items that have not yet received one at the current icon size.
/// If the shell thread has not started yet, retries from the 50 ms poll.
fn request_missing() {
    let ready = shell_thread::thread_id() != 0;
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let px = st.sizes.icon_px(st.metrics) as u32;
        let mut pending = false;
        for e in st.entries.iter_mut() {
            if e.req_px == px {
                continue;
            }
            if !ready {
                pending = true;
                continue;
            }
            e.req_px = px;
            shell_thread::request(ShellRequest::ResolveApp(ResolveApp {
                key: e.key.clone(),
                exe_path: e.exe_path.clone(),
                aumid: e.aumid.clone(),
                hwnd: e.windows.first().copied().unwrap_or(0),
                icon_px: px,
                custom_icon: e.custom_icon.clone(),
            }));
        }
        NEED_REQUESTS.with(|n| n.set(pending));
    });
}

/// Applies a result from the shell thread; true if something changed.
fn apply_shell_event(ev: ShellEvent) -> bool {
    // Network and Start menu data go to the popups OUTSIDE the `UI` borrow (they may relayout).
    let ev = match ev {
        ShellEvent::Network(info) => return apply_network(info),
        ShellEvent::StartApps(list) => {
            popup::on_start_apps(list);
            return false;
        }
        ShellEvent::StartIcons { icon_px, icons } => {
            popup::on_start_icons(icon_px, icons);
            return false;
        }
        // Spatial sound and per-app mixer data: only the audio popup uses them.
        ShellEvent::Spatial(state) => {
            popup::on_audio_spatial(state);
            return false;
        }
        ShellEvent::AudioSessions(list) => {
            popup::on_audio_sessions(list);
            return false;
        }
        other => other,
    };
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let px = st.sizes.icon_px(st.metrics) as u32;
        match ev {
            ShellEvent::AppName { key, name } => {
                let Some(e) = st.entries.iter_mut().find(|e| same_key(&e.key, &key)) else { return false };
                if e.fixed_name || e.name == name {
                    return false;
                }
                e.name = name;
                true
            }
            ShellEvent::AppIcon { key, icon_px, bitmap } => {
                if icon_px != px {
                    return false; // result from a previous DPI
                }
                let Some(e) = st.entries.iter_mut().find(|e| same_key(&e.key, &key)) else { return false };
                match bitmap {
                    Some(b) => {
                        e.icon = Some(b);
                        true
                    }
                    None => false,
                }
            }
            // the audio devices are only needed by the audio popup.
            ShellEvent::AudioOutputs(list) => {
                crate::popup::on_audio_outputs(list);
                false
            }
            // Handled before the borrow.
            ShellEvent::Network(_)
            | ShellEvent::StartApps(_)
            | ShellEvent::StartIcons { .. }
            | ShellEvent::Spatial(_)
            | ShellEvent::AudioSessions(_) => false,
        }
    })
}

/// New network status: stored for the bar glyph and forwarded to the network popup (outside the
/// `UI` borrow: the popup may relayout). true if the glyph changed. A late reply after the module
/// was switched off is ignored.
fn apply_network(info: crate::net::NetInfo) -> bool {
    if !NET_ACTIVE.with(|a| a.get()) {
        return false;
    }
    let status = info.status();
    let changed = UI.with(|u| std::mem::replace(&mut u.borrow_mut().net, status) != status);
    popup::on_network(info);
    changed
}

/// Re-enumerates the windows; if they changed, rebuilds the items and redraws.
fn refresh_apps() {
    if apps::refresh(hwnd()) {
        rebuild_entries();
        // Even with the bar hidden: the next appearance must already have the right width.
        redraw();
    }
}

/// New monitor DPI (or monitor change, or sizes change): metrics, fonts, icon requests, geometry.
fn apply_dpi(mon: u32) {
    let dpi = mon;
    let m = Metrics { dpi };
    let sizes = BarSizes::from_config(&config::get());
    let changed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        st.monitor_dpi = mon;
        if st.metrics == m && st.sizes == sizes {
            return false;
        }
        if sizes.icon_px(m) != st.sizes.icon_px(st.metrics) {
            for e in &mut st.entries {
                e.req_px = 0; // icons must be redone at the new size (the old one stays meanwhile)
            }
        }
        st.metrics = m;
        st.sizes = sizes;
        true
    });
    if changed {
        log!("DPI -> {dpi}, {sizes:?}");
        tray_host::set_icon_px((sizes.tray_px(m)).max(m.px(16)) as u32);
        if let Err(e) = render::set_metrics(m, sizes) {
            log!("render::set_metrics failed: {e}");
        }
        request_missing();
    }
    redraw();
    if VISIBLE.load(Relaxed) {
        if let Some(g) = GEOM.with(|g| g.get()) {
            show_topmost(hwnd(), &g);
        }
    }
}

/// `WM_SETTINGCHANGE` with "intl": the date/time formats changed.
fn on_setting_change(lp: LPARAM) {
    if lp.0 == 0 {
        return;
    }
    // SAFETY: for WM_SETTINGCHANGE a non-null lParam is a pointer to a NUL-terminated wide string,
    // valid for the duration of the message (or a value ignored by `to_string` if it is not text).
    let s = unsafe { PCWSTR(lp.0 as *const u16).to_string() }.unwrap_or_default();
    if s == "intl" {
        crate::clock::invalidate_formats();
        redraw_if_visible();
    }
}

// ------------------------------------------------------------ hover plate

fn lerp_i32(a: i32, b: i32, t: f32) -> i32 {
    (a as f32 + (b - a) as f32 * t).round() as i32
}

fn rect_eq(a: &RECT, b: &RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}

/// Current rectangle and alpha of the plate at instant `now` (out-cubic).
fn plate_now(p: &PlateAnim, now: u64) -> (RECT, u8) {
    if !p.running {
        return (p.to, p.to_alpha);
    }
    let t = (now.saturating_sub(p.start) as f32 / p.dur.max(1) as f32).clamp(0.0, 1.0);
    let e = 1.0 - (1.0 - t).powi(3);
    let rect = RECT {
        left: lerp_i32(p.from.left, p.to.left, e),
        top: lerp_i32(p.from.top, p.to.top, e),
        right: lerp_i32(p.from.right, p.to.right, e),
        bottom: lerp_i32(p.from.bottom, p.to.bottom, e),
    };
    (rect, lerp_i32(p.from_alpha as i32, p.to_alpha as i32, e).clamp(0, 255) as u8)
}

/// Where the plate should be: on the hovered item, with pressed alpha if it is also pressed; with no
/// hover, on the item of the open popup (persistent plate).
fn plate_target(hover: Option<ItemId>, pressed: Option<ItemId>, layout: &Layout, m: Metrics) -> Option<(RECT, u8)> {
    let id = hover.or_else(|| popup::current().and_then(item_for_popup))?;
    let r = render::plate_rect(layout.rect_of(id)?, m);
    Some((r, if pressed == Some(id) { PRESS_ALPHA } else { HOVER_ALPHA }))
}

/// New animation target: starts from the current position/alpha. `None` = fade
/// in place. Turns on `TIMER_ANIM` if animation is needed.
fn retarget_plate(p: &mut PlateAnim, target: Option<(RECT, u8)>, now: u64) {
    let (cur, cur_a) = plate_now(p, now);
    let (to, to_a) = target.unwrap_or((cur, 0));
    if p.to_alpha == to_a && (target.is_none() || rect_eq(&p.to, &to)) {
        return;
    }
    let (from, from_a) = if cur_a == 0 && to_a > 0 { (to, 0) } else { (cur, cur_a) };
    let dur = if to_a >= from_a { PLATE_IN_MS } else { PLATE_OUT_MS };
    *p = PlateAnim { from, to, start: now, from_alpha: from_a, to_alpha: to_a, dur, running: true };
    set_timer(hwnd(), TIMER_ANIM, ANIM_TICK_MS);
}

/// true while something driven by `TIMER_ANIM` is still moving: the hover plate, an indicator
/// width change, the items sliding in a drag or the drop settle.
fn anims_busy(st: &UiState, now: u64) -> bool {
    st.plate.running
        || st.entries.iter().any(|e| e.act_start != 0 && now.saturating_sub(e.act_start) < ACT_ANIM_MS)
        || st.drag.as_ref().is_some_and(|d| d.settle.is_some() || now < d.busy_until)
}

/// Animation timer tick: redraws; when nothing moves any more it stops the timer.
fn on_anim_tick(h: HWND) {
    let now = now_ms();
    UI.with(|u| {
        let p = &mut u.borrow_mut().plate;
        if p.running && now.saturating_sub(p.start) >= p.dur {
            p.running = false;
        }
    });
    redraw_if_visible();
    finish_drag(now_ms(), false);
    // The redraw may have re-armed an animation (`retarget_plate`): then the timer stays.
    if !UI.with(|u| anims_busy(&u.borrow(), now_ms())) {
        kill_timer(h, TIMER_ANIM);
    }
}

/// Width timer tick: redraws; at the end of the run it stops the timer.
fn on_width_tick(h: HWND) {
    redraw_if_visible();
    if !UI.with(|u| u.borrow().wanim.running) {
        kill_timer(h, TIMER_WIDTH);
    }
}

/// Resets hover/press (e.g. when the dock hides) and stops the animation.
fn reset_hover() {
    abort_drag();
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        st.hover = None;
        st.pressed = None;
        st.tracking_mouse = false;
        st.plate = PlateAnim::default();
    });
    kill_timer(hwnd(), TIMER_ANIM);
}

// ------------------------------------------------------------------ mouse

fn lo_i16(lp: LPARAM) -> i32 {
    (lp.0 & 0xFFFF) as u16 as i16 as i32
}

fn hi_i16(lp: LPARAM) -> i32 {
    ((lp.0 >> 16) & 0xFFFF) as u16 as i16 as i32
}

fn hit_at(x: i32, y: i32) -> Option<ItemId> {
    UI.with(|u| u.borrow().layout.hit_test(x, y))
}

/// Sets the hovered item and updates the plate target.
fn set_hover(hit: Option<ItemId>) {
    let changed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        if st.hover == hit {
            return false;
        }
        st.hover = hit;
        let t = plate_target(st.hover, st.pressed, &st.layout, st.metrics);
        retarget_plate(&mut st.plate, t, now_ms());
        true
    });
    if changed {
        picker_hover(hit);
    }
    // Entering a tray icon: a single hover notification to the app.
    if let (true, Some(ItemId::Tray(id))) = (changed, hit) {
        if let Some(p) = cursor_pos() {
            tray_host::hover(id, p);
        }
    }
}

// ------------------------------------------------------------------ window picker

/// App item `hit` with 2+ windows -> (index, group key, name); `None` otherwise.
fn multi_window_app(hit: Option<ItemId>) -> Option<(usize, String, String)> {
    let Some(ItemId::App(i)) = hit else { return None };
    UI.with(|u| {
        let st = u.borrow();
        st.entries.get(i).filter(|e| e.windows.len() >= 2).map(|e| (i, e.group_key.clone(), e.name.clone()))
    })
}

/// The hover changed to `hit`. On an app with 2+ windows (previews on) the picker's open delay is
/// (re)armed; back on the item of the open picker it stays open; on another item the picker closes
/// at once; off the bar it closes by itself unless the pointer reaches it (`picker::arm_leave`).
/// Anything else kills the delay: no timer runs unless the pointer rests on such an item.
fn picker_hover(hit: Option<ItemId>) {
    let target = if config::get().window_previews { multi_window_app(hit) } else { None };
    let open = crate::picker::open_key();
    if let (Some((_, k, _)), Some(o)) = (&target, &open) {
        if k == o {
            kill_timer(hwnd(), TIMER_HOVER_PICKER);
            crate::picker::cancel_leave();
            return;
        }
    }
    if open.is_some() {
        if hit.is_none() {
            crate::picker::arm_leave();
        } else {
            crate::picker::close("hover moved to another item");
        }
    }
    if target.is_some() && !popup::is_open() {
        set_timer(hwnd(), TIMER_HOVER_PICKER, PICKER_HOVER_MS);
    } else {
        kill_timer(hwnd(), TIMER_HOVER_PICKER);
    }
}

/// `TIMER_HOVER_PICKER` (one-shot): the pointer rested on an app with 2+ windows: open its picker.
fn on_hover_picker_timer(h: HWND) {
    kill_timer(h, TIMER_HOVER_PICKER);
    let (hover, busy) = UI.with(|u| {
        let st = u.borrow();
        (st.hover, st.pressed.is_some() || st.drag.is_some())
    });
    if busy || !VISIBLE.load(Relaxed) || popup::is_open() || !config::get().window_previews {
        return;
    }
    let Some((i, key, name)) = multi_window_app(hover) else { return };
    let Some(a) = anchor_for(ItemId::App(i)) else { return };
    let name = if name.is_empty() { crate::i18n::tr("Window", "Finestra").to_string() } else { name };
    crate::picker::open(a, &key, &name, &apps::windows_in_order(&key));
}

/// Kills the picker's open delay and closes the picker (click on the bar, dock hidden, previews off).
fn picker_cancel(reason: &str) {
    kill_timer(hwnd(), TIMER_HOVER_PICKER);
    crate::picker::close(reason);
}

fn on_mouse_move(h: HWND, wp: WPARAM, lp: LPARAM) {
    let need_track = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        !std::mem::replace(&mut st.tracking_mouse, true)
    });
    if need_track {
        let mut t = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: h,
            dwHoverTime: 0,
        };
        // SAFETY: `t` is initialized with the correct cbSize; `h` is our window.
        if let Err(e) = unsafe { TrackMouseEvent(&mut t) } {
            log!("TrackMouseEvent failed: {e}");
            UI.with(|u| u.borrow_mut().tracking_mouse = false);
        }
    }
    let (x, y) = (lo_i16(lp), hi_i16(lp));
    // (drag in progress, a pressed draggable item moved past the threshold with the button still down)
    let (dragging, pending) = UI.with(|u| {
        let st = &*u.borrow();
        let moved = (x - st.press_x).abs() > st.metrics.px(DRAG_THRESHOLD);
        (st.drag.is_some(), st.pressed.filter(|_| st.press_drag && moved && wp.0 & 1 != 0))
    });
    if dragging {
        drag_move(x);
        return;
    }
    if let Some(id) = pending {
        if begin_drag(id) {
            drag_move(x);
            return;
        }
        UI.with(|u| u.borrow_mut().press_drag = false); // nothing to reorder: stays a normal click
    }
    set_hover(hit_at(x, y));
}

fn on_mouse_leave() {
    let pressed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        st.tracking_mouse = false;
        st.pressed.is_some() || st.drag.is_some()
    });
    if !pressed {
        set_hover(None);
    }
}

/// `WM_SETCURSOR`: hand over clickable items, otherwise default (None).
fn on_set_cursor(lp: LPARAM) -> Option<LRESULT> {
    if (lp.0 & 0xFFFF) as u32 != HTCLIENT {
        return None;
    }
    let g = GEOM.with(|g| g.get())?;
    let p = cursor_pos()?;
    hit_at(p.x - g.x, p.y - g.y)?;
    // SAFETY: loads a shared system cursor (null hInstance); no pointers to manage.
    let cur = unsafe { LoadCursorW(None, IDC_HAND) }.ok()?;
    // SAFETY: `cur` is a valid shared cursor.
    unsafe { SetCursor(Some(cur)) };
    Some(LRESULT(1))
}

fn on_lbutton_down(h: HWND, lp: LPARAM) {
    // A drop still settling is committed right away.
    finish_drag(now_ms(), true);
    let hit = hit_at(lo_i16(lp), hi_i16(lp));
    picker_cancel("click on the bar");
    // An open popup closes right away on a click on the bar outside its own item (the bar is
    // NOACTIVATE: the popup does not receive WA_INACTIVE). On its own item `toggle` handles it on release.
    if popup::is_open() {
        let own = popup::current().and_then(item_for_popup);
        if own.is_none() || hit != own {
            popup::close("click on the bar");
        }
    }
    UI.with(|u| {
        let st = &mut *u.borrow_mut();
        st.pressed = hit;
        st.press_x = lo_i16(lp);
        st.press_drag = hit.is_some_and(|id| zone_of(id).is_some());
        let t = plate_target(st.hover, st.pressed, &st.layout, st.metrics);
        retarget_plate(&mut st.plate, t, now_ms());
    });
    if hit.is_some() {
        // SAFETY: `h` is our window; the capture is needed to receive the release even outside the bar.
        unsafe { SetCapture(h) };
    }
}

fn on_lbutton_up(lp: LPARAM) {
    if drag_active() {
        end_drag(false);
        return;
    }
    // SAFETY: no preconditions; fails harmlessly if we do not have the capture.
    let _ = unsafe { ReleaseCapture() };
    if !VISIBLE.load(Relaxed) {
        // The dock hid during the click (capture active): the release must not open anything.
        UI.with(|u| u.borrow_mut().pressed = None);
        return;
    }
    let hit = hit_at(lo_i16(lp), hi_i16(lp));
    let pressed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let pressed = st.pressed.take();
        st.hover = hit;
        let t = plate_target(st.hover, st.pressed, &st.layout, st.metrics);
        retarget_plate(&mut st.plate, t, now_ms());
        pressed
    });
    if let (Some(p), Some(h)) = (pressed, hit) {
        if p == h {
            click_left(h);
        }
    }
}

fn on_mbutton_up(lp: LPARAM) {
    picker_cancel("middle click on the bar");
    match hit_at(lo_i16(lp), hi_i16(lp)) {
        Some(ItemId::App(i)) => {
            if let Some(e) = entry_at(i) {
                match launch_target(&e) {
                    Some(t) => shell_thread::request(ShellRequest::Launch { target: t, args: None }),
                    None => log!("middle click: no launch target for '{}'", e.key),
                }
            }
        }
        Some(ItemId::Volume) => shell_thread::request(ShellRequest::VolumeToggleMute),
        Some(ItemId::Tray(id)) => tray_click(id, TrayClick::Middle),
        _ => {}
    }
}

fn on_rbutton_up(lp: LPARAM) {
    picker_cancel("right click on the bar");
    match hit_at(lo_i16(lp), hi_i16(lp)) {
        Some(ItemId::App(i)) => {
            if let Some(e) = entry_at(i) {
                show_app_menu(&e, ItemId::App(i));
            }
        }
        Some(ItemId::Volume) => launch_target_str("ms-settings:apps-volume"),
        Some(ItemId::Network) => launch_target_str("ms-settings:network"),
        Some(ItemId::Start) => toggle_popup(PopupKind::Start, ItemId::Start),
        Some(ItemId::Tray(id)) => tray_click(id, TrayClick::Right),
        Some(ItemId::Notifications)
        | Some(ItemId::Clock)
        | Some(ItemId::TrayOverflow)
        | Some(ItemId::ShowDesktop)
        | Some(ItemId::Power)
        | Some(ItemId::Metrics)
        | None => {}
    }
}

/// Forwards a click to the app owning tray icon `id` (first closes any open popup).
fn tray_click(id: u64, kind: TrayClick) {
    popup::close("tray click");
    let p = cursor_pos().unwrap_or_default();
    tray_host::click(id, kind, p);
}

/// Left click on a tray icon with manual double-click detection (same icon, within
/// `GetDoubleClickTime` and `SM_CXDOUBLECLK/SM_CYDOUBLECLK`).
fn tray_left_click(id: u64) {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXDOUBLECLK, SM_CYDOUBLECLK};
    let p = cursor_pos().unwrap_or_default();
    let now = now_ms();
    // SAFETY: functions without preconditions.
    let (dt, dx, dy) =
        unsafe { (GetDoubleClickTime(), GetSystemMetrics(SM_CXDOUBLECLK), GetSystemMetrics(SM_CYDOUBLECLK)) };
    let double = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let is_double = matches!(
            st.last_tray_click,
            Some((lid, lt, lp)) if lid == id
                && now.saturating_sub(lt) <= dt as u64
                && (p.x - lp.x).abs() <= dx / 2
                && (p.y - lp.y).abs() <= dy / 2
        );
        // After a double click the next one starts from scratch (like Windows).
        st.last_tray_click = if is_double { None } else { Some((id, now, p)) };
        is_double
    });
    popup::close("tray click");
    tray_host::click(id, if double { TrayClick::Double } else { TrayClick::Left }, p);
}

fn on_mouse_wheel(wp: WPARAM, lp: LPARAM) {
    let Some(g) = GEOM.with(|g| g.get()) else { return };
    // The wheel carries SCREEN coordinates (not client).
    let hit = hit_at(lo_i16(lp) - g.x, hi_i16(lp) - g.y);
    if hit != Some(ItemId::Volume) {
        return;
    }
    let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    // High-resolution wheels/touchpads send fractions of a notch: they accumulate up to 120
    // (WHEEL_DELTA). A change of direction resets the remainder.
    let notches = WHEEL_ACC.with(|a| {
        let mut acc = a.get();
        if acc.signum() * delta.signum() < 0 {
            acc = 0;
        }
        acc += delta;
        let n = acc / 120;
        a.set(acc - n * 120);
        n
    });
    if notches != 0 {
        shell_thread::request(ShellRequest::VolumeAdjust { delta_percent: notches * crate::audio::WHEEL_STEP_PERCENT });
    }
}

// ------------------------------------------------------------ drag & drop reordering

/// The three independent reorder zones: items never cross from one to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Zone {
    Apps,
    Tray,
    Modules,
}

/// Zone of a draggable item (`None` = not draggable, e.g. the overflow chevron).
fn zone_of(id: ItemId) -> Option<Zone> {
    match id {
        ItemId::App(_) => Some(Zone::Apps),
        ItemId::Tray(_) => Some(Zone::Tray),
        // The Start button is fixed at the left end of the bar.
        ItemId::TrayOverflow | ItemId::Start => None,
        ItemId::Notifications
        | ItemId::Volume
        | ItemId::Clock
        | ItemId::ShowDesktop
        | ItemId::Power
        | ItemId::Network
        | ItemId::Metrics => Some(Zone::Modules),
    }
}

/// Horizontal slide (out-cubic) of an item, in px relative to the start of the zone.
#[derive(Clone, Copy, Debug, Default)]
struct SlideAnim {
    from: i32,
    to: i32,
    start: u64,
}

fn slide_at(a: &SlideAnim, now: u64) -> i32 {
    let t = (now.saturating_sub(a.start) as f32 / SLIDE_MS as f32).clamp(0.0, 1.0);
    lerp_i32(a.from, a.to, 1.0 - (1.0 - t).powi(3))
}

/// Drop animation of the dragged item; `commit` = the order changed and is persisted at its end.
#[derive(Clone, Copy, Debug)]
struct Settle {
    anim: SlideAnim,
    commit: bool,
}

/// State of a reorder. The items of the zone being reordered are `layout.items[first..first + n]`
/// (base order); all positions are relative to `x0`, the left edge of the zone.
struct DragState {
    id: ItemId,
    zone: Zone,
    /// App zone: the pinned and the unpinned entries are reordered separately.
    pinned: bool,
    first: usize,
    from: usize,
    slot: usize,
    ids: Vec<ItemId>,
    /// Config key of every item (app key, tray key, module name), to validate and to persist.
    keys: Vec<String>,
    widths: Vec<i32>,
    anims: Vec<SlideAnim>,
    x0: i32,
    total: i32,
    /// Cursor x minus the left edge of the dragged item.
    grab: i32,
    /// Left edge of the dragged item while it follows the cursor.
    cur_rel: i32,
    /// Where the dragged item would land (left edge of `slot`).
    slot_rel: i32,
    settle: Option<Settle>,
    /// Tick until which the sliding items still move.
    busy_until: u64,
}

fn drag_active() -> bool {
    UI.with(|u| u.borrow().drag.is_some())
}

fn release_capture() {
    // SAFETY: no preconditions; fails harmlessly if we do not have the capture. Never call it while
    // `UI` is borrowed: it synchronously sends WM_CAPTURECHANGED.
    let _ = unsafe { ReleaseCapture() };
}

/// Index (in base order) of the item that sits at position `p` when the dragged item `from` is in `slot`.
fn slot_item(p: usize, from: usize, slot: usize) -> usize {
    if p == slot {
        return from;
    }
    let o = if p < slot { p } else { p - 1 };
    if o < from {
        o
    } else {
        o + 1
    }
}

/// Moves the drop slot: the other items slide to their new places, `slot_rel` is where the dragged one lands.
fn retarget_slots(d: &mut DragState, slot: usize, now: u64) {
    d.slot = slot;
    let mut x = 0;
    for p in 0..d.widths.len() {
        let k = slot_item(p, d.from, slot);
        if k == d.from {
            d.slot_rel = x;
        } else {
            let cur = slide_at(&d.anims[k], now);
            d.anims[k] = SlideAnim { from: cur, to: x, start: now };
        }
        x += d.widths[k];
    }
    d.busy_until = now + SLIDE_MS;
}

/// Left edge (relative to the zone) of the dragged item at `now`.
fn dragged_rel(d: &DragState, now: u64) -> i32 {
    match &d.settle {
        Some(s) => slide_at(&s.anim, now),
        None => d.cur_rel,
    }
}

fn key_matches(id: ItemId, key: &str, entries: &[Entry], tray: &[TrayView]) -> bool {
    match id {
        ItemId::App(i) => entries.get(i).is_some_and(|e| e.key == key),
        ItemId::Tray(t) => tray.iter().any(|v| v.id == t && v.key == key),
        other => render::module_name(other) == Some(key),
    }
}

fn key_of(st: &UiState, id: ItemId) -> Option<String> {
    match id {
        ItemId::App(i) => st.entries.get(i).map(|e| e.key.clone()),
        ItemId::Tray(t) => st.tray.iter().find(|v| v.id == t).map(|v| v.key.clone()),
        other => render::module_name(other).map(str::to_string),
    }
}

/// Moves the rectangles of the zone in a freshly computed (base) layout to the drag positions.
/// false (layout untouched) if the items are no longer the ones the drag started with.
fn apply_drag(layout: &mut Layout, d: &mut DragState, entries: &[Entry], tray: &[TrayView], now: u64) -> bool {
    let n = d.ids.len();
    // The zone is found by its first item: changes in earlier zones (an app opening while a tray icon
    // is dragged) only shift its index.
    let Some(first) = layout.items.iter().position(|i| i.id == d.ids[0]) else { return false };
    d.first = first;
    let Some(slice) = layout.items.get_mut(first..first + n) else { return false };
    for (k, it) in slice.iter().enumerate() {
        let same = it.id == d.ids[k] && it.rect.right - it.rect.left == d.widths[k];
        if !same || !key_matches(it.id, &d.keys[k], entries, tray) {
            return false;
        }
    }
    let x0 = slice[0].rect.left;
    d.x0 = x0;
    for (k, it) in slice.iter_mut().enumerate() {
        let rel = if k == d.from { dragged_rel(d, now) } else { slide_at(&d.anims[k], now) };
        it.rect.left = x0 + rel;
        it.rect.right = x0 + rel + d.widths[k];
    }
    true
}

/// Starts a reorder of `id` (the pressed item). false if its zone has nothing to reorder.
fn begin_drag(id: ItemId) -> bool {
    let ok = UI
        .with(|u| {
            let st = &mut *u.borrow_mut();
            let zone = zone_of(id)?;
            let pinned = match id {
                ItemId::App(i) => st.entries.get(i)?.pinned,
                _ => false,
            };
            let items = &st.layout.items;
            let idx = items.iter().position(|i| i.id == id)?;
            let same = |it: &ItemRect| {
                zone_of(it.id) == Some(zone)
                    && match it.id {
                        ItemId::App(i) => st.entries.get(i).map(|e| e.pinned) == Some(pinned),
                        _ => true,
                    }
            };
            let mut first = idx;
            while first > 0 && same(&items[first - 1]) {
                first -= 1;
            }
            let mut end = idx + 1;
            while end < items.len() && same(&items[end]) {
                end += 1;
            }
            let n = end - first;
            if n < 2 {
                return None;
            }
            let x0 = items[first].rect.left;
            let (mut ids, mut keys, mut widths, mut anims) =
                (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
            let mut rel = 0;
            for it in &items[first..end] {
                let w = it.rect.right - it.rect.left;
                ids.push(it.id);
                keys.push(key_of(st, it.id)?);
                widths.push(w);
                anims.push(SlideAnim { from: rel, to: rel, start: 0 });
                rel += w;
            }
            let from = idx - first;
            let left = items[idx].rect.left;
            let drag = DragState {
                id,
                zone,
                pinned,
                first,
                from,
                slot: from,
                ids,
                keys,
                total: rel,
                slot_rel: left - x0,
                anims,
                widths,
                x0,
                grab: st.press_x - left,
                cur_rel: left - x0,
                settle: None,
                busy_until: 0,
            };
            st.drag = Some(drag);
            // The hover plate gives way to the lifted item's own plate.
            st.hover = None;
            st.pressed = None;
            st.plate = PlateAnim::default();
            Some(())
        })
        .is_some();
    if ok {
        popup::close("drag");
    }
    ok
}

/// The cursor moved (client x): the dragged item follows it, clamped to its zone; the drop slot is
/// the number of other items whose centre is left of the dragged item's centre.
fn drag_move(x: i32) {
    let now = now_ms();
    let slid = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let Some(d) = st.drag.as_mut() else { return false };
        if d.settle.is_some() {
            return false;
        }
        d.cur_rel = (x - d.grab - d.x0).clamp(0, (d.total - d.widths[d.from]).max(0));
        let centre = d.cur_rel + d.widths[d.from] / 2;
        let (mut slot, mut pos) = (0, 0);
        for (k, w) in d.widths.iter().enumerate() {
            if k != d.from && pos + w / 2 < centre {
                slot += 1;
            }
            pos += w;
        }
        if slot == d.slot {
            return false;
        }
        retarget_slots(d, slot, now);
        true
    });
    if slid {
        set_timer(hwnd(), TIMER_ANIM, ANIM_TICK_MS);
    }
    redraw_if_visible();
}

/// Drop (`cancel == false`) or cancel (Esc, capture lost): the dragged item glides to its slot and the order is
/// persisted when the glide ends (`finish_drag`). A cancel animates everything back.
fn end_drag(cancel: bool) {
    let now = now_ms();
    let armed = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let Some(d) = st.drag.as_mut() else { return false };
        if d.settle.is_some() {
            return false;
        }
        if cancel && d.slot != d.from {
            let from = d.from;
            retarget_slots(d, from, now);
        }
        let commit = !cancel && d.slot != d.from;
        d.settle = Some(Settle { anim: SlideAnim { from: d.cur_rel, to: d.slot_rel, start: now }, commit });
        d.busy_until = d.busy_until.max(now + SLIDE_MS);
        true
    });
    // After `settle` is set: the WM_CAPTURECHANGED sent by the release is ignored.
    release_capture();
    if armed {
        set_timer(hwnd(), TIMER_ANIM, ANIM_TICK_MS);
        redraw_if_visible();
    }
}

/// WM_CAPTURECHANGED: another window took the mouse capture mid-drag -> cancel.
fn on_capture_lost() {
    end_drag(true);
}

/// Drops a drag immediately (dock hidden...). A drag still lifted is discarded; a drop already
/// settling with a new order is persisted, as if its glide had ended.
fn abort_drag() {
    let Some(d) = UI.with(|u| u.borrow_mut().drag.take()) else { return };
    release_capture();
    if d.settle.is_some_and(|s| s.commit) {
        commit_drag(&d);
    }
}

/// Ends the drag once its settle animation is over (or at once if `force`): persists the new order.
fn finish_drag(now: u64, force: bool) {
    let done = UI.with(|u| {
        let st = &mut *u.borrow_mut();
        let due = st.drag.as_ref().and_then(|d| d.settle).is_some_and(|s| force || now >= s.anim.start + SLIDE_MS);
        if due {
            st.drag.take()
        } else {
            None
        }
    });
    let Some(d) = done else { return };
    if d.settle.is_some_and(|s| s.commit) {
        commit_drag(&d);
    }
    if let (Some(g), Some(p)) = (GEOM.with(|g| g.get()), cursor_pos()) {
        set_hover(hit_at(p.x - g.x, p.y - g.y));
    }
    redraw_if_visible();
}

/// `new` (the reordered keys) first, then the keys of `old` that are not in it, at most `cap`.
fn merge_order(new: &[String], old: &[String], cap: usize) -> Vec<String> {
    let mut out: Vec<String> = new.to_vec();
    out.extend(old.iter().filter(|k| !new.iter().any(|n| same_key(n, k))).cloned());
    out.truncate(cap);
    out
}

/// Rewrites the `pinned` entries named in `keys` so they appear in that order, in the same slots.
fn reorder_pinned(c: &mut config::Config, keys: &[String]) {
    let slots: Vec<usize> =
        c.pinned.iter().enumerate().filter(|(_, p)| keys.iter().any(|k| same_key(k, &p.key))).map(|(i, _)| i).collect();
    let moved: Vec<PinnedApp> =
        keys.iter().filter_map(|k| c.pinned.iter().find(|p| same_key(&p.key, k)).cloned()).collect();
    if slots.len() == moved.len() {
        for (slot, p) in slots.into_iter().zip(moved) {
            c.pinned[slot] = p;
        }
    }
}

/// Full module order after a drag of the VISIBLE modules: the hidden ones keep their slots, the
/// visible slots take `visible` in its new order.
fn reorder_visible_modules(old: &[String], visible: &[String]) -> Vec<String> {
    let mut next = visible.iter();
    render::modules_from_names(old)
        .into_iter()
        .filter_map(render::module_name)
        .map(|n| {
            if visible.iter().any(|v| v == n) {
                next.next().cloned().unwrap_or_else(|| n.to_string())
            } else {
                n.to_string()
            }
        })
        .collect()
}

/// Persists the new order of the dragged zone (atomic config rewrite) and rebuilds the items.
fn commit_drag(d: &DragState) {
    let mut keys = d.keys.clone();
    let moved = keys.remove(d.from);
    keys.insert(d.slot, moved);
    let r = match d.zone {
        Zone::Apps if d.pinned => config::modify(|c| reorder_pinned(c, &keys)),
        Zone::Apps => config::modify(|c| c.app_order = merge_order(&keys, &c.app_order, config::APP_ORDER_CAP)),
        Zone::Tray => config::modify(|c| c.tray_order = merge_order(&keys, &c.tray_order, config::TRAY_ORDER_CAP)),
        Zone::Modules => config::modify(|c| c.modules_order = reorder_visible_modules(&c.modules_order, &keys)),
    };
    match r {
        Ok(()) => on_config_changed(),
        Err(e) => log!("drag: saving the new order failed: {e}"),
    }
}

// ------------------------------------------------------------------ click

fn entry_at(i: usize) -> Option<Entry> {
    UI.with(|u| u.borrow().entries.get(i).cloned())
}

fn launch_target(e: &Entry) -> Option<String> {
    match &e.launch {
        Some(l) if !l.is_empty() => Some(l.clone()),
        _ => compute_launch(&e.exe_path, e.aumid.as_deref()),
    }
}

pub(crate) fn launch_target_str(target: &str) {
    shell_thread::request(ShellRequest::Launch { target: target.to_string(), args: None });
}

fn click_left(id: ItemId) {
    match id {
        ItemId::App(i) => {
            let Some(e) = entry_at(i) else { return };
            match apps::click_group(&e.group_key) {
                ClickOutcome::NoWindows => match launch_target(&e) {
                    Some(t) => shell_thread::request(ShellRequest::Launch { target: t, args: None }),
                    None => log!("click: no launch target for '{}'", e.key),
                },
                ClickOutcome::Activated(_) | ClickOutcome::Minimized(_) => {}
            }
        }
        // Without a popup (class not registered) fall back to the direct actions.
        ItemId::Notifications => {
            if popup::is_available() {
                toggle_popup(PopupKind::Notifications, id);
            } else {
                send_win_n();
            }
        }
        ItemId::Volume => {
            if popup::is_available() {
                toggle_popup(PopupKind::Audio, id);
            } else {
                shell_thread::request(ShellRequest::VolumeToggleMute);
            }
        }
        ItemId::Clock => {
            if popup::is_available() {
                toggle_popup(PopupKind::Calendar, id);
            } else {
                launch_target_str("ms-settings:dateandtime");
            }
        }
        ItemId::Power => {
            if popup::is_available() {
                toggle_popup(PopupKind::Power, id);
            } else {
                log!("Power: popup not available");
            }
        }
        ItemId::TrayOverflow => toggle_popup(PopupKind::TrayOverflow, id),
        ItemId::Tray(tid) => tray_left_click(tid),
        ItemId::ShowDesktop => {
            popup::close("show desktop");
            shell_thread::request(ShellRequest::ToggleDesktop);
        }
        // Left click opens the dock's own Start menu (`popup_startmenu`), like the Win double tap;
        // without a popup (class not registered) it falls back to the Windows Start menu.
        ItemId::Start => {
            if popup::is_available() {
                toggle_popup(PopupKind::StartMenu, id);
            } else {
                shell_thread::request(ShellRequest::OpenStart);
            }
        }
        ItemId::Network => {
            if popup::is_available() {
                toggle_popup(PopupKind::Network, id);
            } else {
                launch_target_str("ms-settings:network");
            }
        }
        ItemId::Metrics => {
            popup::close("task manager");
            launch_target_str("taskmgr.exe");
        }
    }
}

/// Win+N (notification center) with `dwExtraInfo = INJECT_TAG`: the keyboard hook lets it through.
/// `pub` because the notifications popup also uses it ("Open notification center").
pub fn send_win_n() {
    send_win_key(VK_N);
}

/// Click on a notification card (`popup_notif`). Windows offers no way to activate another app's
/// toast, so the app that sent it is brought up instead: its most recent window if it has one (the
/// group with the same AUMID, or a bar item / exe with the same name), otherwise the shell thread
/// launches it from the AppsFolder catalog (`ShellRequest::OpenNotifApp`, Win+N as the last resort).
pub fn open_notification_app(aumid: &str, app: &str) {
    let groups = apps::groups();
    let alive = |g: &&apps::AppGroup| !g.windows.is_empty();
    let by_aumid = (!aumid.is_empty())
        .then(|| {
            groups.iter().filter(alive).find(|g| g.aumid.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(aumid)))
        })
        .flatten();
    let by_name = || {
        if app.is_empty() {
            return None;
        }
        let key = UI.with(|u| {
            u.borrow()
                .entries
                .iter()
                .find(|e| !e.windows.is_empty() && e.name.eq_ignore_ascii_case(app))
                .map(|e| e.group_key.clone())
        });
        key.and_then(|k| groups.iter().filter(alive).find(|g| g.key == k)).or_else(|| {
            groups.iter().filter(alive).find(|g| {
                Path::new(&g.exe_path).file_stem().is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(app))
            })
        })
    };
    if let Some(g) = by_aumid.or_else(by_name) {
        log!("notification: activating '{}' ({} windows)", g.key, g.windows.len());
        apps::activate_window(g.windows[0]);
        return;
    }
    shell_thread::request(ShellRequest::OpenNotifApp { aumid: aumid.to_string(), name: app.to_string() });
}

/// Win + `vk` pressed and released with `dwExtraInfo = INJECT_TAG` (Win+N, Win+S, Win+R ...).
pub fn send_win_key(vk: VIRTUAL_KEY) {
    let ev = |vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: hook::INJECT_TAG },
        },
    };
    let inputs = [
        ev(VK_LWIN, KEYEVENTF_EXTENDEDKEY),
        ev(vk, KEYBD_EVENT_FLAGS(0)),
        ev(vk, KEYEVENTF_KEYUP),
        ev(VK_LWIN, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP),
    ];
    // SAFETY: slice of 4 initialized INPUTs; cbSize = size_of::<INPUT>() as required.
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if n != inputs.len() as u32 {
        log!("SendInput(Win+N): {n}/{} events accepted", inputs.len());
    }
}

// ------------------------------------------------------------ context menu

/// Opens the themed context menu of app item `i` (anchored to the item, above the bar or below if
/// the bar is at the top). The choice comes back as `WM_DOCK_APP_MENU_CMD` (see `on_app_menu_cmd`).
fn show_app_menu(e: &Entry, id: ItemId) {
    let Some(anchor) = anchor_for(id) else {
        log!("app menu: item {id:?} not in the layout");
        return;
    };
    let name = if e.name.is_empty() { e.key.clone() } else { e.name.clone() };
    popup::open_app_menu(
        anchor,
        AppMenuInfo {
            key: e.key.clone(),
            name,
            pinned: e.pinned,
            windows: e.windows.len(),
            can_launch: launch_target(e).is_some(),
        },
    );
}

/// `WM_DOCK_APP_MENU_CMD`: the popup closed the menu with choice `cmd` (`ID_MENU_*`).
fn on_app_menu_cmd(cmd: u32) {
    let Some(key) = popup::take_app_menu_key() else {
        log!("app menu: command {cmd} without key");
        return;
    };
    let e = UI.with(|u| u.borrow().entries.iter().find(|e| same_key(&e.key, &key)).cloned());
    let Some(e) = e else {
        log!("app menu: item '{key}' no longer present");
        return;
    };
    run_menu_command(cmd, e);
}

fn run_menu_command(cmd: u32, e: Entry) {
    match cmd {
        ID_MENU_PIN => {
            let Some(launch) = launch_target(&e) else {
                log!("Pin: no launch target for '{}'", e.key);
                return;
            };
            // Name not arrived yet: pin without a fixed name, the shell thread resolves it.
            let name = e.name.clone();
            match config::pin(PinnedApp { key: e.key.clone(), name, launch, icon: None }) {
                Ok(()) => on_config_changed(),
                Err(err) => log!("Pin failed: {err}"),
            }
        }
        ID_MENU_UNPIN => match config::unpin(&e.key) {
            Ok(()) => on_config_changed(),
            Err(err) => log!("Unpin failed: {err}"),
        },
        ID_MENU_CLOSE_ALL => apps::close_group(&e.group_key),
        ID_MENU_NEW_WINDOW => match launch_target(&e) {
            Some(t) => shell_thread::request(ShellRequest::Launch { target: t, args: None }),
            None => log!("New window: no launch target for '{}'", e.key),
        },
        _ => {}
    }
}

// ------------------------------------------------------------------ geometry

fn config_top() -> bool {
    config::effective_position() == config::Position::Top
}

/// A display the bar can sit on (Settings -> Screen, `config.monitor`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screen {
    /// GDI device name, e.g. "\\.\DISPLAY2" (the value stored in `config.monitor`).
    pub device: String,
    /// Short label: the display number Windows puts in the device name ("2"), or its position.
    pub label: &'static str,
    pub primary: bool,
}

/// Displays offered by the Settings popup (more are ignored: the segments would not fit).
const MAX_SCREENS: usize = 6;
const SCREEN_LABELS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// Every connected display with its `MONITORINFOEXW`, in enumeration order.
fn enum_monitors() -> Vec<(HMONITOR, MONITORINFOEXW)> {
    unsafe extern "system" fn add(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        // SAFETY: `lp` is the `&mut Vec` passed below, alive for the whole (synchronous) enumeration.
        let out = unsafe { &mut *(lp.0 as *mut Vec<(HMONITOR, MONITORINFOEXW)>) };
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // SAFETY: `mi` is a writable MONITORINFOEXW whose cbSize says so.
        if unsafe { GetMonitorInfoW(h, &mut mi.monitorInfo) }.as_bool() {
            out.push((h, mi));
        }
        BOOL(1)
    }
    let mut out: Vec<(HMONITOR, MONITORINFOEXW)> = Vec::new();
    // SAFETY: the callback only touches `out` through `lp`, and only during this call.
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(add), LPARAM(&mut out as *mut _ as isize)) };
    out
}

fn device_name(mi: &MONITORINFOEXW) -> String {
    let n = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len());
    String::from_utf16_lossy(&mi.szDevice[..n])
}

/// Label of the display named `device` ("\\.\DISPLAY2" -> "2"); `idx` (0-based) if it has no number.
fn screen_label(device: &str, idx: usize) -> &'static str {
    let digits: String =
        device.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    let n = digits.parse::<usize>().ok().filter(|n| (1..=SCREEN_LABELS.len()).contains(n)).unwrap_or(idx + 1);
    SCREEN_LABELS[(n - 1).min(SCREEN_LABELS.len() - 1)]
}

/// Connected displays: the primary first, then the others by device name. At most `MAX_SCREENS`.
pub fn screens() -> Vec<Screen> {
    let mut v: Vec<(bool, String)> = enum_monitors()
        .iter()
        .map(|(_, mi)| (mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0, device_name(mi)))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    v.truncate(MAX_SCREENS);
    v.into_iter()
        .enumerate()
        .map(|(i, (primary, device))| Screen { label: screen_label(&device, i), device, primary })
        .collect()
}

/// The display the bar belongs on: `config.monitor` if it is connected, otherwise the primary one.
fn dock_hmonitor() -> HMONITOR {
    let want = config::get().monitor.clone();
    if want.eq_ignore_ascii_case(config::MONITOR_POINTER) {
        return followed_hmonitor();
    }
    let found = if want.is_empty() {
        None
    } else {
        enum_monitors().into_iter().find(|(_, mi)| device_name(mi).eq_ignore_ascii_case(&want)).map(|(h, _)| h)
    };
    // SAFETY: MonitorFromPoint with MONITOR_DEFAULTTOPRIMARY always returns a monitor.
    let h = found.unwrap_or_else(|| unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) });
    // Logged only when the display actually changes (chosen, unplugged, plugged back).
    let prev = LAST_HMON.with(|c| c.replace(h.0 as isize));
    if prev != h.0 as isize {
        if found.is_none() && !want.is_empty() {
            log!("monitor: \"{want}\" not connected, the bar uses the primary monitor");
        } else {
            log!("monitor: the bar uses {}", if want.is_empty() { "the primary monitor" } else { want.as_str() });
        }
    }
    h
}

/// "pointer" mode: the display the Win key last brought the bar to, as long as it is connected;
/// otherwise (first use, display unplugged) the display under the pointer, which becomes the new one.
fn followed_hmonitor() -> HMONITOR {
    let cur = FOLLOW_HMON.with(|c| c.get());
    let mons = enum_monitors();
    let h = match mons.iter().find(|(m, _)| cur != 0 && m.0 as isize == cur) {
        Some((m, _)) => *m,
        None => {
            let m = pointer_hmonitor();
            FOLLOW_HMON.with(|c| c.set(m.0 as isize));
            m
        }
    };
    // Logged only when the display actually changes.
    let prev = LAST_HMON.with(|c| c.replace(h.0 as isize));
    if prev != h.0 as isize {
        let name = mons.iter().find(|(m, _)| *m == h).map(|(_, mi)| device_name(mi)).unwrap_or_default();
        log!("monitor: the bar follows the pointer, now on {name}");
    }
    h
}

/// Effective DPI of the bar's display (96 if the call fails).
fn dock_dpi() -> u32 {
    let mon = dock_hmonitor();
    let (mut dx, mut dy) = (96u32, 96u32);
    // SAFETY: `dx`/`dy` are valid, writable u32s for the duration of the call.
    match unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) } {
        Ok(()) if dx >= 96 => dx,
        _ => 96,
    }
}

fn dock_monitor() -> Result<MONITORINFO, String> {
    monitor_info(dock_hmonitor()).ok_or_else(|| "GetMonitorInfoW failed".into())
}

/// Monitor and work area rects of display `h` (`None` if the call fails).
pub(crate) fn monitor_info(h: HMONITOR) -> Option<MONITORINFO> {
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `mi` has cbSize initialized and is writable.
    unsafe { GetMonitorInfoW(h, &mut mi) }.as_bool().then_some(mi)
}

/// Geometry: bar of `bar_w` physical px, horizontally centered on the work area of the bar's
/// display, `render::bar_height` tall, `EDGE_MARGIN` logical from the edge (bottom or top).
fn compute_geom(top: bool, bar_w: i32, dpi: u32, sizes: BarSizes) -> Result<Geom, String> {
    geom_from(&dock_monitor()?, top, bar_w, dpi, sizes)
}

fn geom_from(mi: &MONITORINFO, top: bool, bar_w: i32, dpi: u32, sizes: BarSizes) -> Result<Geom, String> {
    let m = Metrics { dpi };
    let wa = mi.rcWork;
    let h = render::bar_height(m, sizes);
    let margin = m.px(render::EDGE_MARGIN);
    let w = bar_w.min(wa.right - wa.left - 2 * margin);
    if w < 100 {
        return Err(format!("work area too narrow ({w} px)"));
    }
    let x = wa.left + (wa.right - wa.left - w) / 2;
    let y = if top { wa.top + margin } else { wa.bottom - margin - h };
    Ok(Geom { x, y, w, h, mon: mi.rcMonitor })
}

// ------------------------------------------------------- small unsafe wrappers

fn cursor_pos() -> Option<POINT> {
    let mut p = POINT::default();
    // SAFETY: `p` is a valid, writable POINT for the duration of the call.
    unsafe { GetCursorPos(&mut p) }.ok()?;
    Some(p)
}

fn set_timer(h: HWND, id: usize, ms: u32) {
    use windows::Win32::UI::WindowsAndMessaging::SetTimer;
    // SAFETY: `h` is our window; without a callback the WM_TIMERs arrive at the wnd_proc.
    if unsafe { SetTimer(Some(h), id, ms, None) } == 0 {
        log!("SetTimer {id} failed");
    }
}

/// Shows the window at the top of the topmost band, at the dock's position.
fn show_topmost(h: HWND, g: &Geom) {
    // SAFETY: `h` is our window (alive until cleanup); no pointers passed.
    if let Err(e) = unsafe { SetWindowPos(h, Some(HWND_TOPMOST), g.x, g.y, g.w, g.h, SWP_SHOWWINDOW | SWP_NOACTIVATE) }
    {
        log!("SetWindowPos(HWND_TOPMOST, show) failed: {e}");
    }
}

/// Re-asserts only the topmost Z-order, without moving or resizing.
fn reassert_topmost(h: HWND) {
    // SAFETY: as above.
    if let Err(e) = unsafe { SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE) }
    {
        log!("SetWindowPos(HWND_TOPMOST) failed: {e}");
    }
}

fn kill_timer(h: HWND, id: usize) {
    // SAFETY: our id/window; fails harmlessly if the timer does not exist.
    let _ = unsafe { KillTimer(Some(h), id) };
}

fn hide_window(h: HWND) {
    // SAFETY: `h` is our window; ShowWindow uses no pointers.
    let _ = unsafe { ShowWindow(h, SW_HIDE) };
}

// ------------------------------------------------------- show / hide

/// Shows the dock above everything. Topmost is RE-ASSERTED on every show with
/// `SetWindowPos(HWND_TOPMOST, SWP_SHOWWINDOW | SWP_NOACTIVATE)`, which places the window at the top
/// of the topmost band even if the shell had lowered it. If a fullscreen / borderless window is in
/// the foreground (or with `--foreground`) it also tries to bring itself to the foreground
/// (`force_foreground`) and remembers the app to give the foreground back when it hides.
pub fn show_dock(reason: &str, hide_after_ms: u64) {
    if GEOM.with(|g| g.get()).is_none() {
        return;
    }
    let was_visible = VISIBLE.load(Relaxed);
    if !was_visible {
        // Before the redraw, so the first present is already at alpha 0 / slid out.
        start_vis_anim(1.0, SHOW_ANIM_MS);
    }
    // List from the cache: no EnumWindows in the hook path.
    redraw();
    // The geometry is reread AFTER the redraw: the width may have changed.
    let Some(g) = GEOM.with(|g| g.get()) else { return };
    // The edge must not re-trigger right after a hide: it re-arms when the mouse leaves it.
    EDGE_SINCE.store(0, Relaxed);
    EDGE_ARMED.store(false, Relaxed);
    // The foreground must be read BEFORE showing/activating the dock (afterwards it would be the dock itself).
    let fs = if was_visible { None } else { fullscreen_window(&g.mon) };
    let (dy, _) = vis_offset(config_top());
    show_topmost(hwnd(), &Geom { y: g.y + dy, ..g });
    HIDE_AT.store(now_ms() + hide_after_ms, Relaxed);
    VISIBLE.store(true, Relaxed);
    if !was_visible {
        // The clock and sysmon timers are off while hidden: redraw happened above, re-arm them now.
        arm_clock(hwnd());
        sysmon_sync();
    }
    if hide_after_ms >= u64::MAX / 4 {
        log!("SHOW ({reason}), autohide off");
    } else {
        log!("SHOW ({reason}), hiding in {hide_after_ms} ms");
    }
    if !was_visible {
        FS_SHOWN.store(fs.is_some(), Relaxed);
        FS_PREV.store(fs.map(|h| h.0 as isize).unwrap_or(0), Relaxed);
        FS_RETAKES.store(0, Relaxed);
        CLICK_DOWN.store(mouse_buttons_down(), Relaxed);
        if let Some(h) = fs {
            log!("  fullscreen foreground window: \"{}\"", window_title(h));
        }
    }
    let args = crate::args();
    if args.foreground || (FS_SHOWN.load(Relaxed) && !args.no_foreground) {
        force_foreground(hwnd());
    }
}

/// Hides the dock. If it was over a fullscreen app and the foreground is still ours (timeout,
/// Win key), it gives the foreground back to that app.
pub fn hide_dock(reason: &str) {
    hide(reason, false);
}

/// `hide_dock` without the slide+fade out (`now`), or with it unless over a fullscreen app.
fn hide(reason: &str, now: bool) {
    // Hiding over a fullscreen app is instant; otherwise slide+fade out.
    let fs = FS_SHOWN.swap(false, Relaxed);
    let instant = now || fs;
    let prev = if fs { FS_PREV.swap(0, Relaxed) } else { 0 };
    // A popup linked to the bar (`start_with_bar`) fades out with it and closes in `on_vis_tick`.
    if instant || !crate::popup::is_linked() {
        crate::popup::close("dock hidden");
    }
    picker_cancel("dock hidden");
    if instant {
        stop_vis_anim(0.0);
        hide_window(hwnd());
    }
    VISIBLE.store(false, Relaxed);
    reset_hover();
    kill_timer(hwnd(), TIMER_CLOCK);
    sysmon_sync();
    let was_animating = UI.with(|u| std::mem::replace(&mut u.borrow_mut().wanim.running, false));
    kill_timer(hwnd(), TIMER_WIDTH);
    if was_animating {
        // With the bar hidden the redraw skips the animation: the tray host receives the final geometry.
        redraw();
    }
    if !instant {
        start_vis_anim(0.0, HIDE_ANIM_MS);
    }
    log!("HIDE ({reason})");
    if prev != 0 {
        restore_foreground(HWND(prev as *mut std::ffi::c_void));
    }
}

/// (Re)arms the one-shot clock timer, aligned to the next minute/second: only while the bar is
/// visible and the clock module is shown (otherwise it is killed).
fn arm_clock(h: HWND) {
    if VISIBLE.load(Relaxed) && config::get().show_clock {
        set_timer(h, TIMER_CLOCK, crate::clock::tick_ms());
    } else {
        kill_timer(h, TIMER_CLOCK);
    }
}

// ------------------------------------------------------- system monitor (network, metrics)

/// `TIMER_SYSMON` period: 2 s with metrics on; with only the network on every tick is a network poll,
/// so the timer runs at the poll period instead of waking up for nothing.
fn sysmon_period(metrics: bool) -> u32 {
    if metrics {
        SYSMON_MS
    } else {
        SYSMON_MS * NET_POLL_TICKS
    }
}

/// Matches `TIMER_SYSMON` and the module state to the config and to the bar's visibility. Called on
/// show, hide and config change. ZERO COST WHEN OFF: a disabled module drops its state at once
/// (CPU sampler; the shell thread's `net::Querier` via `NetworkRelease`, which unloads its libraries
/// and COM object), and with both modules off or the bar hidden the timer does not exist.
fn sysmon_sync() {
    let cfg = config::get();
    let (net_on, met_on) = (cfg.show_network, cfg.show_metrics);
    if !met_on {
        UI.with(|u| {
            let st = &mut *u.borrow_mut();
            st.sampler = None;
            st.usage = crate::metrics::Usage::default();
        });
    }
    if !net_on && NET_ACTIVE.with(|a| a.replace(false)) {
        shell_thread::request(ShellRequest::NetworkRelease);
        UI.with(|u| u.borrow_mut().net = crate::net::NetStatus::default());
        popup::on_network(crate::net::NetInfo::default());
    }
    let want = VISIBLE.load(Relaxed) && (net_on || met_on);
    let armed = SYSMON_ARMED.with(|a| a.get());
    if want && !armed {
        SYSMON_ARMED.with(|a| a.set(true));
        SYSMON_TICKS.with(|t| t.set(0));
        set_timer(hwnd(), TIMER_SYSMON, sysmon_period(met_on));
        SYSMON_PERIOD_METRICS.with(|p| p.set(met_on));
        // First sample / query right away: the bar just appeared.
        sysmon_tick();
    } else if want && armed && SYSMON_PERIOD_METRICS.with(|p| p.replace(met_on)) != met_on {
        // Metrics switched: 2 s for the sampler, the network period alone otherwise.
        SYSMON_TICKS.with(|t| t.set(0));
        set_timer(hwnd(), TIMER_SYSMON, sysmon_period(met_on));
    } else if !want && armed {
        SYSMON_ARMED.with(|a| a.set(false));
        kill_timer(hwnd(), TIMER_SYSMON);
    }
}

/// `TIMER_SYSMON`: CPU/RAM sample (metrics on), network query every `NET_POLL_TICKS` ticks (network
/// on; the reply arrives as `ShellEvent::Network`). Redraws only if the metrics changed.
fn sysmon_tick() {
    let cfg = config::get();
    let n = SYSMON_TICKS.with(|t| {
        let n = t.get();
        t.set(n.wrapping_add(1));
        n
    });
    let mut changed = false;
    if cfg.show_metrics {
        changed = UI.with(|u| {
            let st = &mut *u.borrow_mut();
            let usage = match st.sampler.as_mut() {
                Some(s) => s.sample(),
                None => {
                    // Primed now: the CPU figure starts with the next tick.
                    let s = crate::metrics::Sampler::new();
                    let u = s.last();
                    st.sampler = Some(s);
                    u
                }
            };
            std::mem::replace(&mut st.usage, usage) != usage
        });
    }
    if cfg.show_network && (!cfg.show_metrics || n.is_multiple_of(NET_POLL_TICKS)) {
        NET_ACTIVE.with(|a| a.set(true));
        shell_thread::request(ShellRequest::NetworkQuery);
    }
    if changed {
        redraw_if_visible();
    }
}

/// Gives the foreground back to `prev` only if it now belongs to nobody else (null or ours).
fn restore_foreground(prev: HWND) {
    // SAFETY: no preconditions.
    let fg = unsafe { GetForegroundWindow() };
    if fg == prev || (!fg.0.is_null() && !is_own_window(fg)) {
        return;
    }
    // SAFETY: `prev` is a previously saved HWND; IsWindow accepts any value.
    if !unsafe { IsWindow(Some(prev)) }.as_bool() {
        return;
    }
    // SAFETY: SetForegroundWindow is allowed for us: until now we are the foreground.
    let ok = unsafe { SetForegroundWindow(prev) };
    log!("  foreground returned to the fullscreen app -> {}", ok.as_bool());
}

/// true if `h` belongs to our process (dock, popups).
fn is_own_window(h: HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: `pid` is writable; `h` can be any HWND (fails with pid 0).
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    // SAFETY: no preconditions.
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// true if the left or right mouse button is pressed right now.
fn mouse_buttons_down() -> bool {
    // SAFETY: no preconditions; the high bit means "pressed".
    unsafe {
        (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 | GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16) & 0x8000 != 0
    }
}

/// true if a mouse button was pressed since the last `GetAsyncKeyState` read.
/// The low bit is cleared by the read: call it after `mouse_buttons_down` and on every poll.
fn mouse_buttons_tapped() -> bool {
    // SAFETY: no preconditions; the low bit means "pressed since the last call".
    unsafe { (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 | GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16) & 0x1 != 0 }
}

fn window_title(h: HWND) -> String {
    let mut buf = [0u16; 64];
    // SAFETY: `buf` is a writable buffer. InternalGetWindowText reads the title
    // without sending WM_GETTEXT to the window (a hung app does not stall the hook).
    let n = unsafe { InternalGetWindowText(h, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// Foreground window covering the whole monitor `mon` (exclusive fullscreen or borderless),
/// if any. Excludes desktop (Progman/WorkerW), shell, minimized/invisible windows, our own and
/// windows with a title bar (normal maximized ones).
fn fullscreen_window(mon: &RECT) -> Option<HWND> {
    // SAFETY: no preconditions.
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() || is_own_window(fg) {
        return None;
    }
    // SAFETY: `fg` is an HWND obtained from Windows; the queries accept arbitrary HWNDs.
    if !unsafe { IsWindowVisible(fg) }.as_bool() || unsafe { IsIconic(fg) }.as_bool() {
        return None;
    }
    let mut cls = [0u16; 64];
    // SAFETY: `cls` is a writable buffer.
    let n = unsafe { GetClassNameW(fg, &mut cls) }.max(0) as usize;
    let class = String::from_utf16_lossy(&cls[..n.min(cls.len())]);
    if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
        return None;
    }
    // SAFETY: style read; valid HWND or the call returns 0.
    let style = unsafe { GetWindowLongPtrW(fg, GWL_STYLE) } as u32;
    if style & WS_CAPTION.0 == WS_CAPTION.0 {
        return None; // normal window (even maximized): not fullscreen
    }
    // SAFETY: IsZoomed accepts any HWND. Maximized windows without a title bar
    // (custom frame) are not fullscreen/borderless: those are not "zoomed".
    if unsafe { IsZoomed(fg) }.as_bool() {
        return None;
    }
    let mut r = RECT::default();
    // SAFETY: `r` is a writable RECT of the declared size.
    let dwm = unsafe {
        DwmGetWindowAttribute(
            fg,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    // SAFETY: `r` is valid and writable.
    if dwm.is_err() && unsafe { GetWindowRect(fg, &mut r) }.is_err() {
        return None;
    }
    // The window's monitor must be the dock's, and the window must cover it entirely.
    // SAFETY: MonitorFromWindow with NEAREST always returns a monitor.
    let hm = unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) };
    if !monitor_info(hm).is_some_and(|mi| rect_eq(&mi.rcMonitor, mon)) {
        return None;
    }
    let full = r.left <= mon.left && r.top <= mon.top && r.right >= mon.right && r.bottom >= mon.bottom;
    full.then_some(fg)
}

/// `EVENT_SYSTEM_FOREGROUND` (forwarded by `WM_DOCK_APPS_DIRTY`): if the dock is over a fullscreen
/// app and the foreground moves to another window that is not ours (the user went back to the game),
/// the dock hides immediately. A fullscreen app taking the foreground while the dock is
/// visible hides it too.
fn on_foreground_changed(fg: HWND) {
    if !VISIBLE.load(Relaxed) || fg.0.is_null() || is_own_window(fg) || tray_host::foreign_menu_active() {
        return;
    }
    // The event is posted: by now the foreground may already be ours again (stale event).
    // SAFETY: no preconditions.
    let cur = unsafe { GetForegroundWindow() };
    if !cur.0.is_null() && is_own_window(cur) {
        log!("foreground event ignored: the foreground is ours again");
        return;
    }
    let Some(g) = GEOM.with(|g| g.get()) else { return };
    if FS_SHOWN.load(Relaxed) {
        // Closing one of our popups gives the foreground back to the game: it is not a user click.
        // A real click on the game is detected by `on_poll` anyway.
        if fg.0 as isize == FS_PREV.load(Relaxed) && popup::closed_within(POPUP_CLOSE_GRACE_MS) {
            log!("foreground returned to the game by closing a popup: the dock stays");
            return;
        }
        // The game bounced the foreground back right after we took it: take it again (a few times).
        if fs_bounce_grace(fg) && FS_RETAKES.load(Relaxed) < FS_MAX_RETAKES {
            let n = FS_RETAKES.fetch_add(1, Relaxed) + 1;
            log!("fullscreen app bounced the foreground back right after the dock took it: re-taking it ({n})");
            force_foreground(popup::current_hwnd().unwrap_or_else(hwnd));
            return;
        }
        hide_dock("foreground moved to another window (fullscreen)");
    } else if fullscreen_window(&g.mon).is_some() {
        hide_dock("a fullscreen app took the foreground");
    }
}

/// true if `fg` is the fullscreen app under the dock and the dock/popup took the foreground less
/// than `FS_BOUNCE_GRACE_MS` ago: the change is the app bouncing it back, not a user action.
/// `pub` because `popup` must not close on that bounce either.
pub fn fs_bounce_grace(fg: HWND) -> bool {
    FS_SHOWN.load(Relaxed)
        && fg.0 as isize == FS_PREV.load(Relaxed)
        && within_grace(now_ms(), FG_TAKEN_AT.load(Relaxed), FS_BOUNCE_GRACE_MS)
}

/// true if `taken_at` (0 = never) is less than `grace` ms before `now`.
fn within_grace(now: u64, taken_at: u64, grace: u64) -> bool {
    taken_at != 0 && now.saturating_sub(taken_at) < grace
}

/// "Foreground" fallback (the foreground fallback of the original design).
/// Variant 1: AttachThreadInput to the foreground thread + SetForegroundWindow.
/// Variant 2 (only if 1 failed): a synthetic F15 key (releases the foreground
/// lock) and SetForegroundWindow again. The outcome of each is in the log.
/// `pub` because `popup` also uses it to activate the popup window.
pub fn force_foreground(h: HWND) {
    FG_TAKEN_AT.store(now_ms(), Relaxed);
    // SAFETY: no preconditions.
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return;
    }
    // SAFETY: `fg` is an HWND obtained from Windows; the out pid is optional.
    let fg_tid = unsafe { GetWindowThreadProcessId(fg, None) };
    // SAFETY: no preconditions.
    let my_tid = unsafe { GetCurrentThreadId() };
    let attached = fg_tid != 0 && fg_tid != my_tid;
    if attached {
        // SAFETY: valid thread ids; the attach is always undone below.
        let _ = unsafe { AttachThreadInput(my_tid, fg_tid, true) };
    }
    // SAFETY: `h` is our window.
    let ok = unsafe {
        let _ = BringWindowToTop(h);
        SetForegroundWindow(h)
    };
    if attached {
        // SAFETY: undoes the attach done above.
        let _ = unsafe { AttachThreadInput(my_tid, fg_tid, false) };
    }
    log!("  foreground variant 1 (AttachThreadInput): SetForegroundWindow -> {}", ok.as_bool());
    if ok.as_bool() {
        return;
    }
    let sent = send_f15();
    // SAFETY: `h` is our window.
    let ok2 = unsafe { SetForegroundWindow(h) };
    log!("  foreground variant 2 (synthetic F15, SendInput={sent}): SetForegroundWindow -> {}", ok2.as_bool());
}

/// Presses and releases F15 (a key no app uses): unlocks SetForegroundWindow.
/// Returns the number of events accepted by SendInput.
fn send_f15() -> u32 {
    let ev = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_F15, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    };
    let inputs = [ev(KEYBD_EVENT_FLAGS(0)), ev(KEYEVENTF_KEYUP)];
    // SAFETY: slice of 2 initialized INPUTs; cbSize = size_of::<INPUT>() as required.
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) }
}

// ------------------------------------------------------------ poll timer

/// With several monitors: the edge `y == mon.top` (or `mon.bottom - 1`) is a real screen border
/// only if there is no other monitor right beyond it (otherwise the cursor crosses it freely).
fn edge_is_screen_border(mon: &RECT, x: i32) -> bool {
    let beyond = if config_top() { POINT { x, y: mon.top - 1 } } else { POINT { x, y: mon.bottom } };
    // SAFETY: MonitorFromPoint with MONITOR_DEFAULTTONULL has no preconditions; returns NULL if outside.
    let other = unsafe { MonitorFromPoint(beyond, MONITOR_DEFAULTTONULL) };
    other.0.is_null()
}

fn point_in(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// 50 ms timer: auto-hide, screen edge, topmost re-assertion.
/// With `!config::effective_autohide()` the bar never hides by itself; nor does it
/// while the context menu (popup) is open.
fn on_poll() {
    // Esc cancels a drag (the bar never has the keyboard focus, so the key state is polled).
    // SAFETY: GetAsyncKeyState has no preconditions; the high bit means "pressed".
    if drag_active() && unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } as u16 & 0x8000 != 0 {
        end_drag(true);
    }
    let Some(g) = GEOM.with(|g| g.get()) else { return };
    let Some(p) = cursor_pos() else { return };
    let now = now_ms();
    let autohide = crate::config::effective_autohide();
    if VISIBLE.load(Relaxed) {
        let bar = RECT { left: g.x, top: g.y, right: g.x + g.w, bottom: g.y + g.h };
        // Dock open over a fullscreen app: a click outside the bar and our popups
        // (windows of our process) returns to the app and hides the dock immediately.
        let down = mouse_buttons_down();
        let was_down = CLICK_DOWN.swap(down, Relaxed);
        // A click shorter than the poll period only leaves the "pressed since the last call" bit.
        let tapped = mouse_buttons_tapped();
        if FS_SHOWN.load(Relaxed)
            && ((down && !was_down) || tapped)
            && !point_in(&bar, p)
            && !tray_host::foreign_menu_active()
            && !drag_active()
        {
            // SAFETY: WindowFromPoint has no preconditions; returns NULL if there is no window.
            let under = unsafe { WindowFromPoint(p) };
            if under.0.is_null() || !is_own_window(under) {
                popup::close("click outside the dock (fullscreen)");
                hide_dock("click outside the dock over a fullscreen app");
                return;
            }
        }
        if point_in(&bar, p)
            || crate::popup::is_open()
            || crate::picker::is_open()
            || !autohide
            || tray_host::foreign_menu_active()
            || drag_active()
        {
            HIDE_AT.store(now + SHOW_MS, Relaxed);
        } else if now >= HIDE_AT.load(Relaxed) {
            hide_dock("timeout");
        }
        // Re-asserts topmost about every 500 ms while visible.
        let n = POLL_TICKS.with(|t| {
            t.set(t.get().wrapping_add(1));
            t.get()
        });
        if n.is_multiple_of(10) && VISIBLE.load(Relaxed) && !tray_host::foreign_menu_active() {
            reassert_topmost(hwnd());
        }
    }
    // Screen edge: y==top (bar at the top) or y==bottom-1 (bar at the bottom), on the bar's display.
    // "pointer" mode: on the display under the pointer, which the bar then moves to (even while still
    // visible on another display).
    let follow = follows_pointer();
    let mon = if follow { monitor_info(pointer_hmonitor()).map_or(g.mon, |mi| mi.rcMonitor) } else { g.mon };
    let other_display = !rect_eq(&mon, &g.mon);
    let edge_y = if config_top() { mon.top } else { mon.bottom - 1 };
    let at_edge = p.y == edge_y && p.x >= mon.left && p.x < mon.right && edge_is_screen_border(&mon, p.x);
    if !at_edge {
        EDGE_SINCE.store(0, Relaxed);
        EDGE_ARMED.store(true, Relaxed); // re-arms only after leaving the edge
    } else if (!VISIBLE.load(Relaxed) || other_display) && EDGE_ARMED.load(Relaxed) {
        let since = EDGE_SINCE.load(Relaxed);
        if since == 0 {
            EDGE_SINCE.store(now, Relaxed);
        } else if now - since >= EDGE_MS {
            EDGE_SINCE.store(0, Relaxed);
            EDGE_ARMED.store(false, Relaxed);
            // On fullscreen / borderless apps the edge does NOT open the dock: only the Win key.
            if fullscreen_window(&mon).is_none() {
                if other_display {
                    follow_pointer();
                }
                show_dock("screen edge", SHOW_MS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_items_follow_the_drop_slot() {
        let order = |from, slot| (0..4).map(|p| slot_item(p, from, slot)).collect::<Vec<_>>();
        assert_eq!(order(1, 3), [0, 2, 3, 1]);
        assert_eq!(order(1, 0), [1, 0, 2, 3]);
        assert_eq!(order(2, 2), [0, 1, 2, 3]);
        assert_eq!(order(0, 3), [1, 2, 3, 0]);
    }

    #[test]
    fn hidden_modules_keep_their_slots() {
        let old: Vec<String> = ["notifications", "volume", "clock", "show_desktop", "power"].map(String::from).to_vec();
        // Old 5-module config: network / metrics come back next to their default neighbours.
        let full = render::modules_from_names(&old);
        assert_eq!(full.len(), 7);
        assert_eq!(full[0], ItemId::Metrics);
        assert_eq!(full[2], ItemId::Network);
        // A "start" written by an earlier version is ignored: the Start button is not a module any more.
        let with_start: Vec<String> = std::iter::once("start".to_string()).chain(old.iter().cloned()).collect();
        assert_eq!(render::modules_from_names(&with_start), full);
        // Drag among the visible ones (metrics hidden): power moved first.
        let vis: Vec<String> =
            ["power", "notifications", "network", "volume", "clock", "show_desktop"].map(String::from).to_vec();
        let out = reorder_visible_modules(&old, &vis);
        assert_eq!(out, ["metrics", "power", "notifications", "network", "volume", "clock", "show_desktop"]);
    }

    #[test]
    fn merge_order_keeps_unseen_keys_and_caps() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(merge_order(&s(&["b", "a"]), &s(&["a", "z", "b"]), 64), s(&["b", "a", "z"]));
        assert_eq!(merge_order(&s(&["b", "a"]), &s(&["z"]), 2), s(&["b", "a"]));
    }

    #[test]
    fn reorder_pinned_keeps_other_slots() {
        let p = |k: &str| PinnedApp { key: k.into(), ..Default::default() };
        let mut c = config::Config { pinned: vec![p("a"), p("x"), p("b"), p("c")], ..Default::default() };
        reorder_pinned(&mut c, &["c".to_string(), "a".to_string(), "b".to_string()]);
        let keys: Vec<&str> = c.pinned.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, ["c", "x", "a", "b"]);
    }

    #[test]
    fn bounce_grace_window_edges() {
        assert!(!within_grace(1000, 0, 800), "never taken");
        assert!(within_grace(1000, 1000, 800));
        assert!(within_grace(1799, 1000, 800));
        assert!(!within_grace(1800, 1000, 800));
        assert!(within_grace(900, 1000, 800), "clock before the take saturates to 0");
    }
}
