//! OFFSCREEN preview (`dock.exe --preview <folder>`): renders the bar and the popups with
//! FAKE data on the REAL drawing paths (`render::layout/paint`, `Content::size/paint` on `Surface`)
//! and writes PNGs (WIC, `icons::save_png`) composited over a wallpaper-like background, at DPI 120 and 96.
//!
//! Creates NO windows, does NOT touch taskbar/hook/single instance, does NOT call the system for data
//! (no shell/notif/tray threads). Meant for non-elevated designers iterating on the design:
//! build with `DOCK_NO_MANIFEST=1` (manifest without `requireAdministrator`).
//!
//! Files produced for each preview: `<name>.png` (DPI 120) and `<name>_96.png` (DPI 96).
//! Errors go to stderr (the async logger is not started in this mode).
//!
//! HOOKs used in other people's files (marked `PREVIEW-HOOK`): `render::snapshot_pixels`,
//! `Surface::snapshot_pixels`, `notif::preview_set_state`, `popup_tray::preview_new`,
//! `popup_settings::preview_new`, `picker::preview_pixels`.

use std::path::Path;
use std::sync::Arc;

use windows::Win32::Foundation::RECT;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::audio::{DeviceKind, OutputDevice, VolumeState};
use crate::config::{Background, Config, DisplayMode};
use crate::icons::{self, IconBitmap};
use crate::metrics::Usage;
use crate::net::{NetInfo, NetKind, NetStatus};
use crate::notif::{self, NotifAccess, NotifItem, NotifState};
use crate::popup::{Content, DataEvent};
use crate::popup_settings::Page;
use crate::render::{self, AppView, BarSizes, Indicator, ItemId, Lift, Metrics, Plate, Scene, SystemView, TrayIcon};
use crate::surface::Surface;
use crate::ui::Screen;

thread_local! {
    /// Bar style of the next `render_bar` (sizes, background, opacity): applied to the config of the
    /// bar; a no-op except for the style variants.
    static STYLE: std::cell::Cell<Style> = const { std::cell::Cell::new(no_style) };
}

/// A bar style variant: edits the config the bar is drawn with.
type Style = fn(&mut Config);

fn no_style(_: &mut Config) {}

/// Rendered DPIs and file suffix (120 is the "main" file).
const DPIS: [(u32, &str); 2] = [(120, ""), (96, "_96")];

// ------------------------------------------------------------------ mock icons

/// 5x7 glyph (rows of 5 bits, MSB on the left) for the letters used by the mock icons.
fn glyph5x7(c: char) -> [u8; 7] {
    match c {
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        _ => [0b11111; 7],
    }
}

/// Coverage (0..1) of a rounded square `size` x `size` at pixel `(x, y)` (SDF).
fn rounded_cov(x: i32, y: i32, size: i32, radius: f32) -> f32 {
    let half = size as f32 / 2.0;
    let (px, py) = (x as f32 + 0.5 - half, y as f32 + 0.5 - half);
    let (qx, qy) = (px.abs() - (half - radius), py.abs() - (half - radius));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    let d = outside + inside - radius;
    (0.5 - d).clamp(0.0, 1.0)
}

/// Mock icon `size` x `size`: rounded square `rgb` with a white letter. Premultiplied.
fn mock_icon(letter: char, rgb: u32, size: i32) -> IconBitmap {
    let size = size.max(8);
    let mut bmp = IconBitmap::new(size as u32, size as u32);
    let (r, g, b) = (((rgb >> 16) & 255) as f32, ((rgb >> 8) & 255) as f32, (rgb & 255) as f32);
    let rows = glyph5x7(letter);
    let lh = size as f32 * 0.56;
    let lw = lh * 5.0 / 7.0;
    let (gx0, gy0) = ((size as f32 - lw) / 2.0, (size as f32 - lh) / 2.0);
    for y in 0..size {
        for x in 0..size {
            let cov = rounded_cov(x, y, size, size as f32 * 0.26);
            if cov <= 0.0 {
                continue;
            }
            // Coverage of the letter: 4x4 subsamples mapped onto the 5x7 glyph.
            let mut lit = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let fx = x as f32 + (sx as f32 + 0.5) / 4.0;
                    let fy = y as f32 + (sy as f32 + 0.5) / 4.0;
                    let (u, v) = ((fx - gx0) / lw * 5.0, (fy - gy0) / lh * 7.0);
                    if (0.0..5.0).contains(&u)
                        && (0.0..7.0).contains(&v)
                        && rows[v as usize] >> (4 - u as usize) & 1 == 1
                    {
                        lit += 1;
                    }
                }
            }
            let l = lit as f32 / 16.0;
            let mix = |c: f32| (c * (1.0 - l) + 255.0 * l) * cov;
            let a = (cov * 255.0 + 0.5) as u32;
            bmp.pixels[(y * size + x) as usize] =
                (a << 24) | ((mix(r) + 0.5) as u32) << 16 | ((mix(g) + 0.5) as u32) << 8 | (mix(b) + 0.5) as u32;
        }
    }
    bmp
}

// ------------------------------------------------------------------ composition

/// Opaque wallpaper-like background: gradient #2a2f3a -> #10131a plus a light "window" in the
/// right half, to make transparency and border visible on both dark and light.
/// `window_top` = ordinate where the light window starts.
fn wallpaper(w: i32, h: i32, window_top: i32) -> Vec<u32> {
    let mut out = vec![0u32; (w * h) as usize];
    let (c0, c1) = ((0x2au32, 0x2fu32, 0x3au32), (0x10u32, 0x13u32, 0x1au32));
    for y in 0..h {
        let t = y as f32 / (h - 1).max(1) as f32;
        let lerp = |a: u32, b: u32| (a as f32 + (b as f32 - a as f32) * t + 0.5) as u32;
        let (mut r, mut g, mut b) = (lerp(c0.0, c1.0), lerp(c0.1, c1.1), lerp(c0.2, c1.2));
        for x in 0..w {
            if x >= w * 55 / 100 && y >= window_top {
                // Light window with a slightly darker title bar.
                let (wr, wg, wb) = if y < window_top + 28 { (0xb8, 0xbe, 0xca) } else { (0xe3, 0xe6, 0xec) };
                (r, g, b) = (wr, wg, wb);
            }
            out[(y * w + x) as usize] = 0xFF00_0000 | (r << 16) | (g << 8) | b;
        }
    }
    out
}

/// Composites `src` (premultiplied) onto `bg` (opaque) at `(ox, oy)`.
#[allow(clippy::too_many_arguments)] // preview helper: explicit coordinates
fn composite(bg: &mut [u32], bw: i32, bh: i32, src: &[u32], sw: i32, sh: i32, ox: i32, oy: i32) {
    for y in 0..sh {
        let dy = oy + y;
        if dy < 0 || dy >= bh {
            continue;
        }
        for x in 0..sw {
            let dx = ox + x;
            if dx < 0 || dx >= bw {
                continue;
            }
            let s = src[(y * sw + x) as usize];
            let a = s >> 24;
            if a == 0 {
                continue;
            }
            let d = &mut bg[(dy * bw + dx) as usize];
            let inv = 255 - a;
            let ch = |sh_: u32| {
                let sc = (s >> sh_) & 255;
                let dc = (*d >> sh_) & 255;
                (sc + (dc * inv + 127) / 255).min(255)
            };
            *d = 0xFF00_0000 | ch(16) << 16 | ch(8) << 8 | ch(0);
        }
    }
}

/// Writes `pixels` (opaque) as a PNG in `dir/file`.
fn write_png(dir: &Path, file: &str, w: i32, h: i32, pixels: Vec<u32>) -> bool {
    let bmp = IconBitmap { width: w as u32, height: h as u32, pixels };
    let path = dir.join(file);
    if icons::save_png(&path, &bmp) {
        println!("written {}", path.display());
        true
    } else {
        eprintln!("preview: writing {} failed", path.display());
        false
    }
}

// ------------------------------------------------------------------ mock data

struct MockIcons {
    apps: Vec<IconBitmap>,
    tray: Vec<IconBitmap>,
}

/// (name, letter, color, indicator, dimmed)
const APPS: [(&str, char, u32, Indicator, bool); 4] = [
    ("Discord", 'D', 0x5865F2, Indicator::Active, false),
    ("Steam", 'S', 0x2A6A93, Indicator::Open, false),
    ("Firefox", 'F', 0xE8590C, Indicator::Open, false),
    ("VS Code", 'V', 0x0E7AC4, Indicator::None, true),
];

/// Mock tray icons: the first `TRAY_BAR` are shown in the bar, the rest are hidden by the user (overflow).
const TRAY: [(char, u32); 9] = [
    ('T', 0x6264A7),
    ('S', 0x8E3A9B),
    ('O', 0x0F6CBD),
    ('B', 0x0082FC),
    ('N', 0x5E9400),
    ('W', 0x2E8B57),
    ('C', 0x3AA0D8),
    ('M', 0xD44638),
    ('P', 0x7A7F8A),
];
const TRAY_BAR: usize = 6;
/// Names of the mock tray apps (same order as `TRAY`).
const TRAY_NAMES: [&str; 9] =
    ["Teams", "Slack", "OneDrive", "Bluetooth", "NVIDIA", "Wi-Fi", "Cloud Sync", "Mail", "Printer"];

/// Mock icons at the sizes `s` (app and tray icons at their exact pixel size, like the shell
/// thread and the tray host deliver them).
fn mock_icons(m: Metrics, s: BarSizes) -> MockIcons {
    MockIcons {
        apps: APPS.iter().map(|a| mock_icon(a.1, a.2, s.icon_px(m))).collect(),
        tray: TRAY.iter().map(|t| mock_icon(t.0, t.1, s.tray_px(m))).collect(),
    }
}

fn mock_notifications() -> NotifState {
    let item = |id: u32, app: &str, title: &str, body: &str, time: &str| NotifItem {
        id,
        app: app.to_string(),
        title: title.to_string(),
        body: body.to_string(),
        time_ft: 0,
        time_text: time.to_string(),
        icon_key: format!("name:{app}"),
        aumid: String::new(),
    };
    NotifState {
        access: NotifAccess::Allowed,
        items: vec![
            item(
                1,
                "Discord",
                "Marco",
                "See you tonight at 9 on the server? I will bring the new game, already downloaded everything.",
                "14:28",
            ),
            item(2, "Mail", "Weekly summary", "You have 12 new unread messages in your Inbox.", "13:05"),
            item(3, "Windows Update", "Updates available", "", "yesterday 22:41"),
        ],
        dnd: Some(false),
    }
}

fn mock_outputs() -> Vec<OutputDevice> {
    let dev = |id: &str, name: &str, d: bool, kind: DeviceKind| OutputDevice {
        id: id.to_string(),
        name: name.to_string(),
        is_default: d,
        kind,
    };
    vec![
        dev("a", "Speakers (Realtek Audio)", true, DeviceKind::Speaker),
        dev("b", "Headphones (Bluetooth WH-1000XM5)", false, DeviceKind::Headphones),
        dev("c", "Headset (USB Gaming Headset)", false, DeviceKind::Headset),
        dev("d", "Monitor (NVIDIA High Definition Audio)", false, DeviceKind::Display),
        dev("e", "Line Out (Realtek Audio)", false, DeviceKind::LineOut),
        dev("f", "Digital Output (S/PDIF)", false, DeviceKind::Digital),
    ]
}

/// Mock network: Wi-Fi with a good signal and internet.
fn mock_net() -> NetInfo {
    NetInfo {
        kind: NetKind::Wifi,
        name: "Casa-5G".to_string(),
        signal: 72,
        ipv4: "192.168.1.42".to_string(),
        internet: true,
    }
}

// ------------------------------------------------------------------ bar

/// Moves the app items of `layout` as in the middle of a drag: Steam (app 1) is lifted and hovers
/// over the gap that opened between Firefox and VS Code, Firefox already slid into Steam's old place.
fn mock_drag(layout: &mut render::Layout) {
    // The Start button, when on, comes first: the apps begin at the first App item.
    let b = layout.items.iter().position(|i| matches!(i.id, ItemId::App(_))).unwrap_or(0);
    let order = [0usize, 2, 1, 3]; // base indices in their new visual order (slot 2 = the gap)
    let w: Vec<i32> = (0..4).map(|k| layout.items[b + k].rect.right - layout.items[b + k].rect.left).collect();
    let x0 = layout.items[b].rect.left;
    let mut x = x0;
    let mut lefts = [0i32; 4];
    for k in order {
        lefts[k] = x;
        x += w[k];
    }
    // The dragged item floats a little to the right of the open gap, following the cursor.
    lefts[1] += w[1] / 10;
    for k in 0..4 {
        layout.items[b + k].rect.left = lefts[k];
        layout.items[b + k].rect.right = lefts[k] + w[k];
    }
}

/// Renders the bar with the hover plate on `hover` (if any) in the given display mode.
/// `attention` = index of the app whose indicator is the orange "needs attention" bar.
/// `drag` = mid-drag frame (app 1 lifted over an open gap). `top` = bar at the top of the screen.
/// Returns the pixels and the rectangle of `hover` in the bar.
fn render_bar(
    m: Metrics,
    hover: Option<ItemId>,
    mode: DisplayMode,
    attention: Option<usize>,
    drag: bool,
    net: NetStatus,
    top: bool,
) -> Option<(i32, i32, Vec<u32>, Option<RECT>)> {
    // The renderer reads the display mode from the global config snapshot (in memory only, never saved).
    // The metrics module (off by default) is switched on so the preview shows every module.
    let mut cfg = Config { display_mode: mode, show_metrics: true, ..Config::default() };
    STYLE.with(|s| s.get())(&mut cfg);
    let sizes = BarSizes::from_config(&cfg);
    let modules = render::visible_modules(&cfg);
    let cfg_start = cfg.show_start_button;
    crate::config::set(cfg);
    if let Err(e) = render::init(m, sizes) {
        eprintln!("preview: render::init failed: {e}");
        return None;
    }
    let ic = mock_icons(m, sizes);
    let views: Vec<AppView<'_>> = APPS
        .iter()
        .zip(&ic.apps)
        .enumerate()
        .map(|(i, (a, bmp))| AppView {
            name: a.0,
            icon: Some(bmp),
            indicator: if attention == Some(i) { Indicator::Attention } else { a.3 },
            dimmed: a.4,
            active_t: if a.3 == Indicator::Active { 1.0 } else { 0.0 },
        })
        .collect();
    let tray: Vec<TrayIcon<'_>> =
        ic.tray.iter().enumerate().take(TRAY_BAR).map(|(i, bmp)| TrayIcon { id: i as u64 + 1, icon: bmp }).collect();
    let mut scene = Scene {
        metrics: m,
        sizes,
        apps: &views,
        tray: &tray,
        overflow_enabled: true,
        force_chevron: true,
        bar_top: top,
        system: SystemView {
            volume: VolumeState { level: 60, muted: false, available: true },
            show_volume_percent: true,
            time: "14:32",
            date: Some("Fri 23 Oct"),
            bell_badge: true,
            bell_dnd: false,
            net,
            usage: Usage { cpu: 12, ram: 48 },
        },
        plate: None,
        modules: &modules,
        start_button: cfg_start,
        lift: None,
        hover,
        max_width: m.px(1920 - 2 * render::EDGE_MARGIN),
    };
    let mut layout = render::layout(&scene);
    let hover_rect = hover.and_then(|id| layout.rect_of(id));
    if drag {
        mock_drag(&mut layout);
        scene.lift = Some(Lift { id: ItemId::App(1), alpha: crate::surface::PRESS_ALPHA as u8 });
        scene.hover = Some(ItemId::App(1));
    } else if let Some(r) = hover.and_then(|id| layout.rect_of(id)) {
        scene.plate = Some(Plate { rect: render::plate_rect(r, m), alpha: crate::surface::HOVER_ALPHA as u8 });
    }
    let out = match render::resize(layout.width, layout.height) {
        Ok(()) => {
            render::paint(&scene, &layout);
            render::snapshot_pixels().map(|(w, h, px)| (w, h, px, hover_rect))
        }
        Err(e) => {
            eprintln!("preview: render::resize failed: {e}");
            None
        }
    };
    render::destroy();
    out
}

/// Bar on a wallpaper, written to `file`.
#[allow(clippy::too_many_arguments)] // preview helper: one flag per variant
fn bar_png(
    dir: &Path,
    file: &str,
    m: Metrics,
    hover: Option<ItemId>,
    mode: DisplayMode,
    attention: Option<usize>,
    drag: bool,
    net: NetStatus,
) -> bool {
    let Some((bw, bh, px, _)) = render_bar(m, hover, mode, attention, drag, net, true) else { return false };
    let pad = m.px(48);
    let (cw, ch) = (bw + 2 * pad, bh + m.px(150));
    let by = m.px(render::EDGE_MARGIN);
    let mut bg = wallpaper(cw, ch, by + bh / 2);
    composite(&mut bg, cw, ch, &px, bw, bh, pad, by);
    write_png(dir, file, cw, ch, bg)
}

/// Bar at the bottom with Firefox hovered and the window picker open above it: three cards with
/// placeholder boxes where the DWM would compose the live thumbnails.
fn picker_png(dir: &Path, file: &str, m: Metrics) -> bool {
    let firefox = ItemId::App(2);
    let Some((bw, bh, px, Some(item))) =
        render_bar(m, Some(firefox), DisplayMode::IconText, None, false, mock_net().status(), false)
    else {
        return false;
    };
    let titles = [
        "The Rust Programming Language - Mozilla Firefox",
        "Pull requests - GitHub - Mozilla Firefox",
        "Weather forecast for the week ahead in Turin, Italy - Mozilla Firefox",
    ];
    let sources = [Some((1920, 1080)), Some((1280, 1024)), Some((2560, 1080))];
    let Some((pw, ph, pp)) = crate::picker::preview_pixels(m, &titles, &sources, bw, Some(1)) else {
        eprintln!("preview: picker surface failed");
        return false;
    };
    let pad = m.px(48);
    let gap = m.px(crate::popup::POPUP_GAP);
    let margin = m.px(render::EDGE_MARGIN);
    let (cw, ch) = (bw + 2 * pad, ph + gap + bh + margin + m.px(40));
    let by = ch - margin - bh;
    let cx = pad + (item.left + item.right) / 2;
    let px_ = (cx - pw / 2).clamp(m.px(8), (cw - m.px(8) - pw).max(m.px(8)));
    let mut bg = wallpaper(cw, ch, m.px(20));
    composite(&mut bg, cw, ch, &px, bw, bh, pad, by);
    composite(&mut bg, cw, ch, &pp, pw, ph, px_, by - gap - ph);
    write_png(dir, file, cw, ch, bg)
}

// ------------------------------------------------------------------ popup

/// Id of the `nth` control (in scan order from the top left) of the content.
fn pick_hot(c: &dyn Content, w: i32, h: i32, step: i32, nth: usize) -> Option<u32> {
    let mut seen: Vec<u32> = Vec::new();
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            if let Some(id) = c.hit(x, y) {
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            x += step;
        }
        y += step;
    }
    seen.get(nth).copied().or_else(|| seen.last().copied())
}

/// Popup on a background, written to `file`. `nth` = hovered control (scan index).
fn popup_png(dir: &Path, file: &str, m: Metrics, mut content: Box<dyn Content>, nth: usize) -> bool {
    let mut surf = match Surface::new(m) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("preview: Surface::new failed: {e}");
            return false;
        }
    };
    let (w, h) = content.size(&surf);
    if let Err(e) = surf.resize(w.max(1), h.max(1)) {
        eprintln!("preview: Surface::resize failed: {e}");
        return false;
    }
    let hot = pick_hot(content.as_ref(), w, h, m.px(4).max(2), nth);
    content.paint(&mut surf, hot, None);
    let Some((sw, sh, px)) = surf.snapshot_pixels() else {
        eprintln!("preview: no pixels from the popup surface");
        return false;
    };
    let pad = m.px(40);
    let (cw, ch) = (sw + 2 * pad, sh + 2 * pad);
    let mut bg = wallpaper(cw, ch, ch * 40 / 100);
    composite(&mut bg, cw, ch, &px, sw, sh, pad, pad);
    write_png(dir, file, cw, ch, bg)
}

/// Builds the content of popup `name` (mock data).
fn make_popup(name: &str, m: Metrics) -> Option<Box<dyn Content>> {
    Some(match name {
        "popup_notif" => {
            let icons = [("Discord", 'D', 0x5865F2), ("Mail", 'M', 0x0F6CBD), ("Windows Update", 'W', 0x2E8B57)]
                .into_iter()
                .map(|(n, l, c)| (format!("name:{n}"), Arc::new(mock_icon(l, c, m.px(32)))))
                .collect();
            notif::preview_set_state(mock_notifications(), icons);
            crate::popup_notif::new()
        }
        "popup_audio" => {
            let mut c = crate::popup_audio::new(VolumeState { level: 60, muted: false, available: true });
            c.data(&DataEvent::AudioOutputs(mock_outputs()));
            c
        }
        "popup_calendar" => crate::popup_calendar::new(),
        "popup_network" => crate::popup_network::new(mock_net()),
        "popup_network_offline" => crate::popup_network::new(NetInfo {
            kind: NetKind::Ethernet,
            name: "Ethernet".to_string(),
            ipv4: "10.0.0.7".to_string(),
            internet: false,
            ..Default::default()
        }),
        "popup_power" => crate::popup_power::new(),
        // No displays passed: no "Screen" row, whatever monitors this machine has.
        "popup_settings" => crate::popup_settings::preview_with_screens(Vec::new()),
        "popup_settings_navy" => {
            crate::config::set(Config {
                background: Background(0x0E1A33),
                bar_opacity: 80,
                show_metrics: true,
                autohide: false,
                ..Config::default()
            });
            crate::popup_settings::preview_with_screens(Vec::new())
        }
        "popup_settings_screen" | "popup_settings_pointer" => {
            // Two mock displays, bar moved to the second one (or following the pointer).
            let monitor = if name == "popup_settings_pointer" {
                crate::config::MONITOR_POINTER.to_string()
            } else {
                "\\\\.\\DISPLAY2".to_string()
            };
            crate::config::set(Config { monitor, ..Config::default() });
            let d = |device: &str, label, primary| Screen { device: device.into(), label, primary };
            crate::popup_settings::preview_with_screens(vec![
                d("\\\\.\\DISPLAY1", "1", true),
                d("\\\\.\\DISPLAY2", "2", false),
            ])
        }
        "popup_settings_background" => {
            crate::config::set(Config { background: Background(0x2A1F3D), bar_opacity: 60, ..Config::default() });
            crate::popup_settings::preview_new(Page::Background, true)
        }
        "popup_settings_presets" => {
            crate::config::set(Config { background: Background(0x1A222D), ..Config::default() });
            crate::popup_settings::preview_new(Page::Background, false)
        }
        "popup_settings_sizes" => {
            crate::config::set(Config { app_icon_size: 24, module_size: 125, ..Config::default() });
            crate::popup_settings::preview_new(Page::Sizes, false)
        }
        "popup_app" => crate::popup::popup_app::new(crate::popup::AppMenuInfo {
            key: "discord".to_string(),
            name: "Discord".to_string(),
            pinned: false,
            windows: 2,
            can_launch: true,
        }),
        "popup_start" => crate::popup::popup_start::preview_new(),
        "popup_startmenu" | "popup_startmenu_search" => {
            const APPS: [(&str, char, u32); 20] = [
                ("Adobe Acrobat", 'A', 0xB30B00),
                ("Calculator", 'C', 0x3A3A3C),
                ("Camera", 'C', 0x2B5797),
                ("Discord", 'D', 0x5865F2),
                ("File Explorer", 'E', 0xE8A317),
                ("Firefox", 'F', 0xE66000),
                ("GIMP", 'G', 0x5C5543),
                ("Mail", 'M', 0x0F6CBD),
                ("Microsoft Edge", 'E', 0x0C7CD5),
                ("Notepad", 'N', 0x2D7D9A),
                ("Paint", 'P', 0xC239B3),
                ("Photos", 'P', 0x0063B1),
                ("Settings", 'S', 0x4C4A48),
                ("Spotify", 'S', 0x1DB954),
                ("Steam", 'S', 0x1B2838),
                ("Terminal", 'T', 0x2F2F2F),
                ("Visual Studio Code", 'V', 0x007ACC),
                ("VLC media player", 'V', 0xFF8800),
                ("Windows Security", 'W', 0x107C10),
                ("Xbox", 'X', 0x107C10),
            ];
            let pins = [
                "firefox",
                "visual studio code",
                "terminal",
                "spotify",
                "steam",
                "discord",
                "file explorer",
                "settings",
            ];
            crate::config::set(Config {
                start_pins: pins.iter().map(|p| p.to_string()).collect(),
                ..Config::default()
            });
            let apps =
                APPS.iter().map(|&(n, l, c)| (n.to_lowercase(), n.to_string(), mock_icon(l, c, m.px(32)))).collect();
            let query = if name == "popup_startmenu_search" { "te" } else { "" };
            crate::popup::popup_startmenu::preview_new(apps, query)
        }
        "popup_tray" | "popup_tray_customize" => {
            let ic = mock_icons(m, BarSizes::default());
            let bmps: Vec<Arc<IconBitmap>> = ic.tray.into_iter().map(Arc::new).collect();
            let cells = bmps.iter().enumerate().skip(TRAY_BAR).map(|(i, b)| (i as u64 + 1, Some(b.clone()))).collect();
            // Chooser list: all apps sorted by name; the last 3 are hidden from the bar.
            let mut rows: Vec<(String, String, Option<Arc<IconBitmap>>, bool)> = TRAY_NAMES
                .iter()
                .zip(&bmps)
                .enumerate()
                .map(|(i, (n, b))| (n.to_lowercase(), n.to_string(), Some(b.clone()), i < TRAY_BAR))
                .collect();
            rows.sort_by_key(|r| r.1.to_lowercase());
            crate::popup_tray::preview_new(cells, rows, name == "popup_tray_customize")
        }
        _ => return None,
    })
}

// ------------------------------------------------------------------ entry point

/// Entry point of `--preview <dir>`. Returns the exit code (0 = everything written).
pub fn run(dir: &str) -> i32 {
    let dir = Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("preview: cannot create {}: {e}", dir.display());
        return 1;
    }
    // SAFETY: initializes COM (STA) on the current thread for WIC; no preconditions.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        eprintln!("preview: CoInitializeEx failed: {hr:?}");
        return 1;
    }
    crate::calendar::set_preview(true);
    let wifi = mock_net().status();
    let mut failures = 0;
    for (dpi, suffix) in DPIS {
        let m = Metrics { dpi };
        // Entry App(1) = "Steam"; tray icon no. 2 (id 2) for the tray hover.
        for (name, mode) in [
            ("bar_top_icon_text", DisplayMode::IconText),
            ("bar_top_icon", DisplayMode::Icon),
            ("bar_top_text", DisplayMode::Text),
        ] {
            failures += i32::from(!bar_png(
                dir,
                &format!("{name}{suffix}.png"),
                m,
                Some(ItemId::App(1)),
                mode,
                None,
                false,
                wifi,
            ));
        }
        // Firefox (app 2) asks for attention: orange blinking bar (visible phase).
        failures += i32::from(!bar_png(
            dir,
            &format!("bar_attention{suffix}.png"),
            m,
            None,
            DisplayMode::IconText,
            Some(2),
            false,
            wifi,
        ));
        // Mid-drag frame: Steam lifted over the gap that opened between Firefox and VS Code.
        failures += i32::from(!bar_png(
            dir,
            &format!("bar_drag{suffix}.png"),
            m,
            None,
            DisplayMode::IconText,
            None,
            true,
            wifi,
        ));
        // Network variants: ethernet without internet (hovered), offline Wi-Fi, weak Wi-Fi.
        for (name, net, hover) in [
            ("bar_net_ethernet", NetStatus { kind: NetKind::Ethernet, bars: 0, internet: false }, Some(ItemId::Network)),
            ("bar_net_offline", NetStatus::default(), None),
            ("bar_net_weak", NetStatus { kind: NetKind::Wifi, bars: 1, internet: true }, None),
        ] {
            failures += i32::from(!bar_png(
                dir,
                &format!("{name}{suffix}.png"),
                m,
                hover,
                DisplayMode::Icon,
                None,
                false,
                net,
            ));
        }
        // Style variants: transparent background, coloured background, independent sizes.
        let styles: [(&str, Style); 5] = [
            ("bar_opacity_0", |c| c.bar_opacity = 0),
            ("bar_colour", |c| {
                c.background = Background(0x261630);
                c.bar_opacity = 90;
            }),
            ("bar_big_icons", |c| {
                c.app_icon_size = 28;
                c.app_text_size = 11;
            }),
            ("bar_big_modules", |c| {
                c.module_size = 150;
                c.tray_icon_size = 22;
            }),
            ("bar_sizes_small", |c| {
                c.app_icon_size = 14;
                c.app_text_size = 10;
                c.tray_icon_size = 12;
                c.module_size = 75;
            }),
        ];
        for (name, style) in styles {
            STYLE.with(|s| s.set(style));
            failures += i32::from(!bar_png(
                dir,
                &format!("{name}{suffix}.png"),
                m,
                None,
                DisplayMode::IconText,
                None,
                false,
                wifi,
            ));
            STYLE.with(|s| s.set(no_style));
        }
        failures += i32::from(!bar_png(
            dir,
            &format!("bar_hover_tray{suffix}.png"),
            m,
            Some(ItemId::Tray(2)),
            DisplayMode::IconText,
            None,
            false,
            wifi,
        ));
        crate::config::set(Config::default());
        failures += i32::from(!picker_png(dir, &format!("popup_windows{suffix}.png"), m));
        // (name, index of the hovered control)
        for (name, nth) in [
            ("popup_notif", 1),
            ("popup_audio", 3),
            ("popup_calendar", 8),
            ("popup_network", 0),
            ("popup_network_offline", 9),
            ("popup_power", 4),
            ("popup_settings", 3),
            ("popup_settings_navy", 2),
            ("popup_settings_screen", 2),
            ("popup_settings_pointer", 2),
            ("popup_settings_background", 4),
            ("popup_settings_presets", 3),
            ("popup_settings_sizes", 5),
            ("popup_app", 1),
            ("popup_start", 4),
            ("popup_startmenu", 12),
            ("popup_startmenu_search", 1),
            ("popup_tray", 1),
            ("popup_tray_customize", 2),
        ] {
            // Popups read the background tone from the config: every one starts from the defaults.
            crate::config::set(Config::default());
            let ok = match make_popup(name, m) {
                Some(c) => popup_png(dir, &format!("{name}{suffix}.png"), m, c, nth),
                None => false,
            };
            failures += i32::from(!ok);
        }
    }
    if failures > 0 {
        eprintln!("preview: {failures} previews not written");
        return 1;
    }
    0
}
