//! Build with `cargo rustc --locked -p mechanic-app --example workspace_native_smoke -- --cfg test`.
#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!("Build with --cfg test on macOS");
}
#[cfg(all(test, target_os = "macos"))]
include!("panes_smoke_modules.inc");
#[cfg(all(test, target_os = "macos"))]
fn main() {
    use winit::{
        application::ApplicationHandler,
        event_loop::{ActiveEventLoop, EventLoop},
    };
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(45));
        eprintln!("workspace smoke timed out");
        std::process::exit(2);
    });
    struct Smoke {
        app: app::App,
        directory: tempfile::TempDir,
        result: Option<Result<(), String>>,
    }
    impl ApplicationHandler<app::UserEvent> for Smoke {
        fn resumed(&mut self, events: &ActiveEventLoop) {
            if self.result.is_some() {
                return;
            }
            self.result = Some(self.app.smoke_workspace_features(events, self.directory.path()));
            events.exit();
        }
        fn window_event(
            &mut self,
            _: &ActiveEventLoop,
            _: winit::window::WindowId,
            _: winit::event::WindowEvent,
        ) {
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let events = EventLoop::<app::UserEvent>::with_user_event().build().unwrap();
    let mut config = mechanic_config::Config::default();
    config.shell.program = "/bin/sh".into();
    config.shell.integration = false;
    config.session.restore = false;
    config.control.enabled = false;
    config.font.family = "Menlo".into();
    config.theme.logo_size = 0;
    let mut app = app::App::new(config, events.create_proxy(), Default::default(), true);
    app.smoke_hide_windows();
    let mut smoke = Smoke { app, directory: tempfile::tempdir().unwrap(), result: None };
    events.run_app(&mut smoke).unwrap();
    match smoke.result.take() {
        Some(Ok(())) => println!(
            "workspace native smoke passed: loadouts preserve original sessions and directories, Unicode appearance, zoom layout/focus, ANSI foreground, real device destruction/recovery, live shell retained"
        ),
        result => {
            eprintln!("workspace native smoke failed: {result:?}");
            std::process::exit(1);
        }
    }
}
