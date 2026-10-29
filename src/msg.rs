//! `WM_APP + N` messages, timer ids and menu command ids shared between modules.
//!
//! Rule: every message in this list is addressed to the dock window (UI thread)
//! and is posted with `PostMessageW` (never `SendMessage` from other threads: it would block).
//! This is the ONLY place where numbers are assigned; whoever adds a message puts it here.

use windows::Win32::UI::WindowsAndMessaging::WM_APP;

// ------------------------------------------------- messages to the dock window

/// hook -> UI: the Win key was pressed and released on its own, toggle the dock.
/// wParam = 0, lParam = 0. Posted by the `hook` thread.
pub const WM_DOCK_TOGGLE: u32 = WM_APP + 1;
/// hook -> UI, log only. wParam = `hook::EV_*` (PRESSED/ALONE/COMBO/SENDFAIL), lParam = 0.
pub const WM_DOCK_WINKEY: u32 = WM_APP + 2;
/// hook -> UI, diagnostics (`--diag`). wParam = bitfield described in `hook::diag`,
/// lParam = HWND of the foreground window (as isize).
pub const WM_DOCK_DIAG: u32 = WM_APP + 3;
/// apps (WinEvent callback, run on the UI thread) -> UI: the window list may have
/// changed. wParam = WinEvent event code (EVENT_*), lParam = HWND (isize).
/// The UI thread starts (or restarts) the `TIMER_APPS_DEBOUNCE` debounce timer (100 ms).
pub const WM_DOCK_APPS_DIRTY: u32 = WM_APP + 4;
/// taskbar (WinEvent callback, UI thread) -> UI: a native taskbar-class window was
/// shown/created. wParam = event code, lParam = HWND (isize). The UI calls
/// `taskbar::on_winevent(event, hwnd)`.
pub const WM_DOCK_TASKBAR_EVENT: u32 = WM_APP + 5;
/// shell thread -> UI: there are new `ShellEvent`s in the queue. wParam = 0, lParam = 0.
/// The UI calls `shell_thread::drain_events()` (non-blocking) and applies them.
/// Consecutive posts may be coalesced: ALWAYS drain the whole queue.
pub const WM_DOCK_SHELL_READY: u32 = WM_APP + 6;
/// audio (COM callback, arbitrary thread) / shell thread -> UI: the volume or mute state changed.
/// wParam = master level 0..=100 (rounded); lParam = 1 if muted, 0 otherwise,
/// -1 = no render device available (wParam ignored).
pub const WM_DOCK_VOLUME: u32 = WM_APP + 7;
/// watchdog (`stop-watch` thread) -> UI: another `--restore` instance asked to quit.
/// wParam = 0, lParam = 0. The UI performs the same clean exit as Ctrl+Alt+Q.
pub const WM_DOCK_QUIT_REQUEST: u32 = WM_APP + 8;
/// tray host (`tray-host` thread) -> UI: the list of tray icons changed.
/// wParam = 1 only a bitmap changed (redraw), 2 the number/order of visible icons changed (redo the
/// layout). lParam = 0. `main` calls `ui::on_tray_changed(wParam)` and `popup::on_tray_changed()`.
/// Consecutive posts are coalesced: the UI always re-reads `tray_host::snapshot()`.
pub const WM_DOCK_TRAY_CHANGED: u32 = WM_APP + 9;
/// notif (`notif` thread) -> UI: the notification/DND state changed.
/// wParam = 1 list/count/access changed, 2 only the "Do Not Disturb" state changed; lParam = 0.
/// `main` calls `ui::on_notif_changed()` and `popup::on_notif_changed()`; the data is read with
/// `notif::snapshot()` (short clone under a Mutex).
pub const WM_DOCK_NOTIF: u32 = WM_APP + 11;
/// popup (UI thread, popup window) -> dock window: a popup was closed (click outside, Esc,
/// toggle, action). wParam = `popup::PopupKind as usize`, lParam = 0. Posted (not a direct call)
/// to avoid re-entering the UI from the popup's wnd_proc. `main` calls `ui::on_popup_closed()`.
pub const WM_DOCK_POPUP_CLOSED: u32 = WM_APP + 12;
/// app menu popup (UI thread, popup window) -> dock window: the user picked an entry of an app's
/// themed context menu. wParam = `ID_MENU_*` (PIN/UNPIN/CLOSE_ALL/NEW_WINDOW), lParam = 0.
/// The app key is read with `popup::take_app_menu_key()`. Handled by `ui::handle_message`.
pub const WM_DOCK_APP_MENU_CMD: u32 = WM_APP + 13;
/// popup mouse hook (WH_MOUSE_LL, runs on the UI thread, installed only while a popup is open) ->
/// POPUP window (not the dock's): a mouse button went down outside the popup. wParam = lParam = 0.
/// Posted (never handled inside the hook callback); the popup closes itself.
pub const WM_POPUP_OUTSIDE: u32 = WM_APP + 14;
/// Settings popup (UI thread, popup window) -> dock window: a setting was changed and saved
/// (`config::modify`). wParam = lParam = 0. Posted so the bar is rebuilt OUTSIDE the popup's
/// wnd_proc (no re-entry). `main` calls `ui::on_config_changed()`.
pub const WM_DOCK_CONFIG_APPLY: u32 = WM_APP + 15;
/// hook -> UI: the Win key was tapped alone twice in quick succession (`hook::DOUBLE_TAP_MS`): open the
/// dock's Start menu (`popup_startmenu`). wParam = 0, lParam = 0. Posted by the `hook` thread
/// INSTEAD of the second `WM_DOCK_TOGGLE`. `main` calls `ui::open_start_menu()`.
pub const WM_DOCK_START_MENU: u32 = WM_APP + 16;

// `WM_APP + 10` is RESERVED: it is the UI -> hook thread THREAD message (PostThreadMessageW)
// defined privately in `hook.rs` (`WM_HOOK_REINSTALL`). Do not reuse it.

// -------------------------------------- THREAD messages (PostThreadMessageW)

/// UI (or other thread) -> shell thread: there is at least one new `ShellRequest` in the queue.
/// wParam = 0, lParam = 0. The shell thread drains it all. Posted by `shell_thread::request`.
pub const WM_SHELL_WAKE: u32 = WM_APP + 20;
/// audio (`IMMNotificationClient` callback, arbitrary thread) -> shell thread: the default
/// render device was changed/removed/added. The shell thread calls
/// `AudioController::reopen_endpoint` and posts `WM_DOCK_VOLUME`. wParam = 0, lParam = 0.
pub const WM_SHELL_AUDIO_DEVICE_CHANGED: u32 = WM_APP + 21;
// The shell thread is asked to exit with WM_QUIT (PostThreadMessageW(WM_QUIT)).

// --------------------------------------------------------------- timers (SetTimer)

/// 50 ms, always on: auto-hide, screen edge, topmost re-assertion. Handled by `ui`.
pub const TIMER_POLL: usize = 1;
/// 2 s: keyboard hook watchdog (`hook::request_reinstall`) + config mtime check
/// (`config::poll_changed`). Handled by `main`.
pub const TIMER_HOUSEKEEPING: usize = 2;
/// 30 s: RAM log (private working set). Handled by `main`.
pub const TIMER_RAM: usize = 3;
/// 1 s: heartbeat to the watchdog (`watchdog::heartbeat`) + taskbar safety timer
/// (`taskbar::on_safety_timer`). Handled by `main`.
pub const TIMER_SECOND: usize = 4;
/// 5 s: window list reconciliation (`ui::on_reconcile`). Handled by `ui`.
pub const TIMER_RECONCILE: usize = 5;
/// 100 ms one-shot: debounce after `WM_DOCK_APPS_DIRTY`. Handled by `ui`.
pub const TIMER_APPS_DEBOUNCE: usize = 6;
/// 15 ms: hover plate animation; `ui` starts it when the animation begins and calls
/// `KillTimer` when idle.
pub const TIMER_ANIM: usize = 7;
/// Minute-aligned (one-shot, re-armed on every tick with `clock::ms_until_next_minute`).
/// Handled by `ui`.
pub const TIMER_CLOCK: usize = 8;
/// 300 ms one-shot on the dock window: second `TaskbarCreated` announcement after an Explorer
/// restart (the new Shell_TrayWnd is created in front of our tray host). Armed by
/// `taskbar::on_taskbar_created`; `main` kills it and calls `taskbar::on_reannounce_timer()`.
pub const TIMER_TRAY_REANNOUNCE: usize = 9;

/// 2 s on the dock window, ONLY while the bar is visible and the network and/or metrics module is
/// on (`ui::sysmon_sync` arms and kills it): CPU/RAM sample, network query every few ticks.
/// Handled by `ui`.
pub const TIMER_SYSMON: usize = 10;
/// 400 ms one-shot on the dock window: the pointer rested on an app item with 2+ windows, open the
/// window picker. Armed by `ui::set_hover` only for such items (and only with `window_previews`
/// on), killed when the hover changes. Handled by `ui`.
pub const TIMER_HOVER_PICKER: usize = 11;
/// 300 ms one-shot on the PICKER window: the pointer left the picker and the app item; closes the
/// picker unless the pointer came back. Armed and handled only by `picker`.
pub const TIMER_PICKER_LEAVE: usize = 12;

// POPUP window timers (different hwnd from the dock's; handled only by `popup`).
/// 15 ms: popup open fade and hover plate; off when idle.
pub const TIMER_POPUP_ANIM: usize = 30;
/// 1 s while a popup is open: calendar clock, power popup confirmation countdown,
/// safety timer. Off on close.
pub const TIMER_POPUP_TICK: usize = 31;

// --------------------------------------------- context menu command ids

/// "Pin" (pins the entry to the dock).
pub const ID_MENU_PIN: u32 = 1;
/// "Unpin".
pub const ID_MENU_UNPIN: u32 = 2;
/// "Close all windows" (WM_CLOSE to each one).
pub const ID_MENU_CLOSE_ALL: u32 = 3;
/// "New window" (launches a new instance, like a middle click).
pub const ID_MENU_NEW_WINDOW: u32 = 5;
