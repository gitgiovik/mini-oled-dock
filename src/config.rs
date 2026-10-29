//! Configuration `%ProgramData%\mini-oled-dock\config.json` (folder and file security: `datadir`).
//!
//! The config is global and shared across threads: `get()` returns an immutable snapshot
//! (`Arc<Config>`) that anyone (UI, shell thread) can hold without locking. Changes go
//! through `modify()` (read-modify-write with an atomic write: temp file + rename, `datadir::write_atomic`) and through
//! `poll_changed()` (re-read if the mtime changes, called by the UI thread every 2 s).
//!
//! Command-line options override the config ONLY through the `effective_*` functions.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Position of the bar.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Top,
    #[default]
    Bottom,
}

/// What each dock entry shows: icon + label, icon only, or label only.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMode {
    #[default]
    IconText,
    Icon,
    Text,
}

/// Legacy bar size (before the independent sizes): read only to migrate an old config once
/// (`migrate_bar_size`), never written.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum BarSize {
    Small,
    #[default]
    Medium,
    Large,
}

/// Background colour of the bar and the popups, 0xRRGGBB. Written as "#RRGGBB"; the legacy
/// values "black" and "dark" are still accepted. An unreadable value falls back to black instead
/// of rejecting the whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Background(pub u32);

impl Background {
    /// Pure black (OLED), the default.
    pub const BLACK: Background = Background(0x000000);
    /// The legacy "dark" tone (very dark grey).
    pub const DARK: Background = Background(0x1C1C1E);

    /// Background colour (not premultiplied).
    pub fn rgb(self) -> (u32, u32, u32) {
        ((self.0 >> 16) & 255, (self.0 >> 8) & 255, self.0 & 255)
    }

    /// "#RRGGBB" (upper case).
    pub fn hex(self) -> String {
        format!("#{:06X}", self.0 & 0xFF_FFFF)
    }

    /// Parses "#RRGGBB" / "RRGGBB" / "#RGB" (any case) or the legacy "black" / "dark".
    pub fn parse(s: &str) -> Option<Background> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("black") {
            return Some(Background::BLACK);
        }
        if t.eq_ignore_ascii_case("dark") {
            return Some(Background::DARK);
        }
        let h = t.strip_prefix('#').unwrap_or(t);
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        match h.len() {
            6 => u32::from_str_radix(h, 16).ok().map(Background),
            3 => {
                let v = u32::from_str_radix(h, 16).ok()?;
                let (r, g, b) = ((v >> 8) & 15, (v >> 4) & 15, v & 15);
                Some(Background(((r * 17) << 16) | ((g * 17) << 8) | (b * 17)))
            }
            _ => None,
        }
    }
}

impl Serialize for Background {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Background {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Background::parse(&s).unwrap_or_default())
    }
}

/// Preset background colours offered by the Settings popup: dark tones that keep white text readable.
pub const BACKGROUND_PRESETS: [(&str, Background); 7] = [
    ("Black", Background::BLACK),
    ("Graphite", Background::DARK),
    ("Slate", Background(0x1A222D)),
    ("Navy", Background(0x0E1A33)),
    ("Plum", Background(0x261630)),
    ("Wine", Background(0x2E1119)),
    ("Forest", Background(0x10261B)),
];

/// Range of a size setting: `min..=max`; `default` reproduces the original look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeRange {
    pub min: u8,
    pub max: u8,
    pub default: u8,
}

impl SizeRange {
    pub fn clamp(self, v: u8) -> u8 {
        v.clamp(self.min, self.max)
    }
}

/// App icon side (logical px).
pub const APP_ICON_SIZE: SizeRange = SizeRange { min: 14, max: 32, default: 16 };
/// App name text height (logical px; 13 px is the Windows UI text size at 100 %).
pub const APP_TEXT_SIZE: SizeRange = SizeRange { min: 10, max: 18, default: 13 };
/// Tray icon side (logical px).
pub const TRAY_ICON_SIZE: SizeRange = SizeRange { min: 12, max: 24, default: 16 };
/// Scale of the system modules on the right (percent).
pub const MODULE_SIZE: SizeRange = SizeRange { min: 75, max: 150, default: 100 };

/// Default opacity of the bar background (percent).
pub const DEFAULT_BAR_OPACITY: u8 = 95;

/// An app pinned to the dock.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct PinnedApp {
    /// Grouping key (AUMID or normalized lowercase exe path).
    pub key: String,
    /// Display name.
    pub name: String,
    /// Launch target: exe path or `shell:AppsFolder\<AUMID>`.
    pub launch: String,
    /// Optional custom icon: plain file name inside `%ProgramData%\mini-oled-dock\icons\`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// Contents of `config.json`. Missing fields = defaults (`#[serde(default)]`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub position: Position,
    pub autohide: bool,
    pub hide_native_taskbar: bool,
    pub pinned: Vec<PinnedApp>,
    /// Name overrides: key = lowercase exe file name (e.g. "code.exe") or AUMID;
    /// value = name to show.
    pub names: BTreeMap<String, String>,
    pub show_date: bool,
    pub show_volume_percent: bool,
    /// hosts the tray icons of other apps in the dock (`tray_host`). false = never started.
    pub show_tray: bool,
    /// Per-module visibility of the system modules (see `MODULE_NAMES` / `module_visible`).
    /// A hidden module is not laid out and costs nothing: no timer, no polling, nothing loaded.
    pub show_notifications: bool,
    pub show_volume: bool,
    /// Network module: Wi-Fi / ethernet glyph with internet state, popup with the details (`net`).
    pub show_network: bool,
    /// CPU / RAM usage module (`metrics`).
    pub show_metrics: bool,
    pub show_clock: bool,
    pub show_desktop_button: bool,
    /// Start button: fixed item at the left end of the bar (not a movable module); see `module_visible("start")`.
    pub show_start_button: bool,
    /// Entry display: "icon_text" | "icon" | "text".
    pub display_mode: DisplayMode,
    /// Legacy "small" | "medium" | "large": migrated to the four sizes below when read, never written.
    #[serde(skip_serializing)]
    pub bar_size: Option<BarSize>,
    /// App icon side, logical px (`APP_ICON_SIZE`).
    pub app_icon_size: u8,
    /// App name text height, logical px (`APP_TEXT_SIZE`).
    pub app_text_size: u8,
    /// Tray icon side, logical px (`TRAY_ICON_SIZE`).
    pub tray_icon_size: u8,
    /// Scale of the system modules in percent (`MODULE_SIZE`).
    pub module_size: u8,
    /// Background colour of the bar and the popups ("#RRGGBB").
    pub background: Background,
    /// Opacity of the bar background in percent (0..=100; text and icons stay fully opaque).
    pub bar_opacity: u8,
    /// Clock picture for GetTimeFormatEx (default "HH:mm").
    pub time_format: String,
    /// If true and `time_format` has no "s", ":ss" is appended.
    pub show_seconds: bool,
    /// Date picture for GetDateFormatEx (default "ddd d MMM"; empty = no date).
    pub date_format: String,
    /// GUIDs (lowercase, no braces) of the system tray icons NOT to show
    /// (they keep being answered and forwarded to Explorer).
    pub tray_hidden_guids: Vec<String>,
    /// Tray icons the user chose as "not in the bar" (they stay in the chevron popup).
    /// Per-icon key (`tray_host::item_key`): lowercase GUID without braces if the icon has one,
    /// otherwise `<lowercase exe name>#<uID>` (e.g. "discord.exe#1"). Default: empty.
    pub tray_hidden: Vec<String>,
    /// Relative order of the app entries that are NOT pinned (their keys). Pinned entries use the
    /// `pinned` array order. Apps missing from the list go last, in the order they were first seen.
    /// Written when the user drags an entry; capped at `APP_ORDER_CAP` keys.
    pub app_order: Vec<String>,
    /// Order of the tray icons (same key as `tray_hidden`). Unknown icons go last, in registration order.
    pub tray_order: Vec<String>,
    /// Order of the system modules: "metrics", "notifications", "network", "volume", "clock",
    /// "show_desktop", "power". Validated on use (`render::modules_from_names`): unknown names are
    /// ignored, a missing module goes back next to its default neighbour.
    pub modules_order: Vec<String>,
    /// true once the `notif` thread has already asked for notification access once
    /// (`RequestAccessAsync`): the prompt is not shown again on every start.
    pub notif_prompted: bool,
}

/// Maximum number of keys kept in `app_order` / `tray_order` (the oldest, not-seen-recently ones fall off).
pub const APP_ORDER_CAP: usize = 64;
pub const TRAY_ORDER_CAP: usize = 128;

/// Default order of the system modules (names accepted by `modules_order`).
pub const DEFAULT_MODULES_ORDER: [&str; 7] =
    ["metrics", "notifications", "network", "volume", "clock", "show_desktop", "power"];

/// GUIDs of the system tray icons hidden by default: volume, network, power, location,
/// microphone (the dock draws/duplicates the first three).
pub const DEFAULT_TRAY_HIDDEN_GUIDS: [&str; 5] = [
    "7820ae73-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae74-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae75-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae77-23e3-4229-82c1-e41cb67d5b9c",
    "7820ae82-23e3-4229-82c1-e41cb67d5b9c",
];

impl Default for Config {
    fn default() -> Self {
        Config {
            position: Position::Bottom,
            autohide: true,
            hide_native_taskbar: true,
            pinned: Vec::new(),
            names: BTreeMap::new(),
            show_date: true,
            show_volume_percent: true,
            show_tray: true,
            show_notifications: true,
            show_volume: true,
            show_network: true,
            show_metrics: false,
            show_clock: true,
            show_desktop_button: true,
            show_start_button: true,
            display_mode: DisplayMode::IconText,
            bar_size: None,
            app_icon_size: APP_ICON_SIZE.default,
            app_text_size: APP_TEXT_SIZE.default,
            tray_icon_size: TRAY_ICON_SIZE.default,
            module_size: MODULE_SIZE.default,
            background: Background::BLACK,
            bar_opacity: DEFAULT_BAR_OPACITY,
            time_format: "HH:mm".to_string(),
            show_seconds: false,
            date_format: "ddd d MMM".to_string(),
            tray_hidden_guids: DEFAULT_TRAY_HIDDEN_GUIDS.iter().map(|s| s.to_string()).collect(),
            tray_hidden: Vec::new(),
            app_order: Vec::new(),
            tray_order: Vec::new(),
            modules_order: DEFAULT_MODULES_ORDER.iter().map(|s| s.to_string()).collect(),
            notif_prompted: false,
        }
    }
}

static CURRENT: OnceLock<Mutex<Arc<Config>>> = OnceLock::new();

fn cell() -> &'static Mutex<Arc<Config>> {
    CURRENT.get_or_init(|| Mutex::new(Arc::new(Config::default())))
}

// -------------------------------------------------------------------- access

/// Current config snapshot (cheap: clones an `Arc`). Available from any thread.
/// Before `init()` it returns the defaults.
pub fn get() -> Arc<Config> {
    match cell().lock() {
        Ok(g) => Arc::clone(&g),
        Err(p) => Arc::clone(&p.into_inner()),
    }
}

pub(crate) fn set(c: Config) {
    let new = Arc::new(c);
    match cell().lock() {
        Ok(mut g) => *g = new,
        Err(p) => *p.into_inner() = new,
    }
}

/// mtime of `config.json` at our last read/write (`None` = file missing/unknown).
static LAST_MTIME: Mutex<Option<SystemTime>> = Mutex::new(None);
/// Serializes read-modify-writes (`modify`) and writes (`save`) across threads.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn lock_mtime() -> std::sync::MutexGuard<'static, Option<SystemTime>> {
    LAST_MTIME.lock().unwrap_or_else(|p| p.into_inner())
}

fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

/// Reads a config file through `datadir` (no reparse point followed, size-capped). The content is
/// untrusted anyway (the user may edit it), so extra hard links do not matter for a read.
fn read_text(p: &Path) -> std::io::Result<String> {
    let bytes = crate::datadir::read_file(p, crate::datadir::CONFIG_MAX_BYTES, false)?;
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Removes `//` line comments and `/* */` block comments (JSONC), leaving string literals intact
/// (escaped quotes included). Newlines are kept so serde error positions still match the file.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut it = src.chars().peekable();
    let mut in_str = false;
    while let Some(c) = it.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if it.peek() == Some(&'/') => {
                while let Some(&n) = it.peek() {
                    if n == '\n' {
                        break;
                    }
                    it.next();
                }
            }
            '/' if it.peek() == Some(&'*') => {
                it.next();
                let mut prev = '\0';
                for n in it.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Parses the config (JSONC). Tolerates a leading UTF-8 BOM (Notepad). Missing fields = defaults.
fn parse(text: &str) -> Result<Config, String> {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut c = serde_json::from_str::<Config>(&strip_comments(t)).map_err(|e| e.to_string())?;
    migrate_bar_size(&mut c);
    Ok(c)
}

/// Converts a legacy `bar_size` (one knob scaling the whole bar: small 7/8, medium 1, large 5/4)
/// into the four independent sizes, unless those were already changed (a hand edit wins). Clears
/// `bar_size`, which is never written back.
fn migrate_bar_size(c: &mut Config) {
    let Some(size) = c.bar_size.take() else { return };
    let untouched = c.app_icon_size == APP_ICON_SIZE.default
        && c.app_text_size == APP_TEXT_SIZE.default
        && c.tray_icon_size == TRAY_ICON_SIZE.default
        && c.module_size == MODULE_SIZE.default;
    if !untouched {
        return;
    }
    let (icon, text, tray, module) = match size {
        BarSize::Small => (14, 11, 14, 88),
        BarSize::Medium => return,
        BarSize::Large => (20, 16, 20, 125),
    };
    c.app_icon_size = icon;
    c.app_text_size = text;
    c.tray_icon_size = tray;
    c.module_size = module;
}

/// Marker on the first line of every file we write; a config without it is a legacy plain JSON
/// file and gets migrated to the commented template.
const TEMPLATE_MARK: &str = "// mini-oled-dock configuration";

fn js<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

/// Pretty JSON of a collection, continuation lines indented to sit under a 2-space key.
fn js_block<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string()).replace('\n', "\n  ")
}

/// Serializes `c` as a commented JSONC file: an English guide above every field, current values
/// inline, so the comments survive every programmatic rewrite.
fn render(c: &Config) -> String {
    let mode = match c.display_mode {
        DisplayMode::IconText => "icon_text",
        DisplayMode::Icon => "icon",
        DisplayMode::Text => "text",
    };
    let pos = match c.position {
        Position::Top => "top",
        Position::Bottom => "bottom",
    };
    let bg = c.background.hex();
    format!(
        r#"{TEMPLATE_MARK}
// Edit and save: changes are picked up within ~2 seconds, no restart needed.
// The folder is admin-only: edit this file with Notepad (it saves in place) or an elevated editor.
// Editors that save by writing a new file and renaming it over this one cannot write here.
// Comments (// and /* */) are allowed. If the file has a syntax error the previous
// settings are kept; if the dock must save a change (pin, tray icon) before you fix it,
// your file is first copied to config.json.bak so no edit is lost.
{{
  // Where the bar sits on screen: "top" or "bottom".
  "position": "{pos}",

  // What each entry shows: "icon_text" (icon + name), "icon" (icon only) or "text" (name only).
  // The indicator bar under each entry adapts to the chosen mode.
  "display_mode": "{mode}",

  // Sizes, each independent (the bar height follows the tallest content):
  //   app_icon_size   app icons, 14-32 px (default 16)
  //   app_text_size   app names, 10-18 px (default 13)
  //   tray_icon_size  tray icons, 12-24 px (default 16)
  //   module_size     start button, notifications, volume, network, metrics, clock, show desktop and
  //                   power, 75-150 % (default 100)
  // Logical pixels: the Windows display scaling is applied on top.
  "app_icon_size": {icon_size},
  "app_text_size": {text_size},
  "tray_icon_size": {tray_size},
  "module_size": {module_size},

  // Background colour of the bar and the popups as a hex code, #RRGGBB (e.g. #000000, pure black,
  // ideal for OLED). The Settings popup offers dark presets and a colour picker.
  "background": "{bg}",

  // Opacity of the bar background in percent, 0-100 (0 = only text and icons are visible; the
  // bar still reacts to the mouse). Text and icons always stay fully opaque.
  "bar_opacity": {opacity},

  // Hide the bar when the pointer leaves it, show it when you touch the screen edge.
  "autohide": {autohide},

  // Hide Windows' own taskbar while the dock runs (restored when the dock exits).
  "hide_native_taskbar": {hide_native},

  // Show the date next to the clock.
  "show_date": {show_date},

  // Clock format (Windows time picture, uses your system locale).
  //   H / HH = hour 0-23      h / hh = hour 1-12      m / mm = minutes
  //   s / ss = seconds        t / tt = AM/PM marker
  // Examples: "HH:mm" -> 14:05 (24h)    "h:mm tt" -> 2:05 PM (12h)
  "time_format": {time_format},

  // true = always show seconds (":ss" is appended if time_format has no "s").
  // The clock then refreshes every second instead of every minute.
  "show_seconds": {show_seconds},

  // Date format (Windows date picture). Empty string "" = no date.
  //   d / dd = day of month   ddd / dddd = short / full weekday name
  //   M / MM = month number   MMM / MMMM = short / full month name
  //   yy / yyyy = year
  // Examples: "ddd d MMM" -> Fri 23 Oct    "dd/MM/yyyy" -> 23/10/2026
  "date_format": {date_format},

  // Show the percentage next to the volume icon.
  "show_volume_percent": {show_volume},

  // Host other apps' tray icons inside the dock. All visible icons are shown (no limit);
  // the bar widens and narrows dynamically.
  "show_tray": {show_tray},

  // System modules on the right of the bar: true = shown, false = removed from the bar.
  // A hidden module costs nothing (no timer, no polling, no system library loaded).
  // The power button is always shown (it holds the dock's own menu).
  "show_notifications": {show_notif},
  "show_volume": {show_vol_mod},

  // Network: Wi-Fi signal bars or ethernet glyph, with a small globe when the internet is reachable
  // and a small x when it is not. Click for the connection details. Refreshed every ~6 s while
  // the bar is visible.
  "show_network": {show_network},

  // CPU and RAM usage in percent (two small lines). Refreshed every 2 s while the bar is visible.
  // Click to open Task Manager.
  "show_metrics": {show_metrics},

  "show_clock": {show_clock},
  "show_desktop_button": {show_desk},

  // Windows logo button at the left end of the bar: left click opens Start, right click the Win+X menu.
  "show_start_button": {show_start},

  // Tray icons you chose to keep out of the bar (they stay in the tray popup).
  // Normally managed from the tray popup. A key is the icon's GUID (lowercase, no braces)
  // or "<exe name lowercase>#<icon id>", e.g. "discord.exe#1".
  "tray_hidden": {tray_hidden},

  // Order of the tray icons in the bar (same keys as "tray_hidden"). Normally managed by
  // dragging an icon in the bar; icons not listed go last, in the order they registered.
  "tray_order": {tray_order},

  // System tray icons the dock never shows (volume, network, power, location, microphone).
  "tray_hidden_guids": {tray_guids},

  // Pinned apps, in order. Normally managed with Pin/Unpin in the app menu. Fields:
  //   "key"    grouping key: the AUMID, or the lowercase exe path
  //   "name"   fallback label, refreshed automatically from the app itself; use the "names"
  //            map to set a fixed label
  //   "launch" exe path, or "shell:AppsFolder\\<AUMID>" for Store/packaged apps
  //   "icon"   optional custom icon: plain PNG file name inside %ProgramData%\mini-oled-dock\icons\
  //            (e.g. "discord.png"; paths, "..", drive letters and UNC paths are rejected)
  // List the AUMIDs of installed apps with:  Get-StartApps   (PowerShell)
  "pinned": {pinned},

  // Order of the open apps that are NOT pinned (their keys). Pinned apps follow the "pinned" order
  // above. Normally managed by dragging an entry in the bar; apps not listed go last, in the order
  // they were first seen. At most 64 keys are kept.
  "app_order": {app_order},

  // Order of the system buttons on the right of the bar. Normally managed by dragging them.
  // Names: "metrics", "notifications", "network", "volume", "clock", "show_desktop", "power".
  // Missing names go back next to their default neighbour, unknown names are ignored.
  // Hiding a module (the show_* switches above) keeps its place in this list.
  "modules_order": {modules_order},

  // Display-name overrides. Key = exe file name lowercase (e.g. "code.exe") or AUMID;
  // value = the name to show. Example: {{ "code.exe": "VS Code" }}
  "names": {names},

  // Internal: set once the notification-access prompt has been shown. Leave as is.
  "notif_prompted": {notif}
}}
"#,
        opacity = c.bar_opacity.min(100),
        icon_size = c.app_icon_size,
        text_size = c.app_text_size,
        tray_size = c.tray_icon_size,
        module_size = c.module_size,
        autohide = c.autohide,
        hide_native = c.hide_native_taskbar,
        show_date = c.show_date,
        time_format = js(&c.time_format),
        show_seconds = c.show_seconds,
        date_format = js(&c.date_format),
        show_volume = c.show_volume_percent,
        show_tray = c.show_tray,
        show_notif = c.show_notifications,
        show_vol_mod = c.show_volume,
        show_network = c.show_network,
        show_metrics = c.show_metrics,
        show_clock = c.show_clock,
        show_desk = c.show_desktop_button,
        show_start = c.show_start_button,
        tray_hidden = js_block(&c.tray_hidden),
        tray_order = js_block(&c.tray_order),
        app_order = js_block(&c.app_order),
        modules_order = js_block(&c.modules_order),
        tray_guids = js_block(&c.tray_hidden_guids),
        pinned = js_block(&c.pinned),
        names = js_block(&c.names),
        notif = c.notif_prompted,
    )
}

/// Reads the config from disk; if the file is missing it creates it with the defaults; if it is unreadable/invalid
/// it logs and uses the defaults WITHOUT overwriting the file. Sets the global snapshot and remembers
/// the mtime for `poll_changed`. Called once at startup from the main thread.
pub fn init() -> Arc<Config> {
    let Some(path) = crate::datadir::config_path() else {
        crate::log!("config: no usable data folder: running with the defaults, nothing is saved");
        set(Config::default());
        return get();
    };
    report_legacy_dir();
    match read_text(&path) {
        Ok(text) => match parse(&text) {
            Ok(c) => {
                crate::log!("config: read from {}", path.display());
                if !text.contains(TEMPLATE_MARK) || text.contains("\"bar_size\"") {
                    // Legacy plain JSON (or the legacy "bar_size", migrated by `parse`): rewrite
                    // once with the commented guide.
                    let _w = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
                    match write_file(&c) {
                        Ok(()) => crate::log!("config: migrated to the commented format"),
                        Err(e) => crate::log!("config: migration failed: {e}"),
                    }
                }
                set(c);
            }
            Err(e) => {
                crate::log!("config: {} is invalid ({e}): using defaults, the file is left untouched", path.display());
                set(Config::default());
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let c = migrate_legacy().unwrap_or_default();
            crate::log!("config: missing, creating {}", path.display());
            if let Err(e) = save(&c) {
                crate::log!("config: creation failed: {e}");
            }
            set(c);
        }
        Err(e) => {
            crate::log!("config: reading {} failed ({e}): using defaults", path.display());
            set(Config::default());
        }
    }
    *lock_mtime() = file_mtime(&path);
    get()
}

/// One-time migration: reads the legacy `%LOCALAPPDATA%\Dock\config.json` (safely, see
/// `datadir::read_legacy_config`) and returns it parsed; the caller writes it through the safe path.
/// Nothing under the legacy folder is ever written, renamed or deleted.
fn migrate_legacy() -> Option<Config> {
    // First choice: the config of a loosely secured data folder moved aside this run (`datadir`).
    match crate::datadir::read_previous_config() {
        Ok(None) => {}
        Ok(Some(bytes)) => match String::from_utf8(bytes).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
            Ok(c) => {
                crate::log!("config: settings recovered from the data folder moved aside");
                return Some(c);
            }
            Err(e) => crate::log!("config: config.json of the moved data folder not recovered ({e})"),
        },
        Err(e) => crate::log!("config: config.json of the moved data folder not recovered: {e}"),
    }
    match crate::datadir::read_legacy_config() {
        Ok(None) => None,
        Ok(Some(bytes)) => match String::from_utf8(bytes).map_err(|e| e.to_string()).and_then(|t| parse(&t)) {
            Ok(c) => {
                crate::log!("config: settings migrated from the legacy %LOCALAPPDATA%\\Dock\\config.json");
                Some(c)
            }
            Err(e) => {
                crate::log!("config: legacy %LOCALAPPDATA%\\Dock\\config.json not migrated ({e}): using the defaults");
                None
            }
        },
        Err(e) => {
            crate::log!("config: legacy config not migrated: {e}");
            None
        }
    }
}

/// Logs that the legacy folder is no longer used (custom icons must be moved by hand). Never touches it.
fn report_legacy_dir() {
    let Some(old) = crate::datadir::legacy_dir() else { return };
    // symlink_metadata does not follow a reparse point: this only reports.
    if std::fs::symlink_metadata(&old).is_ok() {
        let icons = crate::datadir::icons_dir().map(|p| p.display().to_string()).unwrap_or_default();
        crate::log!(
            "config: the old data folder {} is no longer used. Move your custom icons from its icons \
             subfolder to {icons}, then delete the old folder (no admin rights needed).",
            old.display()
        );
    }
}

/// true if the file's mtime differs from the one remembered at our last read/write (a hand edit that
/// `poll_changed` has not loaded yet). Does not consume the change.
pub fn external_edit_pending() -> bool {
    let Some(path) = crate::datadir::config_path() else { return false };
    file_mtime(&path) != *lock_mtime()
}

/// Checks the mtime of `config.json`; if it changed, re-reads, updates the global snapshot and
/// returns `true` (the UI must rebuild its entries). Does not block: one stat + read of a
/// small file. Called by the UI thread every 2 s (`TIMER_HOUSEKEEPING`).
///
/// If the new content is invalid (half-saved edit or syntax error) the previous
/// snapshot is kept, a message is logged once and `false` is returned; the file is re-read on the next
/// change. Returns `true` only if the parsed content differs from the snapshot.
pub fn poll_changed() -> bool {
    let Some(path) = crate::datadir::config_path() else { return false };
    let now = file_mtime(&path);
    {
        let mut last = lock_mtime();
        if *last == now {
            return false;
        }
        *last = now;
    }
    if now.is_none() {
        // File deleted: the snapshot is kept; it will be recreated on the next save.
        crate::log!("config: file removed, keeping the current configuration");
        return false;
    }
    let text = match read_text(&path) {
        Ok(t) => t,
        Err(e) => {
            crate::log!("config: re-read failed ({e})");
            return false;
        }
    };
    match parse(&text) {
        Ok(c) => {
            if c == *get() {
                return false;
            }
            crate::log!("config: changed on disk, reloaded");
            set(c);
            true
        }
        Err(e) => {
            crate::log!("config: invalid change ({e}), keeping the previous one");
            false
        }
    }
}

/// Atomic read-modify-write of the config: applies `f` to a copy of the current snapshot,
/// writes the file (temp + rename) and, if that succeeds, updates the global snapshot.
/// Used by Pin/Unpin. Write errors -> `Err` (the snapshot stays unchanged).
pub fn modify<F: FnOnce(&mut Config)>(f: F) -> Result<(), String> {
    let _w = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut c = (*get()).clone();
    f(&mut c);
    write_file(&c)?;
    set(c);
    Ok(())
}

/// Atomically writes `c` to `config.json` (`.tmp` file in the same folder + rename).
/// Updates the global snapshot and the remembered mtime (so `poll_changed` does not re-read
/// our own write).
pub fn save(c: &Config) -> Result<(), String> {
    let _w = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    write_file(c)?;
    set(c.clone());
    Ok(())
}

/// If `config.json` exists but does not parse (the user is mid-edit or made a typo), copies it to
/// `config.json.bak` so the rewrite that follows does not destroy those edits.
fn backup_if_broken(path: &Path) -> Result<(), String> {
    let text = match read_text(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    if parse(&text).is_ok() {
        return Ok(());
    }
    let bak = path.with_extension("json.bak");
    crate::datadir::write_atomic(&bak, text.as_bytes(), crate::datadir::FileAcl::Inherit)
        .map_err(|e| format!("backup to {}: {e}", bak.display()))?;
    crate::log!("config: {} has errors, copied to {} before rewriting", path.display(), bak.display());
    Ok(())
}

/// Atomic write + update of the remembered mtime. Call with `WRITE_LOCK` held.
/// The new file gets the user-editable DACL (`datadir::FileAcl::UserEditable`) before it replaces
/// the old one, so hand-editing keeps working after every programmatic save.
fn write_file(c: &Config) -> Result<(), String> {
    if !crate::datadir::writable() {
        return Err("no writable data folder".into());
    }
    let path = crate::datadir::config_path().ok_or_else(|| "no data folder".to_string())?;
    let json = render(c);
    // Never lose unparsed user edits: refuse to write if they cannot be saved aside.
    backup_if_broken(&path)?;
    crate::datadir::write_atomic(&path, json.as_bytes(), crate::datadir::FileAcl::UserEditable)?;
    *lock_mtime() = file_mtime(&path);
    Ok(())
}

// ------------------------------------------------------------- module visibility

/// Names accepted by `Config::module_visible` / `set_module_visible`: the `modules_order` names
/// plus "tray" (`show_tray`). The power button is not listed: it is always shown. The Start button
/// ("start", `show_start_button`) is a fixed item, not a module: the two methods accept it but it is
/// not listed here (the settings popup adds its switch by hand).
pub const MODULE_NAMES: [&str; 7] = ["notifications", "volume", "network", "metrics", "clock", "show_desktop", "tray"];

impl Config {
    fn module_flag(&mut self, name: &str) -> Option<&mut bool> {
        Some(match name {
            "notifications" => &mut self.show_notifications,
            "volume" => &mut self.show_volume,
            "network" => &mut self.show_network,
            "metrics" => &mut self.show_metrics,
            "clock" => &mut self.show_clock,
            "show_desktop" => &mut self.show_desktop_button,
            "start" => &mut self.show_start_button,
            "tray" => &mut self.show_tray,
            _ => return None,
        })
    }

    /// true if module `name` (a `MODULE_NAMES` entry, "start" or "power") is shown. Unknown names: true.
    pub fn module_visible(&self, name: &str) -> bool {
        match name {
            "notifications" => self.show_notifications,
            "volume" => self.show_volume,
            "network" => self.show_network,
            "metrics" => self.show_metrics,
            "clock" => self.show_clock,
            "show_desktop" => self.show_desktop_button,
            "start" => self.show_start_button,
            "tray" => self.show_tray,
            _ => true,
        }
    }

    /// Shows/hides module `name` (a `MODULE_NAMES` entry or "start"). false if the name is unknown.
    pub fn set_module_visible(&mut self, name: &str, on: bool) -> bool {
        match self.module_flag(name) {
            Some(f) => {
                *f = on;
                true
            }
            None => false,
        }
    }
}

impl Config {
    /// Bar background alpha (0..=255) from `bar_opacity` (0..=100 %).
    pub fn bar_alpha(&self) -> u32 {
        (self.bar_opacity.min(100) as u32 * 255 + 50) / 100
    }

    /// The four sizes clamped to their ranges: (app icon px, app text px, tray icon px, module %).
    pub fn sizes(&self) -> (u8, u8, u8, u8) {
        (
            APP_ICON_SIZE.clamp(self.app_icon_size),
            APP_TEXT_SIZE.clamp(self.app_text_size),
            TRAY_ICON_SIZE.clamp(self.tray_icon_size),
            MODULE_SIZE.clamp(self.module_size),
        )
    }
}

// ------------------------------------------------------------------ pin / unpin

impl Config {
    /// Index of the pinned entry with this key (case-insensitive comparison).
    pub fn pinned_index(&self, key: &str) -> Option<usize> {
        self.pinned.iter().position(|p| p.key.eq_ignore_ascii_case(key))
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        self.pinned_index(key).is_some()
    }
}

/// Appends `app` to the pins and saves. If the key is already pinned it does nothing (Ok).
pub fn pin(app: PinnedApp) -> Result<(), String> {
    if get().is_pinned(&app.key) {
        return Ok(());
    }
    modify(|c| {
        if !c.is_pinned(&app.key) {
            c.pinned.push(app);
        }
    })
}

/// Updates the stored display name of the pin `key` (fallback name) when it differs from `name`
/// (the live resolver's result). No pin, empty `name` or same name: nothing is written.
pub fn sync_pinned_name(key: &str, name: &str) -> Result<(), String> {
    if name.trim().is_empty() || !pinned_name_differs(&get(), key, name) {
        return Ok(());
    }
    // A hand edit not yet picked up by `poll_changed` would be overwritten by the in-memory snapshot:
    // skip, the self-heal runs again at the next resolve.
    if external_edit_pending() {
        return Ok(());
    }
    modify(|c| set_pinned_name(c, key, name))
}

fn pinned_name_differs(c: &Config, key: &str, name: &str) -> bool {
    c.pinned_index(key).is_some_and(|i| c.pinned[i].name != name)
}

fn set_pinned_name(c: &mut Config, key: &str, name: &str) {
    if let Some(i) = c.pinned_index(key) {
        c.pinned[i].name = name.to_string();
    }
}

/// Removes the pin with this key and saves. If it does not exist nothing is written (Ok).
pub fn unpin(key: &str) -> Result<(), String> {
    if !get().is_pinned(key) {
        return Ok(());
    }
    modify(|c| c.pinned.retain(|p| !p.key.eq_ignore_ascii_case(key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_partial_uses_defaults() {
        let c = parse("{\"position\":\"top\",\"pinned\":[{\"key\":\"a\"}]}").unwrap();
        assert_eq!(c.position, Position::Top);
        assert!(c.autohide && c.show_date);
        assert_eq!(c.pinned[0].key, "a");
        assert!(c.pinned[0].icon.is_none());
    }

    #[test]
    fn removed_keys_are_ignored() {
        // The tray opt-in moved to HKLM: an old config that still has the key must parse.
        let c = parse("{\"trust_elevated_tray_icons\": true, \"show_tray\": false}").unwrap();
        assert!(!c.show_tray);
    }

    #[test]
    fn parse_bom_and_invalid() {
        assert!(parse("\u{feff}{}").is_ok());
        assert!(parse("{ not json").is_err());
    }

    #[test]
    fn comments_are_stripped_outside_strings() {
        let t = "// hi\n{ /* b */ \"names\": {\"a\": \"x // y \\\" /* z */\"} } // end";
        let c = parse(t).unwrap();
        assert_eq!(c.names["a"], "x // y \" /* z */");
    }

    #[test]
    fn template_roundtrip() {
        let mut c = Config::default();
        c.pinned.push(PinnedApp {
            key: "k".into(),
            name: "N".into(),
            launch: "shell:AppsFolder\\X".into(),
            icon: None,
        });
        c.names.insert("code.exe".into(), "VS Code".into());
        c.show_seconds = true;
        c.display_mode = DisplayMode::Text;
        c.position = Position::Top;
        c.app_icon_size = 24;
        c.app_text_size = 11;
        c.tray_icon_size = 20;
        c.module_size = 125;
        c.background = Background(0x2E1119);
        c.bar_opacity = 0;
        let r = render(&c);
        assert!(r.contains("Get-StartApps") && r.contains("\"background\": \"#2E1119\""));
        assert!(!r.contains("\"bar_size\":"));
        assert_eq!(parse(&r).unwrap(), c);
        assert_eq!(parse(&render(&Config::default())).unwrap(), Config::default());
    }

    #[test]
    fn order_fields_roundtrip() {
        let mut c = Config::default();
        c.app_order = vec!["a".into(), "b".into()];
        c.tray_order = vec!["x#1".into()];
        c.modules_order = vec!["power".into(), "clock".into()];
        let r = render(&c);
        assert!(r.contains("\"modules_order\""));
        assert_eq!(parse(&r).unwrap(), c);
        assert_eq!(Config::default().modules_order.len(), 7);
    }

    #[test]
    fn module_visibility() {
        let mut c = Config::default();
        assert!(c.module_visible("network") && !c.module_visible("metrics") && c.module_visible("power"));
        for n in MODULE_NAMES {
            assert!(c.set_module_visible(n, false));
            assert!(!c.module_visible(n));
        }
        assert!(!c.set_module_visible("power", false));
        // The Start button is a fixed item: not in MODULE_NAMES, but the switch accepts it.
        assert!(c.set_module_visible("start", false) && !c.show_start_button);
        c.show_metrics = true;
        c.show_desktop_button = false;
        c.show_start_button = false;
        let r = render(&c);
        assert!(r.contains("\"show_network\"") && r.contains("\"show_desktop_button\": false"));
        assert!(r.contains("\"show_start_button\": false"));
        assert_eq!(parse(&r).unwrap(), c);
    }

    #[test]
    fn pinned_name_sync() {
        let mut c = Config::default();
        c.pinned.push(PinnedApp { key: "Stremio".into(), name: "Freedom to Stream".into(), ..Default::default() });
        assert!(pinned_name_differs(&c, "stremio", "Stremio"));
        assert!(!pinned_name_differs(&c, "stremio", "Freedom to Stream"));
        assert!(!pinned_name_differs(&c, "other", "Stremio"));
        set_pinned_name(&mut c, "stremio", "Stremio");
        assert_eq!(c.pinned[0].name, "Stremio");
        assert!(!pinned_name_differs(&c, "stremio", "Stremio"));
    }

    #[test]
    fn bar_alpha_is_clamped() {
        let mut c = Config::default();
        assert_eq!(c.bar_alpha(), 242);
        c.bar_opacity = 100;
        assert_eq!(c.bar_alpha(), 255);
        c.bar_opacity = 0;
        assert_eq!(c.bar_alpha(), 0);
        c.bar_opacity = 50;
        assert_eq!(c.bar_alpha(), 128);
        c.bar_opacity = 200;
        assert_eq!(c.bar_alpha(), 255);
    }

    #[test]
    fn background_values() {
        let bg = |v: &str| parse(&format!("{{\"background\": \"{v}\", \"autohide\": false}}")).unwrap().background;
        assert_eq!(bg("black"), Background::BLACK);
        assert_eq!(bg("Dark"), Background::DARK);
        assert_eq!(bg("#1a2B3c"), Background(0x1A2B3C));
        assert_eq!(bg("#fff"), Background(0xFFFFFF));
        // Garbage falls back to black instead of rejecting the file.
        assert_eq!(bg("#12345"), Background::BLACK);
        assert_eq!(bg("teal"), Background::BLACK);
        assert_eq!(Background(0x0E1A33).hex(), "#0E1A33");
        // Presets are dark: white text stays readable (Rec. 601 brightness below 60 / 255).
        assert!(BACKGROUND_PRESETS.iter().all(|(_, b)| {
            let (r, g, b) = b.rgb();
            (r * 299 + g * 587 + b * 114) / 1000 < 60
        }));
    }

    #[test]
    fn legacy_bar_size_migrates() {
        let c = parse("{\"bar_size\": \"large\"}").unwrap();
        assert_eq!(c.sizes(), (20, 16, 20, 125));
        assert_eq!(c.bar_size, None);
        assert_eq!(parse("{\"bar_size\": \"small\"}").unwrap().sizes(), (14, 11, 14, 88));
        assert_eq!(parse("{\"bar_size\": \"medium\"}").unwrap(), Config::default());
        // New keys already set: they win.
        assert_eq!(parse("{\"bar_size\": \"large\", \"app_icon_size\": 30}").unwrap().sizes(), (30, 13, 16, 100));
        // Out-of-range values are clamped on use.
        assert_eq!(parse("{\"app_icon_size\": 200, \"module_size\": 5}").unwrap().sizes(), (32, 13, 16, 75));
    }

    #[test]
    fn roundtrip() {
        let mut c = Config::default();
        c.pinned.push(PinnedApp { key: "k".into(), name: "N".into(), launch: "L".into(), icon: Some("i.png".into()) });
        c.names.insert("code.exe".into(), "VS Code".into());
        assert_eq!(parse(&serde_json::to_string_pretty(&c).unwrap()).unwrap(), c);
    }
}

// ---------------------------------------------------- effective options (CLI > config)

/// Effective position: `--top`/`--bottom` if present, otherwise the config.
pub fn effective_position() -> Position {
    match crate::args().top {
        Some(true) => Position::Top,
        Some(false) => Position::Bottom,
        None => get().position,
    }
}

/// Effective auto-hide: `--no-autohide` forces false, otherwise the config.
pub fn effective_autohide() -> bool {
    !crate::args().no_autohide && get().autohide
}

/// Hide the native taskbar: `--keep-taskbar` forces false, otherwise the config.
pub fn effective_hide_native_taskbar() -> bool {
    !crate::args().keep_taskbar && get().hide_native_taskbar
}
