//! Native one-request OSC 52 approval. No nested run loop or persistent grants.

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::Cell;
    use std::rc::Rc;

    use block2::RcBlock;
    use mechanic_core::clipboard::{ClipboardOperation, ClipboardType};
    use objc2::rc::Retained;
    use objc2_app_kit::{NSAlert, NSAlertSecondButtonReturn, NSModalResponse, NSView, NSWindow};
    use objc2_foundation::{MainThreadMarker, NSString};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;

    use crate::panes::PaneId;

    struct Decision {
        enabled: Cell<bool>,
        callback: Box<dyn Fn(bool)>,
    }

    impl Decision {
        fn complete(&self, allowed: bool) {
            if self.enabled.replace(false) {
                (self.callback)(allowed);
            }
        }
    }

    pub struct ClipboardPrompt {
        alert: Retained<NSAlert>,
        parent: Retained<NSWindow>,
        completion: RcBlock<dyn Fn(NSModalResponse)>,
        decision: Rc<Decision>,
        presented: bool,
    }

    impl ClipboardPrompt {
        pub fn new(
            window: &Window,
            pane: PaneId,
            session: u64,
            operation: ClipboardOperation,
            target: ClipboardType,
            callback: impl Fn(bool) + 'static,
        ) -> Result<Self, String> {
            let mut prompt = Self::prepare(window, pane, session, operation, target, callback)?;
            prompt.presented = true;
            // AppKit copies and retains the completion block until the sheet
            // finishes; application state is reached only through a user event.
            prompt.alert.beginSheetModalForWindow_completionHandler(
                &prompt.parent,
                Some(&prompt.completion),
            );
            Ok(prompt)
        }

        fn prepare(
            window: &Window,
            pane: PaneId,
            session: u64,
            operation: ClipboardOperation,
            target: ClipboardType,
            callback: impl Fn(bool) + 'static,
        ) -> Result<Self, String> {
            let mtm =
                MainThreadMarker::new().ok_or("clipboard approval requires the main thread")?;
            let handle = window.window_handle().map_err(|error| error.to_string())?;
            let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
                return Err("window does not expose an AppKit view".into());
            };
            // SAFETY: The borrowed winit window owns a live NSView. Retain it
            // while obtaining its parent on AppKit's main thread.
            let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
                .ok_or("window has no native view")?;
            let parent = view.window().ok_or("native view has no parent window")?;
            let alert = NSAlert::new(mtm);
            let action = match operation {
                ClipboardOperation::Read => "read",
                ClipboardOperation::Write => "replace",
            };
            let destination = match target {
                ClipboardType::Clipboard => "the system clipboard",
                ClipboardType::Selection => "this pane's primary selection",
            };
            alert.setMessageText(&NSString::from_str("Terminal clipboard request"));
            alert.setInformativeText(&NSString::from_str(&format!(
                "A program in terminal pane {pane} (session {session}) requested to {action} {destination} using OSC 52. Allow this request once?"
            )));
            // Deny is the default and Escape action; Allow Once is explicit.
            let deny = alert.addButtonWithTitle(&NSString::from_str("Deny"));
            deny.setKeyEquivalent(&NSString::from_str("\u{1b}"));
            let allow = alert.addButtonWithTitle(&NSString::from_str("Allow Once"));
            allow.setKeyEquivalent(&NSString::new());
            let decision =
                Rc::new(Decision { enabled: Cell::new(true), callback: Box::new(callback) });
            let dispatch = decision.clone();
            let completion = RcBlock::new(move |response| {
                dispatch.complete(response == NSAlertSecondButtonReturn);
            });
            Ok(Self { alert, parent, completion, decision, presented: false })
        }
    }

    impl Drop for ClipboardPrompt {
        fn drop(&mut self) {
            // Invalidate first: ending a sheet can synchronously invoke its
            // native completion block. No callback survives cancellation/drop.
            self.decision.enabled.set(false);
            if self.presented {
                let sheet = self.alert.window();
                if sheet.sheetParent().is_some() {
                    self.parent.endSheet(&sheet);
                    sheet.orderOut(None);
                }
            }
        }
    }

    /// Exercises native construction, the actual block ABI, one-shot approval,
    /// and cancellation while keeping the sheet unpresented for automation.
    #[allow(dead_code, reason = "called by explicit hidden native smoke examples")]
    pub(crate) fn native_smoke(window: &Window) -> Result<(), String> {
        let decisions = Rc::new(Cell::new(0));
        let counter = decisions.clone();
        let prompt = ClipboardPrompt::prepare(
            window,
            1,
            7,
            ClipboardOperation::Read,
            ClipboardType::Clipboard,
            move |allowed| counter.set(counter.get() + if allowed { 1 } else { 100 }),
        )?;
        prompt.completion.call((NSAlertSecondButtonReturn,));
        prompt.completion.call((NSAlertSecondButtonReturn,));
        if decisions.get() != 1 {
            return Err("clipboard approval completion was not one-shot".into());
        }
        let callback = prompt.completion.clone();
        drop(prompt);
        callback.call((NSAlertSecondButtonReturn,));
        if decisions.get() != 1 {
            return Err("clipboard approval completed after cancellation".into());
        }
        let counter = decisions.clone();
        let prompt = ClipboardPrompt::prepare(
            window,
            2,
            8,
            ClipboardOperation::Write,
            ClipboardType::Selection,
            move |_| counter.set(counter.get() + 1),
        )?;
        let callback = prompt.completion.clone();
        drop(prompt);
        callback.call((NSAlertSecondButtonReturn,));
        if decisions.get() != 1 {
            return Err("cancelled clipboard approval callback remained live".into());
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub use macos::ClipboardPrompt;

#[cfg(target_os = "macos")]
#[allow(unused_imports, reason = "used by explicit hidden native smoke examples")]
pub(crate) use macos::native_smoke;

#[cfg(not(target_os = "macos"))]
pub struct ClipboardPrompt;

#[cfg(not(target_os = "macos"))]
impl ClipboardPrompt {
    pub fn new(
        _window: &winit::window::Window,
        _pane: crate::panes::PaneId,
        _session: u64,
        _operation: mechanic_core::clipboard::ClipboardOperation,
        _target: mechanic_core::clipboard::ClipboardType,
        _callback: impl Fn(bool) + 'static,
    ) -> Result<Self, String> {
        Err("native clipboard approval unavailable on this platform".into())
    }
}
