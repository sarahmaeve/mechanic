//! Mechanic terminal emulator — application entry point.

mod app;
mod convert;
mod input;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    let config = match config_path(std::env::var_os("XDG_CONFIG_HOME"), std::env::var_os("HOME")) {
        Some(path) => mechanic_config::Config::load(&path),
        None => {
            log::warn!("neither $XDG_CONFIG_HOME nor $HOME set — using built-in defaults");
            mechanic_config::Config::default()
        }
    };
    let event_loop = winit::event_loop::EventLoop::new().unwrap();
    let mut app = app::App::new(config);
    event_loop.run_app(&mut app).unwrap();
}

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
    use super::config_path;
    use std::ffi::OsString;

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
}
