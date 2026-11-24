//! Menu bar icons: one-colour template images drawn with AppKit (port of menubar_icons.py).
//!
//! Each style is a glyph drawn black on a 20x20 pt canvas, saved as a 2x PNG. macOS colours
//! template images to suit light and dark menu bars. States: idle · working (dot badge) ·
//! paused (faded) · error (badge with "!").

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use objc2::rc::Retained;
use objc2::AllocAnyThread;
use objc2_app_kit::{
    NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSCalibratedRGBColorSpace, NSColor,
    NSCompositingOperation, NSFontWeightRegular, NSGraphicsContext, NSImage, NSImageRep,
    NSImageSymbolConfiguration, NSLineCapStyle, NSLineJoinStyle, NSWorkspace,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

pub const STATES: [&str; 4] = ["idle", "working", "paused", "error"];
const C: f64 = 10.0; // centre of the 20 pt canvas; glyphs are designed in 18 pt and drawn 1 pt in

#[derive(Clone, Copy)]
enum Glyph {
    Bubble,
    Pulse,
    Bars,
    Ring,
    Dots,
    Pixel,
    Sf(&'static str),
}

/// (key, menu name, glyph), in menu order.
const STYLES: [(&str, &str, Glyph); 8] = [
    ("bubble", "Talking bubble", Glyph::Bubble),
    ("pulse", "Pulse line", Glyph::Pulse),
    ("bars", "Bars", Glyph::Bars),
    ("ring", "Ring", Glyph::Ring),
    ("dots", "Dot matrix", Glyph::Dots),
    ("pixel", "Pixel mic", Glyph::Pixel),
    ("waveform", "Apple waveform", Glyph::Sf("waveform")),
    ("speaker", "Apple speaker voice", Glyph::Sf("person.wave.2")),
];
pub const DEFAULT: &str = "bubble";

/// (key, menu name) of every style, in menu order.
pub fn styles() -> impl Iterator<Item = (&'static str, &'static str)> {
    STYLES.iter().map(|(k, n, _)| (*k, *n))
}

pub fn is_style(key: &str) -> bool {
    STYLES.iter().any(|(k, _, _)| *k == key)
}

fn glyph(key: &str) -> Glyph {
    STYLES.iter().find(|(k, _, _)| *k == key).map(|s| s.2).unwrap_or(Glyph::Bubble)
}

// ------------------------------------------------------------------ drawing primitives

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn rrect(x: f64, y: f64, w: f64, h: f64, r: f64) -> Retained<NSBezierPath> {
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect(x + 1.0, y + 1.0, w, h), r, r)
}

fn oval(cx: f64, cy: f64, r: f64, inset: bool) -> Retained<NSBezierPath> {
    let o = if inset { 1.0 } else { 0.0 };
    NSBezierPath::bezierPathWithOvalInRect(rect(cx - r + o, cy - r + o, 2.0 * r, 2.0 * r))
}

fn set_op(op: NSCompositingOperation) {
    if let Some(ctx) = NSGraphicsContext::currentContext() {
        ctx.setCompositingOperation(op);
    }
}

/// Draw `f` as an eraser: what it fills becomes transparent.
fn clear(f: impl FnOnce()) {
    set_op(NSCompositingOperation::Clear);
    f();
    set_op(NSCompositingOperation::SourceOver);
}

fn g_bars() {
    for (x, h) in [(3.2, 6.0), (6.8, 13.0), (10.4, 9.0), (14.0, 4.0)] {
        rrect(x - 1.15, 9.0 - h / 2.0, 2.3, h, 1.15).fill();
    }
}

fn g_ring() {
    let p = oval(9.0, 9.0, 7.6, true);
    p.setLineWidth(1.5);
    p.stroke();
    for (x, h) in [(6.0, 4.5), (9.0, 8.5), (12.0, 5.5)] {
        rrect(x - 0.85, 9.0 - h / 2.0, 1.7, h, 0.85).fill();
    }
}

fn g_pulse() {
    let p = NSBezierPath::bezierPath();
    let pts = [(0.8, 9.0), (4.6, 9.0), (6.4, 4.2), (8.6, 15.2), (10.6, 5.6), (12.2, 11.6), (13.6, 9.0), (17.2, 9.0)];
    p.moveToPoint(NSPoint::new(pts[0].0 + 1.0, pts[0].1 + 1.0));
    for (x, y) in &pts[1..] {
        p.lineToPoint(NSPoint::new(x + 1.0, y + 1.0));
    }
    p.setLineWidth(1.6);
    p.setLineJoinStyle(NSLineJoinStyle::Round);
    p.setLineCapStyle(NSLineCapStyle::Round);
    p.stroke();
}

fn g_bubble() {
    let b = rrect(1.5, 5.0, 15.0, 11.0, 3.4);
    let tail = NSBezierPath::bezierPath();
    tail.moveToPoint(NSPoint::new(5.2, 6.6));
    tail.lineToPoint(NSPoint::new(4.4, 2.6));
    tail.lineToPoint(NSPoint::new(9.0, 6.6));
    tail.closePath();
    b.appendBezierPath(&tail);
    b.fill();
    clear(|| {
        for (x, h) in [(5.2, 2.6), (7.6, 6.0), (10.0, 4.0), (12.4, 2.0)] {
            rrect(x - 0.7, 10.5 - h / 2.0, 1.4, h, 0.7).fill();
        }
    });
}

fn g_dots() {
    for (i, h) in [1, 3, 5, 3, 2].into_iter().enumerate() {
        for j in 0..h {
            oval(2.6 + i as f64 * 3.2, 9.0 + (j as f64 - (h - 1) as f64 / 2.0) * 3.2, 1.15, true).fill();
        }
    }
}

const PIXEL_MIC: [&str; 9] =
    ["...###...", "..#####..", "..#####..", "#.#####.#", "#..###..#", ".##...##.", "...###...", "....#....", "..#####.."];

fn g_pixel() {
    for (r, row) in PIXEL_MIC.iter().enumerate() {
        for (c, ch) in row.chars().enumerate() {
            if ch == '#' {
                NSBezierPath::fillRect(rect(c as f64 * 2.0 + 1.0, 18.0 - (r as f64 + 1.0) * 2.0 + 1.0, 2.0, 2.0));
            }
        }
    }
}

fn g_sf(name: &str, size: f64) {
    match symbol(name, size) {
        Some(img) => {
            let s = img.size();
            img.drawInRect(rect(C - s.width / 2.0, C - s.height / 2.0, s.width, s.height));
        }
        None => g_bars(), // symbol missing on this macOS: fall back to the bars
    }
}

/// Draw a style's glyph in the current colour on the current 20 pt canvas.
pub fn draw_glyph(key: &str) {
    match glyph(key) {
        Glyph::Bubble => g_bubble(),
        Glyph::Pulse => g_pulse(),
        Glyph::Bars => g_bars(),
        Glyph::Ring => g_ring(),
        Glyph::Dots => g_dots(),
        Glyph::Pixel => g_pixel(),
        Glyph::Sf(name) => g_sf(name, 14.5),
    }
}

fn badge(state: &str) {
    let (cx, cy) = (15.6, 15.6);
    clear(|| oval(cx, cy, 4.6, false).fill());
    if state == "working" {
        oval(cx, cy, 2.5, false).fill();
    } else {
        oval(cx, cy, 3.5, false).fill();
        clear(|| {
            NSBezierPath::fillRect(rect(cx - 0.55, cy - 0.4, 1.1, 2.6));
            NSBezierPath::fillRect(rect(cx - 0.55, cy - 2.1, 1.1, 1.1));
        });
    }
}

// ------------------------------------------------------------------ bitmaps

/// A transparent RGBA bitmap of `w` x `h` points at `scale` pixels per point.
pub fn bitmap(w: f64, h: f64, scale: f64) -> Retained<NSBitmapImageRep> {
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            null_mut(),
            (w * scale) as isize,
            (h * scale) as isize,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            0,
            0,
        )
    }
    .expect("cannot allocate a bitmap");
    rep.setSize(NSSize::new(w, h));
    rep
}

/// Run `f` with `rep` as the current graphics context.
pub fn draw_into(rep: &NSBitmapImageRep, f: impl FnOnce()) {
    NSGraphicsContext::saveGraphicsState_class();
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(rep);
    NSGraphicsContext::setCurrentContext(ctx.as_deref());
    f();
    NSGraphicsContext::restoreGraphicsState_class();
}

/// Write a bitmap as PNG (via a temp file and rename).
pub fn save_png(rep: &NSBitmapImageRep, path: &Path) -> std::io::Result<()> {
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
        .ok_or_else(|| std::io::Error::other("PNG encoding failed"))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    std::fs::write(&tmp, data.to_vec())?;
    std::fs::rename(&tmp, path)
}

/// Draw `src` into the current context at `dst` with `alpha`.
pub fn draw_rep(src: &NSImageRep, dst: NSRect, alpha: f64) {
    unsafe {
        src.drawInRect_fromRect_operation_fraction_respectFlipped_hints(
            dst,
            NSRect::ZERO,
            NSCompositingOperation::SourceOver,
            alpha,
            true,
            None,
        );
    }
}

/// A copy of `src` (same size) with every pixel coloured `ink`, alpha kept.
pub fn tinted(src: &NSImageRep, w: f64, h: f64, scale: f64, ink: &NSColor) -> Retained<NSBitmapImageRep> {
    let out = bitmap(w, h, scale);
    draw_into(&out, || {
        draw_rep(src, rect(0.0, 0.0, w, h), 1.0);
        ink.set();
        set_op(NSCompositingOperation::SourceIn);
        NSBezierPath::fillRect(rect(0.0, 0.0, w, h));
    });
    out
}

/// The 20 pt menu bar icon of `style` in `state`, as a 2x bitmap.
pub fn icon_rep(style: &str, state: &str) -> Retained<NSBitmapImageRep> {
    let rep = bitmap(20.0, 20.0, 2.0);
    draw_into(&rep, || {
        NSColor::blackColor().set();
        draw_glyph(style);
        if state == "working" || state == "error" {
            badge(state);
        }
    });
    if state != "paused" {
        return rep;
    }
    // fade the finished glyph, badge cut-outs and all
    let faded = bitmap(20.0, 20.0, 2.0);
    draw_into(&faded, || draw_rep(&rep, rect(0.0, 0.0, 20.0, 20.0), 0.45));
    faded
}

pub fn draw_icon(path: &Path, style: &str, state: &str) -> std::io::Result<()> {
    save_png(&icon_rep(style, state), path)
}

/// Write `<folder>/<style>-<state>.png` for every state; return (state, path) pairs.
pub fn make_icons(folder: &Path, style: &str) -> Vec<(&'static str, PathBuf)> {
    let _ = std::fs::create_dir_all(folder);
    STATES
        .iter()
        .map(|state| {
            let p = folder.join(format!("{style}-{state}.png"));
            let _ = draw_icon(&p, style, state);
            (*state, p)
        })
        .collect()
}

/// Load a PNG as an image of `size` points.
pub fn load_image(path: &Path, size: f64, template: bool) -> Option<Retained<NSImage>> {
    let img = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&path.to_string_lossy()))?;
    img.setSize(NSSize::new(size, size));
    img.setTemplate(template);
    Some(img)
}

/// An Apple system symbol as a template image for a menu item, or None if this macOS lacks it.
pub fn symbol(name: &str, size: f64) -> Option<Retained<NSImage>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)?;
    let conf = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, unsafe { NSFontWeightRegular });
    let img = img.imageWithSymbolConfiguration(&conf)?;
    img.setTemplate(true);
    Some(img)
}

/// One-colour template symbol cut from an app icon's light logo on a coloured tile (e.g. Claude's
/// white starburst): the near-white pixels become the glyph, cropped and fitted to size. Cached as
/// a PNG; None if the app is not there or its icon has no such logo.
pub fn app_glyph(app: &Path, cache: &Path, size: f64) -> Option<Retained<NSImage>> {
    if !cache.exists() {
        if !app.exists() {
            return None;
        }
        let n = 256usize;
        let src = bitmap(n as f64, n as f64, 1.0);
        let icon = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(&app.to_string_lossy()));
        draw_into(&src, || icon.drawInRect(rect(0.0, 0.0, n as f64, n as f64)));
        let row = src.bytesPerRow() as usize;
        let data = unsafe { std::slice::from_raw_parts(src.bitmapData(), row * n) };
        let mut on = HashSet::new();
        for y in 0..n {
            for x in 0..n {
                let p = &data[y * row + 4 * x..y * row + 4 * x + 4];
                if p[3] > 200 && p[0].min(p[1]).min(p[2]) > 215 {
                    on.insert((x, y));
                }
            }
        }
        if on.len() < 200 {
            return None;
        }
        let x0 = on.iter().map(|p| p.0).min()?;
        let x1 = on.iter().map(|p| p.0).max()?;
        let y0 = on.iter().map(|p| p.1).min()?;
        let y1 = on.iter().map(|p| p.1).max()?;
        let side = (x1 - x0).max(y1 - y0) + 1;
        let out = bitmap(side as f64, side as f64, 1.0);
        let (ox, oy) = ((side - (x1 - x0 + 1)) / 2, (side - (y1 - y0 + 1)) / 2);
        let orow = out.bytesPerRow() as usize;
        let odata = unsafe { std::slice::from_raw_parts_mut(out.bitmapData(), orow * side) };
        for (x, y) in on {
            // both bitmaps run top-down, so the row index carries over directly
            let i = (y - y0 + oy) * orow + 4 * (x - x0 + ox);
            odata[i..i + 4].copy_from_slice(&[0, 0, 0, 255]);
        }
        let _ = std::fs::create_dir_all(cache.parent()?);
        save_png(&out, cache).ok()?;
    }
    load_image(cache, size - 1.0, true)
}

/// The idle icon of a style, small, for its entry in the Icon menu.
pub fn style_image(folder: &Path, key: &str, size: f64) -> Option<Retained<NSImage>> {
    let _ = std::fs::create_dir_all(folder);
    let path = folder.join(format!("{key}-menu.png"));
    draw_icon(&path, key, "idle").ok()?;
    load_image(&path, size, true)
}

/// Sheet: one row per style, its states on a light and a dark bar.
pub fn preview_sheet(path: &Path) -> std::io::Result<()> {
    let (row, w) = (34.0, 20.0 + 4.0 * 30.0 * 2.0 + 40.0);
    let h = row * STYLES.len() as f64 + 10.0;
    let rep = bitmap(w, h, 2.0);
    draw_into(&rep, || {
        NSColor::whiteColor().set();
        NSBezierPath::fillRect(rect(0.0, 0.0, w, h));
        for (r, (key, _, _)) in STYLES.iter().enumerate() {
            let y = h - (r as f64 + 1.0) * row;
            let bars = [(0.93, NSColor::blackColor()), (0.17, NSColor::whiteColor())];
            for (b, (bar, ink)) in bars.iter().enumerate() {
                let x0 = 10.0 + b as f64 * (4.0 * 30.0 + 20.0);
                NSColor::colorWithCalibratedWhite_alpha(*bar, 1.0).set();
                NSBezierPath::fillRect(rect(x0, y + 2.0, 4.0 * 30.0, row - 4.0));
                for (i, state) in STATES.iter().enumerate() {
                    let t = tinted(&icon_rep(key, state), 20.0, 20.0, 2.0, ink);
                    draw_rep(&t, rect(x0 + 5.0 + i as f64 * 30.0, y + (row - 20.0) / 2.0, 20.0, 20.0), 1.0);
                }
            }
        }
    });
    save_png(&rep, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_draws_every_state() {
        for (key, _) in styles() {
            for state in STATES {
                let rep = icon_rep(key, state);
                assert_eq!(rep.pixelsWide(), 40);
                let row = rep.bytesPerRow() as usize;
                let data = unsafe { std::slice::from_raw_parts(rep.bitmapData(), row * 40) };
                let inked = (0..40 * 40).filter(|i| data[(i / 40) * row + 4 * (i % 40) + 3] > 0).count();
                assert!(inked > 40, "{key}/{state} is (nearly) empty: {inked} pixels");
            }
        }
    }

    #[test]
    fn unknown_style_is_not_a_style() {
        assert!(is_style("bubble") && is_style("speaker"));
        assert!(!is_style("nope"));
    }
}
