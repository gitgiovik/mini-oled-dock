//! "Import from Windows Start" (Settings -> Appearance): replaces `config.start_pins` with the pins
//! of the native Start menu, in the Windows order.
//!
//! JOB: one short-lived thread per click (`start`), ended when the job ends: it runs
//! `powershell.exe -NoProfile -NonInteractive -Command Export-StartLayout -Path <file.json>` with
//! `CREATE_NO_WINDOW` (never a visible window, killed after `PS_TIMEOUT`), reads the JSON back
//! (`map_layout`, pure), deletes the file and saves the pins through `config::modify`. The
//! AppsFolder catalog (`shell_thread::enum_apps_folder`) is enumerated on that same thread and only
//! when a `.lnk` entry needs it, so nothing stays in memory afterwards.
//!
//! RESULT: one atomic word (`STATUS`), read by the Settings popup on its 1 s tick (the popup's own
//! timer, alive only while it is open). No new timer, no thread left running.
//!
//! The JSON file goes into the dock's admin-only data folder when the dock can write it (the dock
//! and its PowerShell child run elevated: a user-writable temp folder would let a planted link
//! redirect that write), into the user's temp folder otherwise.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::time::{Duration, Instant};

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{
    CLSIDFromString, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::UI::Shell::{IShellLinkW, SHGetKnownFolderPath, ShellLink, KF_FLAG_DEFAULT};

use crate::log;
use crate::shell_thread::StartApp;

/// Longest wait for PowerShell before it is killed.
const PS_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest layout file read back.
const MAX_JSON: u64 = 1 << 20;

/// `STATUS` values; `DONE + n` = n apps imported.
const IDLE: u32 = 0;
const RUNNING: u32 = 1;
const FAILED: u32 = 2;
const DONE: u32 = 3;

static STATUS: AtomicU32 = AtomicU32::new(IDLE);

/// State of the import, as shown by the Settings row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Idle,
    Running,
    Failed,
    Imported(u32),
}

fn decode(v: u32) -> Status {
    match v {
        IDLE => Status::Idle,
        RUNNING => Status::Running,
        FAILED => Status::Failed,
        n => Status::Imported(n - DONE),
    }
}

/// Raw status word (cheap change detection for the popup tick).
pub(crate) fn status_word() -> u32 {
    STATUS.load(SeqCst)
}

pub(crate) fn status() -> Status {
    decode(status_word())
}

/// Forgets the result of a finished import (the Settings popup opens with the plain label).
pub(crate) fn reset() {
    let cur = STATUS.load(SeqCst);
    if cur != RUNNING {
        let _ = STATUS.compare_exchange(cur, IDLE, SeqCst, SeqCst);
    }
}

/// Starts the import on a short-lived thread. false if one is already running.
pub(crate) fn start() -> bool {
    let cur = STATUS.load(SeqCst);
    if cur == RUNNING || STATUS.compare_exchange(cur, RUNNING, SeqCst, SeqCst).is_err() {
        return false;
    }
    let spawned = std::thread::Builder::new().name("start-import".into()).spawn(|| {
        let r = job();
        let v = match r {
            Ok(n) => {
                log!("start import: {n} apps imported");
                DONE + n as u32
            }
            Err(e) => {
                log!("start import: failed: {e}");
                FAILED
            }
        };
        STATUS.store(v, SeqCst);
    });
    if let Err(e) = spawned {
        log!("start import: thread not started: {e}");
        STATUS.store(FAILED, SeqCst);
    }
    true
}

// ------------------------------------------------------------------ the job (import thread)

/// Folder of the layout file: the data folder when the dock can write it, else the temp folder.
fn work_dir() -> PathBuf {
    if crate::datadir::writable() {
        if let Some(dir) = crate::datadir::config_path().and_then(|p| p.parent().map(Path::to_path_buf)) {
            return dir;
        }
    }
    std::env::temp_dir()
}

fn powershell_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe")
}

/// Runs `Export-StartLayout` into `file` (no window, killed on timeout).
fn export_layout(file: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // Single-quoted PowerShell literal: only the quote itself needs doubling.
    let quoted = file.to_string_lossy().replace('\'', "''");
    let mut child = std::process::Command::new(powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("Export-StartLayout -Path '{quoted}'"))
        .creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot start powershell.exe: {e}"))?;
    let t = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(format!("Export-StartLayout exit code {:?}", st.code())),
            Ok(None) if t.elapsed() > PS_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("powershell.exe is not responding (timeout)".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("waiting for powershell.exe failed: {e}")),
        }
    }
}

fn job() -> Result<usize, String> {
    let file = work_dir().join(format!("start-layout-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&file);
    let exported = export_layout(&file);
    let text = exported.and_then(|()| {
        crate::datadir::read_file(&file, MAX_JSON, true)
            .map_err(|e| format!("reading {}: {e}", file.display()))
            .and_then(|b| String::from_utf8(b).map_err(|e| e.to_string()))
    });
    if let Err(e) = std::fs::remove_file(&file) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log!("start import: cannot delete {}: {e}", file.display());
        }
    }
    let text = text?;
    // COM (STA) only for this job: shell links and the AppsFolder catalog.
    // SAFETY: initializes COM on this thread; paired with CoUninitialize below.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let mapped = {
        let link = Links::new();
        let mut catalog = || -> Vec<(StartApp, Option<String>)> {
            let apps = crate::shell_thread::enum_apps_folder();
            log!("start import: {} apps in the catalog", apps.len());
            apps.into_iter()
                .map(|a| {
                    let p = known_folder_form(&a.id);
                    (a, p)
                })
                .collect()
        };
        map_layout(&text, &mut catalog, &|lnk| link.as_ref().and_then(|l| l.target(lnk)))
    };
    if com.is_ok() {
        // SAFETY: paired with the successful CoInitializeEx above; every COM object is dropped.
        unsafe { CoUninitialize() };
    }
    let pins = mapped?;
    if pins.is_empty() {
        return Err("no pinned app could be mapped".into());
    }
    let n = pins.len();
    crate::config::modify(move |c| c.start_pins = pins)?;
    Ok(n)
}

/// `{GUID}\rest` (an AppsFolder id of a classic program) as a file path, lowercase.
fn known_folder_form(id: &str) -> Option<String> {
    let close = id.find('}')?;
    if !id.starts_with('{') {
        return None;
    }
    let guid_text = HSTRING::from(&id[..=close]);
    // SAFETY: `guid_text` is NUL-terminated and lives for the call.
    let guid = unsafe { CLSIDFromString(PCWSTR(guid_text.as_ptr())) }.ok()?;
    // SAFETY: valid known-folder id, current user; the returned buffer is freed below.
    let p = unsafe { SHGetKnownFolderPath(&guid, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: NUL-terminated string returned by SHGetKnownFolderPath, read once.
    let base = unsafe { p.to_string() }.ok();
    // SAFETY: same pointer, allocated with CoTaskMemAlloc, not used after this line.
    unsafe { CoTaskMemFree(Some(p.0 as *const std::ffi::c_void)) };
    Some(norm(&format!("{}{}", base?, &id[close + 1..])))
}

/// One `ShellLink` object for every `.lnk` of the layout.
struct Links {
    link: IShellLinkW,
    file: IPersistFile,
}

impl Links {
    fn new() -> Option<Links> {
        // SAFETY: COM STA initialized on this thread; ShellLink CLSID with the requested interface.
        let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.ok()?;
        let file = link.cast::<IPersistFile>().ok()?;
        Some(Links { link, file })
    }

    /// Target path of the shortcut `lnk` (no UI, no search).
    fn target(&self, lnk: &str) -> Option<String> {
        let wide = HSTRING::from(lnk);
        // SAFETY: `wide` is NUL-terminated and lives for the call.
        unsafe { self.file.Load(PCWSTR(wide.as_ptr()), STGM_READ) }.ok()?;
        let mut buf = [0u16; 520];
        let mut fd = WIN32_FIND_DATAW::default();
        // SAFETY: `buf` and `fd` are writable and sized as declared; flags 0 = long path.
        unsafe { self.link.GetPath(&mut buf, &mut fd, 0) }.ok()?;
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let s = String::from_utf16_lossy(&buf[..n]);
        Some(s).filter(|s| !s.is_empty())
    }
}

// ------------------------------------------------------------------ mapping (pure)

fn norm(p: &str) -> String {
    p.trim().replace('/', "\\").to_lowercase()
}

/// File name of `path` without the extension.
fn stem(path: &str) -> &str {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// Maps the `Export-StartLayout` JSON to AppsFolder ids, in order, without duplicates.
/// `packagedAppId` / `desktopAppId` are kept as they are; a `desktopAppLink` (environment variables expanded) is matched against the
/// catalog: first an id equal to the shortcut's target (`target`), as a plain path or in its
/// `{KnownFolder}\...` form (the `Option` next to each app, lowercase), then the shortcut's file
/// name against the app names. `catalog` is called once, and only when a link needs it. Entries
/// that cannot be mapped are skipped (logged). Err = not a layout file.
pub(crate) fn map_layout(
    json: &str,
    catalog: &mut dyn FnMut() -> Vec<(StartApp, Option<String>)>,
    target: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json.trim_start_matches('\u{feff}')).map_err(|e| format!("layout JSON: {e}"))?;
    let list = v.get("pinnedList").and_then(|l| l.as_array()).ok_or("layout JSON without pinnedList")?;
    let mut out: Vec<String> = Vec::new();
    let mut loaded: Option<Vec<(StartApp, Option<String>)>> = None;
    for entry in list {
        let id = if let Some(id) = entry.get("packagedAppId").or_else(|| entry.get("desktopAppId")).and_then(|x| x.as_str()) {
            Some(id.trim().to_string()).filter(|s| !s.is_empty())
        } else if let Some(link) = entry.get("desktopAppLink").and_then(|x| x.as_str()) {
            let link = crate::shell_thread::expand_env(link);
            let apps = loaded.get_or_insert_with(&mut *catalog);
            let by_target = target(&link).map(|t| norm(&crate::shell_thread::expand_env(&t))).and_then(|t| {
                apps.iter()
                    .find(|(a, kf)| norm(&a.id) == t || kf.as_deref() == Some(t.as_str()))
                    .map(|(a, _)| a.id.clone())
            });
            let found = by_target.or_else(|| {
                let s = stem(&link);
                apps.iter().find(|(a, _)| a.name.eq_ignore_ascii_case(s)).map(|(a, _)| a.id.clone())
            });
            if found.is_none() {
                log!("start import: no app for the shortcut {link}");
            }
            found
        } else {
            log!("start import: entry skipped: {entry}");
            None
        };
        if let Some(id) = id {
            if !out.iter().any(|o| o.eq_ignore_ascii_case(&id)) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str, kf: Option<&str>) -> (StartApp, Option<String>) {
        (StartApp { id: id.into(), name: name.into() }, kf.map(str::to_string))
    }

    #[test]
    fn layout_maps_ids_links_and_skips_the_rest() {
        let apps = vec![
            app("Microsoft.Windows.Explorer", "File Explorer", None),
            app("{6D809377-6AF0-444B-8957-A3773F02200E}\\Git\\git-bash.exe", "Git Bash", Some("c:\\program files\\git\\git-bash.exe")),
            app("C:\\Tools\\tool.exe", "Some Tool", None),
            app("Microsoft.WindowsStore_8wekyb3d8bbwe!App", "Microsoft Store", None),
        ];
        let json = r#"{"pinnedList":[
            {"packagedAppId":"Microsoft.WindowsStore_8wekyb3d8bbwe!App"},
            {"desktopAppLink":"%APPDATA%\\Microsoft\\Windows\\Start Menu\\Programs\\File Explorer.lnk"},
            {"desktopAppLink":"C:\\ProgramData\\Microsoft\\Windows\\Start Menu\\Programs\\Git\\Bash here.lnk"},
            {"desktopAppLink":"C:\\Links\\Tool.lnk"},
            {"desktopAppLink":"C:\\Links\\Missing.lnk"},
            {"desktopAppId":"Custom.App!Main"},
            {"packagedAppId":"microsoft.windowsstore_8wekyb3d8bbwe!app"},
            {"secondaryTile":"x"}
        ]}"#;
        let target = |lnk: &str| -> Option<String> {
            if lnk.ends_with("Bash here.lnk") {
                Some("C:/Program Files/Git/git-bash.exe".into())
            } else if lnk.ends_with("Tool.lnk") {
                Some("c:\\tools\\TOOL.exe".into())
            } else {
                None
            }
        };
        let mut calls = 0;
        let mut cat = || {
            calls += 1;
            apps.clone()
        };
        let got = map_layout(json, &mut cat, &target).unwrap();
        assert_eq!(
            got,
            vec![
                "Microsoft.WindowsStore_8wekyb3d8bbwe!App".to_string(),
                "Microsoft.Windows.Explorer".into(),
                "{6D809377-6AF0-444B-8957-A3773F02200E}\\Git\\git-bash.exe".into(),
                "C:\\Tools\\tool.exe".into(),
                "Custom.App!Main".into(),
            ]
        );
        assert_eq!(calls, 1, "the catalog is enumerated once");
    }

    #[test]
    fn layout_without_links_never_loads_the_catalog() {
        let mut cat = || -> Vec<(StartApp, Option<String>)> { panic!("catalog not needed") };
        let got = map_layout("\u{feff}{\"pinnedList\":[{\"packagedAppId\":\"A!B\"}]}", &mut cat, &|_| None).unwrap();
        assert_eq!(got, vec!["A!B".to_string()]);
        assert!(map_layout("{}", &mut cat, &|_| None).is_err());
        assert!(map_layout("not json", &mut cat, &|_| None).is_err());
        assert!(map_layout("{\"pinnedList\":[]}", &mut cat, &|_| None).unwrap().is_empty());
    }

    #[test]
    fn stems_and_status() {
        assert_eq!(stem("C:\\a\\File Explorer.lnk"), "File Explorer");
        assert_eq!(stem("x/y/Tool"), "Tool");
        assert_eq!(decode(IDLE), Status::Idle);
        assert_eq!(decode(DONE + 7), Status::Imported(7));
        assert_eq!(decode(FAILED), Status::Failed);
    }
}
