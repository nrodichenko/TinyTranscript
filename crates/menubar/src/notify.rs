//! "Transcript ready" notifications (NSUserNotification, as the Python app used; it is deprecated
//! but still works and needs no authorisation prompt).
#![allow(deprecated)]

use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_foundation::{
    NSDictionary, NSString, NSUserNotification, NSUserNotificationCenter, NSUserNotificationCenterDelegate,
};

pub struct NotifyIvars {
    /// Opened when the notification's transcript has been renamed or moved since.
    fallback: PathBuf,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; NotifyDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NotifyIvars]
    /// Show notifications even while the app is active; clicking one opens its transcript.
    pub struct NotifyDelegate;

    unsafe impl NSObjectProtocol for NotifyDelegate {}

    unsafe impl NSUserNotificationCenterDelegate for NotifyDelegate {
        #[unsafe(method(userNotificationCenter:shouldPresentNotification:))]
        fn should_present(&self, _center: &NSUserNotificationCenter, _note: &NSUserNotification) -> bool {
            true
        }

        #[unsafe(method(userNotificationCenter:didActivateNotification:))]
        fn did_activate(&self, _center: &NSUserNotificationCenter, note: &NSUserNotification) {
            let path = note
                .userInfo()
                .and_then(|info| info.objectForKey(&NSString::from_str("path")))
                .and_then(|v| v.downcast::<NSString>().ok())
                .map(|s| PathBuf::from(s.to_string()));
            let target = match path {
                Some(p) if p.exists() => p,
                _ => self.ivars().fallback.clone(), // renamed since: open the folder
            };
            let _ = std::process::Command::new("/usr/bin/open").arg(&target).output();
        }
    }
);

pub struct Notifier {
    center: Retained<NSUserNotificationCenter>,
    _delegate: Retained<NotifyDelegate>,
}

impl Notifier {
    /// None when there is no notification center (e.g. the binary runs outside an app bundle).
    pub fn new(mtm: MainThreadMarker, fallback: &Path) -> Option<Notifier> {
        let center: Option<Retained<NSUserNotificationCenter>> =
            unsafe { msg_send![NSUserNotificationCenter::class(), defaultUserNotificationCenter] };
        let center = center?;
        let delegate = NotifyDelegate::alloc(mtm).set_ivars(NotifyIvars { fallback: fallback.to_path_buf() });
        let delegate: Retained<NotifyDelegate> = unsafe { msg_send![super(delegate), init] };
        unsafe { center.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        Some(Notifier { center, _delegate: delegate })
    }

    pub fn deliver(&self, title: &str, subtitle: &str, informative: Option<&str>, path: &Path) {
        let note = NSUserNotification::new();
        note.setTitle(Some(&NSString::from_str(title)));
        note.setSubtitle(Some(&NSString::from_str(subtitle)));
        if let Some(text) = informative {
            note.setInformativeText(Some(&NSString::from_str(text)));
        }
        let key = NSString::from_str("path");
        let value = NSString::from_str(&path.to_string_lossy());
        let info = NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[&*value as &AnyObject]);
        unsafe { note.setUserInfo(Some(&info)) };
        self.center.deliverNotification(&note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_class_registers() {
        // registering checks the protocol methods' signatures against the protocol (debug builds)
        let cls = NotifyDelegate::class();
        assert!(cls.instance_method(objc2::sel!(userNotificationCenter:shouldPresentNotification:)).is_some());
        assert!(cls.instance_method(objc2::sel!(userNotificationCenter:didActivateNotification:)).is_some());
    }
}
