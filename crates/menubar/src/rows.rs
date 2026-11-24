//! Menu rows with columns and small buttons on the right (port of menubar_rows.py).
//!
//! A standard NSMenuItem holds one action, so each source row (Telegram, Zoom, Drive, MacWhisper,
//! Speakers, Claude) is an NSMenuItem with a custom view: symbol · name · state · detail, then icon
//! buttons ("check now", "pause"/"resume", "open"). Clicking the row itself runs its on_click (open
//! the app or folder) and closes the menu; it highlights under the pointer like a normal menu item.
//! Buttons work while the menu stays open and light up under the pointer.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSButton, NSCellImagePosition, NSColor, NSControl,
    NSEvent, NSFont, NSFontAttributeName, NSImage, NSImageScaling, NSImageView, NSMenuItem, NSTextField,
    NSTrackingArea, NSTrackingAreaOptions, NSView,
};
use objc2_foundation::{NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::icons;

pub const ROW_H: f64 = 24.0;
const ICON_X: f64 = 14.0;
const TEXT_X: f64 = 37.0;
const GAP: f64 = 14.0;
const BTN: f64 = 22.0;
const BTN_GAP: f64 = 2.0;
const RIGHT: f64 = 12.0;
const HILITE_INSET: f64 = 5.0;
const HILITE_RADIUS: f64 = 6.0;

pub type Callback = Rc<dyn Fn()>;

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

fn inset(r: NSRect, dx: f64, dy: f64) -> NSRect {
    rect(r.origin.x + dx, r.origin.y + dy, r.size.width - 2.0 * dx, r.size.height - 2.0 * dy)
}

thread_local! {
    /// Button tag -> what it does. Tags are handed out once per button and never reused.
    static CALLBACKS: RefCell<HashMap<isize, Callback>> = RefCell::new(HashMap::new());
    static TARGET: RefCell<Option<Retained<ClickTarget>>> = const { RefCell::new(None) };
}

fn track(view: &NSView) {
    for t in view.trackingAreas().iter() {
        view.removeTrackingArea(&t);
    }
    let area = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            view.bounds(),
            NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(view as &AnyObject),
            None,
        )
    };
    view.addTrackingArea(&area);
}

// ------------------------------------------------------------------ click target

define_class!(
    // SAFETY: NSObject has no subclassing requirements; ClickTarget does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    pub struct ClickTarget;

    impl ClickTarget {
        #[unsafe(method(clicked:))]
        fn clicked(&self, sender: &NSControl) {
            let cb = CALLBACKS.with(|c| c.borrow().get(&sender.tag()).cloned());
            if let Some(cb) = cb {
                cb();
            }
        }
    }
);

fn target(mtm: MainThreadMarker) -> Retained<ClickTarget> {
    TARGET.with(|t| {
        t.borrow_mut()
            .get_or_insert_with(|| unsafe { msg_send![ClickTarget::alloc(mtm), init] })
            .clone()
    })
}

// ------------------------------------------------------------------ hover button

#[derive(Default)]
pub struct HoverIvars {
    hovered: Cell<bool>,
}

define_class!(
    // SAFETY: NSButton has no subclassing requirements; HoverButton does not implement Drop.
    #[unsafe(super(NSButton, NSControl, NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = HoverIvars]
    /// Icon button that lights up under the pointer: full-strength icon on a soft rounded backdrop.
    pub struct HoverButton;

    impl HoverButton {
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
            track(self);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            if self.ivars().hovered.get() {
                let base = if self.row_selected() { NSColor::selectedMenuItemTextColor() } else { NSColor::labelColor() };
                base.colorWithAlphaComponent(0.2).set();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(self.bounds(), 5.0, 5.0).fill();
            }
            let _: () = unsafe { msg_send![super(self), drawRect: dirty] };
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.hover(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.hover(false);
        }

        #[unsafe(method(setEnabled:))]
        fn set_enabled(&self, flag: bool) {
            let _: () = unsafe { msg_send![super(self), setEnabled: flag] };
            if !flag {
                self.hover(false);
            }
        }
    }
);

impl HoverButton {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(HoverIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn row_selected(&self) -> bool {
        unsafe { self.superview() }
            .and_then(|v| v.downcast::<RowView>().ok())
            .is_some_and(|row| row.ivars().highlighted.get())
    }

    pub fn is_hovered(&self) -> bool {
        self.ivars().hovered.get()
    }

    /// Light the button up (or not); its tint follows the row's highlight.
    pub fn hover(&self, on: bool) {
        let on = on && self.isEnabled();
        self.ivars().hovered.set(on);
        let selected = self.row_selected();
        let base = if selected { NSColor::selectedMenuItemTextColor() } else { NSColor::labelColor() };
        let tint = if on || selected { base } else { NSColor::secondaryLabelColor() };
        self.setContentTintColor(Some(&tint));
        let view: &NSView = self;
        view.setNeedsDisplay(true);
    }
}

// ------------------------------------------------------------------ row view

#[derive(Default)]
pub struct RowIvars {
    highlighted: Cell<bool>,
    on_click: RefCell<Option<Callback>>,
    icon: RefCell<Option<Retained<NSImageView>>>,
    /// Each label with its colour when the row is not highlighted.
    label_colors: RefCell<Vec<(Retained<NSTextField>, Retained<NSColor>)>>,
    buttons: RefCell<Vec<Retained<HoverButton>>>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; RowView does not implement Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[ivars = RowIvars]
    /// Highlights like a menu item under the pointer; a click outside its buttons calls on_click.
    pub struct RowView;

    impl RowView {
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
            track(self);
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            if self.ivars().highlighted.get() {
                NSColor::selectedContentBackgroundColor().set();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                    inset(self.bounds(), HILITE_INSET, 0.0), HILITE_RADIUS, HILITE_RADIUS).fill();
            }
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            if self.window().is_none() {
                // menu closed, possibly with the pointer still over this row
                self.set_highlighted(false);
                for b in self.ivars().buttons.borrow().iter() {
                    if b.is_hovered() {
                        b.hover(false);
                    }
                }
            }
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            self.set_highlighted(true);
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.set_highlighted(false);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, _event: &NSEvent) {
            let Some(cb) = self.ivars().on_click.borrow().clone() else { return };
            self.set_highlighted(false);
            if let Some(menu) = self.enclosingMenuItem().and_then(|item| unsafe { item.menu() }) {
                menu.cancelTracking();
            }
            cb();
        }
    }
);

impl RowView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RowIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn is_highlighted(&self) -> bool {
        self.ivars().highlighted.get()
    }

    /// The menu-item highlight: accent background, white text and icons. Only clickable rows light up.
    pub fn set_highlighted(&self, on: bool) {
        let iv = self.ivars();
        let on = on && iv.on_click.borrow().is_some();
        if on == iv.highlighted.get() {
            return;
        }
        iv.highlighted.set(on);
        let selected = NSColor::selectedMenuItemTextColor();
        for (f, normal) in iv.label_colors.borrow().iter() {
            f.setTextColor(Some(if on { &selected } else { normal }));
        }
        if let Some(icon) = iv.icon.borrow().as_ref() {
            icon.setContentTintColor(Some(&*if on { selected.clone() } else { NSColor::labelColor() }));
        }
        for b in iv.buttons.borrow().iter() {
            b.hover(b.is_hovered());
        }
        self.setNeedsDisplay(true);
    }
}

// ------------------------------------------------------------------ Row

/// The row's leading icon: an SF Symbol name, or a ready image (e.g. a glyph cut from an app icon).
pub enum RowIcon {
    Symbol(&'static str),
    Image(Retained<NSImage>),
}

/// A button: (key, SF Symbol, tooltip, what it does).
pub struct ButtonSpec {
    pub key: &'static str,
    pub symbol: &'static str,
    pub tip: String,
    pub action: Callback,
}

fn text_width(text: &str, font: &NSFont) -> f64 {
    let attrs = NSDictionary::<NSString, AnyObject>::from_slices(&[unsafe { NSFontAttributeName }], &[font as &AnyObject]);
    let s = unsafe { NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &NSString::from_str(text), Some(&attrs)) };
    s.size().width
}

/// One source row. `widths`: the widest text each column will ever hold (fixed, so nothing jumps).
/// `buttons`: None leaves that button slot empty, so buttons line up in columns across rows.
/// `on_click`: what clicking the row opens.
pub struct Row {
    pub view: Retained<RowView>,
    labels: Vec<Retained<NSTextField>>,
    pub buttons: HashMap<&'static str, Retained<HoverButton>>,
}

impl Row {
    pub fn new(
        mtm: MainThreadMarker,
        item: &NSMenuItem,
        icon: RowIcon,
        widths: &[Vec<String>],
        buttons: Vec<Option<ButtonSpec>>,
        on_click: Option<Callback>,
    ) -> Row {
        let font = NSFont::menuFontOfSize(0.0);
        let cols: Vec<f64> =
            widths.iter().map(|ws| ws.iter().map(|t| text_width(t, &font)).fold(0.0, f64::max)).collect();
        let mut xs = vec![TEXT_X];
        for w in &cols[..cols.len() - 1] {
            xs.push(xs.last().unwrap() + w + GAP);
        }
        let right = xs.last().unwrap() + cols.last().unwrap() + GAP;
        let total = right + buttons.len() as f64 * (BTN + BTN_GAP) + RIGHT;
        let view = RowView::new(mtm, rect(0.0, 0.0, total, ROW_H));
        *view.ivars().on_click.borrow_mut() = on_click;

        let image = match icon {
            RowIcon::Symbol(name) => icons::symbol(name, 13.0),
            RowIcon::Image(img) => Some(img),
        };
        let iv = match image {
            Some(img) => NSImageView::imageViewWithImage(&img, mtm),
            None => NSImageView::new(mtm),
        };
        iv.setFrame(rect(ICON_X, (ROW_H - 16.0) / 2.0, 16.0, 16.0));
        iv.setContentTintColor(Some(&NSColor::labelColor()));
        view.addSubview(&iv);
        *view.ivars().icon.borrow_mut() = Some(iv);

        let mut labels = Vec::new();
        for (i, (x, w)) in xs.iter().zip(&cols).enumerate() {
            let f = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            f.setFont(Some(&font));
            let normal = if i == cols.len() - 1 { NSColor::secondaryLabelColor() } else { NSColor::labelColor() };
            f.setTextColor(Some(&normal));
            let h = f.fittingSize().height;
            f.setFrame(rect(*x, (ROW_H - h) / 2.0, w + 4.0, h));
            view.addSubview(&f);
            view.ivars().label_colors.borrow_mut().push((f.clone(), normal));
            labels.push(f);
        }

        let tgt = target(mtm);
        let mut btns = HashMap::new();
        let mut x = right;
        for spec in buttons {
            let Some(spec) = spec else {
                x += BTN + BTN_GAP;
                continue;
            };
            let tag = CALLBACKS.with(|c| {
                let mut c = c.borrow_mut();
                let tag = c.len() as isize + 1;
                c.insert(tag, spec.action);
                tag
            });
            let b = HoverButton::new(mtm, rect(x, (ROW_H - BTN) / 2.0, BTN, BTN));
            b.setTitle(&NSString::from_str(""));
            b.setImage(icons::symbol(spec.symbol, 12.0).as_deref());
            b.setImagePosition(NSCellImagePosition::ImageOnly);
            b.setImageScaling(NSImageScaling::ScaleProportionallyDown);
            b.setBordered(false);
            unsafe {
                b.setTarget(Some(&tgt));
                b.setAction(Some(sel!(clicked:)));
            }
            b.setTag(tag);
            b.setToolTip(Some(&NSString::from_str(&spec.tip)));
            b.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
            view.addSubview(&b);
            view.ivars().buttons.borrow_mut().push(b.clone());
            btns.insert(spec.key, b);
            x += BTN + BTN_GAP;
        }
        item.setView(Some(&view));
        Row { view, labels, buttons: btns }
    }

    pub fn set(&self, texts: &[&str]) {
        for (f, t) in self.labels.iter().zip(texts) {
            if f.stringValue().to_string() != *t {
                f.setStringValue(&NSString::from_str(t));
            }
        }
    }

    pub fn text(&self, col: usize) -> String {
        self.labels.get(col).map(|f| f.stringValue().to_string()).unwrap_or_default()
    }

    /// Show the state column in red (an error) with the message as the row's tooltip.
    pub fn set_alert(&self, on: bool, tip: Option<&str>) {
        let f = &self.labels[1];
        let normal = if on { NSColor::systemRedColor() } else { NSColor::labelColor() };
        for (g, c) in self.view.ivars().label_colors.borrow_mut().iter_mut() {
            if g == f {
                *c = normal.clone();
            }
        }
        if !self.view.is_highlighted() {
            f.setTextColor(Some(&normal));
        }
        let tip = tip.filter(|t| on && !t.is_empty());
        self.set_tooltip(tip);
    }

    pub fn set_tooltip(&self, tip: Option<&str>) {
        self.view.setToolTip(tip.map(NSString::from_str).as_deref());
    }

    pub fn show_buttons(&self, shown: bool) {
        for b in self.buttons.values() {
            if b.isHidden() == shown {
                b.setHidden(!shown);
            }
        }
    }

    pub fn set_button(&self, key: &str, symbol: &str, tooltip: &str) {
        if let Some(b) = self.buttons.get(key) {
            b.setImage(icons::symbol(symbol, 12.0).as_deref());
            b.setToolTip(Some(&NSString::from_str(tooltip)));
        }
    }
}

/// The top row: symbol · text (the inbox count, dimmed like a disabled item), and small grey text
/// on the right (the build version), as wide as the source rows.
pub struct Header {
    pub view: Retained<NSView>,
    text: Retained<NSTextField>,
}

impl Header {
    pub fn new(mtm: MainThreadMarker, width: f64, symbol: &str, right: &str) -> Header {
        let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, ROW_H));
        if let Some(img) = icons::symbol(symbol, 13.0) {
            let iv = NSImageView::imageViewWithImage(&img, mtm);
            iv.setFrame(rect(ICON_X, (ROW_H - 16.0) / 2.0, 16.0, 16.0));
            iv.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
            view.addSubview(&iv);
        }
        let small = NSFont::systemFontOfSize(NSFont::smallSystemFontSize());
        let ver = NSTextField::labelWithString(&NSString::from_str(right), mtm);
        ver.setFont(Some(&small));
        ver.setTextColor(Some(&NSColor::tertiaryLabelColor()));
        let (vw, vh) = (text_width(right, &small) + 4.0, ver.fittingSize().height);
        ver.setFrame(rect(width - RIGHT - vw, (ROW_H - vh) / 2.0, vw, vh));
        view.addSubview(&ver);
        let text = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        text.setFont(Some(&NSFont::menuFontOfSize(0.0)));
        text.setTextColor(Some(&NSColor::secondaryLabelColor()));
        let th = text.fittingSize().height;
        text.setFrame(rect(TEXT_X, (ROW_H - th) / 2.0, width - TEXT_X - RIGHT - vw - GAP, th));
        view.addSubview(&text);
        Header { view, text }
    }

    pub fn set_text(&self, s: &str) {
        self.text.setStringValue(&NSString::from_str(s));
    }

    pub fn text(&self) -> String {
        self.text.stringValue().to_string()
    }
}
