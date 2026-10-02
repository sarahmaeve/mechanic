//! Explicit native smoke check; ordinary builds and tests do not create windows.
//! Build: cargo rustc -p mechanic-app --example link_native_smoke -- --cfg test
//! Run: target/debug/examples/link_native_smoke
//! Checks AppKit tooltip copying/clearing and menu target/action delivery. Does
//! not show a menu, launch a browser, or change the clipboard.

#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!(
        "Build on macOS with: cargo rustc -p mechanic-app --example link_native_smoke -- --cfg test"
    );
}

#[cfg(all(test, target_os = "macos"))]
#[allow(
    dead_code,
    unused_imports,
    reason = "this example exercises only native tooltip/menu internals"
)]
#[path = "../src/link_platform.rs"]
mod link_platform;

#[cfg(all(test, target_os = "macos"))]
fn main() {
    use std::time::Duration;
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, EventLoop},
        window::{Window, WindowId},
    };

    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        eprintln!("native link smoke timed out; result is inconclusive");
        std::process::exit(2);
    });
    struct Smoke {
        result: Option<Result<(), String>>,
    }
    impl ApplicationHandler for Smoke {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let window = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_visible(false)
                        .with_title("Mechanic link smoke"),
                )
                .unwrap();
            self.result = Some(link_platform::native_smoke(&window));
            event_loop.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let mut smoke = Smoke { result: None };
    EventLoop::new().unwrap().run_app(&mut smoke).unwrap();
    match smoke.result {
        Some(Ok(())) => println!(
            "native link smoke passed: tooltip copies/clears; Open/Copy actions delivered; disabled Open blocked"
        ),
        Some(Err(error)) => {
            eprintln!("native link smoke failed: {error}");
            std::process::exit(1);
        }
        None => {
            eprintln!("native link smoke did not execute");
            std::process::exit(2);
        }
    }
}
