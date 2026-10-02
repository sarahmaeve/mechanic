//! Modeless native find UI. All AppKit objects stay on the main thread.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchAction {
    Query(String),
    CaseSensitive(bool),
    Next,
    Previous,
    Close,
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use objc2::{
        DefinedClass, MainThreadOnly, define_class, msg_send,
        rc::Retained,
        runtime::{AnyObject, ProtocolObject, Sel},
        sel,
    };
    use objc2_app_kit::{
        NSApplication, NSBackingStoreType, NSButton, NSControl, NSControlTextEditingDelegate,
        NSEvent, NSEventModifierFlags, NSPanel, NSSearchField, NSSearchFieldDelegate, NSTextField,
        NSTextFieldDelegate, NSTextView, NSView, NSWindow, NSWindowDelegate, NSWindowOrderingMode,
        NSWindowStyleMask,
    };
    use objc2_foundation::{
        MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString,
    };
    use winit::{
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
        window::Window,
    };

    use super::SearchAction;

    struct Dispatch {
        enabled: Cell<bool>,
        callback: Box<dyn Fn(SearchAction)>,
    }

    impl Dispatch {
        fn send(&self, action: SearchAction) {
            if self.enabled.get() {
                (self.callback)(action);
            }
        }
    }

    define_class!(
        // SAFETY: NSPanel adds no subclass requirements. This subclass only
        // handles cancellation and otherwise keeps AppKit's standard editing.
        #[unsafe(super = NSPanel)]
        #[name = "MechanicSearchPanel"]
        #[thread_kind = MainThreadOnly]
        #[ivars = Rc<Dispatch>]
        struct NativePanel;

        unsafe impl NSObjectProtocol for NativePanel {}

        impl NativePanel {
            #[unsafe(method(performKeyEquivalent:))]
            fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
                let flags = event.modifierFlags();
                let find = flags.contains(NSEventModifierFlags::Command)
                    && !flags.intersects(NSEventModifierFlags::Control | NSEventModifierFlags::Option)
                    && event.charactersIgnoringModifiers()
                        .is_some_and(|text| text.to_string().eq_ignore_ascii_case("g"));
                if find {
                    self.ivars().send(if flags.contains(NSEventModifierFlags::Shift) {
                        SearchAction::Previous
                    } else { SearchAction::Next });
                    true
                } else {
                    // SAFETY: Forward the same live event to NSPanel's standard
                    // responder handling so native editing shortcuts still work.
                    let handled: bool = unsafe { msg_send![super(self), performKeyEquivalent: event] };
                    handled
                }
            }

            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: Option<&AnyObject>) {
                self.dismiss();
            }
        }
    );

    impl NativePanel {
        fn dismiss(&self) {
            self.orderOut(None);
            self.ivars().send(SearchAction::Close);
        }
    }

    struct TargetState {
        dispatch: Rc<Dispatch>,
        field: Retained<NSSearchField>,
        panel: Retained<NativePanel>,
        last_query: RefCell<String>,
    }

    define_class!(
        // SAFETY: NSObject has no subclass requirements; AppKit retains neither
        // its action target nor delegates, so SearchPanel owns this object.
        #[unsafe(super = NSObject)]
        #[name = "MechanicSearchTarget"]
        #[thread_kind = MainThreadOnly]
        #[ivars = TargetState]
        struct SearchTarget;

        unsafe impl NSObjectProtocol for SearchTarget {}
        unsafe impl NSTextFieldDelegate for SearchTarget {}
        unsafe impl NSSearchFieldDelegate for SearchTarget {}

        unsafe impl NSControlTextEditingDelegate for SearchTarget {
            #[unsafe(method(controlTextDidChange:))]
            fn control_text_did_change(&self, _notification: &NSNotification) {
                self.send_query();
            }

            #[unsafe(method(control:textView:doCommandBySelector:))]
            unsafe fn do_command(&self, _control: &NSControl, _editor: &NSTextView, selector: Sel) -> bool {
                if selector == sel!(cancelOperation:) {
                    self.ivars().panel.dismiss();
                    true
                } else if selector == sel!(insertNewline:) || selector == sel!(insertNewlineIgnoringFieldEditor:) {
                    let shift = NSApplication::sharedApplication(self.mtm()).currentEvent()
                        .is_some_and(|event| event.modifierFlags().contains(NSEventModifierFlags::Shift));
                    self.ivars().dispatch.send(if shift { SearchAction::Previous } else { SearchAction::Next });
                    true
                } else {
                    false
                }
            }
        }

        unsafe impl NSWindowDelegate for SearchTarget {
            #[unsafe(method(windowShouldClose:))]
            fn window_should_close(&self, _window: &NSWindow) -> bool {
                // Hide for reuse; no NSWindow close autorelease or nested loop.
                self.ivars().panel.dismiss();
                false
            }
        }

        impl SearchTarget {
            #[unsafe(method(queryChanged:))]
            fn query_changed(&self, _sender: &NSControl) {
                // NSSearchField's built-in clear button also sends this action.
                self.send_query();
            }

            #[unsafe(method(nextMatch:))]
            fn next_match(&self, _sender: &NSButton) {
                self.ivars().dispatch.send(SearchAction::Next);
            }

            #[unsafe(method(previousMatch:))]
            fn previous_match(&self, _sender: &NSButton) {
                self.ivars().dispatch.send(SearchAction::Previous);
            }

            #[unsafe(method(caseSensitivityChanged:))]
            fn case_sensitivity_changed(&self, sender: &NSButton) {
                // Leave the native field editor and its selection intact.
                self.ivars().dispatch.send(SearchAction::CaseSensitive(sender.state() != 0));
            }
        }
    );

    impl SearchTarget {
        fn send_query(&self) {
            let query = self.ivars().field.stringValue().to_string();
            let changed = {
                let mut last = self.ivars().last_query.borrow_mut();
                if *last == query {
                    false
                } else {
                    last.clone_from(&query);
                    true
                }
            };
            // AppKit can report an edit through both the delegate and action.
            if changed {
                self.ivars().dispatch.send(SearchAction::Query(query));
            }
        }

        fn new(mtm: MainThreadMarker, state: TargetState) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(state);
            // SAFETY: Initialize the already allocated NSObject superclass.
            unsafe { msg_send![super(this), init] }
        }
    }

    pub struct SearchPanel {
        panel: Retained<NativePanel>,
        parent: Retained<NSWindow>,
        field: Retained<NSSearchField>,
        status: Retained<NSTextField>,
        buttons: Vec<Retained<NSButton>>,
        match_case: Retained<NSButton>,
        target: Retained<SearchTarget>,
    }

    impl SearchPanel {
        /// `callback` runs on the AppKit main thread and must not panic. It
        /// should enqueue an event rather than mutate the application directly.
        pub fn new(
            window: &Window,
            callback: impl Fn(SearchAction) + 'static,
        ) -> Result<Self, String> {
            let mtm = MainThreadMarker::new().ok_or("native search requires the main thread")?;
            let handle = window.window_handle().map_err(|error| error.to_string())?;
            let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                return Err("window does not expose an AppKit view".into());
            };
            // SAFETY: winit owns this live view; retain its borrowed reference.
            let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
                .ok_or("window has no native view")?;
            let parent = view.window().ok_or("native view has no window")?;
            let dispatch =
                Rc::new(Dispatch { enabled: Cell::new(true), callback: Box::new(callback) });
            let frame = parent.frame();
            let origin = NSPoint::new(
                frame.origin.x + (frame.size.width - 440.0) / 2.0,
                frame.origin.y + frame.size.height - 180.0,
            );
            let panel = NativePanel::alloc(mtm).set_ivars(dispatch.clone());
            // SAFETY: This is NSWindow's designated initializer, applied to our
            // NSPanel subclass. Closing must not release our retained ownership.
            let panel: Retained<NativePanel> = unsafe {
                msg_send![super(panel), initWithContentRect: NSRect::new(origin, NSSize::new(440.0, 140.0)),
                    styleMask: NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::UtilityWindow,
                    backing: NSBackingStoreType::Buffered, defer: false]
            };
            unsafe {
                panel.setReleasedWhenClosed(false);
            }
            panel.setTitle(&NSString::from_str("Find in Scrollback"));
            panel.setFloatingPanel(false);
            panel.setBecomesKeyOnlyIfNeeded(false);
            panel.setHidesOnDeactivate(true);
            let content = panel.contentView().ok_or("search panel has no content view")?;
            let field = NSSearchField::initWithFrame(
                NSSearchField::alloc(mtm),
                rect(12.0, 101.0, 416.0, 26.0),
            );
            field.setPlaceholderString(Some(&NSString::from_str("Search scrollback")));
            field.setSendsSearchStringImmediately(true);
            let status = NSTextField::labelWithString(&NSString::new(), mtm);
            status.setFrame(rect(14.0, 62.0, 412.0, 22.0));
            let target = SearchTarget::new(
                mtm,
                TargetState {
                    dispatch,
                    field: field.clone(),
                    panel: panel.clone(),
                    last_query: RefCell::new(String::new()),
                },
            );
            // SAFETY: The protocol implementations and selectors above match
            // AppKit's signatures. Target is retained until delegates detach.
            unsafe {
                field.setDelegate(Some(ProtocolObject::from_ref(&*target)));
                field.setTarget(Some(&target));
                field.setAction(Some(sel!(queryChanged:)));
            }
            panel.setDelegate(Some(ProtocolObject::from_ref(&*target)));
            content.addSubview(&field);
            content.addSubview(&status);
            // SAFETY: The target is retained by SearchPanel and the action's
            // NSButton argument matches case_sensitivity_changed above.
            let match_case = unsafe {
                NSButton::checkboxWithTitle_target_action(
                    &NSString::from_str("Match case"),
                    Some(&target),
                    Some(sel!(caseSensitivityChanged:)),
                    mtm,
                )
            };
            match_case.setState(0);
            match_case.setFrame(rect(14.0, 15.0, 160.0, 24.0));
            content.addSubview(&match_case);
            let mut buttons = Vec::new();
            for (title, x, selector) in
                [("Previous", 236.0, sel!(previousMatch:)), ("Next", 335.0, sel!(nextMatch:))]
            {
                let button =
                    NSButton::initWithFrame(NSButton::alloc(mtm), rect(x, 12.0, 93.0, 30.0));
                button.setTitle(&NSString::from_str(title));
                unsafe {
                    button.setTarget(Some(&target));
                    button.setAction(Some(selector));
                }
                content.addSubview(&button);
                buttons.push(button);
            }
            // SAFETY: Parent and child are live, distinct main-thread windows.
            unsafe {
                parent.addChildWindow_ordered(&panel, NSWindowOrderingMode::Above);
            }
            panel.orderOut(None);
            Ok(Self { panel, parent, field, status, buttons, match_case, target })
        }

        pub fn show(&self) {
            self.panel.makeKeyAndOrderFront(None);
            self.panel.makeFirstResponder(Some(&self.field));
            // SAFETY: A nil sender is accepted by AppKit's standard action.
            unsafe {
                self.field.selectText(None);
            }
        }

        pub fn set_status(&self, text: &str) {
            self.status.setStringValue(&NSString::from_str(text));
        }

        /// Hide without dispatching another Close action.
        pub fn close(&self) {
            self.panel.orderOut(None);
            if self.parent.isVisible() {
                self.parent.makeKeyAndOrderFront(None);
            }
        }
    }

    impl Drop for SearchPanel {
        fn drop(&mut self) {
            // Disable callbacks before closing, then detach AppKit's unretained
            // pointers while target is alive. A queued event owns its payload.
            self.target.ivars().dispatch.enabled.set(false);
            self.panel.setDelegate(None);
            unsafe {
                self.field.setDelegate(None);
                self.field.setTarget(None);
                self.field.setAction(None);
                self.match_case.setTarget(None);
                self.match_case.setAction(None);
                for button in &self.buttons {
                    button.setTarget(None);
                    button.setAction(None);
                }
            }
            self.parent.removeChildWindow(&self.panel);
            self.panel.close();
        }
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }

    /// Explicit main-thread native check. The panel and parent stay hidden.
    #[cfg(test)]
    #[allow(dead_code, reason = "called by the opt-in native smoke example")]
    pub(crate) fn native_smoke(window: &Window) -> Result<(), String> {
        use objc2_app_kit::NSEventType;
        let mtm = MainThreadMarker::new().ok_or("smoke requires the main thread")?;
        let actions = Rc::new(RefCell::new(Vec::new()));
        let captured = actions.clone();
        let panel = SearchPanel::new(window, move |action| captured.borrow_mut().push(action))?;
        panel.field.setStringValue(&NSString::from_str("日本語 needle"));
        // SAFETY: This text-edit notification's object is the associated field;
        // all selectors below match the delegate/action signatures above.
        unsafe {
            let notification = NSNotification::notificationWithName_object(
                &NSString::from_str("NSControlTextDidChangeNotification"),
                Some(&panel.field),
            );
            let _: () = msg_send![&*panel.target, controlTextDidChange: &*notification];
            let _: () = msg_send![&*panel.target, queryChanged: &*panel.field];
            panel.buttons[1].performClick(None);
            panel.buttons[0].performClick(None);
        }
        if *actions.borrow()
            != [
                SearchAction::Query("日本語 needle".into()),
                SearchAction::Next,
                SearchAction::Previous,
            ]
        {
            return Err("native editing, query deduplication, or navigation failed".into());
        }
        if panel.match_case.state() != 0 {
            return Err("Match case did not default to off".into());
        }
        if !panel.panel.makeFirstResponder(Some(&panel.field)) {
            return Err("hidden panel could not focus its native search field".into());
        }
        let native_editor = panel.field.currentEditor().ok_or("search field has no editor")?;
        let responder = panel.panel.firstResponder().ok_or("search field has no responder")?;
        let mut selection = native_editor.selectedRange();
        selection.location = 1;
        selection.length = 2;
        native_editor.setSelectedRange(selection);
        let before_toggle = actions.borrow().len();
        unsafe {
            panel.match_case.performClick(None);
        }
        unsafe {
            panel.match_case.performClick(None);
        }
        if actions.borrow()[before_toggle..]
            != [SearchAction::CaseSensitive(true), SearchAction::CaseSensitive(false)]
        {
            return Err("Match case toggle did not send its native on/off state".into());
        }
        let focused = panel.panel.firstResponder().ok_or("toggle lost first responder")?;
        if !std::ptr::eq(&*responder, &*focused)
            || native_editor.selectedRange() != selection
            || panel.field.stringValue().to_string() != "日本語 needle"
        {
            return Err(
                "Match case toggle disturbed native editing focus, selection, or query".into()
            );
        }
        let editor = NSTextView::new(mtm);
        let returned: bool = unsafe {
            msg_send![&*panel.target, control: &*panel.field, textView: &*editor,
                doCommandBySelector: sel!(insertNewline:)]
        };
        if !returned || actions.borrow().last() != Some(&SearchAction::Next) {
            return Err("Return selector was not consumed as Next".into());
        }
        for (modifiers, expected) in [
            (NSEventModifierFlags::Command, SearchAction::Next),
            (NSEventModifierFlags::Command | NSEventModifierFlags::Shift, SearchAction::Previous),
        ] {
            let event = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown, NSPoint::new(0.0, 0.0), modifiers, 0.0,
                panel.panel.windowNumber(), None, &NSString::from_str("g"), &NSString::from_str("g"), false, 5,
            ).ok_or("could not construct native key-equivalent event")?;
            if !panel.panel.performKeyEquivalent(&event)
                || actions.borrow().last() != Some(&expected)
            {
                return Err("Command-G navigation was not consumed".into());
            }
        }
        let escaped: bool = unsafe {
            msg_send![&*panel.target, control: &*panel.field, textView: &*editor,
                doCommandBySelector: sel!(cancelOperation:)]
        };
        if !escaped
            || actions.borrow().last() != Some(&SearchAction::Close)
            || panel.panel.isVisible()
        {
            return Err("Escape selector did not hide and close search".into());
        }
        let before_close = actions.borrow().len();
        panel.close();
        if actions.borrow().len() != before_close || panel.parent.isVisible() {
            return Err("programmatic close emitted an action or showed the hidden parent".into());
        }
        let allow_close: bool =
            unsafe { msg_send![&*panel.target, windowShouldClose: &*panel.panel] };
        if allow_close || actions.borrow().last() != Some(&SearchAction::Close) {
            return Err("window close delegate did not hide for reuse".into());
        }
        panel.set_status("No matches in retained scrollback");
        if panel.status.stringValue().to_string() != "No matches in retained scrollback"
            || panel.status.frame().size.width < 400.0
        {
            return Err("status label did not retain its full-width text".into());
        }
        let target = panel.target.clone();
        let field = panel.field.clone();
        let native_panel = panel.panel.clone();
        let buttons = panel.buttons.clone();
        let match_case = panel.match_case.clone();
        let before_drop = actions.borrow().len();
        drop(panel);
        if field.delegate().is_some()
            || field.target().is_some()
            || native_panel.delegate().is_some()
            || buttons.iter().any(|button| button.target().is_some())
            || match_case.target().is_some()
        {
            return Err("Drop left an AppKit delegate or target attached".into());
        }
        field.setStringValue(&NSString::from_str("late edit"));
        unsafe {
            let _: () = msg_send![&*target, queryChanged: &*field];
            let _: () = msg_send![&*native_panel, cancelOperation: None::<&AnyObject>];
            buttons[1].performClick(None);
            match_case.performClick(None);
            let _: () = msg_send![&*target, caseSensitivityChanged: &*match_case];
        }
        if actions.borrow().len() != before_drop {
            return Err("a native callback escaped after Drop".into());
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn disabled_native_callbacks_stop_and_release_their_capture() {
            let actions = Rc::new(RefCell::new(Vec::new()));
            let captured = actions.clone();
            let weak = Rc::downgrade(&captured);
            let dispatch = Rc::new(Dispatch {
                enabled: Cell::new(true),
                callback: Box::new(move |action| captured.borrow_mut().push(action)),
            });
            let native_owner = dispatch.clone();
            dispatch.send(SearchAction::Query("日本語".into()));
            dispatch.send(SearchAction::Next);
            assert_eq!(
                *actions.borrow(),
                [SearchAction::Query("日本語".into()), SearchAction::Next]
            );
            dispatch.enabled.set(false);
            native_owner.send(SearchAction::Close);
            assert_eq!(actions.borrow().len(), 2);
            drop(actions);
            drop(dispatch);
            assert!(weak.upgrade().is_some());
            drop(native_owner);
            assert!(weak.upgrade().is_none());
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::SearchPanel;

#[cfg(all(test, target_os = "macos"))]
#[allow(unused_imports, reason = "used by the opt-in native smoke example")]
pub(crate) use macos::native_smoke;

#[cfg(not(target_os = "macos"))]
pub struct SearchPanel;

#[cfg(not(target_os = "macos"))]
impl SearchPanel {
    pub fn new(
        _: &winit::window::Window,
        _: impl Fn(SearchAction) + 'static,
    ) -> Result<Self, String> {
        Err("native search is only supported on macOS".into())
    }
    pub fn show(&self) {}
    pub fn set_status(&self, _: &str) {}
    pub fn close(&self) {}
}
