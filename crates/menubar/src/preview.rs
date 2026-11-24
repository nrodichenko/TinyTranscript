//! `recordings_menubar --render-preview <dir>`: draw the menu off-screen to PNG files, in light
//! and dark appearance, without a status item, timer or notifications. The rows are the real row
//! views filled by a real refresh (which only reads: launchctl print, logs, MacWhisper's database);
//! plain menu items and separators are drawn as simple stand-ins in the same order. Icon PNGs go
//! to `<dir>/icon-cache`, not to the app's cache.

use std::path::Path;

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy, NSBezierPath, NSBox, NSBoxType, NSColor, NSFont, NSImageView, NSMenu, NSTextField,
    NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::app::{self, Paths};
use crate::icons;

const ITEM_H: f64 = 22.0;
const SEP_H: f64 = 11.0;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn label(mtm: MainThreadMarker, text: &str, color: &NSColor) -> Retained<NSTextField> {
    let f = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    f.setFont(Some(&NSFont::menuFontOfSize(0.0)));
    f.setTextColor(Some(color));
    f
}

/// A plain menu item as AppKit lays it out roughly: checkmark, image, title, submenu arrow.
fn plain_item(mtm: MainThreadMarker, item: &objc2_app_kit::NSMenuItem, width: f64) -> Retained<NSView> {
    let v = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, ITEM_H));
    let enabled = item.action().is_some() || item.hasSubmenu();
    let ink = if enabled { NSColor::labelColor() } else { NSColor::disabledControlTextColor() };
    if item.state() == objc2_app_kit::NSControlStateValueOn {
        let c = label(mtm, "✓", &ink);
        c.setFrame(rect(1.0, 3.0, 14.0, 16.0));
        v.addSubview(&c);
    }
    if let Some(img) = item.image() {
        let iv = NSImageView::imageViewWithImage(&img, mtm);
        iv.setFrame(rect(14.0, 3.0, 16.0, 16.0));
        iv.setContentTintColor(Some(&ink));
        v.addSubview(&iv);
    }
    let t = label(mtm, &item.title().to_string(), &ink);
    let h = t.fittingSize().height;
    t.setFrame(rect(37.0, (ITEM_H - h) / 2.0, width - 60.0, h));
    v.addSubview(&t);
    if item.hasSubmenu() {
        let a = label(mtm, "›", &ink);
        a.setFrame(rect(width - 22.0, (ITEM_H - h) / 2.0, 12.0, h));
        v.addSubview(&a);
    }
    v
}

/// Lay the menu's visible items out top to bottom in one view and draw it to `out` (2x PNG).
fn snapshot(mtm: MainThreadMarker, menu: &NSMenu, dark: bool, out: &Path, before_draw: impl Fn()) -> Result<()> {
    let items: Vec<_> = menu.itemArray().iter().filter(|i| !i.isHidden()).collect();
    let width = items
        .iter()
        .filter_map(|i| i.view().map(|v| v.frame().size.width))
        .fold(240.0_f64, f64::max);
    let heights: Vec<f64> = items
        .iter()
        .map(|i| if i.isSeparatorItem() { SEP_H } else { i.view().map(|v| v.frame().size.height).unwrap_or(ITEM_H) })
        .collect();
    let total = heights.iter().sum::<f64>() + 10.0;
    let container = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, total));
    let name = unsafe { if dark { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua } };
    let appearance = NSAppearance::appearanceNamed(name).ok_or_else(|| anyhow!("no appearance"))?;
    container.setAppearance(Some(&appearance));
    let mut y = total - 5.0;
    let mut attached = Vec::new();
    for (item, h) in items.iter().zip(&heights) {
        y -= h;
        if item.isSeparatorItem() {
            let b = NSBox::initWithFrame(NSBox::alloc(mtm), rect(14.0, y + SEP_H / 2.0, width - 28.0, 1.0));
            b.setBoxType(NSBoxType::Separator);
            container.addSubview(&b);
        } else if let Some(v) = item.view() {
            // a holder per row: the menu item keeps its view's own frame
            let holder = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, y, width, *h));
            v.setFrameOrigin(NSPoint::new(0.0, 0.0));
            holder.addSubview(&v);
            container.addSubview(&holder);
            attached.push(v);
        } else {
            let v = plain_item(mtm, item, width);
            v.setFrameOrigin(NSPoint::new(0.0, y));
            container.addSubview(&v);
        }
    }
    before_draw();
    let bounds = container.bounds();
    let shot = icons::bitmap(width, total, 2.0);
    container.cacheDisplayInRect_toBitmapImageRep(bounds, &shot);
    let page = icons::bitmap(width, total, 2.0);
    icons::draw_into(&page, || {
        let bg = if dark { 0.17 } else { 0.95 };
        NSColor::colorWithCalibratedWhite_alpha(bg, 1.0).set();
        NSBezierPath::fillRect(bounds);
        icons::draw_rep(&shot, bounds, 1.0);
    });
    for v in attached {
        v.removeFromSuperview();
    }
    icons::save_png(&page, out)?;
    println!("wrote {}", out.display());
    Ok(())
}

pub fn render(dir: &Path) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| anyhow!("must run on the main thread"))?;
    let nsapp = NSApplication::sharedApplication(mtm);
    nsapp.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir)?;
    let mut paths = Paths::live();
    paths.icon_dir = dir.join("icon-cache");
    let app = app::for_preview(mtm, paths);
    app.refresh();
    println!("style {}, icon state {}", app.style(), app.icon_state());
    for (row_key, row) in &app.rows {
        let tip = row.view.toolTip().map(|t| t.to_string()).unwrap_or_default();
        println!("row {row_key}: {} | {} | {} | tooltip: {tip}", row.text(0), row.text(1), row.text(2));
    }
    print!("{}", app.recent_tree());
    let drive = &app.rows["local.drive-recordings"];
    let speakers = &app.rows["local.speaker-matching"];
    for (dark, tag) in [(false, "light"), (true, "dark")] {
        snapshot(mtm, &app.menu, dark, &dir.join(format!("menu-{tag}.png")), || {})?;
        // pointer over the Speakers row's pause button; then the Drive row hovered without a button
        snapshot(mtm, &app.menu, dark, &dir.join(format!("menu-{tag}-hover.png")), || {
            speakers.view.set_highlighted(true);
            if let Some(b) = speakers.buttons.get("pause") {
                b.hover(true);
            }
        })?;
        speakers.view.set_highlighted(false);
        if let Some(b) = speakers.buttons.get("pause") {
            b.hover(false);
        }
        snapshot(mtm, &app.menu, dark, &dir.join(format!("menu-{tag}-hover-row.png")), || {
            drive.view.set_highlighted(true);
        })?;
        drive.view.set_highlighted(false);
        snapshot(mtm, &app.icon_menu, dark, &dir.join(format!("icon-menu-{tag}.png")), || {})?;
    }
    let sheet = dir.join("icons.png");
    icons::preview_sheet(&sheet)?;
    println!("wrote {}", sheet.display());
    Ok(())
}
