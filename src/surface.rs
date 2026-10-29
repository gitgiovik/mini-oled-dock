//! Drawing surface for the POPUPs: own premultiplied 32-bit DIB + `UpdateLayeredWindow` (or `BitBlt` for
//! the non-layered window picker),
//! with the same primitives as the bar (AA rounded rect, `IconBitmap` blit, 16x16 glyphs with
//! supersampling, hand-composited grayscale GDI text) but WITHOUT global state: each open popup has
//! its own `Surface` (only one at a time).
//!
//! The reference code is `render.rs` (same formulas: `div255`, `argb`, `over`, `rr_cov`,
//! `draw_text` with coverage from the G channel): this is a local adapted copy (`render.rs` belongs
//! to "bar" and must not be touched).
//!
//! THREADING: UI thread only (GDI objects of the thread).

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, DeleteDC, DeleteObject, DrawTextW, GdiFlush,
    GetDC, GetTextExtentPoint32W, GetTextFaceW, GetTextMetricsW, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_LEFT, DT_NOCLIP, DT_NOPREFIX, DT_SINGLELINE, DT_TOP, HBITMAP, HDC, HFONT,
    HGDIOBJ, LOGFONTW, SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::icons::IconBitmap;
use crate::log;
use crate::render::Metrics;

// --------------------------------------------------------------- style

/// Corner radius of the panel (logical px).
pub const PANEL_RADIUS: i32 = 14;
/// Alpha of the panel's black background (more opaque than the bar: it holds long text).
pub const PANEL_BG_ALPHA: u32 = 245;
/// Alpha of the 1 px white border.
pub const PANEL_BORDER_ALPHA: u32 = 22;
/// Inner padding of the panel (logical px).
pub const PANEL_PAD: i32 = 12;
/// Radius of the hover/press plates inside the popups (logical px).
pub const ROW_RADIUS: i32 = 7;
/// Alpha of the hover / press plate (same as the bar).
pub const HOVER_ALPHA: u32 = 20;
pub const PRESS_ALPHA: u32 = 36;
/// Inner 1 px border of the hover plates (same as the bar).
pub const HOVER_RIM_ALPHA: u32 = 18;
/// Text column of all popups (logical px from the panel edge): row inset 8 + 12.
pub const TEXT_COL: i32 = 20;
/// Alpha of primary / secondary / tertiary text (white).
pub const TEXT_ALPHA: u32 = 230;
pub const TEXT2_ALPHA: u32 = 170;
pub const TEXT3_ALPHA: u32 = 110;
/// Alpha of the 1 px horizontal separators.
pub const SEP_ALPHA: u32 = 30;

/// White color, for the callers' convenience.
pub const WHITE: (u32, u32, u32) = (255, 255, 255);

/// Fonts available on the surface ("Segoe UI Variable Text", fallback "Segoe UI";
/// "Segoe UI Variable Display" for `Big`, fallback "Segoe UI Light").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Font {
    /// 11 px, weight 400: times, subtitles, day headers.
    Small,
    /// 13 px, weight 400: normal text of the rows.
    Normal,
    /// 13 px, weight 600: titles (app name of a notification, month title).
    Bold,
    /// 32 px, weight 300: big time of the calendar popup.
    Big,
}

impl Font {
    fn idx(self) -> usize {
        match self {
            Font::Small => 0,
            Font::Normal => 1,
            Font::Bold => 2,
            Font::Big => 3,
        }
    }
}

/// 32-bit top-down DIB selected into a DC.
struct Dib {
    bmp: HBITMAP,
    /// Bitmap that was selected in the DC before ours (to restore before deleting the DIB).
    old: HGDIOBJ,
    bits: *mut u32,
    w: i32,
    h: i32,
}

/// DIB + DC + fonts of a popup.
pub struct Surface {
    metrics: Metrics,
    /// DC holding the main DIB (source of `UpdateLayeredWindow`).
    dc_main: HDC,
    /// DC holding the scratch DIB for text.
    dc_txt: HDC,
    fonts: [HFONT; 4],
    lhs: [i32; 4],
    main: Option<Dib>,
    txt: Option<Dib>,
    w: i32,
    h: i32,
    /// Clipping rectangle (physical px) applied to every write; `None` = the whole DIB.
    clip: Option<RECT>,
}

// ------------------------------------------------------------- GDI resources

/// Pixels of a DIB as a slice.
/// Invariant: the bitmap (w*h*4 bytes) is alive as long as the `Dib` exists; the slice must be used in short
/// scopes and must not coexist with GDI calls that write into the same DIB (GDI only writes into the
/// scratch DIB for text, and never while a slice of the latter is being read).
fn dib_pixels<'a>(d: &Dib) -> &'a mut [u32] {
    // SAFETY: see the invariant above; `bits` points to w*h u32 allocated by CreateDIBSection.
    unsafe { std::slice::from_raw_parts_mut(d.bits, (d.w as usize) * (d.h as usize)) }
}

fn make_dib(dc: HDC, w: i32, h: i32) -> Result<Dib, String> {
    if w <= 0 || h <= 0 {
        return Err(format!("invalid DIB dimensions {w}x{h}"));
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bmi` is valid; `bits` receives the pointer to the buffer owned by the bitmap.
    let bmp = unsafe { CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|e| format!("CreateDIBSection {w}x{h}: {e}"))?;
    if bits.is_null() {
        // SAFETY: `bmp` was just created and is not selected anywhere.
        let _ = unsafe { DeleteObject(HGDIOBJ(bmp.0)) };
        return Err("CreateDIBSection: null bits".into());
    }
    // SAFETY: `dc` and `bmp` are valid; the previous bitmap is saved for restoring.
    let old = unsafe { SelectObject(dc, HGDIOBJ(bmp.0)) };
    Ok(Dib { bmp, old, bits: bits.cast::<u32>(), w, h })
}

fn free_dib(dc: HDC, d: Dib) {
    // SAFETY: `d` was created by `make_dib` with this same `dc`; the original bitmap is restored
    // before deleting ours (a selected bitmap cannot be deleted).
    unsafe {
        SelectObject(dc, d.old);
        let _ = DeleteObject(HGDIOBJ(d.bmp.0));
    }
}

fn make_font(face: &str, height_px: i32, weight: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -height_px,
        lfWeight: weight,
        lfCharSet: DEFAULT_CHARSET,
        lfQuality: ANTIALIASED_QUALITY, // grayscale, no ClearType
        ..Default::default()
    };
    for (d, s) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *d = s;
    }
    // SAFETY: `lf` is fully initialized.
    unsafe { CreateFontIndirectW(&lf) }
}

/// Creates the font trying the `faces` in order. GDI silently substitutes a missing font, so
/// the actual name is verified with `GetTextFaceW`; "Segoe UI" goes last without verification.
fn create_font(dc: HDC, faces: &[&str], height_px: i32, weight: i32) -> Result<HFONT, String> {
    for face in faces {
        let f = make_font(face, height_px, weight);
        if f.is_invalid() {
            continue;
        }
        let mut buf = [0u16; 64];
        // SAFETY: `dc` is valid; the font is selected only for the check and then restored.
        let got = unsafe {
            let old = SelectObject(dc, HGDIOBJ(f.0));
            let n = GetTextFaceW(dc, Some(&mut buf));
            SelectObject(dc, old);
            n
        };
        let name = String::from_utf16_lossy(&buf[..(got.max(0) as usize).min(buf.len())]);
        if name.trim_end_matches('\0').eq_ignore_ascii_case(face) {
            return Ok(f);
        }
        // SAFETY: `f` was created above and is not selected.
        let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
    }
    let f = make_font("Segoe UI", height_px, weight);
    if f.is_invalid() {
        Err("CreateFontIndirectW failed".into())
    } else {
        Ok(f)
    }
}

fn line_height_of(dc: HDC, font: HFONT) -> i32 {
    let mut tm = TEXTMETRICW::default();
    // SAFETY: `dc` is valid, `tm` is writable; the font is restored right away.
    let ok = unsafe {
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let ok = GetTextMetricsW(dc, &mut tm);
        SelectObject(dc, old);
        ok
    };
    if ok.as_bool() && tm.tmHeight > 0 {
        tm.tmHeight
    } else {
        17
    }
}

fn delete_fonts(fonts: &[HFONT; 4]) {
    for f in fonts {
        if !f.is_invalid() {
            // SAFETY: font created by `create_font`, never left selected in a DC.
            let _ = unsafe { DeleteObject(HGDIOBJ(f.0)) };
        }
    }
}

fn build_fonts(dc: HDC, m: Metrics) -> Result<([HFONT; 4], [i32; 4]), String> {
    let text = ["Segoe UI Variable Text", "Segoe UI"];
    let display = ["Segoe UI Variable Display", "Segoe UI Light"];
    let mut fonts = [HFONT::default(); 4];
    let specs: [(&[&str], i32, i32); 4] =
        [(&text, m.px(11), 400), (&text, m.px(13), 400), (&text, m.px(13), 600), (&display, m.px(32), 300)];
    for (i, (faces, px, weight)) in specs.iter().enumerate() {
        match create_font(dc, faces, *px, *weight) {
            Ok(f) => fonts[i] = f,
            Err(e) => {
                delete_fonts(&fonts);
                return Err(e);
            }
        }
    }
    let lhs = [
        line_height_of(dc, fonts[0]),
        line_height_of(dc, fonts[1]),
        line_height_of(dc, fonts[2]),
        line_height_of(dc, fonts[3]),
    ];
    Ok((fonts, lhs))
}

impl Surface {
    /// Creates the memory DC and the four fonts for `m`. The DIB is created by `resize`.
    pub fn new(m: Metrics) -> Result<Surface, String> {
        // SAFETY: the screen DC is taken and released here; the memory DCs are ours.
        let (dc_main, dc_txt) = unsafe {
            let screen = GetDC(None);
            let a = CreateCompatibleDC(Some(screen));
            let b = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            (a, b)
        };
        if dc_main.is_invalid() || dc_txt.is_invalid() {
            // SAFETY: any valid DCs created above, not yet used.
            unsafe {
                if !dc_main.is_invalid() {
                    let _ = DeleteDC(dc_main);
                }
                if !dc_txt.is_invalid() {
                    let _ = DeleteDC(dc_txt);
                }
            }
            return Err("CreateCompatibleDC failed".into());
        }
        // SAFETY: `dc_txt` is valid. White text, transparent background: the coverage ends up in the G channel.
        unsafe {
            SetBkMode(dc_txt, TRANSPARENT);
            SetTextColor(dc_txt, COLORREF(0x00FF_FFFF));
        }
        let (fonts, lhs) = match build_fonts(dc_txt, m) {
            Ok(v) => v,
            Err(e) => {
                // SAFETY: DCs created above, with no selected objects.
                unsafe {
                    let _ = DeleteDC(dc_main);
                    let _ = DeleteDC(dc_txt);
                }
                return Err(e);
            }
        };
        Ok(Surface { metrics: m, dc_main, dc_txt, fonts, lhs, main: None, txt: None, w: 0, h: 0, clip: None })
    }

    /// Current metrics.
    pub fn metrics(&self) -> Metrics {
        self.metrics
    }

    fn free_dibs(&mut self) {
        if let Some(d) = self.main.take() {
            free_dib(self.dc_main, d);
        }
        if let Some(d) = self.txt.take() {
            free_dib(self.dc_txt, d);
        }
        self.w = 0;
        self.h = 0;
    }

    /// (Re)creates the main `w` x `h` physical-px DIB and the scratch one for text. No-op if
    /// the dimensions do not change.
    pub fn resize(&mut self, w: i32, h: i32) -> Result<(), String> {
        if self.main.is_some() && self.w == w && self.h == h {
            return Ok(());
        }
        self.free_dibs();
        let main = make_dib(self.dc_main, w, h)?;
        let txt_h = self.lhs.iter().copied().max().unwrap_or(17) + 4;
        let txt = match make_dib(self.dc_txt, w, txt_h) {
            Ok(t) => t,
            Err(e) => {
                free_dib(self.dc_main, main);
                return Err(e);
            }
        };
        self.main = Some(main);
        self.txt = Some(txt);
        self.w = w;
        self.h = h;
        Ok(())
    }

    /// Sets (or clears) the clipping rectangle of the drawing primitives.
    pub fn set_clip(&mut self, rc: Option<RECT>) {
        self.clip = rc;
    }

    /// Current DIB dimensions (physical px).
    pub fn size(&self) -> (i32, i32) {
        (self.w, self.h)
    }

    #[inline]
    fn blend(buf: &mut [u32], w: i32, h: i32, clip: Option<RECT>, x: i32, y: i32, src: u32) {
        if x < 0 || y < 0 || x >= w || y >= h {
            return;
        }
        if let Some(c) = clip {
            if x < c.left || x >= c.right || y < c.top || y >= c.bottom {
                return;
            }
        }
        let i = (y * w + x) as usize;
        buf[i] = over(buf[i], src);
    }

    /// Background panel over the whole DIB: config `background` tone (black by default) at `PANEL_BG_ALPHA`, 1 px border `PANEL_BORDER_ALPHA`,
    /// antialiased `PANEL_RADIUS` corners, transparent outside the corners. Replaces all pixels.
    pub fn panel(&mut self) {
        let Some(main) = self.main.as_ref() else { return };
        let (w, h) = (main.w, main.h);
        let buf = dib_pixels(main);
        let radius = self.metrics.px(PANEL_RADIUS);
        let bw = 1.max(self.metrics.px(1));
        let (r, g, b) = crate::config::get().background.rgb();
        for y in 0..h {
            for x in 0..w {
                let outer = rr_cov(x, y, 0, 0, w, h, radius);
                let i = (y * w + x) as usize;
                if outer <= 0.0 {
                    buf[i] = 0;
                    continue;
                }
                let inner = rr_cov(x, y, bw, bw, w - bw, h - bw, radius - bw);
                let base = argb((PANEL_BG_ALPHA as f32 * outer + 0.5) as u32, r, g, b);
                let ring = (outer - inner).max(0.0);
                let ba = (PANEL_BORDER_ALPHA as f32 * ring + 0.5) as u32;
                buf[i] = over(base, white(ba));
            }
        }
    }

    /// Filled AA rounded rectangle (client coordinates, physical px; `radius` in physical px).
    pub fn fill_rr(&mut self, rc: RECT, radius: i32, rgb: (u32, u32, u32), alpha: u32) {
        if alpha == 0 {
            return;
        }
        let Some(main) = self.main.as_ref() else { return };
        let (w, h) = (main.w, main.h);
        let buf = dib_pixels(main);
        let clip = self.clip;
        for y in rc.top.max(0)..rc.bottom.min(h) {
            for x in rc.left.max(0)..rc.right.min(w) {
                let cov = rr_cov(x, y, rc.left, rc.top, rc.right, rc.bottom, radius);
                let a = (alpha as f32 * cov + 0.5) as u32;
                if a > 0 {
                    Self::blend(buf, w, h, clip, x, y, argb(a, rgb.0, rgb.1, rgb.2));
                }
            }
        }
    }

    /// Inner ring of `bw` physical px of a rounded rectangle, white with alpha `alpha`.
    pub fn stroke_rr(&mut self, rc: RECT, radius: i32, bw: i32, alpha: u32) {
        if alpha == 0 {
            return;
        }
        let Some(main) = self.main.as_ref() else { return };
        let (w, h) = (main.w, main.h);
        let buf = dib_pixels(main);
        let clip = self.clip;
        for y in rc.top.max(0)..rc.bottom.min(h) {
            for x in rc.left.max(0)..rc.right.min(w) {
                let outer = rr_cov(x, y, rc.left, rc.top, rc.right, rc.bottom, radius);
                let inner = rr_cov(x, y, rc.left + bw, rc.top + bw, rc.right - bw, rc.bottom - bw, radius - bw);
                let a = (alpha as f32 * (outer - inner).max(0.0) + 0.5) as u32;
                if a > 0 {
                    Self::blend(buf, w, h, clip, x, y, white(a));
                }
            }
        }
    }

    /// Hover/press plate like the bar one: white fill `alpha` + inner border
    /// 1 px `HOVER_RIM_ALPHA` (scaled if `alpha` is below the hover value). `radius` in physical px.
    pub fn plate(&mut self, rc: RECT, radius: i32, alpha: u32) {
        self.fill_rr(rc, radius, WHITE, alpha);
        let rim = HOVER_RIM_ALPHA * alpha.min(HOVER_ALPHA) / HOVER_ALPHA;
        let bw = self.metrics.px(1).max(1);
        self.stroke_rr(rc, radius, bw, rim);
    }

    /// Opaque fill of the rounded rectangle `rc` (AA corners, `radius` physical px) with a colour
    /// computed per pixel by `color(x, y)` (client coordinates): colour picker surfaces and swatches.
    pub fn fill_fn(&mut self, rc: RECT, radius: i32, color: &dyn Fn(i32, i32) -> (u32, u32, u32)) {
        let Some(main) = self.main.as_ref() else { return };
        let (w, h) = (main.w, main.h);
        let buf = dib_pixels(main);
        let clip = self.clip;
        for y in rc.top.max(0)..rc.bottom.min(h) {
            for x in rc.left.max(0)..rc.right.min(w) {
                let cov = rr_cov(x, y, rc.left, rc.top, rc.right, rc.bottom, radius);
                let a = (255.0 * cov + 0.5) as u32;
                if a > 0 {
                    let (r, g, b) = color(x, y);
                    Self::blend(buf, w, h, clip, x, y, argb(a, r.min(255), g.min(255), b.min(255)));
                }
            }
        }
    }

    /// Blend of a premultiplied bitmap into the `size` square. A different size is scaled: area
    /// average when shrinking (stays crisp, e.g. tray icons rendered for a larger bar), nearest
    /// when enlarging.
    pub fn blit(&mut self, bmp: &IconBitmap, x: i32, y: i32, size: i32, opacity: u32) {
        let Some(main) = self.main.as_ref() else { return };
        let (w, h) = (main.w, main.h);
        let buf = dib_pixels(main);
        let clip = self.clip;
        let (sw, sh) = (bmp.width as i32, bmp.height as i32);
        if bmp.is_empty() || size <= 0 || bmp.pixels.len() < (sw as usize) * (sh as usize) {
            return;
        }
        let same = sw == size && sh == size;
        for dy in 0..size {
            let yy = y + dy;
            if yy < 0 || yy >= h {
                continue;
            }
            let sy = if same { dy } else { dy * sh / size };
            for dx in 0..size {
                let xx = x + dx;
                if xx < 0 || xx >= w {
                    continue;
                }
                let sx = if same { dx } else { dx * sw / size };
                let mut s = if same || (sw <= size && sh <= size) {
                    bmp.pixels[(sy * sw + sx) as usize]
                } else {
                    area_sample(bmp, dx, dy, size)
                };
                if s >> 24 == 0 {
                    continue;
                }
                if opacity < 255 {
                    s = (div255((s >> 24) * opacity) << 24)
                        | (div255(((s >> 16) & 255) * opacity) << 16)
                        | (div255(((s >> 8) & 255) * opacity) << 8)
                        | div255((s & 255) * opacity);
                }
                Self::blend(buf, w, h, clip, xx, yy, s);
            }
        }
    }

    /// Glyph from `crate::glyphs` (16x16 space) in white, in a `size` px square with corner
    /// `(x, y)`, 4x4 supersampling.
    pub fn glyph(&mut self, x: i32, y: i32, size: i32, alpha: u32, shape: &dyn Fn(f32, f32) -> bool) {
        self.glyph_rgb(x, y, size, WHITE, alpha, shape);
    }

    /// Like `glyph` with color `rgb` (e.g. black on the white plate).
    pub fn glyph_rgb(
        &mut self,
        x: i32,
        y: i32,
        size: i32,
        rgb: (u32, u32, u32),
        alpha: u32,
        shape: &dyn Fn(f32, f32) -> bool,
    ) {
        if size <= 0 {
            return;
        }
        let Some(main) = self.main.as_ref() else { return };
        let (w, h) = (main.w, main.h);
        let buf = dib_pixels(main);
        let clip = self.clip;
        crate::glyphs::set_scale(size);
        let k = 16.0 / size as f32;
        for py in 0..size {
            for px in 0..size {
                let mut hits = 0u32;
                for sy in 0..4 {
                    for sx in 0..4 {
                        let u = (px as f32 + (sx as f32 + 0.5) / 4.0) * k;
                        let v = (py as f32 + (sy as f32 + 0.5) / 4.0) * k;
                        if shape(u, v) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    Self::blend(buf, w, h, clip, x + px, y + py, argb(alpha * hits / 16, rgb.0, rgb.1, rgb.2));
                }
            }
        }
    }

    /// Single-line text with the top-left corner at `(x, y)`; `rgb` non-premultiplied
    /// (white normally, black for today's day on the white plate).
    pub fn text(&mut self, s: &str, x: i32, y: i32, font: Font, rgb: (u32, u32, u32), alpha: u32) {
        let (Some(main), Some(txt)) = (self.main.as_ref(), self.txt.as_ref()) else { return };
        if s.is_empty() || alpha == 0 {
            return;
        }
        let fnt = self.fonts[font.idx()];
        let w = self.text_width(s, font).min(txt.w);
        let h = self.lhs[font.idx()].min(txt.h);
        if w <= 0 || h <= 0 {
            return;
        }
        let tbuf = dib_pixels(txt);
        for row in 0..h as usize {
            let start = row * txt.w as usize;
            tbuf[start..start + w as usize].fill(0);
        }
        let mut wide: Vec<u16> = s.encode_utf16().collect();
        let mut rc = RECT { left: 0, top: 0, right: w, bottom: h };
        // SAFETY: `dc_txt` has the scratch DIB selected; `wide` and `rc` outlive the call;
        // the font is restored right away. GdiFlush guarantees GDI has written before the read.
        unsafe {
            let old = SelectObject(self.dc_txt, HGDIOBJ(fnt.0));
            DrawTextW(self.dc_txt, &mut wide, &mut rc, DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP);
            SelectObject(self.dc_txt, old);
            let _ = GdiFlush();
        }
        let (mw, mh) = (main.w, main.h);
        let buf = dib_pixels(main);
        let clip = self.clip;
        for row in 0..h {
            for col in 0..w {
                let cov = (tbuf[(row * txt.w + col) as usize] >> 8) & 0xFF; // coverage = G channel
                if cov == 0 {
                    continue;
                }
                Self::blend(buf, mw, mh, clip, x + col, y + row, argb(div255(cov * alpha), rgb.0, rgb.1, rgb.2));
            }
        }
    }

    /// Width in physical px of `s` with `font`.
    pub fn text_width(&self, s: &str, font: Font) -> i32 {
        if s.is_empty() {
            return 0;
        }
        let wide: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE::default();
        // SAFETY: `dc_txt` is valid; `wide` and `sz` outlive the call; the font is restored.
        let ok = unsafe {
            let old = SelectObject(self.dc_txt, HGDIOBJ(self.fonts[font.idx()].0));
            let ok = GetTextExtentPoint32W(self.dc_txt, &wide, &mut sz);
            SelectObject(self.dc_txt, old);
            ok
        };
        if ok.as_bool() {
            sz.cx
        } else {
            wide.len() as i32 * self.metrics.px(7)
        }
    }

    /// Line height (physical px) of `font`.
    pub fn line_height(&self, font: Font) -> i32 {
        self.lhs[font.idx()]
    }

    /// `s` truncated with "…" so that `text_width <= max_w`.
    pub fn ellipsize(&self, s: &str, font: Font, max_w: i32) -> String {
        if self.text_width(s, font) <= max_w {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        // Binary search for the longest prefix that, with "…", fits in `max_w`.
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let mut cand: String = chars[..mid].iter().collect();
            cand = cand.trim_end().to_string();
            cand.push('\u{2026}');
            if self.text_width(&cand, font) <= max_w {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let mut out: String = chars[..lo].iter().collect();
        out = out.trim_end().to_string();
        out.push('\u{2026}');
        out
    }

    /// Copies the DIB as is (no alpha) onto `hdc` at `(x, y)`: for a NON-layered window drawn in
    /// `WM_PAINT` (the window picker), where every pixel has been painted opaque.
    pub fn copy_to(&self, hdc: HDC, x: i32, y: i32) {
        let Some(main) = self.main.as_ref() else { return };
        // SAFETY: `hdc` comes from BeginPaint of the caller; `dc_main` holds our DIB of `w` x `h`.
        if let Err(e) = unsafe { BitBlt(hdc, x, y, main.w, main.h, Some(self.dc_main), 0, 0, SRCCOPY) } {
            log!("surface: BitBlt failed: {e}");
        }
    }

    /// `UpdateLayeredWindow(ULW_ALPHA)` with corner at screen `(x, y)` and constant alpha `alpha`
    /// (opening fade). false on failure (logged). The caller must not hold a borrow
    /// of its own state during the call (it may re-enter with messages to the same window).
    pub fn present(&self, hwnd: HWND, x: i32, y: i32, alpha: u8) -> bool {
        let Some(main) = self.main.as_ref() else { return false };
        let pos = POINT { x, y };
        let size = SIZE { cx: main.w, cy: main.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        // SAFETY: all pointers point to local variables alive for the call; `dc_main` has the
        // DIB selected and lives as long as `self`; the screen DC is taken and released here.
        let r = unsafe {
            let screen = GetDC(None);
            let r = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&pos),
                Some(&size),
                Some(self.dc_main),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            ReleaseDC(None, screen);
            r
        };
        match r {
            Ok(()) => true,
            Err(e) => {
                log!("popup: UpdateLayeredWindow failed: {e}");
                false
            }
        }
    }
}

impl Drop for Surface {
    /// Releases DIB, fonts and DCs (first restoring the selected objects).
    fn drop(&mut self) {
        self.free_dibs();
        delete_fonts(&self.fonts);
        // SAFETY: the DCs are ours and no longer have any of our objects selected.
        unsafe {
            let _ = DeleteDC(self.dc_main);
            let _ = DeleteDC(self.dc_txt);
        }
    }
}

// ------------------------------------------------------- pixel primitives (copy of render.rs)

#[inline]
fn div255(x: u32) -> u32 {
    (x + 128 + ((x + 128) >> 8)) >> 8
}

/// Premultiplied 0xAARRGGBB pixel from a non-premultiplied color and alpha.
#[inline]
fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
    (a << 24) | (div255(r * a) << 16) | (div255(g * a) << 8) | div255(b * a)
}

/// White with alpha `a`, premultiplied.
#[inline]
fn white(a: u32) -> u32 {
    argb(a, 255, 255, 255)
}

/// "Source over" composition of `src` onto `dst`, both premultiplied.
#[inline]
fn over(dst: u32, src: u32) -> u32 {
    let sa = src >> 24;
    if sa == 255 {
        return src;
    }
    if sa == 0 {
        return dst;
    }
    let inv = 255 - sa;
    let ch = |s: u32, d: u32| (s + div255(d * inv)).min(255);
    (ch(src >> 24, dst >> 24) << 24)
        | (ch((src >> 16) & 255, (dst >> 16) & 255) << 16)
        | (ch((src >> 8) & 255, (dst >> 8) & 255) << 8)
        | ch(src & 255, dst & 255)
}

/// Coverage (0..1) of pixel `(x, y)` with respect to the rounded rectangle `[l, r) x [t, b)` with
/// radius `rad`: manual antialiasing at the corners only (the sides are pixel-aligned).
fn rr_cov(x: i32, y: i32, l: i32, t: i32, r: i32, b: i32, rad: i32) -> f32 {
    if x < l || x >= r || y < t || y >= b {
        return 0.0;
    }
    let rad = rad.min((r - l) / 2).min((b - t) / 2).max(0);
    let cx = if x < l + rad {
        l + rad
    } else if x >= r - rad {
        r - rad
    } else {
        return 1.0;
    };
    let cy = if y < t + rad {
        t + rad
    } else if y >= b - rad {
        b - rad
    } else {
        return 1.0;
    };
    let dx = (x as f32 + 0.5) - cx as f32;
    let dy = (y as f32 + 0.5) - cy as f32;
    (rad as f32 + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
}

/// `RECT` from origin and size.
/// Average (premultiplied) of the source pixels covered by destination pixel `(dx, dy)` of a
/// `size` x `size` image drawn from `bmp`.
pub(crate) fn area_sample(bmp: &IconBitmap, dx: i32, dy: i32, size: i32) -> u32 {
    let (sw, sh) = (bmp.width as i32, bmp.height as i32);
    let (x0, x1) = (dx * sw / size, ((dx + 1) * sw / size).max(dx * sw / size + 1).min(sw));
    let (y0, y1) = (dy * sh / size, ((dy + 1) * sh / size).max(dy * sh / size + 1).min(sh));
    let mut acc = [0u32; 4];
    for yy in y0..y1 {
        for xx in x0..x1 {
            let p = bmp.pixels[(yy * sw + xx) as usize];
            for (k, a) in acc.iter_mut().enumerate() {
                *a += (p >> (8 * k)) & 255;
            }
        }
    }
    let n = (((x1 - x0) * (y1 - y0)).max(1)) as u32;
    acc.iter().enumerate().fold(0, |out, (k, a)| out | (((a + n / 2) / n).min(255) << (8 * k)))
}

pub fn rect_wh(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

/// true if `(x, y)` is inside `rc`.
pub fn in_rect(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

// PREVIEW-HOOK (used by `preview.rs`): copy of the main DIB pixels (premultiplied 0xAARRGGBB).
impl Surface {
    pub(crate) fn snapshot_pixels(&self) -> Option<(i32, i32, Vec<u32>)> {
        let d = self.main.as_ref()?;
        Some((d.w, d.h, dib_pixels(d).to_vec()))
    }
}
