//! Workspace persistence is explicitly configured by normal GUI startup.
use super::*;
use crate::session::{SessionSnapshot, SessionStore, WindowSnapshot};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SAVE_DELAY: Duration = Duration::from_millis(250);

#[derive(Default)]
pub(super) struct SessionService {
    pub(super) store: Option<SessionStore>,
    restore: Option<SessionSnapshot>,
    pending: Option<SessionSnapshot>,
    observed: Option<SessionSnapshot>,
    deadline: Option<Instant>,
}

impl App {
    /// App::new remains inert so native tests can opt into isolated services.
    pub fn configure_services(
        &mut self,
        restore: bool,
        state_directory: Option<PathBuf>,
        control_directory: Option<PathBuf>,
    ) {
        if self.config.session.restore
            && let Some(directory) = state_directory
        {
            match SessionStore::open(&directory) {
                Ok(store) => {
                    if restore {
                        self.session_service.restore = store.load();
                    }
                    self.session_service.store = Some(store);
                }
                Err(error) => log::warn!("workspace persistence unavailable: {error}"),
            }
        }
        if self.config.control.enabled
            && let Some(directory) = control_directory
        {
            let proxy = self.proxy.clone();
            match crate::control::ControlServer::start_in(directory, move |event| {
                proxy.send_event(UserEvent::Control(event)).map_err(|_| ())
            }) {
                Ok(server) => self.control_service.server = Some(server),
                Err(error) => log::warn!("local control unavailable: {error}"),
            }
        }
    }

    pub(super) fn workspace_snapshot(&self) -> SessionSnapshot {
        let mut windows: Vec<_> = self.windows.iter().collect();
        // HashMap iteration order must not turn equivalent snapshots into writes.
        windows.sort_by_key(|(id, _)| format!("{id:?}"));
        SessionSnapshot::new(
            windows.into_iter().map(|(_, state)| state.workspace_snapshot()).collect(),
        )
    }

    pub(super) fn note_session_change(&mut self) {
        if self.session_service.store.is_none() || self.windows.is_empty() {
            return;
        }
        let snapshot = self.workspace_snapshot();
        if self.session_service.observed.as_ref() == Some(&snapshot) {
            return;
        }
        self.session_service.observed = Some(snapshot.clone());
        self.session_service.pending = Some(snapshot);
        // A burst of changes shares one deadline; dragging cannot defer saves forever.
        self.session_service.deadline.get_or_insert_with(|| Instant::now() + SAVE_DELAY);
    }

    pub(super) fn service_session_deadline(
        &mut self,
        now: Instant,
        deadline: &mut Option<Instant>,
    ) {
        if let Some(due) = self.session_service.deadline {
            if now >= due {
                self.save_pending_session();
            } else {
                merge_deadline(deadline, due);
            }
        }
    }

    fn save_pending_session(&mut self) {
        self.session_service.deadline = None;
        if let Some(snapshot) = self.session_service.pending.take()
            && let Some(store) = &self.session_service.store
            && let Err(error) = store.save(snapshot)
        {
            log::warn!("could not save workspace: {error}");
        }
    }

    pub(super) fn flush_session(&mut self) {
        self.save_pending_session();
        if let Some(store) = &self.session_service.store
            && let Err(error) = store.flush()
        {
            log::warn!("could not flush workspace: {error}");
        }
    }

    pub(super) fn restore_workspace(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let Some(snapshot) = self.session_service.restore.take() else { return false };
        for window in snapshot.windows {
            self.spawn_restored_window(event_loop, &window);
        }
        !self.windows.is_empty()
    }

    pub(super) fn quit(&mut self, event_loop: &ActiveEventLoop) {
        self.note_session_change();
        self.flush_session();
        self.control_service.close_all();
        event_loop.exit();
    }
}

impl AppState {
    pub(super) fn workspace_snapshot(&self) -> WindowSnapshot {
        let scale = self.window.scale_factor();
        let size = self.window.inner_size().to_logical::<f64>(scale);
        let position = self.window.outer_position().ok().map(|position| {
            let position = position.to_logical::<f64>(scale);
            [position.x, position.y]
        });
        let mut directories = BTreeMap::new();
        for id in self.tree.pane_ids() {
            if let Some(directory) = self.pane_state(id).and_then(|pane| pane.directory.as_ref()) {
                directories.insert(id, directory.clone());
            }
        }
        WindowSnapshot {
            logical_size: [size.width, size.height],
            logical_position: position,
            font_size: self.current_font_size,
            panes: self.tree.snapshot(),
            directories,
        }
    }
}

/// Store OSC metadata without filesystem work on the event loop. Availability
/// is checked when the next fresh PTY starts, which already has a cwd fallback.
fn local_metadata_directory(cwd: Option<&str>, host: Option<&str>) -> Option<PathBuf> {
    let path = Path::new(cwd?);
    let host = host?;
    if !path.is_absolute()
        || !(host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || local_hostname().is_some_and(|local| host.eq_ignore_ascii_case(local)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

fn local_hostname() -> Option<&'static str> {
    static HOST: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let mut bytes = [0u8; 256];
        // SAFETY: gethostname writes at most the supplied buffer length.
        if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
            return None;
        }
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        String::from_utf8(bytes[..end].to_vec()).ok()
    })
    .as_deref()
}

pub(super) fn refresh_directory(pane: &mut PaneState) -> bool {
    let shell = pane.terminal.shell_integration();
    if (pane.directory_metadata.0.as_deref(), pane.directory_metadata.1.as_deref())
        == (shell.cwd(), shell.cwd_host())
    {
        return false;
    }
    let metadata = (shell.cwd().map(str::to_owned), shell.cwd_host().map(str::to_owned));
    let directory = local_metadata_directory(metadata.0.as_deref(), metadata.1.as_deref());
    pane.directory_metadata = metadata;
    if let Some(directory) = directory
        && pane.directory.as_ref() != Some(&directory)
    {
        pane.directory = Some(directory);
        return true;
    }
    false
}

pub(super) fn position_is_visible(
    event_loop: &ActiveEventLoop,
    position: [f64; 2],
    size: [f64; 2],
) -> bool {
    let mut found_monitor = false;
    for monitor in event_loop.available_monitors() {
        found_monitor = true;
        let scale = monitor.scale_factor();
        let origin = monitor.position().to_logical::<f64>(scale);
        let extent = monitor.size().to_logical::<f64>(scale);
        if rectangles_intersect(position, size, [origin.x, origin.y], [extent.width, extent.height])
        {
            return true;
        }
    }
    // Headless platforms may not expose monitor metadata at all.
    !found_monitor
}

fn rectangles_intersect(a: [f64; 2], a_size: [f64; 2], b: [f64; 2], b_size: [f64; 2]) -> bool {
    a[0] < b[0] + b_size[0]
        && a[1] < b[1] + b_size[1]
        && a[0] + a_size[0] > b[0]
        && a[1] + a_size[1] > b[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_monitor_positions_fall_back_and_negative_monitor_positions_survive() {
        let size = [1024.0, 768.0];
        assert!(!rectangles_intersect([5000.0, 5000.0], size, [0.0, 0.0], [1920.0, 1080.0]));
        assert!(rectangles_intersect([-1200.0, 100.0], size, [-1920.0, 0.0], [1920.0, 1080.0]));
        assert!(rectangles_intersect([-200.0, 100.0], size, [0.0, 0.0], [1920.0, 1080.0]));
    }

    #[test]
    fn only_absolute_local_metadata_can_replace_the_known_launch_directory() {
        assert_eq!(
            local_metadata_directory(Some("/tmp/saved directory"), Some("localhost")),
            Some(PathBuf::from("/tmp/saved directory"))
        );
        assert_eq!(local_metadata_directory(Some("/tmp"), Some("")), Some(PathBuf::from("/tmp")));
        assert!(local_metadata_directory(Some("relative"), Some("localhost")).is_none());
        assert!(local_metadata_directory(Some("/tmp"), Some("remote.invalid")).is_none());
        assert!(local_metadata_directory(Some("/tmp"), None).is_none());
    }
}
