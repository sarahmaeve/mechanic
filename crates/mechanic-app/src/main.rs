//! Mechanic terminal emulator — application entry point.

mod app;
mod control;
mod control_cli;
mod convert;
mod hyperlinks;
mod input;
mod link_input;
mod link_platform;
mod mouse;
mod notifications;
mod notifications_platform;
mod panes;
mod preedit;
mod scheduling;
mod search;
mod search_platform;
mod session;

use app::UserEvent;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = match parse_args(std::env::args().skip(1)) {
        Ok(Invocation::App(cli)) => cli,
        Ok(Invocation::Control(cli)) => std::process::exit(control_cli::run(cli)),
        Ok(Invocation::Help) => {
            print_help();
            return;
        }
        Ok(Invocation::ControlHelp) => {
            control_cli::print_help();
            return;
        }
        Ok(Invocation::Version) => {
            println!("mechanic {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Err(error) => {
            if error.control {
                control_cli::print_error("invalid_arguments", &error.message);
            } else {
                eprintln!("mechanic: {}", error.message);
                eprintln!("try 'mechanic --help' for usage");
            }
            std::process::exit(2);
        }
    };

    let config = match config_path(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME")) {
        Some(path) => mechanic_config::Config::load(&path),
        None => {
            log::warn!("neither $XDG_CONFIG_HOME nor $HOME set — using built-in defaults");
            mechanic_config::Config::default()
        }
    };

    let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to build event loop");
    let proxy = event_loop.create_proxy();

    let animations = cli.animations(config.theme.animation);
    let mut app = app::App::new(config, proxy, animations, cli.mouse_tracking);
    let state_directory = session::state_directory()
        .map_err(|err| {
            log::warn!("cannot resolve session storage: {err}");
        })
        .ok();
    let control_directory = control::runtime_directory()
        .map_err(|err| {
            log::warn!("cannot resolve local control directory: {err}");
        })
        .ok();
    app.configure_services(!cli.no_restore, state_directory, control_directory);
    event_loop.run_app(&mut app).expect("event loop exited with error");
}

/// Parsed command-line options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cli {
    /// Enable continuous shader animation while focused.
    hot_cpu: bool,
    animate_background: Option<bool>,
    animate_logo: Option<bool>,
    /// Forward mouse events when requested by the terminal program.
    mouse_tracking: bool,
    /// Skip loading persisted windows and panes at startup.
    no_restore: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            hot_cpu: false,
            animate_background: None,
            animate_logo: None,
            mouse_tracking: true,
            no_restore: false,
        }
    }
}

impl Cli {
    fn animations(
        self,
        config: mechanic_config::theme::AnimationConfig,
    ) -> mechanic_config::theme::AnimationConfig {
        mechanic_config::theme::AnimationConfig {
            background: self.animate_background.unwrap_or(config.background || self.hot_cpu),
            logo: self.animate_logo.unwrap_or(config.logo || self.hot_cpu),
        }
    }
}

/// Parse `mechanic`'s command-line arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Invocation {
    App(Cli),
    Control(control_cli::Cli),
    Help,
    ControlHelp,
    Version,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliError {
    message: String,
    control: bool,
}

fn parse_args<I>(args: I) -> Result<Invocation, CliError>
where
    I: IntoIterator<Item = String>,
{
    let args: Vec<_> = args.into_iter().collect();
    let control = args.iter().any(|arg| arg == "ctl");
    let mut args = args.into_iter();
    let mut cli = Cli::default();
    let mut socket = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--animate" | "--hot-cpu" => {
                cli.hot_cpu = true;
                cli.animate_background = Some(true);
                cli.animate_logo = Some(true);
            }
            "--animate-background" => cli.animate_background = Some(true),
            "--animate-logo" => cli.animate_logo = Some(true),
            "--no-animate-background" => cli.animate_background = Some(false),
            "--no-animate-logo" => cli.animate_logo = Some(false),
            "--no-mouse-tracking" => cli.mouse_tracking = false,
            "--no-restore" => cli.no_restore = true,
            "--socket" => {
                if socket.is_some() {
                    return Err(CliError {
                        message: "--socket may only be supplied once".into(),
                        control,
                    });
                }
                let value = args.next().ok_or_else(|| CliError {
                    message: "--socket requires a path".into(),
                    control,
                })?;
                socket = Some(std::path::PathBuf::from(value));
            }
            "ctl" => {
                return control_cli::parse(args.collect(), socket)
                    .map(|parsed| match parsed {
                        control_cli::Parsed::Command(cli) => Invocation::Control(cli),
                        control_cli::Parsed::Help => Invocation::ControlHelp,
                    })
                    .map_err(|message| CliError { message, control: true });
            }
            "-h" | "--help" => {
                return Ok(if control { Invocation::ControlHelp } else { Invocation::Help });
            }
            "-V" | "--version" => return Ok(Invocation::Version),
            other => {
                return Err(CliError { message: format!("unknown argument '{other}'"), control });
            }
        }
    }
    if socket.is_some() {
        return Err(CliError { message: "--socket requires a ctl command".into(), control: false });
    }
    Ok(Invocation::App(cli))
}

fn print_help() {
    println!("mechanic — a GPU-accelerated terminal emulator");
    println!();
    println!("USAGE:");
    println!("    mechanic [OPTIONS]");
    println!("    mechanic [--socket PATH] ctl COMMAND [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --animate              Animate the gradient and logo while focused");
    println!("                           (paced at about 30 FPS; animations off by default)");
    println!("    --hot-cpu              Alias for --animate");
    println!("    --animate-background   Animate background lighting while focused");
    println!("    --animate-logo         Animate the logo while focused");
    println!("    --no-animate-background Disable background animation");
    println!("    --no-animate-logo      Disable logo animation and focus glow");
    println!("    --no-mouse-tracking    Keep selection and middle-click paste local");
    println!("    --no-restore           Start fresh without loading saved windows and panes");
    println!("    ctl                    Control a running instance; see 'mechanic ctl --help'");
    println!("    -h, --help             Show this help and exit");
    println!("    -V, --version          Show version and exit");
    println!("\n    Cmd+Shift+A toggles all animations on/off for this session.");
    println!("    Cmd+D splits side by side; Cmd+Shift+D splits top and bottom.");
    println!("    Cmd+[ / Cmd+] cycles panes; Cmd+W closes a pane; Cmd+Shift+W closes a window.");
    println!("    Cmd+Option+arrow focuses the pane in that direction.");
}

/// Resolve the user's `mechanic.toml` config path using XDG then HOME.
fn config_path(
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    if let Some(base) = xdg {
        return Some(std::path::PathBuf::from(base).join("mechanic").join("mechanic.toml"));
    }
    if let Some(h) = home {
        return Some(std::path::PathBuf::from(h).join(".config/mechanic/mechanic.toml"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{Cli, Invocation, config_path};
    use std::ffi::OsString;

    fn parse_args(args: impl IntoIterator<Item = String>) -> Cli {
        let Invocation::App(cli) = super::parse_args(args).unwrap() else {
            panic!("expected GUI options")
        };
        cli
    }

    #[test]
    fn non_gui_modes_are_returned_without_process_exit() {
        assert_eq!(super::parse_args(["--help".into()]).unwrap(), Invocation::Help);
        assert_eq!(
            super::parse_args(["ctl".into(), "--help".into()]).unwrap(),
            Invocation::ControlHelp
        );
        assert_eq!(super::parse_args(["--version".into()]).unwrap(), Invocation::Version);
        assert!(matches!(
            super::parse_args(["ctl".into(), "list".into()]).unwrap(),
            Invocation::Control(_)
        ));
        let error = super::parse_args(["ctl".into(), "read".into()]).unwrap_err();
        assert!(error.control);
        assert!(error.message.contains("--pane"));
    }

    #[test]
    fn no_restore_flag_and_global_socket_are_parsed() {
        assert!(parse_args(["--no-restore".into()]).no_restore);
        let Invocation::Control(cli) = super::parse_args([
            "--socket".into(),
            "/tmp/control.sock".into(),
            "ctl".into(),
            "list".into(),
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(cli.socket, Some("/tmp/control.sock".into()));
        assert!(super::parse_args(["--socket".into(), "/tmp/control.sock".into()]).is_err());
    }

    #[test]
    fn animation_overrides_are_independent_and_last_flag_wins() {
        let defaults = mechanic_config::theme::AnimationConfig::default();
        let effective = Cli::default().animations(defaults);
        assert!(!effective.logo);
        assert!(!effective.background);
        for (flags, logo, background) in [
            (vec!["--no-animate-logo", "--animate-background"], false, true),
            (vec!["--animate", "--no-animate-background"], true, false),
            (vec!["--no-animate-logo", "--animate"], true, true),
            (vec!["--no-animate-logo"], false, false),
        ] {
            let effective = parse_args(flags.into_iter().map(str::to_owned)).animations(defaults);
            assert_eq!(effective.logo, logo);
            assert_eq!(effective.background, background);
        }
        let configured = mechanic_config::theme::AnimationConfig { logo: false, background: true };
        let effective = Cli::default().animations(configured);
        assert!(!effective.logo);
        assert!(effective.background);
    }

    #[test]
    fn xdg_takes_priority() {
        let path =
            config_path(Some(OsString::from("/custom/xdg")), Some(OsString::from("/home/user")))
                .unwrap();
        assert_eq!(path, std::path::PathBuf::from("/custom/xdg/mechanic/mechanic.toml"));
    }

    #[test]
    fn home_fallback() {
        let path = config_path(None, Some(OsString::from("/home/user"))).unwrap();
        assert_eq!(path, std::path::PathBuf::from("/home/user/.config/mechanic/mechanic.toml"));
    }

    #[test]
    fn neither_set_returns_none() {
        assert!(config_path(None, None).is_none());
    }

    #[test]
    fn cli_default_has_hot_cpu_off() {
        let cli = parse_args(Vec::<String>::new());
        assert!(!cli.hot_cpu);
        assert!(cli.mouse_tracking);
    }

    #[test]
    fn cli_hot_cpu_enables() {
        for flag in ["--animate", "--hot-cpu"] {
            let cli = parse_args(vec![flag.to_string()]);
            assert!(cli.hot_cpu);
            assert!(cli.mouse_tracking);
        }
    }

    #[test]
    fn cli_no_mouse_tracking_disables() {
        let cli = parse_args(vec!["--no-mouse-tracking".to_string()]);
        assert!(!cli.mouse_tracking);
        assert!(!cli.hot_cpu);
    }

    #[test]
    fn cli_both_flags_combine() {
        let cli = parse_args(vec!["--hot-cpu".to_string(), "--no-mouse-tracking".to_string()]);
        assert!(cli.hot_cpu);
        assert!(!cli.mouse_tracking);
    }

    #[test]
    fn cli_default_matches_no_args() {
        assert_eq!(Cli::default(), parse_args(Vec::<String>::new()));
    }
}
