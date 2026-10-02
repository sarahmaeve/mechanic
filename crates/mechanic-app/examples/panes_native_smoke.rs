//! Hidden native pane/PTY smoke. Build with `cargo rustc -p mechanic-app
//! --example panes_native_smoke --locked -- --cfg test`.

#[cfg(not(all(test, target_os = "macos")))]
fn main() {
    eprintln!(
        "Build on macOS with: cargo rustc -p mechanic-app --example panes_native_smoke --locked -- --cfg test"
    );
}

#[cfg(all(test, target_os = "macos"))]
include!("panes_smoke_modules.inc");

#[cfg(all(test, target_os = "macos"))]
fn main() {
    use std::{path::PathBuf, time::Duration};
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, EventLoop},
        window::WindowId,
    };
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(45));
        eprintln!("native pane smoke timed out");
        std::process::exit(2);
    });
    let directory =
        std::env::temp_dir().join(format!("mechanic-panes-smoke-{}-Grüße", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let directory = directory.canonicalize().unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    struct Smoke {
        app: app::App,
        directory: PathBuf,
        result: Option<Result<(), String>>,
    }
    impl ApplicationHandler<app::UserEvent> for Smoke {
        fn resumed(&mut self, events: &ActiveEventLoop) {
            if self.result.is_some() {
                return;
            }
            self.app.resumed(events);
            self.result = Some(self.app.smoke_exercise_panes(events, &self.directory));
            events.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let events = EventLoop::<app::UserEvent>::with_user_event().build().unwrap();
    let mut policy =
        notifications::NotificationPolicy::new(&mechanic_config::NotificationsConfig {
            enabled: true,
            ..Default::default()
        });
    let notice = policy
        .consider(
            notifications::CompletionScope { window: WindowId::from(1), pane: 1, session: 1 },
            false,
            &mechanic_core::CommandCompletion {
                id: 1,
                duration: Duration::from_secs(12),
                exit_status: Some(7),
                cwd: None,
                cwd_host: None,
            },
        )
        .unwrap();
    notifications_platform::native_smoke(&notice).expect("native notification construction");
    let mut config = mechanic_config::Config::default();
    config.shell.program = "/bin/sh".into();
    config.shell.integration = false;
    config.font.family = "Menlo".into();
    config.theme.logo_size = 0;
    let mut app = app::App::new(
        config,
        events.create_proxy(),
        mechanic_config::theme::AnimationConfig { logo: false, background: false },
        true,
    );
    app.smoke_hide_windows();
    let mut smoke = Smoke { app, directory, result: None };
    events.run_app(&mut smoke).unwrap();
    match smoke.result {
        Some(Ok(())) => println!(
            "native pane smoke passed: independent PTYs, inherited cwd, splits, pointer routing, selection/search isolation, divider drag, font resize, render preparation, close/collapse and stale wake"
        ),
        result => {
            eprintln!("native pane smoke failed: {result:?}");
            std::process::exit(1);
        }
    }
}
