//! Bounded, same-user local control over Unix sockets.
//!
//! Each connection carries one newline-delimited JSON request and response. Pane
//! handles include a random application identity so that handles cannot silently
//! address a different shell after an application restart.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_CONNECTIONS: usize = 8;
pub const MAX_TEXT_BYTES: usize = 128 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 128 * 1024;
pub const MAX_OUTPUT_LINES: usize = 5000;
pub const MAX_WAIT_MS: u64 = 60_000;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_REPLY_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const WAIT_ALLOWANCE: Duration = Duration::from_secs(2);
const SOCKET_PREFIX: &str = "ctl-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSelector {
    pub instance_id: String,
    pub session_id: u64,
}

impl fmt::Display for SessionSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.instance_id, self.session_id)
    }
}

impl FromStr for SessionSelector {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (instance_id, session_id) = value
            .split_once(':')
            .ok_or_else(|| "pane handle must be INSTANCE:SESSION".to_owned())?;
        if !valid_instance_id(instance_id) {
            return Err("invalid instance identity in pane handle".to_owned());
        }
        let session_id = session_id.parse().map_err(|_| "invalid session identity".to_owned())?;
        Ok(Self { instance_id: instance_id.to_owned(), session_id })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Paste,
    Raw,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ListPanes,
    ReadOutput {
        session: SessionSelector,
        max_lines: usize,
        max_bytes: usize,
    },
    SendInput {
        session: SessionSelector,
        text: String,
        #[serde(default)]
        mode: InputMode,
        #[serde(default)]
        enter: bool,
    },
    Wait {
        session: SessionSelector,
        after_command_id: u64,
        timeout_ms: u64,
    },
    Ping,
}

impl Operation {
    pub fn session(&self) -> Option<&SessionSelector> {
        match self {
            Self::ReadOutput { session, .. }
            | Self::SendInput { session, .. }
            | Self::Wait { session, .. } => Some(session),
            Self::ListPanes | Self::Ping => None,
        }
    }

    fn timeout(&self) -> Duration {
        match self {
            Self::Wait { timeout_ms, .. } => Duration::from_millis(*timeout_ms) + WAIT_ALLOWANCE,
            _ => REQUEST_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(flatten)]
    pub operation: Operation,
}

impl Request {
    pub fn new(instance_id: Option<String>, operation: Operation) -> Self {
        Self { version: PROTOCOL_VERSION, instance_id, operation }
    }

    pub(crate) fn validate(&self, instance_id: &str) -> Result<(), (ErrorCode, &'static str)> {
        if self.version != PROTOCOL_VERSION {
            return Err((ErrorCode::UnsupportedVersion, "unsupported protocol version"));
        }
        if self.instance_id.as_deref().is_some_and(|id| id != instance_id)
            || self.operation.session().is_some_and(|session| session.instance_id != instance_id)
        {
            return Err((ErrorCode::StaleInstance, "pane belongs to another application instance"));
        }
        match &self.operation {
            Operation::ReadOutput { max_lines, max_bytes, .. }
                if *max_lines == 0
                    || *max_lines > MAX_OUTPUT_LINES
                    || *max_bytes == 0
                    || *max_bytes > MAX_OUTPUT_BYTES =>
            {
                Err((ErrorCode::InvalidRequest, "output limits exceed allowed bounds"))
            }
            Operation::SendInput { text, .. } if text.len() > MAX_TEXT_BYTES => {
                Err((ErrorCode::InvalidRequest, "input exceeds allowed byte limit"))
            }
            Operation::Wait { timeout_ms, .. } if *timeout_ms > MAX_WAIT_MS => {
                Err((ErrorCode::InvalidRequest, "wait exceeds allowed timeout"))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub session: SessionSelector,
    pub window_id: String,
    pub pane_id: u64,
    pub title: String,
    pub cwd: Option<String>,
    pub focused: bool,
    pub active: bool,
    pub running: bool,
    pub rows: usize,
    pub columns: usize,
    pub shell_integration: bool,
    pub latest_command_id: u64,
    pub last_command: Option<String>,
    pub last_status: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completion {
    pub session: SessionSelector,
    pub command_id: u64,
    pub exit_status: Option<i32>,
    pub command: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    StaleInstance,
    UnknownSession,
    Busy,
    Deadline,
    Closed,
    Restarted,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ResponseResult {
    Panes { panes: Vec<PaneInfo> },
    Output { session: SessionSelector, text: String, truncated: bool },
    InputSent { session: SessionSelector },
    Completed { completion: Completion },
    Timeout { session: SessionSelector },
    Pong,
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub instance_id: String,
    #[serde(flatten)]
    pub result: ResponseResult,
}

impl Response {
    pub fn new(instance_id: impl Into<String>, result: ResponseResult) -> Self {
        Self { version: PROTOCOL_VERSION, instance_id: instance_id.into(), result }
    }

    pub fn error(
        instance_id: impl Into<String>,
        code: ErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self::new(instance_id, ResponseResult::Error { code, message: message.into() })
    }
}

/// The app receives this on its main thread. It must check `deadline` before
/// performing a mutation, and may retain `reply` for an event-driven wait.
#[derive(Debug, Clone)]
pub struct ControlEvent {
    pub request: Request,
    pub reply: mpsc::SyncSender<Response>,
    pub deadline: Instant,
}

#[derive(Debug, Clone, Serialize)]
pub struct Endpoint {
    pub socket_path: PathBuf,
    pub instance_id: String,
}

struct ActiveConnection {
    stream: UnixStream,
    reply: Option<mpsc::SyncSender<Response>>,
}

struct Shared {
    stopping: AtomicBool,
    next_connection: AtomicU64,
    active: Mutex<BTreeMap<u64, ActiveConnection>>,
    instance_id: String,
}

struct ConnectionGuard {
    id: u64,
    shared: Arc<Shared>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.shared.active.lock().unwrap_or_else(|error| error.into_inner()).remove(&self.id);
    }
}

/// Owns an accept thread and at most eight connection workers. Drop wakes its
/// private socket pair and pending reply waits, closes streams, and joins workers.
pub struct ControlServer {
    socket_path: PathBuf,
    wake: UnixStream,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    socket_identity: (u64, u64),
}

impl ControlServer {
    pub fn start_in<F>(directory: PathBuf, dispatch: F) -> io::Result<Self>
    where
        F: Fn(ControlEvent) -> Result<(), ()> + Send + Sync + 'static,
    {
        let _directory = ensure_private_directory(&directory)?;
        let instance_id = random_instance_id()?;
        let socket_path = directory.join(format!("{SOCKET_PREFIX}{instance_id}.sock"));
        check_socket_path_length(&socket_path)?;
        // A random name must never replace an existing entry, including a symlink.
        match fs::symlink_metadata(&socket_path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "socket already exists"));
            }
        }
        let (wake, accept_wake) = UnixStream::pair()?;
        let listener = UnixListener::bind(&socket_path)?;
        let setup = (|| {
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
            // Readiness is multiplexed with the private wake socket. A spurious
            // listener readiness notification must never block a subsequent accept.
            listener.set_nonblocking(true)?;
            fs::symlink_metadata(&socket_path)
        })();
        let metadata = match setup {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                return Err(error);
            }
        };
        let socket_identity = (metadata.dev(), metadata.ino());
        let shared = Arc::new(Shared {
            stopping: AtomicBool::new(false),
            next_connection: AtomicU64::new(0),
            active: Mutex::new(BTreeMap::new()),
            instance_id,
        });
        let accept_shared = shared.clone();
        let thread = thread::Builder::new().name("mechanic-control".to_owned()).spawn(move || {
            accept_loop(listener, accept_wake, accept_shared, Arc::new(dispatch));
        });
        match thread {
            Ok(thread) => {
                Ok(Self { socket_path, wake, shared, thread: Some(thread), socket_identity })
            }
            Err(error) => {
                let _ = fs::remove_file(&socket_path);
                Err(error)
            }
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.shared.instance_id
    }

    // The normal GUI uses discovery; standalone service examples inspect theirs.
    #[allow(dead_code)]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shared.stopping.store(true, Ordering::Release);
        {
            let active = self.shared.active.lock().unwrap_or_else(|error| error.into_inner());
            for connection in active.values() {
                if let Some(reply) = &connection.reply {
                    let _ = reply.try_send(Response::error(
                        self.instance_id(),
                        ErrorCode::Closed,
                        "application control server stopped",
                    ));
                }
                let _ = connection.stream.shutdown(std::net::Shutdown::Both);
            }
        }
        // The peer sees EOF even if the public socket or directory was unlinked.
        // This descriptor is private, so waking cannot depend on filesystem state.
        let _ = self.wake.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Ok(metadata) = fs::symlink_metadata(&self.socket_path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.socket_identity
        {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

fn accept_loop<F>(listener: UnixListener, wake: UnixStream, shared: Arc<Shared>, dispatch: Arc<F>)
where
    F: Fn(ControlEvent) -> Result<(), ()> + Send + Sync + 'static,
{
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while let Ok(Some(mut stream)) = accept_until_woken(&listener, &wake) {
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        if peer_uid(&stream).ok() != Some(effective_uid()) {
            continue;
        }
        let id = shared.next_connection.fetch_add(1, Ordering::Relaxed);
        {
            let mut active = shared.active.lock().unwrap_or_else(|error| error.into_inner());
            if shared.stopping.load(Ordering::Acquire) {
                break;
            }
            if active.len() >= MAX_CONNECTIONS {
                drop(active);
                let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
                let _ = write_response(
                    &mut stream,
                    &Response::error(
                        &shared.instance_id,
                        ErrorCode::Busy,
                        "control server is busy",
                    ),
                );
                continue;
            }
            let Ok(clone) = stream.try_clone() else { continue };
            active.insert(id, ActiveConnection { stream: clone, reply: None });
        }
        let worker_shared = shared.clone();
        let worker_dispatch = dispatch.clone();
        match thread::Builder::new().name("mechanic-control-request".to_owned()).spawn(move || {
            let _connection = ConnectionGuard { id, shared: worker_shared.clone() };
            let response = handle_connection(&mut stream, id, &worker_shared, &*worker_dispatch);
            let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = write_response(&mut stream, &response);
        }) {
            Ok(worker) => workers.push(worker),
            Err(_) => {
                shared.active.lock().unwrap_or_else(|error| error.into_inner()).remove(&id);
            }
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
}

/// Block until a connection or shutdown, with no idle timeout or periodic wake.
fn accept_until_woken(
    listener: &UnixListener,
    wake: &UnixStream,
) -> io::Result<Option<UnixStream>> {
    loop {
        let mut descriptors = [
            libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: descriptors contains two live descriptors and writable pollfd
        // storage. A negative timeout blocks until a descriptor event or signal.
        let status =
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as libc::nfds_t, -1) };
        if status < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if descriptors[1].revents != 0 {
            return Ok(None);
        }
        if descriptors[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "control listener closed"));
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            match listener.accept() {
                Ok((stream, _)) => {
                    // macOS can inherit the listener's nonblocking flag.
                    stream.set_nonblocking(false)?;
                    return Ok(Some(stream));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

fn handle_connection<F>(stream: &mut UnixStream, id: u64, shared: &Shared, dispatch: &F) -> Response
where
    F: Fn(ControlEvent) -> Result<(), ()>,
{
    let error = |code, message| Response::error(&shared.instance_id, code, message);
    let started = Instant::now();
    let frame = match read_frame(stream, MAX_REQUEST_BYTES, started + REQUEST_TIMEOUT) {
        Ok(frame) => frame,
        Err(_) => {
            return error(ErrorCode::InvalidRequest, "request missing, oversized, or expired");
        }
    };
    let request: Request = match serde_json::from_slice(&frame) {
        Ok(request) => request,
        Err(_) => return error(ErrorCode::InvalidRequest, "invalid JSON request"),
    };
    if let Err((code, message)) = request.validate(&shared.instance_id) {
        return error(code, message);
    }
    if matches!(request.operation, Operation::Ping) {
        return Response::new(&shared.instance_id, ResponseResult::Pong);
    }
    let deadline = started + request.operation.timeout();
    let (reply, receive) = mpsc::sync_channel(1);
    {
        let mut active = shared.active.lock().unwrap_or_else(|error| error.into_inner());
        if shared.stopping.load(Ordering::Acquire) {
            return error(ErrorCode::Closed, "application control server stopped");
        }
        if let Some(connection) = active.get_mut(&id) {
            connection.reply = Some(reply.clone());
        }
    }
    if dispatch(ControlEvent { request, reply, deadline }).is_err() {
        return error(ErrorCode::Closed, "application event loop closed");
    }
    match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(response) => response,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            error(ErrorCode::Deadline, "application reply expired")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            error(ErrorCode::Closed, "application reply closed")
        }
    }
}

fn write_response(stream: &mut UnixStream, response: &Response) -> io::Result<()> {
    let mut bytes = match serialize_bounded(response, MAX_REPLY_BYTES) {
        Ok(bytes) => bytes,
        Err(_) => serialize_bounded(
            &Response::error(
                &response.instance_id,
                ErrorCode::Internal,
                "application response exceeds allowed byte limit",
            ),
            MAX_REPLY_BYTES,
        )?,
    };
    bytes.push(b'\n');
    let timeout = stream.write_timeout()?.unwrap_or(WRITE_TIMEOUT);
    write_frame(stream, &bytes, Instant::now() + timeout)
}

fn write_frame(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "JSON write expired"));
        }
        stream.set_write_timeout(Some(remaining))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "JSON write closed")),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn serialize_bounded(value: &impl Serialize, maximum: usize) -> io::Result<Vec<u8>> {
    struct Writer {
        bytes: Vec<u8>,
        maximum: usize,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "JSON exceeds byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer { bytes: Vec::new(), maximum };
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.bytes)
}

fn read_frame(stream: &mut UnixStream, maximum: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "JSON frame expired"));
        }
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "JSON frame needs a newline"));
        }
        if let Some(end) = buffer[..count].iter().position(|byte| *byte == b'\n') {
            if bytes.len() + end > maximum || end + 1 != count {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid JSON frame length",
                ));
            }
            bytes.extend_from_slice(&buffer[..end]);
            return Ok(bytes);
        }
        if bytes.len() + count > maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON frame exceeds byte limit",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

/// Send one bounded request without initializing any GUI infrastructure.
pub fn request(socket: &Path, request: &Request) -> io::Result<Response> {
    check_socket_path_length(socket)?;
    validate_private_directory(socket.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "socket needs a parent directory")
    })?)?;
    validate_socket(socket)?;
    // Connect must also have a deadline: a full Unix listen backlog can block.
    let timeout =
        request.operation.timeout().min(Duration::from_millis(MAX_WAIT_MS) + WAIT_ALLOWANCE);
    let deadline = Instant::now() + timeout;
    let mut stream = connect_with_deadline(socket, deadline.min(Instant::now() + WRITE_TIMEOUT))?;
    if peer_uid(&stream)? != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket peer has another owner",
        ));
    }
    let mut bytes = serialize_bounded(request, MAX_REQUEST_BYTES)?;
    // Limit timeout before sending, including requests that a server would reject.
    stream.set_write_timeout(Some(WRITE_TIMEOUT.min(timeout)))?;
    bytes.push(b'\n');
    write_frame(&mut stream, &bytes, deadline.min(Instant::now() + WRITE_TIMEOUT))?;
    let frame = read_frame(&mut stream, MAX_REPLY_BYTES, deadline)?;
    let response: Response = serde_json::from_slice(&frame)?;
    if response.version != PROTOCOL_VERSION
        || !valid_instance_id(&response.instance_id)
        || (!matches!(
            response.result,
            ResponseResult::Error { code: ErrorCode::StaleInstance, .. }
        ) && (request.instance_id.as_deref().is_some_and(|id| id != response.instance_id)
            || request
                .operation
                .session()
                .is_some_and(|session| session.instance_id != response.instance_id)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response has an unexpected identity or version",
        ));
    }
    Ok(response)
}

pub fn runtime_directory() -> io::Result<PathBuf> {
    if let Some(directory) = std::env::var_os("XDG_RUNTIME_DIR") {
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XDG_RUNTIME_DIR must be absolute",
            ));
        }
        validate_private_directory(&directory)?;
        Ok(directory.join("mechanic"))
    } else {
        Ok(PathBuf::from("/tmp").join(format!("mechanic-{}", effective_uid())))
    }
}

pub fn discover_endpoints() -> io::Result<Vec<Endpoint>> {
    discover_endpoints_in(&runtime_directory()?)
}

pub fn discover_endpoints_in(directory: &Path) -> io::Result<Vec<Endpoint>> {
    match validate_private_directory(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => {
            result?;
        }
    }
    let mut endpoints = Vec::new();
    let mut candidates = 0;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(instance_id) = name
            .to_str()
            .and_then(|name| name.strip_prefix(SOCKET_PREFIX))
            .and_then(|name| name.strip_suffix(".sock"))
        else {
            continue;
        };
        if !valid_instance_id(instance_id) {
            continue;
        }
        candidates += 1;
        if candidates > 256 || Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control endpoint discovery exceeds limits",
            ));
        }
        let socket_path = entry.path();
        check_socket_path_length(&socket_path)?;
        validate_socket(&socket_path)?;
        let probe =
            request(&socket_path, &Request::new(Some(instance_id.to_owned()), Operation::Ping));
        match probe {
            Ok(response)
                if matches!(
                    response.result,
                    ResponseResult::Pong | ResponseResult::Error { code: ErrorCode::Busy, .. }
                ) =>
            {
                endpoints.push(Endpoint { socket_path, instance_id: instance_id.to_owned() });
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
            Ok(_) => continue,
        }
        if endpoints.len() > 256 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too many control endpoints"));
        }
    }
    endpoints.sort_by(|left, right| left.socket_path.cmp(&right.socket_path));
    Ok(endpoints)
}

fn connect_with_deadline(path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    check_socket_path_length(path)?;
    // SAFETY: socket creates a new owned descriptor without pointer arguments.
    let descriptor = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful socket call returned a new descriptor we own.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    // SAFETY: fcntl changes flags on this live, exclusively owned descriptor.
    if unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: all-zero sockaddr_un is valid storage before fields are filled.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, source) in address.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
        *target = *source as libc::c_char;
    }
    let length =
        std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.as_os_str().as_bytes().len() + 1;
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        address.sun_len = length as u8;
    }
    // SAFETY: address points to initialized sockaddr_un storage of length bytes.
    let status = unsafe {
        libc::connect(
            descriptor.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length as libc::socklen_t,
        )
    };
    if status != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(error);
        }
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "socket connection expired"));
            }
            let mut poll =
                libc::pollfd { fd: descriptor.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
            let timeout = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
            // SAFETY: poll points to one initialized writable pollfd.
            let status = unsafe { libc::poll(&mut poll, 1, timeout) };
            if status < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if status == 0 {
                continue;
            }
            let mut socket_error: libc::c_int = 0;
            let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: socket_error and length are writable storage of the given size.
            if unsafe {
                libc::getsockopt(
                    descriptor.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut socket_error as *mut libc::c_int).cast(),
                    &mut length,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if socket_error != 0 {
                return Err(io::Error::from_raw_os_error(socket_error));
            }
            break;
        }
    }
    let stream = UnixStream::from(descriptor);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no pointer arguments or preconditions.
    unsafe { libc::geteuid() }
}

fn valid_instance_id(value: &str) -> bool {
    value.len() == 32
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn random_instance_id() -> io::Result<String> {
    let mut random = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn check_socket_path_length(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    const MAXIMUM: usize = 103;
    #[cfg(not(target_os = "macos"))]
    const MAXIMUM: usize = 107;
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute() || bytes.len() > MAXIMUM || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unix socket path must be absolute and fit sockaddr_un",
        ));
    }
    Ok(())
}

fn validate_private_directory(path: &Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control directory must be an owned 0700 directory",
        ));
    }
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let opened = directory.metadata()?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control directory changed during validation",
        ));
    }
    Ok(directory)
}

fn ensure_private_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control directory must be absolute",
        ));
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    validate_private_directory(path)
}

fn validate_socket(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control endpoint must be an owned 0600 socket",
        ));
    }
    Ok(())
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: stream is live and uid/gid point to initialized writable values.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut credentials = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials and length are writable buffers of the advertised size.
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if status == 0 && length as usize == std::mem::size_of::<libc::ucred>() {
        Ok(credentials.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
fn peer_uid(_stream: &UnixStream) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "peer authentication is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> tempfile::TempDir {
        let directory = tempfile::Builder::new().prefix("mctl-").tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn server(directory: &Path) -> ControlServer {
        ControlServer::start_in(directory.to_owned(), |event| {
            event
                .reply
                .send(Response::new(
                    event.request.instance_id.unwrap_or_default(),
                    ResponseResult::Panes { panes: Vec::new() },
                ))
                .map_err(|_| ())
        })
        .unwrap()
    }

    fn raw(socket: &Path, bytes: &[u8]) -> Response {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(bytes).unwrap();
        let bytes =
            read_frame(&mut stream, MAX_REPLY_BYTES, Instant::now() + REQUEST_TIMEOUT).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn actual_socket_authentication_identity_discovery_and_shutdown() {
        let directory = directory();
        let server = server(directory.path());
        let endpoint = discover_endpoints_in(directory.path()).unwrap().pop().unwrap();
        assert_eq!(endpoint.instance_id, server.instance_id());
        assert_eq!(endpoint.socket_path, server.socket_path());
        assert_eq!(fs::metadata(server.socket_path()).unwrap().mode() & 0o777, 0o600);
        let response = request(server.socket_path(), &Request::new(None, Operation::Ping)).unwrap();
        assert!(matches!(response.result, ResponseResult::Pong));
        assert_eq!(response.instance_id, server.instance_id());
        let stream = UnixStream::connect(server.socket_path()).unwrap();
        assert_eq!(peer_uid(&stream).unwrap(), effective_uid());
        let socket = server.socket_path().to_owned();
        drop(server);
        assert!(!socket.exists());
    }

    fn assert_shutdown_after_endpoint_removal(remove_directory: bool) {
        let directory = directory();
        let server = server(directory.path());
        // Exercise the listener before removal so this tests its idle wait too.
        let response = request(server.socket_path(), &Request::new(None, Operation::Ping)).unwrap();
        assert!(matches!(response.result, ResponseResult::Pong));
        let cleanup_wake = server.wake.try_clone().unwrap();
        if remove_directory {
            fs::remove_dir_all(directory.path()).unwrap();
        } else {
            fs::remove_file(server.socket_path()).unwrap();
        }
        let (finished, completion) = mpsc::channel();
        let worker = thread::spawn(move || {
            drop(server);
            let _ = finished.send(());
        });
        let completed = completion.recv_timeout(REQUEST_TIMEOUT).is_ok();
        // Always wake and join before asserting: a missing wake in Drop must not
        // leave a regression test's server thread alive after a timeout failure.
        let _ = cleanup_wake.shutdown(std::net::Shutdown::Both);
        worker.join().unwrap();
        assert!(completed, "control shutdown blocked after its endpoint was removed");
    }

    #[test]
    fn shutdown_survives_socket_unlink() {
        assert_shutdown_after_endpoint_removal(false);
    }

    #[test]
    fn shutdown_survives_runtime_directory_removal() {
        assert_shutdown_after_endpoint_removal(true);
    }

    #[test]
    fn stale_handles_and_oversized_input_never_reach_the_app() {
        let directory = directory();
        let server = ControlServer::start_in(directory.path().to_owned(), |_| {
            panic!("invalid request dispatched")
        })
        .unwrap();
        let mut selector = SessionSelector { instance_id: "0".repeat(32), session_id: 1 };
        let invalid = Request::new(
            None,
            Operation::SendInput {
                session: selector.clone(),
                text: "x".to_owned(),
                mode: InputMode::Paste,
                enter: false,
            },
        );
        let mut bytes = serde_json::to_vec(&invalid).unwrap();
        bytes.push(b'\n');
        assert!(matches!(
            raw(server.socket_path(), &bytes).result,
            ResponseResult::Error { code: ErrorCode::StaleInstance, .. }
        ));
        selector.instance_id = server.instance_id().to_owned();
        let invalid = Request::new(
            None,
            Operation::SendInput {
                session: selector,
                text: "x".repeat(MAX_TEXT_BYTES + 1),
                mode: InputMode::Paste,
                enter: false,
            },
        );
        let mut bytes = serde_json::to_vec(&invalid).unwrap();
        bytes.push(b'\n');
        assert!(matches!(
            raw(server.socket_path(), &bytes).result,
            ResponseResult::Error { code: ErrorCode::InvalidRequest, .. }
        ));
    }

    #[test]
    fn malformed_and_overlong_frames_are_bounded() {
        let directory = directory();
        let server = server(directory.path());
        assert!(matches!(
            raw(server.socket_path(), b"{broken}\n").result,
            ResponseResult::Error { code: ErrorCode::InvalidRequest, .. }
        ));
        let mut stream = UnixStream::connect(server.socket_path()).unwrap();
        // The worker closes after this cap; tolerate the writer observing closure.
        let _ = stream.write_all(&vec![b'x'; MAX_REQUEST_BYTES + 1]);
        let frame =
            read_frame(&mut stream, MAX_REPLY_BYTES, Instant::now() + REQUEST_TIMEOUT).unwrap();
        let response: Response = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(
            response.result,
            ResponseResult::Error { code: ErrorCode::InvalidRequest, .. }
        ));
    }

    #[test]
    fn outstanding_gui_calls_are_bounded_and_stop_wakes_waiters() {
        let directory = directory();
        let (events, received) = mpsc::channel();
        let server = ControlServer::start_in(directory.path().to_owned(), move |event| {
            events.send(event).map_err(|_| ())
        })
        .unwrap();
        let request = Request::new(
            Some(server.instance_id().to_owned()),
            Operation::Wait {
                session: SessionSelector {
                    instance_id: server.instance_id().to_owned(),
                    session_id: 1,
                },
                after_command_id: 0,
                timeout_ms: MAX_WAIT_MS,
            },
        );
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        let mut clients = Vec::new();
        let mut pending = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let mut stream = UnixStream::connect(server.socket_path()).unwrap();
            stream.write_all(&bytes).unwrap();
            pending.push(received.recv_timeout(REQUEST_TIMEOUT).unwrap());
            clients.push(stream);
        }
        assert!(matches!(
            raw(server.socket_path(), &bytes).result,
            ResponseResult::Error { code: ErrorCode::Busy, .. }
        ));
        assert!(received.try_recv().is_err());
        drop(server);
        for mut client in clients {
            let mut byte = [0_u8; 1];
            assert_eq!(client.read(&mut byte).unwrap(), 0);
        }
        drop(pending);
    }

    #[test]
    fn rejects_unsafe_directories_socket_symlinks_and_long_paths() {
        let directory = directory();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ControlServer::start_in(directory.path().to_owned(), |_| Ok(())).is_err());
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(directory.path(), &link).unwrap();
        assert!(ControlServer::start_in(link, |_| Ok(())).is_err());
        let bad_socket = directory.path().join(format!("ctl-{}.sock", "a".repeat(32)));
        std::os::unix::fs::symlink("/tmp/absent", &bad_socket).unwrap();
        assert!(discover_endpoints_in(directory.path()).is_err());
        assert!(
            check_socket_path_length(&PathBuf::from(format!("/tmp/{}", "a".repeat(200)))).is_err()
        );
    }

    #[test]
    fn handles_are_opaque_and_input_defaults_to_paste_without_enter() {
        assert!("12".parse::<SessionSelector>().is_err());
        let value = format!("{}:42", "a".repeat(32));
        assert_eq!(value.parse::<SessionSelector>().unwrap().to_string(), value);
        let wire = format!(
            r#"{{"version":1,"op":"send_input","session":{{"instance_id":"{}","session_id":1}},"text":"echo ok"}}"#,
            "a".repeat(32)
        );
        let request: Request = serde_json::from_str(&wire).unwrap();
        assert!(matches!(
            request.operation,
            Operation::SendInput { mode: InputMode::Paste, enter: false, .. }
        ));
    }

    #[test]
    fn discovery_sorts_multiple_instances_and_ignores_dead_sockets() {
        let directory = directory();
        let first = server(directory.path());
        let second = server(directory.path());
        assert_ne!(first.instance_id(), second.instance_id());
        let stale = directory.path().join(format!("ctl-{}.sock", "0".repeat(32)));
        let listener = UnixListener::bind(&stale).unwrap();
        fs::set_permissions(&stale, fs::Permissions::from_mode(0o600)).unwrap();
        drop(listener);
        let endpoints = discover_endpoints_in(directory.path()).unwrap();
        assert_eq!(endpoints.len(), 2);
        assert!(endpoints[0].socket_path < endpoints[1].socket_path);
        assert!(endpoints.iter().all(|endpoint| endpoint.socket_path != stale));
    }

    #[test]
    fn large_application_replies_are_replaced_by_a_bounded_error() {
        let directory = directory();
        let server = ControlServer::start_in(directory.path().to_owned(), |event| {
            let instance = event.request.instance_id.unwrap();
            event
                .reply
                .send(Response::new(
                    &instance,
                    ResponseResult::Output {
                        session: SessionSelector { instance_id: instance.clone(), session_id: 1 },
                        text: "x".repeat(MAX_REPLY_BYTES),
                        truncated: false,
                    },
                ))
                .map_err(|_| ())
        })
        .unwrap();
        let response = request(
            server.socket_path(),
            &Request::new(Some(server.instance_id().to_owned()), Operation::ListPanes),
        )
        .unwrap();
        assert!(matches!(response.result, ResponseResult::Error { code: ErrorCode::Internal, .. }));
        assert!(serialize_bounded(&"x".repeat(100), 10).is_err());
    }

    #[test]
    fn validated_requests_reach_the_mock_app_without_interpreting_text() {
        let directory = directory();
        let (sent, received) = mpsc::channel();
        let server = ControlServer::start_in(directory.path().to_owned(), move |event| {
            let Operation::SendInput { session, text, mode, enter } = event.request.operation
            else {
                panic!("unexpected operation")
            };
            sent.send((text, mode, enter)).unwrap();
            event
                .reply
                .send(Response::new(
                    &session.instance_id,
                    ResponseResult::InputSent { session: session.clone() },
                ))
                .map_err(|_| ())
        })
        .unwrap();
        let text = "literal $() `quotes` ; & |\n日本語 café\u{301}";
        let selector =
            SessionSelector { instance_id: server.instance_id().to_owned(), session_id: 9 };
        let response = request(
            server.socket_path(),
            &Request::new(
                None,
                Operation::SendInput {
                    session: selector,
                    text: text.to_owned(),
                    mode: InputMode::Raw,
                    enter: false,
                },
            ),
        )
        .unwrap();
        assert!(matches!(response.result, ResponseResult::InputSent { .. }));
        assert_eq!(received.recv().unwrap(), (text.to_owned(), InputMode::Raw, false));
    }

    #[test]
    fn expired_io_deadlines_do_not_block() {
        let (mut first, _second) = UnixStream::pair().unwrap();
        assert_eq!(
            read_frame(&mut first, 10, Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            write_frame(&mut first, b"text", Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }
}
