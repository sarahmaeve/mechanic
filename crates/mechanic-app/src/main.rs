//! Mechanic terminal emulator — application entry point.

mod app;
mod convert;
mod input;
mod mouse;

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

    // Typed event loop: `UserEvent::PtyOutput(WindowId)` is how PTY reader
    // threads (and any other background producer) wake the main thread.
    // Without this, switching the main loop to `ControlFlow::Wait` would
    // make the shell appear frozen until the user moves the mouse.
    let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to build event loop");
    let proxy = event_loop.create_proxy();

    let mut app = app::App::new(config, proxy, cli.animate, cli.mouse_tracking);
    event_loop.run_app(&mut app).expect("event loop exited with error");
}

// ── CLI ───────────────────────────────────────────────────────────────────────

/// Parsed command-line options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cli {
    /// Whether time-based visual effects should run.  `--no-animation`
    /// forces this to `false`; absent the flag it's `true` and the
    /// corner-gradient pulse, electron animation, and opacity fade all
    /// run normally.
    animate: bool,
    /// Whether to honour programs' mouse-reporting requests (DECSET
    /// 1000/1002/1003/1006).  `--no-mouse-tracking` forces this to
    /// `false`; absent the flag it's `true` and programs like vim,
    /// tmux, fzf get their mouse events forwarded.  Users who'd
    /// rather keep drag-select and middle-click-paste working
    /// unconditionally pass the flag.
    mouse_tracking: bool,
}

impl Default for Cli {
    fn default() -> Self {
        Self { animate: true, mouse_tracking: true }
    }
}

/// Parse `mechanic`'s command-line arguments.
///
/// Hand-rolled rather than pulling in clap: the flag surface is tiny
/// and is unlikely to grow quickly.  Unknown flags exit with code 2
/// (conventional for CLI usage errors).
fn parse_args<I>(args: I) -> Cli
where
    I: IntoIterator<Item = String>,
{
    let mut cli = Cli::default();
    for arg in args {
        match arg.as_str() {
            "--no-animation" => cli.animate = false,
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
    println!("    --no-animation         Disable all time-based visual effects");
    println!("                           (opacity fade, corner gradient pulse,");
    println!("                           electron traces).  Useful on battery,");
    println!("                           on low-end GPUs, or for recording demos.");
    println!("    --no-mouse-tracking    Ignore programs' DECSET 1000/1002/1003/1006");
    println!("                           mouse-reporting requests.  Drag-select and");
    println!("                           middle-click-paste always work locally, at");
    println!("                           the cost of vim/tmux/fzf mouse support.");
    println!("    -h, --help             Show this help and exit");
    println!("    -V, --version          Show version and exit");
}

// ── Config path resolution ────────────────────────────────────────────────────

/// Resolve the user's `mechanic.toml` config path using XDG then HOME.
///
/// Returns `None` if neither `$XDG_CONFIG_HOME` nor `$HOME` is set
/// (essentially never on macOS/Linux — warn and fall back to
/// defaults when that happens).
fn config_path(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
    if let Some(base) = xdg {
        return Some(std::path::PathBuf::from(base).join("mechanic").join("mechanic.toml"));
    }
    if let Some(h) = home {
        return Some(std::path::PathBuf::from(h).join(".config/mechanic/mechanic.toml"));
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{Cli, config_path, parse_args};
    use std::ffi::OsString;

    // ── Config path resolution ────────────────────────────────────────────────

    #[test]
    fn xdg_takes_priority() {
        let path = config_path(
            Some(OsString::from("/custom/xdg")),
            Some(OsString::from("/home/user")),
        )
        .unwrap();
        assert_eq!(path, std::path::PathBuf::from("/custom/xdg/mechanic/mechanic.toml"));
    }

    #[test]
    fn home_fallback() {
        let path = config_path(None, Some(OsString::from("/home/user"))).unwrap();
        assert_eq!(
            path,
            std::path::PathBuf::from("/home/user/.config/mechanic/mechanic.toml")
        );
    }

    #[test]
    fn neither_set_returns_none() {
        assert!(config_path(None, None).is_none());
    }

    // ── CLI parsing ───────────────────────────────────────────────────────────

    #[test]
    fn cli_default_enables_animation() {
        // No arguments: animation on.
        let cli = parse_args(Vec::<String>::new());
        assert!(cli.animate);
    }

    #[test]
    fn cli_no_animation_disables() {
        let cli = parse_args(vec!["--no-animation".to_string()]);
        assert!(!cli.animate);
        // --no-animation alone should NOT touch mouse tracking.
        assert!(cli.mouse_tracking);
    }

    #[test]
    fn cli_no_mouse_tracking_disables() {
        let cli = parse_args(vec!["--no-mouse-tracking".to_string()]);
        assert!(!cli.mouse_tracking);
        // --no-mouse-tracking alone should NOT touch animation.
        assert!(cli.animate);
    }

    #[test]
    fn cli_both_flags_combine() {
        let cli = parse_args(vec![
            "--no-animation".to_string(),
            "--no-mouse-tracking".to_string(),
        ]);
        assert!(!cli.animate);
        assert!(!cli.mouse_tracking);
    }

    #[test]
    fn cli_default_is_animate() {
        // Default-constructed Cli matches the no-args case.
        assert_eq!(Cli::default(), parse_args(Vec::<String>::new()));
    }
}
