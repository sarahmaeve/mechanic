//! Opt-in AppKit test, with a hidden parent and no screenshots or clipboard.
//! Build: cargo rustc -p mechanic-app --example palette_native_smoke -- --cfg test
//! Run: target/debug/examples/palette_native_smoke

#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!(
        "Build on macOS with: cargo rustc -p mechanic-app --example palette_native_smoke -- --cfg test"
    );
}

#[cfg(all(test, target_os = "macos"))]
#[allow(dead_code, reason = "example exercises only native palette behavior")]
#[path = "../src/palette.rs"]
mod palette;

#[cfg(all(test, target_os = "macos"))]
#[allow(dead_code, unused_imports, reason = "example exercises native palette internals")]
#[path = "../src/palette_platform.rs"]
mod palette_platform;

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
        eprintln!("native palette smoke timed out; result is inconclusive");
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
                        .with_title("Mechanic palette smoke"),
                )
                .unwrap();
            self.result = Some(palette_platform::native_smoke(&window));
            event_loop.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let mut smoke = Smoke { result: None };
    EventLoop::new().unwrap().run_app(&mut smoke).unwrap();
    match smoke.result {
        Some(Ok(())) => println!(
            "native palette smoke passed: filtering, Up wrap/scroll, Return, click, Unicode prompt, validation focus, input limit, Escape, and teardown"
        ),
        Some(Err(error)) => {
            eprintln!("native palette smoke failed: {error}");
            std::process::exit(1);
        }
        None => {
            eprintln!("native palette smoke did not execute");
            std::process::exit(2);
        }
    }
}
