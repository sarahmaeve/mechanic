//! Native command-completion delivery. No external process, polling, or retry timer.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    };

    use block2::{Block, RcBlock};
    use objc2::{
        AnyThread, define_class, extern_protocol, msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject, Bool},
    };
    use objc2_foundation::{MainThreadMarker, NSBundle, NSObject, NSObjectProtocol, NSString};
    use winit::window::{UserAttentionType, Window};

    use crate::notifications::CompletionNotification;

    // Bind the modern framework while using the application's existing objc2
    // version. The available generated UserNotifications crate uses objc2 0.5.
    #[link(name = "UserNotifications", kind = "framework")]
    unsafe extern "C" {}

    const UNREQUESTED: u8 = 0;
    const PENDING: u8 = 1;
    const AUTHORIZED: u8 = 2;
    const STOPPED: u8 = 3;
    const MAX_IN_FLIGHT: usize = 8;
    // Apple's UNAuthorizationOptionAlert; no sound or badge authorization.
    const ALERT_AUTHORIZATION: usize = 1 << 2;
    const PRESENT_BANNER_AND_LIST: usize = (1 << 3) | (1 << 4);

    extern_protocol!(
        /// Native presentation callback for policy-approved notifications.
        ///
        /// # Safety
        /// Implementations must preserve the documented Objective-C method
        /// signature and invoke the completion block exactly once, from any
        /// native callback thread, without allowing a panic across the ABI.
        #[allow(
            clippy::missing_safety_doc,
            reason = "extern_protocol! expansion prevents Clippy from seeing the Safety section above"
        )]
        unsafe trait UNUserNotificationCenterDelegate: NSObjectProtocol {
            #[optional]
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                center: &AnyObject,
                notification: &AnyObject,
                completion: &Block<dyn Fn(usize)>,
            );
        }
    );

    define_class!(
        // SAFETY: NSObject has no subclass requirements. There are no ivars,
        // and the presentation callback has no thread-specific behavior.
        #[unsafe(super = NSObject)]
        #[name = "MechanicCommandNotificationDelegate"]
        struct NotificationDelegate;

        // SAFETY: NSObjectProtocol adds no extra requirements.
        unsafe impl NSObjectProtocol for NotificationDelegate {}

        // SAFETY: The method uses the protocol's exact object/block ABI.
        unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &AnyObject,
                _notification: &AnyObject,
                completion: &Block<dyn Fn(usize)>,
            ) {
                // A different Mechanic window may be focused. The app's
                // per-window policy already approved this source completion.
                completion.call((PRESENT_BANNER_AND_LIST,));
            }
        }
    );

    impl NotificationDelegate {
        fn new() -> Retained<Self> {
            // SAFETY: NSObject's initializer initializes this allocated subclass.
            unsafe { msg_send![Self::alloc(), init] }
        }
    }

    #[derive(Default)]
    struct DeliveryState {
        authorization: AtomicU8,
        in_flight: AtomicUsize,
    }

    impl DeliveryState {
        fn fail(&self, message: &str) {
            if self.authorization.swap(STOPPED, Ordering::AcqRel) != STOPPED {
                log::warn!("command notifications disabled for this launch: {message}");
            }
        }

        fn reserve_delivery(&self) -> bool {
            self.in_flight
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < MAX_IN_FLIGHT).then_some(count + 1)
                })
                .is_ok()
        }
    }

    enum Route {
        Uninitialized,
        Dock,
        Center {
            center: Retained<AnyObject>,
            state: Arc<DeliveryState>,
            _delegate: Retained<NotificationDelegate>,
        },
        Disabled,
    }

    pub struct NativeNotifications {
        route: RefCell<Route>,
        ready: Arc<dyn Fn(CompletionNotification) + Send + Sync>,
    }

    impl NativeNotifications {
        /// `ready` may run on a native background queue. It must only enqueue
        /// an owned event and must not panic. The application rechecks window,
        /// pane, session, and focus before resubmitting that notification.
        pub fn new(
            enabled: bool,
            ready: impl Fn(CompletionNotification) + Send + Sync + 'static,
        ) -> Self {
            Self {
                route: RefCell::new(if enabled { Route::Uninitialized } else { Route::Disabled }),
                ready: Arc::new(ready),
            }
        }

        /// Call only for a currently valid, unfocused completion scope.
        pub fn deliver(&self, window: &Window, notification: CompletionNotification) {
            if window.id() != notification.scope.window || window.has_focus() {
                return;
            }
            let mut route = self.route.borrow_mut();
            if matches!(*route, Route::Uninitialized) {
                *route = match initialize() {
                    Ok(route) => route,
                    Err(error) => {
                        log::warn!("command notifications disabled for this launch: {error}");
                        Route::Disabled
                    }
                };
            }
            match &*route {
                Route::Dock => {
                    window.request_user_attention(Some(UserAttentionType::Informational))
                }
                Route::Center { center, state, .. } => {
                    match state.authorization.load(Ordering::Acquire) {
                        UNREQUESTED => self.request_authorization(center, state, notification),
                        AUTHORIZED => submit(center, state, &notification),
                        // Keep just the first pending candidate in the callback;
                        // a slow permission UI never creates an unbounded queue.
                        PENDING | STOPPED => {}
                        _ => unreachable!("invalid native authorization state"),
                    }
                }
                Route::Disabled | Route::Uninitialized => {}
            }
        }

        fn request_authorization(
            &self,
            center: &AnyObject,
            state: &Arc<DeliveryState>,
            notification: CompletionNotification,
        ) {
            if state
                .authorization
                .compare_exchange(UNREQUESTED, PENDING, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let state = state.clone();
            let ready = self.ready.clone();
            let completion = RcBlock::new(move |granted: Bool, error: *mut NSObject| {
                if !error.is_null() {
                    state.fail("macOS authorization failed");
                } else if granted.as_bool() {
                    state.authorization.store(AUTHORIZED, Ordering::Release);
                    ready(notification.clone());
                } else {
                    state.authorization.store(STOPPED, Ordering::Release);
                    log::debug!("command notifications declined by macOS authorization");
                }
            });
            // SAFETY: UNUserNotificationCenter's documented signature takes
            // NSUInteger flags and a copied block (BOOL, NSError*). NSObject*
            // has the same object-pointer ABI; the error is never dereferenced.
            // The block owns all callback state and never borrows this service.
            unsafe {
                let _: () = msg_send![center,
                    requestAuthorizationWithOptions: ALERT_AUTHORIZATION,
                    completionHandler: &*completion];
            }
        }
    }

    impl Drop for NativeNotifications {
        fn drop(&mut self) {
            if let Route::Center { center, .. } = self.route.get_mut() {
                // SAFETY: The center's delegate is weak; detach it while our
                // retained delegate is still alive, before dropping the route.
                unsafe {
                    let _: () = msg_send![&**center, setDelegate: None::<&AnyObject>];
                }
            }
        }
    }

    fn initialize() -> Result<Route, String> {
        let _mtm = MainThreadMarker::new().ok_or("native notifications require the main thread")?;
        // currentNotificationCenter raises an exception without a bundle ID.
        // Cargo/CLI builds use AppKit's bounded informational Dock request.
        if NSBundle::mainBundle().bundleIdentifier().is_none_or(|id| id.is_empty()) {
            log::debug!("command notifications use native Dock attention for this unbundled build");
            return Ok(Route::Dock);
        }
        let class = AnyClass::get(c"UNUserNotificationCenter")
            .ok_or("macOS UserNotifications is unavailable")?;
        // SAFETY: The linked class exposes currentNotificationCenter as an
        // autoreleased object result; Retained applies the appropriate retain.
        // Bundle identity above avoids its documented unbundled exception.
        let center: Retained<AnyObject> = unsafe { msg_send![class, currentNotificationCenter] };
        let delegate = NotificationDelegate::new();
        // SAFETY: The retained delegate implements the native protocol. Route
        // owns it until the center's weak delegate pointer is detached in Drop.
        unsafe {
            let _: () = msg_send![&*center, setDelegate: &*delegate];
        }
        Ok(Route::Center { center, state: Arc::new(DeliveryState::default()), _delegate: delegate })
    }

    fn request(notification: &CompletionNotification) -> Result<Retained<AnyObject>, String> {
        let content_class = AnyClass::get(c"UNMutableNotificationContent")
            .ok_or("macOS notification content is unavailable")?;
        let request_class = AnyClass::get(c"UNNotificationRequest")
            .ok_or("macOS notification requests are unavailable")?;
        // SAFETY: These are the documented NSObject initializer, NSString
        // setters, and factory signature for UNNotificationRequest. A nil
        // trigger means immediate delivery. AppKit copies all text content.
        unsafe {
            let content: Retained<AnyObject> = msg_send![content_class, new];
            let _: () = msg_send![&*content, setTitle: &*NSString::from_str(notification.title())];
            let _: () = msg_send![&*content, setBody: &*NSString::from_str(notification.body())];
            let identifier = NSString::from_str(&notification.identifier());
            let request: Retained<AnyObject> = msg_send![request_class,
                requestWithIdentifier: &*identifier,
                content: &*content,
                trigger: None::<&AnyObject>];
            Ok(request)
        }
    }

    fn submit(
        center: &AnyObject,
        state: &Arc<DeliveryState>,
        notification: &CompletionNotification,
    ) {
        let request = match request(notification) {
            Ok(request) => request,
            Err(error) => {
                state.fail(&error);
                return;
            }
        };
        if !state.reserve_delivery() {
            return;
        }
        let state = state.clone();
        let completion = RcBlock::new(move |error: *mut NSObject| {
            state.in_flight.fetch_sub(1, Ordering::AcqRel);
            if !error.is_null() {
                state.fail("macOS could not schedule the notification");
            }
        });
        // SAFETY: The documented method accepts a UNNotificationRequest and
        // copies its NSError* completion block. The block owns atomic state;
        // native delivery retains request contents independently of this call.
        unsafe {
            let _: () = msg_send![center,
                addNotificationRequest: &*request,
                withCompletionHandler: &*completion];
        }
    }

    /// Opt-in native smoke: construct/read native content without touching the
    /// notification center, requesting permission, or delivering any alert.
    #[cfg(test)]
    #[allow(dead_code, reason = "used by the explicit native smoke example")]
    pub(crate) fn native_smoke(notification: &CompletionNotification) -> Result<(), String> {
        let native = request(notification)?;
        // SAFETY: Inspect only documented, retained native request properties.
        unsafe {
            let identifier: Retained<NSString> = msg_send![&*native, identifier];
            let content: Retained<AnyObject> = msg_send![&*native, content];
            let title: Retained<NSString> = msg_send![&*content, title];
            let body: Retained<NSString> = msg_send![&*content, body];
            let trigger: Option<Retained<AnyObject>> = msg_send![&*native, trigger];
            if identifier.to_string() != notification.identifier()
                || title.to_string() != notification.title()
                || body.to_string() != notification.body()
                || trigger.is_some()
            {
                return Err("macOS notification content did not retain its private snapshot".into());
            }
        }
        let presentation = std::cell::Cell::new(0);
        let completion = RcBlock::new(|options: usize| presentation.set(options));
        let delegate = NotificationDelegate::new();
        // SAFETY: Invoke our implementation with the same documented signature
        // the native center uses; this tests the Objective-C selector wiring.
        unsafe {
            let _: () = msg_send![&*delegate,
                userNotificationCenter: &*native,
                willPresentNotification: &*native,
                withCompletionHandler: &*completion];
        }
        if presentation.get() != PRESENT_BANNER_AND_LIST {
            return Err("macOS foreground presentation options were not delivered".into());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_failure_and_inflight_work_are_bounded() {
            let state = DeliveryState::default();
            assert_eq!(state.authorization.load(Ordering::Acquire), UNREQUESTED);
            for _ in 0..MAX_IN_FLIGHT {
                assert!(state.reserve_delivery());
            }
            assert!(!state.reserve_delivery());
            state.in_flight.fetch_sub(1, Ordering::AcqRel);
            assert!(state.reserve_delivery());
            state.fail("test failure");
            state.fail("duplicate failure");
            assert_eq!(state.authorization.load(Ordering::Acquire), STOPPED);
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::NativeNotifications;

#[cfg(all(test, target_os = "macos"))]
#[allow(unused_imports, reason = "used by the explicit native smoke example")]
pub(crate) use macos::native_smoke;

#[cfg(not(target_os = "macos"))]
pub struct NativeNotifications;

#[cfg(not(target_os = "macos"))]
impl NativeNotifications {
    pub fn new(
        _: bool,
        _: impl Fn(crate::notifications::CompletionNotification) + Send + Sync + 'static,
    ) -> Self {
        Self
    }

    pub fn deliver(
        &self,
        _: &winit::window::Window,
        _: crate::notifications::CompletionNotification,
    ) {
    }
}
