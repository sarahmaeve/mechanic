//! Mechanic terminal emulator — application entry point.

mod app;
mod convert;
mod input;
mod mouse;
mod preedit;
mod scheduling;

use app::UserEvent;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let cli = parse_args(std::env::args().skip(1));

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
}

impl Default for Cli {
    fn default() -> Self {
        Self { hot_cpu: false, animate_background: None, animate_logo: None, mouse_tracking: true }
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
fn parse_args<I>(args: I) -> Cli
where
    I: IntoIterator<Item = String>,
{
    let mut cli = Cli::default();
    for arg in args {
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
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("mechanic {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => {
                eprintln!("mechanic: unknown argument '{other}'");
                eprintln!("try 'mechanic --help' for usage");
                std::process::exit(2);
            }
        }
    }
    cli
}

fn print_help() {
    println!("mechanic — a GPU-accelerated terminal emulator");
    println!();
    println!("USAGE:");
    println!("    mechanic [OPTIONS]");
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
    println!("    -h, --help             Show this help and exit");
    println!("    -V, --version          Show version and exit");
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
    use super::{Cli, config_path, parse_args};
    use std::ffi::OsString;

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
