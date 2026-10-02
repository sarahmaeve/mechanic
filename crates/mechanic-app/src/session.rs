//! Private, versioned layout persistence. Every restored pane starts a fresh shell.

use crate::panes::{PaneId, PaneTreeSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

pub const SESSION_VERSION: u32 = 1;
pub const MAX_WINDOWS: usize = 16;
pub const MAX_SESSION_BYTES: u64 = 1024 * 1024;
const SESSION_FILE: &str = "session.json";
const LOCK_FILE: &str = "session.lock";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSnapshot {
    pub logical_size: [f64; 2],
    pub logical_position: Option<[f64; 2]>,
    pub font_size: f32,
    pub panes: PaneTreeSnapshot,
    pub directories: BTreeMap<PaneId, PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub version: u32,
    pub windows: Vec<WindowSnapshot>,
}

impl SessionSnapshot {
    pub fn new(windows: Vec<WindowSnapshot>) -> Self {
        Self { version: SESSION_VERSION, windows }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != SESSION_VERSION {
            return Err(format!("unsupported session version {}", self.version));
        }
        if self.windows.len() > MAX_WINDOWS {
            return Err("session exceeds the window limit".into());
        }
        for window in &self.windows {
            if !window
                .logical_size
                .iter()
                .all(|size| size.is_finite() && (1.0..=65536.0).contains(size))
            {
                return Err("invalid logical window size".into());
            }
            if window.logical_position.is_some_and(|position| {
                !position.iter().all(|value| value.is_finite() && value.abs() <= 1_000_000.0)
            }) {
                return Err("invalid logical window position".into());
            }
            if !window.font_size.is_finite() || !(1.0..=256.0).contains(&window.font_size) {
                return Err("invalid font size".into());
            }
            window.panes.validate()?;
            let ids = window.panes.pane_ids();
            for (id, directory) in &window.directories {
                let bytes = directory.as_os_str().as_bytes();
                if !ids.contains(id)
                    || !directory.is_absolute()
                    || bytes.len() > 4096
                    || bytes.contains(&0)
                {
                    return Err("invalid pane directory".into());
                }
            }
        }
        Ok(())
    }
}

/// State and configuration are intentionally stored in separate directories.
pub fn default_directory(xdg: Option<&Path>, home: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(path) = xdg.filter(|path| path.is_absolute()) {
        return Ok(path.join("mechanic"));
    }
    home.filter(|path| path.is_absolute())
        .map(|path| path.join(".local/state/mechanic"))
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no absolute state home or HOME directory")
        })
}

pub fn state_directory() -> io::Result<PathBuf> {
    let xdg = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    default_directory(xdg.as_deref().map(Path::new), home.as_deref().map(Path::new))
}

/// Open a private, owner-controlled directory without following its final symlink.
/// All session operations subsequently use this descriptor, avoiding path races.
pub fn ensure_private_directory(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = directory.metadata()?;
    // SAFETY: geteuid takes no arguments and has no memory preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "state directory has a different owner",
        ));
    }
    // SAFETY: the descriptor is a live owned directory and mode is valid.
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(directory)
}

fn open_at(directory: &File, name: &str, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    let name = CString::new(name).map_err(io::Error::other)?;
    // SAFETY: directory and nul-terminated name remain valid; mode is supplied for O_CREAT.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returns a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn validate_private_file(file: &File, allow_unlinked: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no memory preconditions.
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() > 1
        || (!allow_unlinked && metadata.nlink() == 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "state file must be regular, owned by this user, and without extra hard links",
        ));
    }
    Ok(())
}

fn load_at(directory: &File) -> io::Result<SessionSnapshot> {
    let file = open_at(directory, SESSION_FILE, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    // An atomic replacement may unlink this already-open, complete old snapshot.
    validate_private_file(&file, true)?;
    if file.metadata()?.len() > MAX_SESSION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session file exceeds the byte limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SESSION_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SESSION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session file exceeds the byte limit",
        ));
    }
    let snapshot: SessionSnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    snapshot.validate().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(snapshot)
}

fn unlink_at(directory: &File, name: &CString) {
    // SAFETY: the descriptor and nul-terminated filename remain valid.
    unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
}

fn write_at(directory: &File, snapshot: &SessionSnapshot) -> io::Result<()> {
    snapshot.validate().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let bytes = serde_json::to_vec(snapshot).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_SESSION_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "session exceeds the byte limit"));
    }
    // Refuse directory, hard-link, or symlink collisions instead of clobbering them.
    match open_at(directory, SESSION_FILE, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
        Ok(file) => validate_private_file(&file, false)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let (mut temporary, name) = (0..128)
        .find_map(|_| {
            let name = format!(
                ".session-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            );
            match open_at(directory, &name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600) {
                Ok(file) => Some(Ok((
                    file,
                    CString::new(name).expect("generated filename contains no nul"),
                ))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .unwrap_or_else(|| {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "too many temporary session filename collisions",
            ))
        })?;
    let destination = CString::new(SESSION_FILE).expect("static filename contains no nul");
    let result = (|| {
        // SAFETY: temporary is an owned regular file descriptor.
        if unsafe { libc::fchmod(temporary.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        // SAFETY: both names are valid and both directory descriptors are live.
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                name.as_ptr(),
                directory.as_raw_fd(),
                destination.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        directory.sync_all()
    })();
    unlink_at(directory, &name);
    result
}

#[derive(Default)]
struct WriterState {
    pending: Option<(u64, SessionSnapshot)>,
    requested: u64,
    completed: u64,
    last_error: Option<(io::ErrorKind, String)>,
    stopping: bool,
}

type SharedWriter = Arc<(Mutex<WriterState>, Condvar)>;

/// Holds an independent instance lock for the lifetime of the background writer.
/// `save` replaces any waiting snapshot; `flush` waits for the latest requested save.
pub struct SessionStore {
    directory: Arc<File>,
    _lock: File,
    writer: SharedWriter,
    thread: Option<JoinHandle<()>>,
}

impl SessionStore {
    pub fn open(path: &Path) -> io::Result<Self> {
        let directory = Arc::new(ensure_private_directory(path)?);
        let lock =
            open_at(&directory, LOCK_FILE, libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK, 0o600)?;
        validate_private_file(&lock, false)?;
        // SAFETY: lock is a live descriptor; flock and fchmod operate only on it.
        if unsafe { libc::fchmod(lock.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: lock is a live descriptor and LOCK_EX | LOCK_NB is a valid operation.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let writer = Arc::new((Mutex::new(WriterState::default()), Condvar::new()));
        let shared = writer.clone();
        let worker_directory = directory.clone();
        let thread = thread::Builder::new().name("mechanic-session".into()).spawn(move || {
            let (mutex, changed) = &*shared;
            loop {
                let (sequence, snapshot) = {
                    let mut state = mutex.lock().expect("session writer mutex poisoned");
                    while state.pending.is_none() && !state.stopping {
                        state = changed.wait(state).expect("session writer mutex poisoned");
                    }
                    let Some(pending) = state.pending.take() else { break };
                    pending
                };
                let error = write_at(&worker_directory, &snapshot).err();
                if let Some(error) = &error {
                    log::warn!("mechanic-session: could not save session: {error}");
                }
                let mut state = mutex.lock().expect("session writer mutex poisoned");
                state.completed = sequence;
                state.last_error = error.map(|error| (error.kind(), error.to_string()));
                changed.notify_all();
            }
        })?;
        Ok(Self { directory, _lock: lock, writer, thread: Some(thread) })
    }

    pub fn load(&self) -> Option<SessionSnapshot> {
        match load_at(&self.directory) {
            Ok(snapshot) => Some(snapshot),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                log::warn!("mechanic-session: could not restore session: {error}");
                None
            }
        }
    }

    pub fn save(&self, snapshot: SessionSnapshot) -> io::Result<()> {
        snapshot.validate().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let (mutex, changed) = &*self.writer;
        let mut state =
            mutex.lock().map_err(|_| io::Error::other("session writer mutex poisoned"))?;
        state.requested = state
            .requested
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        state.pending = Some((state.requested, snapshot));
        changed.notify_one();
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        let (mutex, changed) = &*self.writer;
        let mut state =
            mutex.lock().map_err(|_| io::Error::other("session writer mutex poisoned"))?;
        let requested = state.requested;
        while state.completed < requested {
            state = changed
                .wait(state)
                .map_err(|_| io::Error::other("session writer mutex poisoned"))?;
        }
        match &state.last_error {
            Some((kind, message)) => Err(io::Error::new(*kind, message.clone())),
            None => Ok(()),
        }
    }
}

impl Drop for SessionStore {
    fn drop(&mut self) {
        let (mutex, changed) = &*self.writer;
        if let Ok(mut state) = mutex.lock() {
            state.stopping = true;
            changed.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::{Axis, PaneTree};
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn snapshot(directory: &Path) -> SessionSnapshot {
        let mut panes = PaneTree::new(0);
        panes.split_active(Axis::Vertical);
        panes.split_active(Axis::Horizontal);
        SessionSnapshot::new(vec![WindowSnapshot {
            logical_size: [900.5, 600.25],
            logical_position: Some([-200.0, 40.5]),
            font_size: 18.0,
            panes: panes.snapshot(),
            directories: BTreeMap::from([
                (0, directory.join("日本語 café")),
                (1, directory.to_owned()),
            ]),
        }])
    }

    #[test]
    fn state_paths_are_separate_and_relative_xdg_is_ignored() {
        assert_eq!(
            default_directory(Some(Path::new("/state")), Some(Path::new("/home"))).unwrap(),
            Path::new("/state/mechanic")
        );
        assert_eq!(
            default_directory(Some(Path::new("relative")), Some(Path::new("/home"))).unwrap(),
            Path::new("/home/.local/state/mechanic")
        );
        assert!(default_directory(None, None).is_err());
        assert!(default_directory(None, Some(Path::new("relative"))).is_err());
    }

    #[test]
    fn atomic_reload_preserves_geometry_unicode_and_private_permissions() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state");
        let store = SessionStore::open(&path).unwrap();
        assert!(store.load().is_none());
        let original = snapshot(temporary.path());
        store.save(original.clone()).unwrap();
        store.flush().unwrap();
        assert_eq!(store.load(), Some(original));
        let mut updated = snapshot(temporary.path());
        updated.windows[0].font_size = 20.0;
        store.save(updated.clone()).unwrap();
        store.flush().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(path.join(SESSION_FILE)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.join(LOCK_FILE)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 2);
        drop(store);
        assert_eq!(SessionStore::open(&path).unwrap().load(), Some(updated));
    }

    #[test]
    fn independent_lock_excludes_instances_and_releases_on_drop() {
        let temporary = tempfile::tempdir().unwrap();
        let first = SessionStore::open(temporary.path()).unwrap();
        let error = SessionStore::open(temporary.path()).err().expect("second store must fail");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        assert!(SessionStore::open(temporary.path()).is_ok());
    }

    #[test]
    fn lock_collisions_do_not_change_link_targets() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let target = temporary.path().join("preserve");
        std::fs::write(&target, "preserve").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let lock = state.join(LOCK_FILE);
        symlink(&target, &lock).unwrap();
        assert!(SessionStore::open(&state).is_err());
        assert!(std::fs::symlink_metadata(&lock).unwrap().is_symlink());
        std::fs::remove_file(&lock).unwrap();
        std::fs::hard_link(&target, &lock).unwrap();
        assert!(SessionStore::open(&state).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn malformed_unsupported_oversized_and_invalid_layouts_fall_back() {
        let temporary = tempfile::tempdir().unwrap();
        let store = SessionStore::open(temporary.path()).unwrap();
        let path = temporary.path().join(SESSION_FILE);
        for bytes in [
            b"not json".as_slice(),
            br#"{"version":999,"windows":[]}"#,
            br#"{"version":1,"windows":[],"commands":["danger"]}"#,
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(store.load().is_none());
        }
        let mut bad = snapshot(temporary.path());
        bad.windows[0].panes.active = 999;
        std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(store.load().is_none());
        let oversized = File::create(&path).unwrap();
        oversized.set_len(MAX_SESSION_BYTES + 1).unwrap();
        assert!(store.load().is_none());
        store.save(snapshot(temporary.path())).unwrap();
        store.flush().unwrap();
        assert!(store.load().is_some());
    }

    #[test]
    fn invalid_snapshots_are_rejected_before_the_previous_file_changes() {
        let temporary = tempfile::tempdir().unwrap();
        let store = SessionStore::open(temporary.path()).unwrap();
        let original = snapshot(temporary.path());
        store.save(original.clone()).unwrap();
        store.flush().unwrap();
        for value in [f64::NAN, 0.0, 65537.0] {
            let mut invalid = original.clone();
            invalid.windows[0].logical_size[0] = value;
            assert!(store.save(invalid).is_err());
        }
        let mut invalid = original.clone();
        invalid.windows[0].directories.insert(999, temporary.path().to_owned());
        assert!(store.save(invalid).is_err());
        let mut invalid = original.clone();
        invalid.windows[0].directories.insert(0, PathBuf::from("relative"));
        assert!(store.save(invalid).is_err());
        let mut invalid = original.clone();
        invalid.windows = vec![invalid.windows[0].clone(); MAX_WINDOWS + 1];
        assert!(store.save(invalid).is_err());
        assert_eq!(store.load(), Some(original));
    }

    #[test]
    fn coalescing_and_drop_complete_the_latest_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let store = SessionStore::open(temporary.path()).unwrap();
        let mut latest = snapshot(temporary.path());
        for index in 0..100 {
            latest.windows[0].font_size = 10.0 + index as f32;
            store.save(latest.clone()).unwrap();
        }
        drop(store);
        assert_eq!(SessionStore::open(temporary.path()).unwrap().load(), Some(latest));
    }

    #[test]
    fn serialized_byte_limit_does_not_replace_the_previous_session() {
        let temporary = tempfile::tempdir().unwrap();
        let store = SessionStore::open(temporary.path()).unwrap();
        let original = snapshot(temporary.path());
        store.save(original.clone()).unwrap();
        store.flush().unwrap();
        let mut tree = PaneTree::new(0);
        for _ in 1..crate::panes::MAX_PANES {
            tree.split_active(Axis::Vertical);
        }
        // Escaped control characters make otherwise bounded paths exceed the JSON cap.
        let directory = PathBuf::from(format!("/{}", "\u{1}".repeat(4095)));
        let window = WindowSnapshot {
            logical_size: [800.0, 600.0],
            logical_position: None,
            font_size: 16.0,
            panes: tree.snapshot(),
            directories: tree.pane_ids().into_iter().map(|id| (id, directory.clone())).collect(),
        };
        let oversized = SessionSnapshot::new(vec![window; MAX_WINDOWS]);
        assert!(oversized.validate().is_ok());
        store.save(oversized).unwrap();
        assert!(store.flush().is_err());
        assert_eq!(store.load(), Some(original));
    }

    #[test]
    fn atomic_readers_observe_only_complete_snapshots() {
        let temporary = tempfile::tempdir().unwrap();
        let store = SessionStore::open(temporary.path()).unwrap();
        store.save(snapshot(temporary.path())).unwrap();
        store.flush().unwrap();
        let directory = ensure_private_directory(temporary.path()).unwrap();
        let reader = std::thread::spawn(move || {
            for _ in 0..200 {
                load_at(&directory)
                    .expect("atomic replacement always leaves a complete valid session");
            }
        });
        for index in 0..100 {
            let mut current = snapshot(temporary.path());
            current.windows[0].font_size = 10.0 + index as f32;
            store.save(current).unwrap();
            store.flush().unwrap();
        }
        reader.join().unwrap();
    }

    #[test]
    fn collisions_and_symlinks_are_preserved_and_flush_reports_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let file_collision = temporary.path().join("file");
        std::fs::write(&file_collision, "preserve").unwrap();
        assert!(SessionStore::open(&file_collision).is_err());
        assert_eq!(std::fs::read_to_string(&file_collision).unwrap(), "preserve");

        let linked_directory = temporary.path().join("linked-state");
        symlink(temporary.path(), &linked_directory).unwrap();
        assert!(SessionStore::open(&linked_directory).is_err());
        assert!(std::fs::symlink_metadata(&linked_directory).unwrap().is_symlink());

        let state = temporary.path().join("state");
        let store = SessionStore::open(&state).unwrap();
        let destination = state.join(SESSION_FILE);
        let target = temporary.path().join("target");
        std::fs::write(&target, "preserve target").unwrap();
        symlink(&target, &destination).unwrap();
        assert!(store.load().is_none());
        store.save(snapshot(temporary.path())).unwrap();
        assert!(store.flush().is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve target");
        assert!(std::fs::symlink_metadata(&destination).unwrap().is_symlink());

        std::fs::remove_file(&destination).unwrap();
        std::fs::create_dir(&destination).unwrap();
        store.save(snapshot(temporary.path())).unwrap();
        assert!(store.flush().is_err());
        assert!(destination.is_dir());

        std::fs::remove_dir(&destination).unwrap();
        std::fs::hard_link(&target, &destination).unwrap();
        store.save(snapshot(temporary.path())).unwrap();
        assert!(store.flush().is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "preserve target");
    }
}
