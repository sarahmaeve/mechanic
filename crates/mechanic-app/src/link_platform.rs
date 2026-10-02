//! Native link presentation and actions. Menu callers retain their URL snapshot.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    Open,
    Copy,
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::Cell;

    use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, rc::Retained, sel};
    use objc2_app_kit::{NSMenu, NSMenuItem, NSView, NSWorkspace};
    use objc2_foundation::{
        MainThreadMarker, NSObject, NSObjectProtocol, NSPoint, NSString, NSURL,
    };
    use winit::{
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
        window::Window,
    };

    use super::LinkAction;

    struct Selection {
        action: Cell<Option<LinkAction>>,
        allow_open: bool,
    }

    define_class!(
        // SAFETY: NSObject has no subclass requirements; this class has no Drop implementation.
        #[unsafe(super = NSObject)]
        #[name = "MechanicLinkMenuTarget"]
        #[thread_kind = MainThreadOnly]
        #[ivars = Selection]
        struct MenuTarget;

        // SAFETY: NSObjectProtocol adds no safety requirements.
        unsafe impl NSObjectProtocol for MenuTarget {}

        impl MenuTarget {
            // SAFETY: AppKit invokes this action with the NSMenuItem that owns the selector.
            #[unsafe(method(chooseLink:))]
            fn choose_link(&self, item: &NSMenuItem) {
                self.ivars().action.set(match item.tag() {
                    1 if self.ivars().allow_open => Some(LinkAction::Open),
                    2 => Some(LinkAction::Copy),
                    _ => None,
                });
            }
        }
    );

    impl MenuTarget {
        fn new(mtm: MainThreadMarker, allow_open: bool) -> Retained<Self> {
            let this =
                Self::alloc(mtm).set_ivars(Selection { action: Cell::new(None), allow_open });
            // SAFETY: NSObject init initializes the allocated superclass.
            unsafe { msg_send![super(this), init] }
        }
    }

    // Drop the menu and its items before releasing their unretained action target.
    struct LinkMenu {
        menu: Retained<NSMenu>,
        target: Retained<MenuTarget>,
    }

    impl LinkMenu {
        fn new(mtm: MainThreadMarker, allow_open: bool) -> Self {
            let target = MenuTarget::new(mtm, allow_open);
            let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Link"));
            menu.setAutoenablesItems(false);
            for (title, tag, enabled) in
                [("Open Link", 1, allow_open), ("Copy Link Address", 2, true)]
            {
                // SAFETY: The selector matches MenuTarget::choose_link, and
                // LinkMenu retains the target until after the menu is dropped.
                let item = unsafe {
                    let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(title),
                        Some(sel!(chooseLink:)),
                        &NSString::new(),
                    );
                    item.setTarget(Some(&target));
                    item
                };
                item.setTag(tag);
                item.setEnabled(enabled);
                menu.addItem(&item);
            }
            Self { menu, target }
        }
    }

    fn native_view(window: &Window) -> Result<(MainThreadMarker, Retained<NSView>), String> {
        let mtm =
            MainThreadMarker::new().ok_or("native link interactions require the main thread")?;
        let handle = window.window_handle().map_err(|error| error.to_string())?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err("window does not expose an AppKit view".into());
        };
        // SAFETY: winit owns this live NSView while `window` is borrowed. Retain
        // it rather than taking ownership of winit's existing reference. The
        // marker above guarantees its main-thread-only methods are safe here.
        let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
            .ok_or("window has no native view")?;
        Ok((mtm, view))
    }

    pub fn set_hover(window: &Window, target: Option<&str>) -> Result<(), String> {
        let (_, view) = native_view(window)?;
        let text = target.map(NSString::from_str);
        // AppKit copies the tooltip, so no Rust target borrow escapes this call.
        view.setToolTip(text.as_deref());
        Ok(())
    }

    fn menu_position(
        position: (f64, f64),
        scale: f64,
        origin: (f64, f64),
        size: (f64, f64),
        flipped: bool,
    ) -> Option<NSPoint> {
        if ![position.0, position.1, scale, origin.0, origin.1, size.0, size.1]
            .iter()
            .all(|value| value.is_finite())
            || scale <= 0.0
            || size.0 < 0.0
            || size.1 < 0.0
        {
            return None;
        }
        let x = (position.0 / scale).clamp(0.0, size.0);
        let y = (position.1 / scale).clamp(0.0, size.1);
        Some(NSPoint::new(origin.0 + x, origin.1 + if flipped { y } else { size.1 - y }))
    }

    /// The popup runs AppKit's tracking loop. Its target is retained until that
    /// loop returns; the caller must keep the URL selected before opening it.
    pub fn context_menu(
        window: &Window,
        position: (f64, f64),
        allow_open: bool,
    ) -> Option<LinkAction> {
        let (mtm, view) = native_view(window).ok()?;
        let bounds = view.bounds();
        let point = menu_position(
            position,
            window.scale_factor(),
            (bounds.origin.x, bounds.origin.y),
            (bounds.size.width, bounds.size.height),
            view.isFlipped(),
        )?;
        let popup = LinkMenu::new(mtm, allow_open);
        popup.menu.popUpMenuPositioningItem_atLocation_inView(None, point, Some(&view));
        popup.target.ivars().action.get()
    }

    fn open_with(target: &str, launch: impl FnOnce(&NSURL) -> bool) -> Result<(), String> {
        if target.chars().any(char::is_control) {
            return Err("link contains control characters".into());
        }
        let url =
            NSURL::URLWithString(&NSString::from_str(target)).ok_or("link is not a valid URL")?;
        let scheme = url.scheme().map(|scheme| scheme.to_string().to_ascii_lowercase());
        if !matches!(scheme.as_deref(), Some("http" | "https"))
            || url.host().is_none_or(|host| host.is_empty())
        {
            return Err("only HTTP and HTTPS links with a host can be opened".into());
        }
        if launch(&url) { Ok(()) } else { Err("macOS could not open the link".into()) }
    }

    pub fn open_web_url(target: &str) -> Result<(), String> {
        let _mtm = MainThreadMarker::new().ok_or("opening links requires the main thread")?;
        open_with(target, |url| NSWorkspace::sharedWorkspace().openURL(url))
    }

    /// Used only by the explicitly compiled main-thread native smoke example.
    #[cfg(test)]
    #[allow(dead_code, reason = "opt-in example runs this on the AppKit main thread")]
    pub(crate) fn native_smoke(window: &Window) -> Result<(), String> {
        let (mtm, view) = native_view(window)?;
        {
            let target = String::from("https://example.com/native-tooltip");
            set_hover(window, Some(&target))?;
        }
        if view.toolTip().map(|text| text.to_string()).as_deref()
            != Some("https://example.com/native-tooltip")
        {
            return Err("AppKit did not retain the tooltip text".into());
        }
        set_hover(window, None)?;
        if view.toolTip().is_some_and(|text| !text.is_empty()) {
            return Err("AppKit did not clear the tooltip".into());
        }
        let allowed = LinkMenu::new(mtm, true);
        if allowed.target.ivars().action.get().is_some() {
            return Err("a new menu already has an action".into());
        }
        allowed.menu.performActionForItemAtIndex(0);
        if allowed.target.ivars().action.get() != Some(LinkAction::Open) {
            return Err("Open Link action was not delivered".into());
        }
        allowed.menu.performActionForItemAtIndex(1);
        if allowed.target.ivars().action.get() != Some(LinkAction::Copy) {
            return Err("Copy Link Address action was not delivered".into());
        }
        let blocked = LinkMenu::new(mtm, false);
        if blocked.menu.itemAtIndex(0).is_none_or(|item| item.isEnabled()) {
            return Err("unsafe Open Link item is enabled".into());
        }
        blocked.menu.performActionForItemAtIndex(0);
        if blocked.target.ivars().action.get().is_some() {
            return Err("disabled Open Link delivered an action".into());
        }
        blocked.menu.performActionForItemAtIndex(1);
        if blocked.target.ivars().action.get() != Some(LinkAction::Copy) {
            return Err("Copy Link Address was disabled with Open Link".into());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn menu_position_accounts_for_retina_origin_and_view_flipping() {
            let point = menu_position((40.0, 60.0), 2.0, (5.0, 7.0), (100.0, 80.0), false).unwrap();
            assert_eq!((point.x, point.y), (25.0, 57.0));
            let point = menu_position((40.0, 60.0), 2.0, (5.0, 7.0), (100.0, 80.0), true).unwrap();
            assert_eq!((point.x, point.y), (25.0, 37.0));
            let point =
                menu_position((-3.0, 500.0), 2.0, (0.0, 0.0), (100.0, 80.0), false).unwrap();
            assert_eq!((point.x, point.y), (0.0, 0.0));
        }

        #[test]
        fn invalid_menu_coordinates_are_rejected() {
            assert!(
                menu_position((f64::NAN, 0.0), 2.0, (0.0, 0.0), (100.0, 80.0), false).is_none()
            );
            assert!(menu_position((0.0, 0.0), 0.0, (0.0, 0.0), (100.0, 80.0), false).is_none());
        }

        #[test]
        fn unsafe_and_relative_urls_never_reach_the_native_opener() {
            for target in [
                "file:///etc/passwd",
                "javascript:alert(1)",
                "example.com",
                "http:relative",
                "https://example.com/\n",
            ] {
                assert!(open_with(target, |_| panic!("unsafe URL reached the opener")).is_err());
            }
        }

        #[test]
        fn web_urls_use_native_url_objects_and_report_launch_failure() {
            let launched = Cell::new(false);
            assert!(
                open_with("https://example.com/a?q=x#part", |url| {
                    assert_eq!(url.host().unwrap().to_string(), "example.com");
                    launched.set(true);
                    true
                })
                .is_ok()
            );
            assert!(launched.get());
            assert!(open_with("http://[::1]:8080/", |_| false).is_err());
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{context_menu, open_web_url, set_hover};

#[cfg(all(test, target_os = "macos"))]
#[allow(unused_imports, reason = "used by the opt-in native smoke example")]
pub(crate) use macos::native_smoke;

#[cfg(not(target_os = "macos"))]
pub fn set_hover(_: &winit::window::Window, _: Option<&str>) -> Result<(), String> {
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub fn context_menu(_: &winit::window::Window, _: (f64, f64), _: bool) -> Option<LinkAction> {
    None
}
#[cfg(not(target_os = "macos"))]
pub fn open_web_url(_: &str) -> Result<(), String> {
    Err("native link opening is only supported on macOS".into())
}
