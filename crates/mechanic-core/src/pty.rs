//! Nonblocking PTY transport with ordered, bounded writes.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::ExitStatus;
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{
    self, ChildEvent, EventedPty as _, EventedReadWrite as _, Options, Shell,
};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use mechanic_config::Config;
use polling::{Event, Events, PollMode, Poller};

use crate::{PtyWaker, TerminalSize, error::TerminalError};

const READ_BUF_SIZE: usize = 64 * 1024;
const OUTPUT_CHUNKS: usize = 64;
const OUTPUT_BYTE_LIMIT: usize = OUTPUT_CHUNKS * READ_BUF_SIZE;
const WRITE_MESSAGES: usize = 1024;
const IO_BUDGET: usize = 1024 * 1024;
/// Includes queued and partially written payloads. A paste is accepted whole or rejected.
const WRITE_BYTE_LIMIT: usize = 8 * 1024 * 1024;
static WORKER_ID: AtomicU64 = AtomicU64::new(0);
// Concurrent PTY creation can fail inside macOS openpty; serialize setup only.
pub(crate) static SPAWN_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct State {
    wake_pending: AtomicBool,
    cancelled: AtomicBool,
    closed: AtomicBool,
    /// The worker cannot enqueue any more output, even if child cleanup continues.
    output_done: AtomicBool,
    queued_bytes: AtomicUsize,
    failure: Mutex<Option<io::Error>>,
    failure_pending: AtomicBool,
}

impl State {
    fn wake_app(&self, waker: &PtyWaker) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            waker();
        }
    }

    fn fail(&self, error: io::Error, waker: &PtyWaker) {
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_some() {
            return;
        }
        log::error!("PTY transport error: {error}");
        *failure = Some(error);
        drop(failure);
        self.closed.store(true, Ordering::Release);
        self.failure_pending.store(true, Ordering::Release);
        self.wake_app(waker);
    }

    fn error(&self) -> io::Error {
        let failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        match failure.as_ref() {
            Some(error) => io::Error::new(error.kind(), error.to_string()),
            None => io::Error::new(io::ErrorKind::BrokenPipe, "PTY transport closed"),
        }
    }
}

struct PendingWrite {
    bytes: Vec<u8>,
    offset: usize,
    state: Arc<State>,
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        self.state.queued_bytes.fetch_sub(self.bytes.len(), Ordering::AcqRel);
    }
}

#[derive(Default)]
struct OutputBuffer {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
}

/// Byte-bounded output with partial-tail coalescing. Even repeated one-byte
/// reads consume the full byte capacity rather than one channel slot each.
#[derive(Default)]
pub(crate) struct OutputQueue {
    buffer: Mutex<OutputBuffer>,
    #[cfg(test)]
    ready: Condvar,
}

impl OutputQueue {
    fn try_send(&self, bytes: Vec<u8>) -> Result<(), TrySendError<Vec<u8>>> {
        let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        if bytes.len() > OUTPUT_BYTE_LIMIT - buffer.bytes {
            return Err(TrySendError::Full(bytes));
        }
        let mut remaining = bytes.as_slice();
        if let Some(tail) = buffer.chunks.back_mut() {
            let length = remaining.len().min(READ_BUF_SIZE - tail.len());
            tail.extend_from_slice(&remaining[..length]);
            remaining = &remaining[length..];
        }
        for part in remaining.chunks(READ_BUF_SIZE) {
            let mut chunk = Vec::with_capacity(READ_BUF_SIZE);
            chunk.extend_from_slice(part);
            buffer.chunks.push_back(chunk);
        }
        buffer.bytes += bytes.len();
        drop(buffer);
        #[cfg(test)]
        self.ready.notify_one();
        Ok(())
    }

    pub(crate) fn try_recv(&self) -> Result<Vec<u8>, crossbeam_channel::TryRecvError> {
        let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = buffer.chunks.pop_front().ok_or(crossbeam_channel::TryRecvError::Empty)?;
        buffer.bytes -= bytes.len();
        Ok(bytes)
    }

    #[cfg(test)]
    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Vec<u8>, crossbeam_channel::RecvTimeoutError> {
        let buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let (mut buffer, _) = self
            .ready
            .wait_timeout_while(buffer, timeout, |b| b.chunks.is_empty())
            .unwrap_or_else(|e| e.into_inner());
        let bytes =
            buffer.chunks.pop_front().ok_or(crossbeam_channel::RecvTimeoutError::Timeout)?;
        buffer.bytes -= bytes.len();
        Ok(bytes)
    }

    #[cfg(test)]
    fn is_full(&self) -> bool {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner()).bytes == OUTPUT_BYTE_LIMIT
    }
}

/// Queued input, output channels, and a descriptor used only for resizing.
pub struct PtyHandle {
    resize_fd: Option<File>,
    writes: Sender<PendingWrite>,
    state: Arc<State>,
    poller: Arc<Poller>,
    waker: PtyWaker,
    pub(crate) rx: Arc<OutputQueue>,
    pub(crate) exit_rx: Receiver<Option<ExitStatus>>,
    // Cleanup runs on the worker; dropping a window never waits for a child.
    _worker: Option<JoinHandle<()>>,
    _shell_integration: Option<tempfile::TempDir>,
}

impl PtyHandle {
    pub fn spawn(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
    ) -> Result<Self, TerminalError> {
        Self::spawn_in_directory(config, size, waker, None)
    }

    /// Start only the PTY child in `directory`, leaving the application's cwd alone.
    /// The Unix PTY launcher ignores a failed child chdir, including a directory
    /// removed between this validation and spawning.
    pub fn spawn_in_directory(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
        directory: Option<&Path>,
    ) -> Result<Self, TerminalError> {
        let poller = Arc::new(Poller::new().map_err(TerminalError::Io)?);
        let mut options = Options {
            working_directory: directory
                .filter(|path| path.is_absolute() && path.is_dir())
                .map(Path::to_path_buf),
            ..Options::default()
        };
        if !config.shell.program.is_empty() {
            options.shell = Some(Shell::new(config.shell.program.clone(), vec![]));
        }
        let shell_integration = crate::shell_integration::configure(&config.shell, &mut options)
            .map_err(TerminalError::Io)?;
        let pty = {
            let _spawn = SPAWN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            tty::setup_env();
            tty::new(&options, size.to_window_size(), 0).map_err(TerminalError::PtySpawn)?
        };
        // alacritty opens the master nonblocking. Its duplicate shares that flag.
        let resize_fd = pty.file().try_clone().map_err(TerminalError::Io)?;
        let (writes, incoming) = bounded(WRITE_MESSAGES);
        let rx = Arc::new(OutputQueue::default());
        let tx = Arc::clone(&rx);
        let (exit_tx, exit_rx) = bounded(1);
        let state = Arc::new(State::default());
        let worker_state = Arc::clone(&state);
        let worker_poller = Arc::clone(&poller);
        let worker_waker = Arc::clone(&waker);
        let id = WORKER_ID.fetch_add(1, Ordering::Relaxed);
        let worker = thread::Builder::new()
            .name(format!("mechanic-pty-{id}"))
            .spawn(move || {
                worker(pty, incoming, tx, exit_tx, worker_state, worker_poller, worker_waker);
            })
            .map_err(TerminalError::Io)?;
        Ok(Self {
            resize_fd: Some(resize_fd),
            writes,
            state,
            poller,
            waker,
            rx,
            exit_rx,
            _worker: Some(worker),
            _shell_integration: shell_integration,
        })
    }

    /// Queue all bytes in order. WouldBlock rejects the entire payload when full.
    /// Success means accepted by the transport, not consumed by the child.
    pub fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        if self.state.closed.load(Ordering::Acquire) {
            return Err(TerminalError::Io(self.state.error()));
        }
        if data.is_empty() {
            return Ok(());
        }
        self.state
            .queued_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(data.len()).filter(|&size| size <= WRITE_BYTE_LIMIT)
            })
            .map_err(|_| {
                TerminalError::Io(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "PTY input queue exceeds 8 MiB",
                ))
            })?;
        let request =
            PendingWrite { bytes: data.to_vec(), offset: 0, state: Arc::clone(&self.state) };
        self.writes.try_send(request).map_err(|e| {
            TerminalError::Io(match e {
                TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "PTY input queue is full")
                }
                TrySendError::Disconnected(_) => self.state.error(),
            })
        })?;
        self.notify();
        Ok(())
    }

    /// Resume reads after the application frees output-channel capacity.
    pub(crate) fn output_drained(&self) {
        self.notify();
    }

    /// Rearm before checking output/status so concurrent publications cannot lose a wake.
    pub(crate) fn begin_input_turn(&self) {
        self.state.wake_pending.swap(false, Ordering::AcqRel);
    }

    fn notify(&self) {
        if let Err(error) = self.poller.notify() {
            // Already accepted bytes must not be reported as an enqueue rejection.
            self.state.cancelled.store(true, Ordering::Release);
            self.state.fail(error, &self.waker);
        }
    }

    pub(crate) fn take_failure(&self) -> Option<String> {
        self.state
            .failure_pending
            .swap(false, Ordering::AcqRel)
            .then(|| self.state.error().to_string())
    }

    pub(crate) fn output_done(&self) -> bool {
        self.state.output_done.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn test_pair(waker: PtyWaker) -> (Self, TestPtyPeer) {
        let (writes, incoming) = bounded(WRITE_MESSAGES);
        let (exit_tx, exit_rx) = bounded(1);
        let state = Arc::new(State::default());
        let rx = Arc::new(OutputQueue::default());
        let peer = TestPtyPeer {
            output: Arc::clone(&rx),
            state: Arc::clone(&state),
            exit: exit_tx,
            incoming,
            waker: Arc::clone(&waker),
        };
        let handle = Self {
            resize_fd: None,
            writes,
            state,
            poller: Arc::new(Poller::new().unwrap()),
            waker,
            rx,
            exit_rx,
            _worker: None,
            _shell_integration: None,
        };
        (handle, peer)
    }

    pub(crate) fn writer_fd(&self) -> std::os::fd::RawFd {
        self.resize_fd.as_ref().expect("live PTY descriptor").as_raw_fd()
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        self.state.cancelled.store(true, Ordering::Release);
        self.resize_fd.take();
        let _ = self.poller.notify();
    }
}

struct IoLoop {
    pending_write: Option<PendingWrite>,
    pending_output: Option<Vec<u8>>,
    child_status: Option<Option<ExitStatus>>,
    post_exit_remaining: usize,
    eof: bool,
    input_closed: bool,
    master_active: bool,
}

fn worker(
    mut pty: tty::Pty,
    incoming: Receiver<PendingWrite>,
    output: Arc<OutputQueue>,
    exit: Sender<Option<ExitStatus>>,
    state: Arc<State>,
    poller: Arc<Poller>,
    waker: PtyWaker,
) {
    let mut io = IoLoop {
        pending_write: None,
        pending_output: None,
        child_status: None,
        post_exit_remaining: IO_BUDGET,
        eof: false,
        input_closed: false,
        master_active: true,
    };
    // SAFETY: sources live until deregistration below. The signal pipe remains
    // registered even while output backpressure suspends the master descriptor.
    let registered = unsafe { pty.register(&poller, Event::readable(0), PollMode::Oneshot) };
    let result = match registered {
        Ok(()) => io.run(&mut pty, &incoming, &output, &exit, &state, &poller, &waker),
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        state.fail(error, &waker);
    }
    state.closed.store(true, Ordering::Release);
    // Publish completion before potentially slow child cleanup. A UI-side
    // notify failure can arrive before this point, so wake again once its
    // preceding output can safely be considered complete.
    state.output_done.store(true, Ordering::Release);
    state.wake_app(&waker);
    let _ = pty.deregister(&poller);
    drop(io.pending_write.take());
    drop(incoming);
    cleanup_child(&mut pty, io.child_status.is_some());
}

#[cfg(test)]
pub(crate) struct TestPtyPeer {
    output: Arc<OutputQueue>,
    state: Arc<State>,
    exit: Sender<Option<ExitStatus>>,
    incoming: Receiver<PendingWrite>,
    waker: PtyWaker,
}

#[cfg(test)]
impl TestPtyPeer {
    pub(crate) fn send(&self, bytes: Vec<u8>) {
        self.output.try_send(bytes).unwrap();
        self.state.wake_app(&self.waker);
    }

    pub(crate) fn exit(&self, status: Option<ExitStatus>) {
        self.publish_exit(status);
        self.finish();
    }

    pub(crate) fn publish_exit(&self, status: Option<ExitStatus>) {
        self.exit.try_send(status).unwrap();
        self.state.wake_app(&self.waker);
    }

    pub(crate) fn fail(&self) {
        self.state.fail(io::Error::other("test transport failure"), &self.waker);
    }

    pub(crate) fn finish(&self) {
        self.state.output_done.store(true, Ordering::Release);
        self.state.wake_app(&self.waker);
    }

    pub(crate) fn reply(&self) -> Vec<u8> {
        self.incoming.try_recv().unwrap().bytes.clone()
    }
}

impl IoLoop {
    fn record_read(&mut self, bytes: usize) {
        if self.child_status.is_some() {
            self.post_exit_remaining -= bytes;
            // The turn budget can end on this exact boundary. Mark EOF now
            // rather than waiting for another readable event to check the cap.
            self.eof |= self.post_exit_remaining == 0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        pty: &mut tty::Pty,
        incoming: &Receiver<PendingWrite>,
        output: &OutputQueue,
        exit: &Sender<Option<ExitStatus>>,
        state: &State,
        poller: &Arc<Poller>,
        waker: &PtyWaker,
    ) -> io::Result<()> {
        let mut events = Events::new();
        let mut buffer = vec![0; READ_BUF_SIZE];
        loop {
            if state.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            // Keep draining level-triggered signal notifications after exit.
            if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
                self.child_status.get_or_insert(status);
                state.closed.store(true, Ordering::Release);
                self.input_closed = true;
                self.pending_write = None;
            }

            let mut produced_output = false;
            if let Some(bytes) = self.pending_output.take() {
                match output.try_send(bytes) {
                    Ok(()) => produced_output = true,
                    Err(TrySendError::Full(bytes)) => self.pending_output = Some(bytes),
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                }
            }
            let mut read_bytes = 0;
            while !self.eof && self.pending_output.is_none() && read_bytes < IO_BUDGET {
                if state.cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                // Bound final draining so surviving descendants cannot keep a
                // dead shell's window alive by continuously writing output.
                let read_limit = if self.child_status.is_some() {
                    if self.post_exit_remaining == 0 {
                        self.eof = true;
                        break;
                    }
                    buffer.len().min(self.post_exit_remaining)
                } else {
                    buffer.len()
                }
                .min(IO_BUDGET - read_bytes);
                // Batch available reads to reduce output-queue locking.
                let batch = read_batch(pty.reader(), &mut buffer[..read_limit], &state.cancelled)?;
                if state.cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                read_bytes += batch.len;
                self.record_read(batch.len);
                if batch.len > 0 {
                    match output.try_send(buffer[..batch.len].to_vec()) {
                        Ok(()) => produced_output = true,
                        Err(TrySendError::Full(bytes)) => self.pending_output = Some(bytes),
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                }
                self.eof |= batch.eof;
                if batch.blocked {
                    // Flush partial batches promptly; never wait for a full
                    // buffer when the shell has produced only a few bytes.
                    if self.child_status.is_some() {
                        self.eof = true;
                    }
                    break;
                }
            }

            if produced_output {
                state.wake_app(waker);
            }
            if self.eof {
                self.input_closed = true;
                state.closed.store(true, Ordering::Release);
                self.pending_write = None;
            }
            if self.eof
                && self.pending_output.is_none()
                && let Some(status) = self.child_status
            {
                let _ = exit.try_send(status);
                state.wake_app(waker);
                return Ok(());
            }

            let mut written = 0;
            while !self.input_closed && written < IO_BUDGET {
                if state.cancelled.load(Ordering::Acquire) {
                    return Ok(());
                }
                if self.pending_write.is_none() {
                    self.pending_write = incoming.try_recv().ok();
                }
                let Some(request) = &mut self.pending_write else {
                    break;
                };
                let end = request.bytes.len().min(request.offset + READ_BUF_SIZE);
                match pty.writer().write(&request.bytes[request.offset..end]) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => {
                        request.offset += n;
                        written += n;
                        if request.offset == request.bytes.len() {
                            self.pending_write = None;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if error.raw_os_error() == Some(libc::EIO)
                            || error.kind() == io::ErrorKind::BrokenPipe =>
                    {
                        // Slave closure can precede SIGCHLD. Keep draining final
                        // output and wait for the actual child status.
                        self.input_closed = true;
                        state.closed.store(true, Ordering::Release);
                        self.pending_write = None;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            // A budget boundary can leave queued work without a pending write.
            if self.pending_write.is_none() && !self.input_closed {
                self.pending_write = incoming.try_recv().ok();
            }
            let interest = Event::new(
                0,
                !self.eof && self.pending_output.is_none(),
                self.pending_write.is_some(),
            );
            if interest.readable || interest.writable {
                poller.modify_with_mode(pty.file(), interest, PollMode::Oneshot)?;
                self.master_active = true;
            } else if self.master_active {
                // Disable once, without rearming on unrelated wakes. A hung-up
                // descriptor may report HUP even with no read/write interest.
                poller.modify_with_mode(pty.file(), Event::none(0), PollMode::Oneshot)?;
                self.master_active = false;
            }
            events.clear();
            if let Err(error) = poller.wait(&mut events, None)
                && error.kind() != io::ErrorKind::Interrupted
            {
                return Err(error);
            }
        }
    }
}

struct ReadBatch {
    len: usize,
    eof: bool,
    blocked: bool,
}

/// Fill a bounded batch from immediately available nonblocking reads.
fn read_batch(
    reader: &mut impl Read,
    buffer: &mut [u8],
    cancelled: &AtomicBool,
) -> io::Result<ReadBatch> {
    let mut batch = ReadBatch { len: 0, eof: false, blocked: false };
    while batch.len < buffer.len() {
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        match reader.read(&mut buffer[batch.len..]) {
            Ok(0) => {
                batch.eof = true;
                break;
            }
            Ok(n) => batch.len += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                batch.blocked = true;
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                batch.eof = true;
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(batch)
}

// Upstream Pty::drop waits for its child. Keep it off the UI thread and give
// a cancelled shell a short hangup grace period before forcing termination.
fn cleanup_child(pty: &mut tty::Pty, exited: bool) {
    if exited {
        return;
    }
    let pid = pty.child().id() as libc::pid_t;
    if pty.next_child_event().is_some() {
        return;
    }
    unsafe {
        // The child creates a session/process group during PTY setup.
        if libc::kill(-pid, libc::SIGHUP) != 0 {
            libc::kill(pid, libc::SIGHUP);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        if pty.next_child_event().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        if libc::kill(-pid, libc::SIGKILL) != 0 {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    // On macOS, slave closure can wait for queued output to drain. Upstream
    // waits for the child before closing the master, so discard output here.
    let mut buffer = [0; READ_BUF_SIZE];
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if pty.next_child_event().is_some() {
            return;
        }
        let mut drained = 0;
        while drained < IO_BUDGET {
            match pty.reader().read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

impl TerminalSize {
    pub(crate) fn to_window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.columns as u16,
            cell_width: self.cell_width as u16,
            cell_height: self.cell_height as u16,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct ShortReader {
        data: Vec<u8>,
        offset: usize,
        eof: bool,
        calls: usize,
    }

    impl Read for ShortReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.offset == self.data.len() {
                return if self.eof { Ok(0) } else { Err(io::ErrorKind::WouldBlock.into()) };
            }
            let len = buffer.len().min(1024).min(self.data.len() - self.offset);
            buffer[..len].copy_from_slice(&self.data[self.offset..self.offset + len]);
            self.offset += len;
            Ok(len)
        }
    }

    #[test]
    fn repeated_partial_would_block_batches_use_the_full_byte_capacity() {
        let expected: Vec<_> = (0..OUTPUT_BYTE_LIMIT).map(|i| (i % 251) as u8).collect();
        let output = OutputQueue::default();
        let mut buffer = vec![0; READ_BUF_SIZE];
        for part in expected.chunks(1024) {
            let mut reader = ShortReader { data: part.to_vec(), offset: 0, eof: false, calls: 0 };
            let batch = read_batch(&mut reader, &mut buffer, &AtomicBool::new(false)).unwrap();
            assert!(batch.blocked && !batch.eof);
            assert_eq!(batch.len, part.len());
            output
                .try_send(buffer[..batch.len].to_vec())
                .expect("partial reads exhausted capacity before 4 MiB");
        }
        assert!(output.is_full());
        assert_eq!(output.buffer.lock().unwrap().chunks.len(), OUTPUT_CHUNKS);
        match output.try_send(vec![99]) {
            Err(TrySendError::Full(bytes)) => assert_eq!(bytes, vec![99]),
            result => panic!("byte cap failed: {result:?}"),
        }
        let mut received = Vec::new();
        while let Ok(bytes) = output.try_recv() {
            received.extend(bytes);
        }
        assert_eq!(received, expected);
        assert!(!output.is_full());
        output.try_send(vec![99]).unwrap();
        assert_eq!(output.try_recv().unwrap(), vec![99]);
    }

    #[test]
    fn one_byte_fragments_merge_and_split_at_chunk_boundaries() {
        let output = OutputQueue::default();
        for _ in 0..OUTPUT_CHUNKS + 1 {
            output.try_send(vec![7]).unwrap();
        }
        assert_eq!(output.buffer.lock().unwrap().chunks.len(), 1);
        assert_eq!(output.try_recv().unwrap(), vec![7; OUTPUT_CHUNKS + 1]);
        output.try_send(vec![1; READ_BUF_SIZE - 1]).unwrap();
        output.try_send(vec![2, 3, 4]).unwrap();
        let first = output.try_recv().unwrap();
        assert_eq!(first.len(), READ_BUF_SIZE);
        assert!(first[..READ_BUF_SIZE - 1].iter().all(|&byte| byte == 1));
        assert_eq!(first[READ_BUF_SIZE - 1], 2);
        assert_eq!(output.try_recv().unwrap(), vec![3, 4]);
        assert_eq!(output.buffer.lock().unwrap().bytes, 0);
    }

    #[test]
    fn short_reads_fit_a_mebibyte_into_the_bounded_output_queue() {
        let data: Vec<_> = (0..IO_BUDGET).map(|i| (i % 251) as u8).collect();
        let mut reader = ShortReader { data: data.clone(), offset: 0, eof: false, calls: 0 };
        let cancelled = AtomicBool::new(false);
        let mut buffer = vec![0; READ_BUF_SIZE];
        let (tx, rx) = bounded(OUTPUT_CHUNKS);
        while reader.offset < data.len() {
            let batch = read_batch(&mut reader, &mut buffer, &cancelled).unwrap();
            assert_eq!(batch.len, READ_BUF_SIZE);
            assert!(!batch.eof && !batch.blocked);
            tx.try_send(buffer[..batch.len].to_vec())
                .expect("short reads exhausted output capacity");
        }
        assert_eq!(rx.len(), IO_BUDGET / READ_BUF_SIZE);
        assert_eq!(rx.try_iter().flatten().collect::<Vec<_>>(), data);
        assert_eq!(reader.calls, IO_BUDGET / 1024);
    }

    #[test]
    fn post_exit_limit_marks_eof_at_exact_read_budget_boundary() {
        let mut io = IoLoop {
            pending_write: None,
            pending_output: None,
            child_status: Some(None),
            eof: false,
            input_closed: true,
            post_exit_remaining: IO_BUDGET,
            master_active: true,
        };
        let data = vec![7; IO_BUDGET];
        let mut reader = ShortReader { data: data.clone(), offset: 0, eof: false, calls: 0 };
        let mut buffer = vec![0; READ_BUF_SIZE];
        let (tx, rx) = bounded(OUTPUT_CHUNKS);
        let mut read_bytes = 0;
        while !io.eof && read_bytes < IO_BUDGET {
            let batch = read_batch(&mut reader, &mut buffer, &AtomicBool::new(false)).unwrap();
            read_bytes += batch.len;
            io.record_read(batch.len);
            // EOF at the cap must preserve the final permitted batch.
            tx.try_send(buffer[..batch.len].to_vec()).unwrap();
        }
        assert_eq!(read_bytes, IO_BUDGET);
        assert_eq!(io.post_exit_remaining, 0);
        assert!(io.eof, "cap boundary would enter poll wait without another readable event");
        assert_eq!(rx.try_iter().flatten().collect::<Vec<_>>(), data);
        assert_eq!(reader.calls, IO_BUDGET / 1024, "cap detection needed another read");
    }

    #[test]
    fn partial_short_reads_flush_without_waiting_for_more_output() {
        for eof in [false, true] {
            let data = b"prompt".repeat(501);
            let mut reader = ShortReader { data: data.clone(), offset: 0, eof, calls: 0 };
            let mut buffer = vec![0; READ_BUF_SIZE];
            let batch = read_batch(&mut reader, &mut buffer, &AtomicBool::new(false)).unwrap();
            assert_eq!(&buffer[..batch.len], data);
            assert_eq!(batch.eof, eof);
            assert_eq!(batch.blocked, !eof);
        }
    }

    #[test]
    fn batching_respects_the_remaining_budget_and_cancellation() {
        let mut reader =
            ShortReader { data: vec![7; READ_BUF_SIZE], offset: 0, eof: false, calls: 0 };
        let mut buffer = [0; 1500];
        let cancelled = AtomicBool::new(false);
        let batch = read_batch(&mut reader, &mut buffer, &cancelled).unwrap();
        assert_eq!(batch.len, buffer.len());
        assert_eq!(reader.offset, 1500);
        assert_eq!(reader.calls, 2);
        cancelled.store(true, Ordering::Release);
        let batch = read_batch(&mut reader, &mut buffer, &cancelled).unwrap();
        assert_eq!(batch.len, 0);
        assert_eq!(reader.calls, 2, "cancelled batching performed another read");
    }

    fn spawn_script(body: &str) -> (tempfile::TempPath, PtyHandle) {
        spawn_script_in_directory(body, None)
    }

    fn spawn_script_in_directory(
        body: &str,
        directory: Option<&Path>,
    ) -> (tempfile::TempPath, PtyHandle) {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "#!/bin/sh\n{body}").unwrap();
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = file.into_temp_path();
        let mut config = Config::default();
        config.shell.program = path.to_str().unwrap().into();
        let pty = PtyHandle::spawn_in_directory(
            &config,
            TerminalSize::default(),
            Arc::new(|| {}),
            directory,
        )
        .unwrap();
        (path, pty)
    }

    #[test]
    fn inherited_directories_are_independent_and_do_not_change_application_cwd() {
        let original = std::env::current_dir().unwrap();
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("a space 雪");
        let second = temporary.path().join("another directory");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let (_first_script, first_pty) =
            spawn_script_in_directory("stty raw -echo; /bin/pwd -P; exec /bin/cat", Some(&first));
        let (_second_script, second_pty) =
            spawn_script_in_directory("stty raw -echo; /bin/pwd -P; exec /bin/cat", Some(&second));
        let first_expected = format!("{}\n", first.canonicalize().unwrap().display());
        let second_expected = format!("{}\n", second.canonicalize().unwrap().display());
        assert_eq!(receive(&first_pty, first_expected.len()), first_expected.as_bytes());
        assert_eq!(receive(&second_pty, second_expected.len()), second_expected.as_bytes());
        assert_eq!(std::env::current_dir().unwrap(), original);
    }

    #[test]
    fn missing_and_remote_metadata_use_the_default_child_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing");
        let expected =
            format!("{}\n", std::env::current_dir().unwrap().canonicalize().unwrap().display());
        let mut integration = crate::shell_state::ShellIntegration::default();
        let remote = format!("file://remote.invalid{}", temporary.path().display());
        integration.marker(
            &[b"7".to_vec(), remote.into_bytes()],
            alacritty_terminal::index::Point::default(),
            0,
            0,
        );
        assert!(integration.local_working_directory().is_none());
        for candidate in [Some(missing.as_path()), integration.local_working_directory()] {
            let (_script, pty) =
                spawn_script_in_directory("stty raw -echo; /bin/pwd -P; exec /bin/cat", candidate);
            assert_eq!(receive(&pty, expected.len()), expected.as_bytes());
        }
    }

    #[test]
    fn inaccessible_inherited_directory_falls_back_without_hiding_launch_errors() {
        if unsafe { libc::geteuid() } == 0 {
            return; // A privileged child can enter a mode-000 directory.
        }
        let temporary = tempfile::tempdir().unwrap();
        let blocked = temporary.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let expected =
            format!("{}\n", std::env::current_dir().unwrap().canonicalize().unwrap().display());
        let (_script, pty) =
            spawn_script_in_directory("stty raw -echo; /bin/pwd -P; exec /bin/cat", Some(&blocked));
        let actual = receive(&pty, expected.len());
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(actual, expected.as_bytes());
        let mut config = Config::default();
        config.shell.program = "/missing-mechanic-shell".into();
        assert!(matches!(
            PtyHandle::spawn_in_directory(
                &config,
                TerminalSize::default(),
                Arc::new(|| {}),
                Some(&blocked)
            ),
            Err(TerminalError::PtySpawn(_))
        ));
    }

    fn receive(pty: &PtyHandle, length: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = Vec::new();
        while bytes.len() < length {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "received {} of {length} bytes", bytes.len());
            bytes.extend(pty.rx.recv_timeout(remaining).expect("PTY output stalled"));
            pty.output_drained();
        }
        bytes
    }

    #[test]
    fn blocked_reader_does_not_block_enqueue_and_writes_remain_ordered() {
        let (_script, mut pty) =
            spawn_script("stty raw -echo; printf READY; sleep 0.3; exec /bin/cat");
        assert_eq!(receive(&pty, 5), b"READY");
        let mut expected: Vec<_> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let start = Instant::now();
        pty.write(&expected).unwrap();
        pty.write(b"TAIL").unwrap();
        assert!(start.elapsed() < Duration::from_millis(200), "enqueue waited for the reader");
        expected.extend_from_slice(b"TAIL");
        assert_eq!(receive(&pty, expected.len()), expected);
    }

    #[test]
    fn byte_limit_rejects_whole_payload_and_releases_capacity() {
        let (_script, mut pty) =
            spawn_script("stty raw -echo; printf READY; sleep 0.3; exec /bin/cat");
        assert_eq!(receive(&pty, 5), b"READY");
        let data = vec![b'x'; WRITE_BYTE_LIMIT];
        pty.write(&data).unwrap();
        assert!(
            matches!(pty.write(b"REJECTED"), Err(TerminalError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock)
        );
        assert_eq!(receive(&pty, data.len()), data);
        let deadline = Instant::now() + Duration::from_secs(1);
        while pty.state.queued_bytes.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(pty.state.queued_bytes.load(Ordering::Acquire), 0);
        pty.write(b"TAIL").unwrap();
        assert_eq!(receive(&pty, 4), b"TAIL", "rejected bytes must never reach the child");
    }

    #[test]
    fn exit_with_pending_input_keeps_status_and_final_output() {
        let (_script, mut pty) =
            spawn_script("stty raw -echo; printf READY; sleep 0.1; printf FINAL; exit 7");
        assert_eq!(receive(&pty, 5), b"READY");
        pty.write(&vec![b'x'; 2 * 1024 * 1024]).unwrap();
        assert_eq!(receive(&pty, 5), b"FINAL");
        let status = pty.exit_rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(pty.take_failure().is_none());
    }

    #[test]
    fn cancellation_stops_a_backpressured_worker_and_hup_ignoring_child() {
        let (_script, mut pty) =
            spawn_script("trap '' HUP; stty raw -echo; printf READY; exec /bin/cat /dev/zero");
        let ready = receive(&pty, 5);
        assert!(ready.starts_with(b"READY"));
        // Removing READY can leave capacity that is smaller than the next
        // batch. Backpressure need not fill the byte limit exactly.
        let backpressured =
            || pty.rx.buffer.lock().unwrap().bytes > OUTPUT_BYTE_LIMIT - READ_BUF_SIZE;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !backpressured() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(backpressured());
        let state = Arc::clone(&pty.state);
        let worker = pty._worker.take().unwrap();
        let start = Instant::now();
        drop(pty);
        assert!(start.elapsed() < Duration::from_millis(100));
        while !worker.is_finished() && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            worker.is_finished(),
            "cancelled worker did not reap its child (transport closed: {})",
            state.closed.load(Ordering::Acquire)
        );
        worker.join().unwrap();
    }

    #[test]
    fn fatal_failure_is_reported_once_and_rejects_later_input() {
        let (_script, mut pty) = spawn_script("stty raw -echo; printf READY; exec /bin/cat");
        assert_eq!(receive(&pty, 5), b"READY");
        pty.state.fail(io::Error::other("injected transport failure"), &pty.waker);
        assert_eq!(pty.take_failure().as_deref(), Some("injected transport failure"));
        assert!(pty.take_failure().is_none());
        assert!(pty.write(b"must not be queued").is_err());
        assert_eq!(pty.state.queued_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn surviving_writer_does_not_prevent_shell_exit_delivery() {
        let (_script, pty) =
            spawn_script("stty raw -echo; trap '' HUP; printf READY; /bin/cat /dev/zero & exit 7");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(status) = pty.exit_rx.try_recv() {
                assert_eq!(status.unwrap().code(), Some(7));
                return;
            }
            for _ in 0..OUTPUT_CHUNKS {
                if pty.rx.try_recv().is_err() {
                    break;
                }
            }
            pty.output_drained();
            assert!(Instant::now() < deadline, "descendant output prevented exit delivery");
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn terminal_size_to_window_size() {
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let ws = size.to_window_size();
        assert_eq!((ws.num_cols, ws.num_lines, ws.cell_width, ws.cell_height), (80, 24, 8, 16));
    }
}
