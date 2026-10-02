//! Explicit native smoke check; ordinary builds and tests do not create windows.
//! Build: cargo rustc -p mechanic-app --example search_native_smoke -- --cfg test
//! Run: target/debug/examples/search_native_smoke
//! Checks a hidden modeless search panel; does not capture screens or clipboard.

#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!(
        "Build on macOS with: cargo rustc -p mechanic-app --example search_native_smoke -- --cfg test"
    );
}

#[cfg(all(test, target_os = "macos"))]
#[allow(dead_code, unused_imports, reason = "this example exercises native search internals")]
#[path = "../src/search_platform.rs"]
mod search_platform;

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
        eprintln!("native search smoke timed out; result is inconclusive");
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
                        .with_title("Mechanic search smoke"),
                )
                .unwrap();
            self.result = Some(search_platform::native_smoke(&window));
            event_loop.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let mut smoke = Smoke { result: None };
    EventLoop::new().unwrap().run_app(&mut smoke).unwrap();
    match smoke.result {
        Some(Ok(())) => println!(
            "native search smoke passed: query/editing, deduplication, Match case toggle/focus, navigation, Return/Escape, Command-G, status, and teardown"
        ),
        Some(Err(error)) => {
            eprintln!("native search smoke failed: {error}");
            std::process::exit(1);
        }
        None => {
            eprintln!("native search smoke did not execute");
            std::process::exit(2);
        }
    }
}
