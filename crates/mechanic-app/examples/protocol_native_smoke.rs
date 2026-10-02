//! Build with `cargo rustc --locked -p mechanic-app --example protocol_native_smoke -- --cfg test`.
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
        std::thread::sleep(std::time::Duration::from_secs(30));
        eprintln!("protocol native smoke timed out");
        std::process::exit(2);
    });
    struct Smoke {
        app: app::App,
        fixture: Option<app::protocol_smoke::ProtocolFixture>,
        result: Option<Result<(), String>>,
    }
    impl ApplicationHandler<app::UserEvent> for Smoke {
        fn resumed(&mut self, events: &ActiveEventLoop) {
            if self.fixture.is_some() || self.result.is_some() {
                return;
            }
            match self.app.smoke_prepare_protocols(events) {
                Ok(fixture) => self.fixture = Some(fixture),
                Err(e) => {
                    self.result = Some(Err(e));
                    events.exit();
                }
            }
        }
        fn user_event(&mut self, events: &ActiveEventLoop, event: app::UserEvent) {
            self.app.user_event(events, event);
        }
        fn window_event(
            &mut self,
            events: &ActiveEventLoop,
            id: winit::window::WindowId,
            event: winit::event::WindowEvent,
        ) {
            self.app.window_event(events, id, event);
        }
        fn about_to_wait(&mut self, events: &ActiveEventLoop) {
            self.app.about_to_wait(events);
            if let Some(fixture) = &self.fixture {
                match self.app.smoke_protocols_done(fixture) {
                    Ok(true) => {
                        self.result = Some(Ok(()));
                        events.exit();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        self.result = Some(Err(e));
                        events.exit();
                    }
                }
            }
        }
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let events = EventLoop::<app::UserEvent>::with_user_event().build().unwrap();
    let mut config = mechanic_config::Config::default();
    config.shell.program = "/bin/sh".into();
    config.shell.integration = false;
    config.session.restore = false;
    config.control.enabled = false;
    config.terminal.close_on_exit = mechanic_config::CloseOnExitPolicy::Never;
    config.font.family = "Menlo".into();
    config.theme.logo_size = 0;
    let mut app = app::App::new(config, events.create_proxy(), Default::default(), true);
    app.smoke_hide_windows();
    let mut smoke = Smoke { app, fixture: None, result: None };
    events.run_app(&mut smoke).unwrap();
    match smoke.result {
        Some(Ok(())) => println!(
            "protocol native smoke passed: Kitty negotiation/input/pop via real PTY, OSC52 pane selection read/write/reply, native approval lifecycle without pasteboard access, hidden synchronized output timeout and return to idle"
        ),
        result => {
            eprintln!("protocol native smoke failed: {result:?}");
            std::process::exit(1);
        }
    }
}
