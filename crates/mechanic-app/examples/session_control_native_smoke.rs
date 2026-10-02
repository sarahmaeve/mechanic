//! Hidden native session/control smoke. Build on macOS with
//! `cargo rustc -p mechanic-app --example session_control_native_smoke --locked -- --cfg test`.

#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!(
        "Build on macOS with: cargo rustc -p mechanic-app --example session_control_native_smoke --locked -- --cfg test"
    );
}

#[cfg(all(test, target_os = "macos"))]
include!("panes_smoke_modules.inc");

#[cfg(all(test, target_os = "macos"))]
fn main() {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        window::WindowId,
    };

    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(45));
        eprintln!("native session/control smoke timed out");
        std::process::exit(2);
    });
    // A short path keeps the instance socket below macOS's sockaddr_un limit.
    let directory = PathBuf::from(format!("/tmp/mechanic-sc-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    type ResultSlot = Arc<Mutex<Option<Result<(), String>>>>;
    struct Smoke {
        app: app::App,
        directory: PathBuf,
        started: bool,
        result: ResultSlot,
    }
    impl ApplicationHandler<app::UserEvent> for Smoke {
        fn resumed(&mut self, events: &ActiveEventLoop) {
            if self.started {
                return;
            }
            self.started = true;
            self.app.resumed(events);
            match self.app.smoke_restore_session_control(events, &self.directory) {
                Ok(fixture) => {
                    let result = self.result.clone();
                    std::thread::spawn(move || {
                        *result.lock().unwrap() = Some(fixture.exercise());
                    });
                }
                Err(error) => {
                    *self.result.lock().unwrap() = Some(Err(error));
                    events.exit();
                }
            }
        }
        fn user_event(&mut self, events: &ActiveEventLoop, event: app::UserEvent) {
            self.app.user_event(events, event);
        }
        fn window_event(&mut self, events: &ActiveEventLoop, window: WindowId, event: WindowEvent) {
            self.app.window_event(events, window, event);
        }
        fn about_to_wait(&mut self, events: &ActiveEventLoop) {
            self.app.about_to_wait(events);
            if self.result.lock().unwrap().is_some() {
                events.exit();
            } else {
                events.set_control_flow(ControlFlow::WaitUntil(
                    Instant::now() + Duration::from_millis(5),
                ));
            }
        }
        fn exiting(&mut self, events: &ActiveEventLoop) {
            self.app.exiting(events);
        }
    }
    let events = EventLoop::<app::UserEvent>::with_user_event().build().unwrap();
    let mut config = mechanic_config::Config::default();
    config.shell.program = "/bin/sh".into();
    config.shell.integration = false;
    config.font.family = "Menlo".into();
    config.theme.logo_size = 0;
    config.notifications.enabled = false;
    let mut app = app::App::new(
        config,
        events.create_proxy(),
        mechanic_config::theme::AnimationConfig { logo: false, background: false },
        true,
    );
    app.smoke_hide_windows();
    let result = Arc::new(Mutex::new(None));
    let mut smoke = Smoke { app, directory, started: false, result: result.clone() };
    events.run_app(&mut smoke).unwrap();
    drop(smoke);
    match result.lock().unwrap().take() {
        Some(Ok(())) => println!(
            "native session/control smoke passed: workspace save/restore, fresh PTYs, split ratios/active pane/font/cwd, discovery, list/read, paste and explicit Enter, raw input, completion waits/timeouts, stale handles, background focus/cache invalidation, pane create/split/focus/zoom/dock/detach/close, appearance set/clear, live session/output preservation, loadout save/list/open/delete with directories"
        ),
        result => {
            eprintln!("native session/control smoke failed: {result:?}");
            std::process::exit(1);
        }
    }
}
