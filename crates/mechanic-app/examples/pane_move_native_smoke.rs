//! Hidden native live-pane move smoke. Build with
//! `cargo rustc --locked -p mechanic-app --example pane_move_native_smoke -- --cfg test`.
#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!(
        "Build on macOS with: cargo rustc --locked -p mechanic-app --example pane_move_native_smoke -- --cfg test"
    );
}

#[cfg(all(test, target_os = "macos"))]
include!("panes_smoke_modules.inc");

#[cfg(all(test, target_os = "macos"))]
fn main() {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        window::WindowId,
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(30));
        eprintln!("native live pane move smoke timed out");
        std::process::exit(2);
    });
    type ResultSlot = Arc<Mutex<Option<Result<(), String>>>>;
    struct Smoke {
        app: app::App,
        // Drop the app/control listener before its socket directory disappears.
        _directory: Option<tempfile::TempDir>,
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
            match self.app.smoke_prepare_live_detach(events) {
                Ok((fixture, directory)) => {
                    self._directory = Some(directory);
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
    config.session.restore = false;
    config.control.enabled = true;
    config.notifications.enabled = false;
    config.font.family = "Menlo".into();
    config.theme.logo_size = 0;
    let mut app = app::App::new(
        config,
        events.create_proxy(),
        mechanic_config::theme::AnimationConfig { logo: false, background: false },
        true,
    );
    app.smoke_hide_windows();
    let result = Arc::new(Mutex::new(None));
    let mut smoke = Smoke { app, _directory: None, started: false, result: result.clone() };
    events.run_app(&mut smoke).unwrap();
    drop(smoke);
    match result.lock().unwrap().take() {
        Some(Ok(())) => println!(
            "native live pane move smoke passed: original shell variable/history/selection/Find preserved, no replacement PTY, original queued and future reader wakes after source window closes, stable control handle and pre-move completion waiter"
        ),
        result => {
            eprintln!("native live pane move smoke failed: {result:?}");
            std::process::exit(1);
        }
    }
}
