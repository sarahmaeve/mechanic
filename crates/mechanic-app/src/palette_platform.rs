//! A modeless native command palette. AppKit owns text editing and key repeat.

#[cfg(not(target_os = "macos"))]
use crate::palette::PaletteEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteAction {
    Execute(String),
    Submit { id: String, value: String },
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
        NSBackingStoreType, NSBezelStyle, NSButton, NSButtonType, NSControl,
        NSControlTextEditingDelegate, NSPanel, NSSearchField, NSSearchFieldDelegate,
        NSTextAlignment, NSTextField, NSTextFieldDelegate, NSTextView, NSView, NSWindow,
        NSWindowDelegate, NSWindowOrderingMode, NSWindowStyleMask,
    };
    use objc2_foundation::{
        MainThreadMarker, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
        NSString,
    };
    use winit::{
        raw_window_handle::{HasWindowHandle, RawWindowHandle},
        window::Window,
    };

    use crate::palette::{MAX_QUERY_CHARS, PaletteEntry, PaletteModel};

    use super::PaletteAction;

    const WIDTH: f64 = 540.0;
    const HEIGHT: f64 = 382.0;
    const PROMPT_HEIGHT: f64 = 112.0;
    const ROWS: usize = 8;
    const MAX_PROMPT_CHARS: usize = 4096;

    struct Dispatch {
        enabled: Cell<bool>,
        callback: Box<dyn Fn(PaletteAction)>,
    }

    impl Dispatch {
        fn send(&self, action: PaletteAction) {
            if self.enabled.get() {
                (self.callback)(action);
            }
        }
    }

    define_class!(
        // SAFETY: NSPanel adds no subclass requirements. Native editing remains
        // in AppKit; this subclass handles cancellation only.
        #[unsafe(super = NSPanel)]
        #[name = "MechanicPalettePanel"]
        #[thread_kind = MainThreadOnly]
        #[ivars = Rc<Dispatch>]
        struct NativePanel;

        unsafe impl NSObjectProtocol for NativePanel {}

        impl NativePanel {
            #[unsafe(method(cancelOperation:))]
            fn cancel_operation(&self, _sender: Option<&AnyObject>) {
                self.dismiss();
            }
        }
    );

    impl NativePanel {
        fn dismiss(&self) {
            self.orderOut(None);
            self.ivars().send(PaletteAction::Close);
        }
    }

    enum Mode {
        Commands,
        Prompt { id: String },
    }

    struct TargetState {
        dispatch: Rc<Dispatch>,
        field: Retained<NSSearchField>,
        status: Retained<NSTextField>,
        panel: Retained<NativePanel>,
        buttons: Vec<Retained<NSButton>>,
        model: RefCell<PaletteModel>,
        mode: RefCell<Mode>,
        first_visible: Cell<usize>,
    }

    define_class!(
        // SAFETY: NSObject adds no subclass requirements. PalettePanel owns
        // this object until AppKit's unretained targets/delegates are detached.
        #[unsafe(super = NSObject)]
        #[name = "MechanicPaletteTarget"]
        #[thread_kind = MainThreadOnly]
        #[ivars = TargetState]
        struct PaletteTarget;

        unsafe impl NSObjectProtocol for PaletteTarget {}
        unsafe impl NSTextFieldDelegate for PaletteTarget {}
        unsafe impl NSSearchFieldDelegate for PaletteTarget {}

        unsafe impl NSControlTextEditingDelegate for PaletteTarget {
            #[unsafe(method(controlTextDidChange:))]
            fn control_text_did_change(&self, _notification: &NSNotification) {
                self.update_query();
            }

            #[unsafe(method(control:textView:doCommandBySelector:))]
            unsafe fn do_command(&self, _control: &NSControl, _editor: &NSTextView, selector: Sel) -> bool {
                if selector == sel!(cancelOperation:) {
                    self.ivars().panel.dismiss();
                    true
                } else if selector == sel!(insertNewline:) || selector == sel!(insertNewlineIgnoringFieldEditor:) {
                    self.activate();
                    true
                } else if matches!(*self.ivars().mode.borrow(), Mode::Commands)
                    && (selector == sel!(moveUp:) || selector == sel!(moveDown:))
                {
                    self.ivars().model.borrow_mut().move_selection(selector == sel!(moveUp:));
                    self.refresh_rows();
                    true
                } else {
                    false
                }
            }
        }

        unsafe impl NSWindowDelegate for PaletteTarget {
            #[unsafe(method(windowShouldClose:))]
            fn window_should_close(&self, _window: &NSWindow) -> bool {
                self.ivars().panel.dismiss();
                false
            }
        }

        impl PaletteTarget {
            #[unsafe(method(queryChanged:))]
            fn query_changed(&self, _sender: &NSControl) {
                self.update_query();
            }

            #[unsafe(method(commandClicked:))]
            fn command_clicked(&self, sender: &NSButton) {
                if matches!(*self.ivars().mode.borrow(), Mode::Commands) {
                    let Ok(row) = usize::try_from(sender.tag()) else { return; };
                    if row < ROWS {
                        self.ivars().model.borrow_mut().select(self.ivars().first_visible.get() + row);
                        self.activate();
                    }
                }
            }
        }
    );

    impl PaletteTarget {
        fn new(mtm: MainThreadMarker, state: TargetState) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(state);
            // SAFETY: Initialize the allocated NSObject superclass.
            unsafe { msg_send![super(this), init] }
        }

        fn update_query(&self) {
            if !matches!(*self.ivars().mode.borrow(), Mode::Commands) {
                return;
            }
            let value = self.ivars().field.stringValue();
            // NSString's length is constant-time and measured in UTF-16 units.
            // Avoid copying arbitrarily large native paste contents into Rust.
            if value.length() > MAX_QUERY_CHARS * 2 {
                self.ivars().model.borrow_mut().set_query(&" ".repeat(MAX_QUERY_CHARS + 1));
            } else {
                self.ivars().model.borrow_mut().set_query(&value.to_string());
            }
            self.ivars().first_visible.set(0);
            self.refresh_rows();
        }

        fn refresh_rows(&self) {
            if !matches!(*self.ivars().mode.borrow(), Mode::Commands) {
                return;
            }
            let model = self.ivars().model.borrow();
            let mut first = self.ivars().first_visible.get();
            if let Some(selected) = model.selected_index() {
                if selected < first {
                    first = selected;
                } else if selected >= first + ROWS {
                    first = selected + 1 - ROWS;
                }
            }
            first = first.min(model.count().saturating_sub(ROWS));
            self.ivars().first_visible.set(first);
            for (row, button) in self.ivars().buttons.iter().enumerate() {
                let index = first + row;
                if let Some(entry) = model.entry(index) {
                    let title = match &entry.shortcut {
                        Some(shortcut) => format!("{}     {shortcut}", entry.label),
                        None => entry.label.clone(),
                    };
                    button.setTitle(&NSString::from_str(&title));
                    button.setState(i64::from(model.selected_index() == Some(index)) as _);
                    button.setHidden(false);
                    button.setEnabled(true);
                } else {
                    button.setHidden(true);
                    button.setEnabled(false);
                }
            }
            let status = if model.query_too_long() {
                format!("Search is too long (maximum {MAX_QUERY_CHARS} characters)")
            } else if model.count() == 0 {
                "No matching commands".into()
            } else {
                format!("{} commands · ↑/↓ to choose · Return to run · Esc to close", model.count())
            };
            self.ivars().status.setStringValue(&NSString::from_str(&status));
        }

        fn activate(&self) {
            let action = match &*self.ivars().mode.borrow() {
                Mode::Commands => self
                    .ivars()
                    .model
                    .borrow()
                    .selected()
                    .map(|entry| PaletteAction::Execute(entry.id.clone())),
                Mode::Prompt { id } => {
                    let value = self.ivars().field.stringValue();
                    if value.length() > MAX_PROMPT_CHARS * 2 {
                        self.prompt_too_long();
                        return;
                    }
                    let value = value.to_string();
                    if value.chars().take(MAX_PROMPT_CHARS + 1).count() > MAX_PROMPT_CHARS {
                        self.prompt_too_long();
                        return;
                    }
                    Some(PaletteAction::Submit { id: id.clone(), value })
                }
            };
            // Drop all model/mode borrows before invoking external code.
            if let Some(action) = action {
                self.ivars().dispatch.send(action);
            }
        }

        fn prompt_too_long(&self) {
            self.ivars().status.setStringValue(&NSString::from_str(&format!(
                "Value is too long (maximum {MAX_PROMPT_CHARS} characters)"
            )));
        }
    }

    pub struct PalettePanel {
        panel: Retained<NativePanel>,
        parent: Retained<NSWindow>,
        target: Retained<PaletteTarget>,
    }

    impl PalettePanel {
        /// The callback runs on AppKit's main thread and must not panic. Queue
        /// an application event instead of modifying the application directly.
        pub fn new(
            window: &Window,
            callback: impl Fn(PaletteAction) + 'static,
        ) -> Result<Self, String> {
            let mtm = MainThreadMarker::new().ok_or("native palette requires the main thread")?;
            let handle = window.window_handle().map_err(|error| error.to_string())?;
            let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                return Err("window does not expose an AppKit view".into());
            };
            // SAFETY: winit owns this live view; retain the borrowed reference.
            let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
                .ok_or("window has no native view")?;
            let parent = view.window().ok_or("native view has no window")?;
            let dispatch =
                Rc::new(Dispatch { enabled: Cell::new(true), callback: Box::new(callback) });
            let panel = NativePanel::alloc(mtm).set_ivars(dispatch.clone());
            // SAFETY: NSWindow's designated initializer initializes our NSPanel
            // subclass. Retained Rust ownership controls the panel's lifetime.
            let panel: Retained<NativePanel> = unsafe {
                msg_send![super(panel), initWithContentRect: rect(0.0, 0.0, WIDTH, HEIGHT),
                    styleMask: NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::UtilityWindow,
                    backing: NSBackingStoreType::Buffered, defer: false]
            };
            unsafe {
                panel.setReleasedWhenClosed(false);
            }
            panel.setTitle(&NSString::from_str("Command Palette"));
            panel.setFloatingPanel(false);
            panel.setBecomesKeyOnlyIfNeeded(false);
            panel.setHidesOnDeactivate(true);
            let content = panel.contentView().ok_or("palette has no content view")?;
            let field = NSSearchField::initWithFrame(
                NSSearchField::alloc(mtm),
                rect(14.0, HEIGHT - 42.0, WIDTH - 28.0, 28.0),
            );
            field.setPlaceholderString(Some(&NSString::from_str("Search commands")));
            field.setSendsSearchStringImmediately(true);
            let status = NSTextField::labelWithString(&NSString::new(), mtm);
            status.setFrame(rect(16.0, HEIGHT - 74.0, WIDTH - 32.0, 22.0));
            content.addSubview(&field);
            content.addSubview(&status);
            let mut buttons = Vec::with_capacity(ROWS);
            for row in 0..ROWS {
                let button = NSButton::initWithFrame(
                    NSButton::alloc(mtm),
                    rect(14.0, HEIGHT - 116.0 - row as f64 * 36.0, WIDTH - 28.0, 32.0),
                );
                button.setTag(row as _);
                button.setButtonType(NSButtonType::PushOnPushOff);
                button.setBezelStyle(NSBezelStyle::AccessoryBar);
                button.setAlignment(NSTextAlignment::Left);
                content.addSubview(&button);
                buttons.push(button);
            }
            let target = PaletteTarget::new(
                mtm,
                TargetState {
                    dispatch,
                    field: field.clone(),
                    status,
                    panel: panel.clone(),
                    buttons,
                    model: RefCell::new(PaletteModel::default()),
                    mode: RefCell::new(Mode::Commands),
                    first_visible: Cell::new(0),
                },
            );
            // SAFETY: Selectors and protocols match the implementations above;
            // PalettePanel retains the target until these pointers detach.
            unsafe {
                field.setDelegate(Some(ProtocolObject::from_ref(&*target)));
                field.setTarget(Some(&target));
                field.setAction(Some(sel!(queryChanged:)));
                for button in &target.ivars().buttons {
                    button.setTarget(Some(&target));
                    button.setAction(Some(sel!(commandClicked:)));
                }
            }
            panel.setDelegate(Some(ProtocolObject::from_ref(&*target)));
            unsafe {
                parent.addChildWindow_ordered(&panel, NSWindowOrderingMode::Above);
            }
            target.refresh_rows();
            panel.orderOut(None);
            Ok(Self { panel, parent, target })
        }

        pub fn set_entries(&self, entries: Vec<PaletteEntry>) {
            self.target.ivars().model.borrow_mut().set_entries(entries);
            self.target.refresh_rows();
        }

        /// Start a fresh command search, preserving the current command list.
        pub fn show(&self) {
            *self.target.ivars().mode.borrow_mut() = Mode::Commands;
            self.panel.setTitle(&NSString::from_str("Command Palette"));
            self.layout(HEIGHT);
            let field = &self.target.ivars().field;
            field.setPlaceholderString(Some(&NSString::from_str("Search commands")));
            field.setStringValue(&NSString::new());
            self.target.ivars().model.borrow_mut().set_query("");
            self.target.ivars().first_visible.set(0);
            self.target.refresh_rows();
            self.present();
        }

        /// Gather a value using native text editing. Validation errors can be
        /// shown with set_status without changing text or the field selection.
        pub fn set_prompt(&self, id: &str, title: &str, placeholder: &str, value: &str) {
            *self.target.ivars().mode.borrow_mut() = Mode::Prompt { id: id.into() };
            self.panel.setTitle(&NSString::from_str(title));
            self.layout(PROMPT_HEIGHT);
            let field = &self.target.ivars().field;
            field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
            field.setStringValue(&NSString::from_str(value));
            for button in &self.target.ivars().buttons {
                button.setHidden(true);
                button.setEnabled(false);
            }
            self.set_status("Return to save · Esc to cancel");
            self.present();
        }

        pub fn set_status(&self, text: &str) {
            self.target.ivars().status.setStringValue(&NSString::from_str(text));
        }

        /// Hide without dispatching a second Close action.
        pub fn close(&self) {
            self.panel.orderOut(None);
            if self.parent.isVisible() {
                self.parent.makeKeyAndOrderFront(None);
            }
        }

        fn layout(&self, height: f64) {
            self.panel.setContentSize(NSSize::new(WIDTH, height));
            self.target.ivars().field.setFrame(rect(14.0, height - 42.0, WIDTH - 28.0, 28.0));
            self.target.ivars().status.setFrame(rect(16.0, height - 74.0, WIDTH - 32.0, 22.0));
        }

        fn present(&self) {
            let parent = self.parent.frame();
            let panel = self.panel.frame();
            self.panel.setFrameOrigin(NSPoint::new(
                parent.origin.x + (parent.size.width - panel.size.width) / 2.0,
                parent.origin.y + parent.size.height - panel.size.height - 50.0,
            ));
            self.panel.makeKeyAndOrderFront(None);
            self.panel.makeFirstResponder(Some(&self.target.ivars().field));
            // SAFETY: AppKit's standard selection action accepts a nil sender.
            unsafe {
                self.target.ivars().field.selectText(None);
            }
        }
    }

    impl Drop for PalettePanel {
        fn drop(&mut self) {
            self.target.ivars().dispatch.enabled.set(false);
            self.panel.setDelegate(None);
            unsafe {
                self.target.ivars().field.setDelegate(None);
                self.target.ivars().field.setTarget(None);
                self.target.ivars().field.setAction(None);
                for button in &self.target.ivars().buttons {
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

    /// Explicit opt-in main-thread AppKit check; parent stays hidden.
    #[cfg(test)]
    #[allow(dead_code, reason = "called by the opt-in native smoke example")]
    pub(crate) fn native_smoke(window: &Window) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("smoke requires the main thread")?;
        let actions = Rc::new(RefCell::new(Vec::new()));
        let captured = actions.clone();
        let panel = PalettePanel::new(window, move |action| captured.borrow_mut().push(action))?;
        panel.set_entries(
            (0..12)
                .map(|i| PaletteEntry {
                    id: format!("command-{i}"),
                    label: format!("Pane command {i}"),
                    shortcut: None,
                })
                .collect(),
        );
        let field = &panel.target.ivars().field;
        field.setStringValue(&NSString::from_str("command"));
        let editor = NSTextView::new(mtm);
        // SAFETY: Native selectors match the live control and delegate above.
        unsafe {
            let notification = NSNotification::notificationWithName_object(
                &NSString::from_str("NSControlTextDidChangeNotification"),
                Some(field),
            );
            let _: () = msg_send![&*panel.target, controlTextDidChange: &*notification];
            let consumed: bool = msg_send![&*panel.target, control: &**field, textView: &*editor,
                doCommandBySelector: sel!(moveUp:)];
            if !consumed || panel.target.ivars().first_visible.get() != 4 {
                return Err("Up did not wrap selection and scroll visible commands".into());
            }
            let consumed: bool = msg_send![&*panel.target, control: &**field, textView: &*editor,
                doCommandBySelector: sel!(insertNewline:)];
            if !consumed
                || actions.borrow().last() != Some(&PaletteAction::Execute("command-11".into()))
            {
                return Err("Return did not execute selected command".into());
            }
        }
        field.setStringValue(&NSString::from_str("command 10"));
        panel.target.update_query();
        unsafe {
            panel.target.ivars().buttons[0].performClick(None);
        }
        if actions.borrow().last() != Some(&PaletteAction::Execute("command-10".into())) {
            return Err("native filtered result click did not execute its command ID".into());
        }
        field.setStringValue(&NSString::from_str("no matching item"));
        panel.target.update_query();
        let before_empty = actions.borrow().len();
        panel.target.activate();
        if actions.borrow().len() != before_empty {
            return Err("Return executed a command with no matching results".into());
        }
        field.setStringValue(&NSString::from_str(&"界".repeat(MAX_QUERY_CHARS + 1)));
        panel.target.update_query();
        if !panel.target.ivars().model.borrow().query_too_long()
            || !panel.target.ivars().status.stringValue().to_string().contains("too long")
        {
            return Err("oversized native query had no validation error".into());
        }
        // Set prompt state directly to exercise text/focus without showing a window.
        *panel.target.ivars().mode.borrow_mut() = Mode::Prompt { id: "save-loadout".into() };
        field.setStringValue(&NSString::from_str("日本語 workspace"));
        if !panel.panel.makeFirstResponder(Some(field)) {
            return Err("palette could not focus its native text field".into());
        }
        let native_editor = field.currentEditor().ok_or("palette field has no editor")?;
        let responder = panel.panel.firstResponder().ok_or("palette field has no responder")?;
        let mut selection = native_editor.selectedRange();
        selection.location = 1;
        selection.length = 2;
        native_editor.setSelectedRange(selection);
        panel.set_status("Name already exists");
        let focused = panel.panel.firstResponder().ok_or("status lost first responder")?;
        if !std::ptr::eq(&*responder, &*focused)
            || native_editor.selectedRange() != selection
            || field.stringValue().to_string() != "日本語 workspace"
        {
            return Err("validation status disturbed native editing or selection".into());
        }
        unsafe {
            let consumed: bool = msg_send![&*panel.target, control: &**field, textView: &*editor,
                doCommandBySelector: sel!(moveDown:)];
            if consumed {
                return Err("prompt consumed an arrow key needed for native editing".into());
            }
        }
        panel.target.activate();
        if actions.borrow().last()
            != Some(&PaletteAction::Submit {
                id: "save-loadout".into(),
                value: "日本語 workspace".into(),
            })
        {
            return Err("prompt did not submit native Unicode text".into());
        }
        field.setStringValue(&NSString::from_str(&"界".repeat(MAX_PROMPT_CHARS + 1)));
        let before_long = actions.borrow().len();
        panel.target.activate();
        if actions.borrow().len() != before_long
            || !panel.target.ivars().status.stringValue().to_string().contains("too long")
        {
            return Err("oversized prompt was submitted or had no validation error".into());
        }
        unsafe {
            let escaped: bool = msg_send![&*panel.target, control: &**field, textView: &*editor,
                doCommandBySelector: sel!(cancelOperation:)];
            if !escaped
                || actions.borrow().last() != Some(&PaletteAction::Close)
                || panel.panel.isVisible()
            {
                return Err("Escape did not consume and close palette".into());
            }
        }
        let before_close = actions.borrow().len();
        panel.close();
        if actions.borrow().len() != before_close {
            return Err("programmatic close dispatched a duplicate Close".into());
        }
        let field = field.clone();
        let target = panel.target.clone();
        let native_panel = panel.panel.clone();
        let buttons = panel.target.ivars().buttons.clone();
        drop(panel);
        if field.delegate().is_some()
            || field.target().is_some()
            || native_panel.delegate().is_some()
            || buttons.iter().any(|button| button.target().is_some())
        {
            return Err("Drop left an AppKit delegate or action target attached".into());
        }
        unsafe {
            let _: () = msg_send![&*native_panel, cancelOperation: None::<&AnyObject>];
            let _: () = msg_send![&*target, queryChanged: &*field];
        }
        field.setStringValue(&NSString::from_str("late prompt"));
        target.activate();
        if actions.borrow().len() != before_close {
            return Err("native callback escaped after Drop".into());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use macos::PalettePanel;

#[cfg(all(test, target_os = "macos"))]
#[allow(unused_imports, reason = "used by the opt-in native smoke example")]
pub(crate) use macos::native_smoke;

#[cfg(not(target_os = "macos"))]
pub struct PalettePanel;

#[cfg(not(target_os = "macos"))]
impl PalettePanel {
    pub fn new(
        _: &winit::window::Window,
        _: impl Fn(PaletteAction) + 'static,
    ) -> Result<Self, String> {
        Err("native command palette is only supported on macOS".into())
    }
    pub fn set_entries(&self, _: Vec<PaletteEntry>) {}
    pub fn show(&self) {}
    pub fn set_prompt(&self, _: &str, _: &str, _: &str, _: &str) {}
    pub fn set_status(&self, _: &str) {}
    pub fn close(&self) {}
}
