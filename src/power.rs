//! Power actions of the homonymous popup.
//!
//!  * Sleep: `SetSuspendState(false, false, false)` (may hibernate if hibernation is enabled:
//!    known limitation, documented in the README).
//!  * Lock: `LockWorkStation`.
//!  * Restart / Shut down: enable `SeShutdownPrivilege` (`OpenProcessToken` + `LookupPrivilegeValueW`
//!    + `AdjustTokenPrivileges`, checking `ERROR_NOT_ALL_ASSIGNED`), then `ExitWindowsEx`
//!      (`EWX_REBOOT` / `EWX_POWEROFF | EWX_HYBRID_SHUTDOWN`, NEVER `EWX_FORCE`), reason
//!      `SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED`. The taskbar is NOT restored by hand:
//!      `WM_ENDSESSION` does it (already handled in `main`), so a cancelled shutdown does not leave it visible.
//!  * Sign out (Start quick-links menu): `ExitWindowsEx(EWX_LOGOFF)`, no privilege needed, NEVER
//!    `EWX_FORCE` (apps may ask to save or cancel), on the `power` thread like Restart / Shut down.
//!  * Exit the dock: `PostMessageW(crate::hwnd(), WM_CLOSE)` = the existing clean exit (restores the
//!    native taskbar and destroys the tray host before the `TaskbarCreated` broadcast).
//!
//! THREADING: `run` is called from the UI thread and does not block: Sleep/Restart/Shut down run on a
//! throwaway `power` thread (SetSuspendState may return only on wake-up). Errors: `log!`.

use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LPARAM, LUID, WPARAM};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::{
    ExitWindowsEx, LockWorkStation, EWX_HYBRID_SHUTDOWN, EWX_LOGOFF, EWX_POWEROFF, EWX_REBOOT,
    SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

use crate::log;

/// Action chosen in the power popup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Sleep,
    Lock,
    Restart,
    Shutdown,
    /// Signs the user out (Start quick-links menu).
    SignOut,
    /// Closes the dock (clean exit, the native taskbar comes back).
    ExitDock,
}

/// Runs `action` without blocking the caller. The popup must be closed FIRST (the caller does it).
pub fn run(action: PowerAction) {
    log!("power::run({action:?})");
    match action {
        PowerAction::ExitDock => {
            // SAFETY: PostMessageW to the dock window; if it no longer exists it just fails.
            if let Err(e) = unsafe { PostMessageW(Some(crate::hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
                log!("power: PostMessageW(WM_CLOSE) failed: {e}");
            }
        }
        PowerAction::Lock => {
            // SAFETY: no preconditions; returns immediately.
            if let Err(e) = unsafe { LockWorkStation() } {
                log!("power: LockWorkStation failed: {e}");
            }
        }
        PowerAction::Sleep | PowerAction::Restart | PowerAction::Shutdown | PowerAction::SignOut => {
            // Throwaway thread: SetSuspendState returns only on wake-up, ExitWindowsEx may wait.
            let spawned = std::thread::Builder::new().name("power".into()).spawn(move || worker(action));
            if let Err(e) = spawned {
                log!("power: thread spawn failed: {e}");
            }
        }
    }
}

/// Body of the `power` thread.
fn worker(action: PowerAction) {
    match action {
        PowerAction::Sleep => {
            // SAFETY: no preconditions. With hibernation enabled it may hibernate (known limitation).
            let ok = unsafe { SetSuspendState(false, false, false) };
            log!("power: SetSuspendState -> {ok}");
        }
        PowerAction::Restart | PowerAction::Shutdown => {
            if let Err(e) = enable_shutdown_privilege() {
                log!("power: SeShutdownPrivilege not enabled: {e}");
                return;
            }
            let flags = if action == PowerAction::Restart { EWX_REBOOT } else { EWX_POWEROFF | EWX_HYBRID_SHUTDOWN };
            // Never EWX_FORCE: apps may ask to save or cancel the shutdown.
            // SAFETY: privilege enabled above; no pointers passed.
            let r = unsafe { ExitWindowsEx(flags, SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED) };
            match r {
                Ok(()) => log!("power: ExitWindowsEx({action:?}) accepted"),
                Err(e) => log!("power: ExitWindowsEx({action:?}) failed: {e}"),
            }
        }
        PowerAction::SignOut => {
            // Never EWX_FORCE. SAFETY: no pointers passed; logging off needs no privilege.
            match unsafe { ExitWindowsEx(EWX_LOGOFF, SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_FLAG_PLANNED) } {
                Ok(()) => log!("power: ExitWindowsEx(SignOut) accepted"),
                Err(e) => log!("power: ExitWindowsEx(SignOut) failed: {e}"),
            }
        }
        PowerAction::Lock | PowerAction::ExitDock => {}
    }
}

/// Enables `SeShutdownPrivilege` in the process token.
fn enable_shutdown_privilege() -> Result<(), String> {
    let mut token = HANDLE::default();
    // SAFETY: `token` receives a handle that is closed below; the process pseudo-handle is always valid.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) }
        .map_err(|e| format!("OpenProcessToken: {e}"))?;
    let mut luid = LUID::default();
    // SAFETY: NULL local name = this computer; `luid` is writable; SE_SHUTDOWN_NAME is a static constant.
    let r = unsafe { LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid) };
    let result = match r {
        Err(e) => Err(format!("LookupPrivilegeValueW: {e}")),
        Ok(()) => {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            // SAFETY: `token` is valid with TOKEN_ADJUST_PRIVILEGES; `tp` outlives the call and has a
            // single privilege, as declared by PrivilegeCount.
            let adj = unsafe { AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None) };
            // SAFETY: GetLastError right after the call: distinguishes partial success.
            let last = unsafe { GetLastError() };
            match adj {
                Err(e) => Err(format!("AdjustTokenPrivileges: {e}")),
                Ok(()) if last == ERROR_NOT_ALL_ASSIGNED => Err("privilege not assigned to the token".to_string()),
                Ok(()) => Ok(()),
            }
        }
    };
    // SAFETY: `token` was opened above and is not used afterwards.
    let _ = unsafe { CloseHandle(token) };
    result
}
