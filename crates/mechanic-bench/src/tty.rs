use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub struct Tty {
    file: File,
    saved: libc::termios,
    cancelled: Arc<AtomicBool>,
    signals: Vec<signal_hook::SigId>,
}

impl Tty {
    pub fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")?;
        let fd = file.as_raw_fd();
        let mut saved = std::mem::MaybeUninit::uninit();
        // SAFETY: fd is open and saved points to writable termios storage.
        if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let saved = unsafe { saved.assume_init() };
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut tty = Self { file, saved, cancelled, signals: Vec::new() };
        // Ctrl-Z cancels this benchmark instead of suspending it with the
        // user's terminal left in noncanonical mode.
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGTSTP,
        ] {
            tty.signals.push(signal_hook::flag::register(signal, tty.cancelled.clone())?);
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_iflag &= !(libc::ICRNL | libc::IXON);
        raw.c_oflag &= !libc::OPOST;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: fd is valid and raw is an initialized termios value.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(tty)
    }

    pub fn size(&self) -> io::Result<(usize, usize)> {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: ioctl writes winsize to valid storage for this open terminal.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((usize::from(size.ws_col), usize::from(size.ws_row)))
    }

    fn ready(&self, events: i16, deadline: Instant, check_cancelled: bool) -> io::Result<()> {
        loop {
            if check_cancelled && self.cancelled.load(Ordering::Relaxed) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "benchmark interrupted"));
            }
            let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "terminal did not complete within timeout")
            })?;
            let fd = self.file.as_raw_fd();
            if fd as usize >= libc::FD_SETSIZE {
                return Err(io::Error::other("terminal descriptor exceeds select capacity"));
            }
            let mut descriptors: libc::fd_set = unsafe { std::mem::zeroed() };
            let mut timeout =
                libc::timeval { tv_sec: 0, tv_usec: remaining.as_micros().clamp(1, 100_000) as _ };
            // macOS poll rejects /dev/tty with POLLNVAL; select supports it.
            // SAFETY: fd fits fd_set and pointers remain valid through select.
            let result = unsafe {
                libc::FD_ZERO(&mut descriptors);
                libc::FD_SET(fd, &mut descriptors);
                let (read, write) = if events == libc::POLLIN {
                    (&mut descriptors as *mut _, std::ptr::null_mut())
                } else {
                    (std::ptr::null_mut(), &mut descriptors as *mut _)
                };
                libc::select(fd + 1, read, write, std::ptr::null_mut(), &mut timeout)
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            } else if result > 0 {
                return Ok(());
            }
        }
    }

    pub fn write(&mut self, data: &[u8], deadline: Instant) -> io::Result<()> {
        self.write_inner(data, deadline, true)
    }

    fn write_inner(
        &mut self,
        mut data: &[u8],
        deadline: Instant,
        check_cancelled: bool,
    ) -> io::Result<()> {
        while !data.is_empty() {
            self.ready(libc::POLLOUT, deadline, check_cancelled)?;
            match self.file.write(&data[..data.len().min(65536)]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => data = &data[n..],
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn fence(&mut self, deadline: Instant) -> io::Result<()> {
        self.write(b"\x1b[6n", deadline)?;
        let mut response = Vec::new();
        loop {
            self.ready(libc::POLLIN, deadline, true)?;
            let mut bytes = [0; 256];
            match self.file.read(&mut bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => response.extend_from_slice(&bytes[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            }
            if cursor_reply(&response) {
                return Ok(());
            }
            if response.len() > 4096 {
                return Err(io::Error::other("unexpected input while waiting for cursor reply"));
            }
        }
    }

    pub fn reset(&mut self, scrollback: bool, timeout: Duration) -> io::Result<()> {
        let screen = if scrollback { "\x1b[?1049l\x1b[3J" } else { "\x1b[?1049h" };
        let deadline = Instant::now() + timeout;
        self.write(format!("{screen}\x1b[r\x1b[0m\x1b[2J\x1b[H\x1b[?25l").as_bytes(), deadline)?;
        self.fence(deadline)
    }
}

impl Drop for Tty {
    fn drop(&mut self) {
        // Retry partial/nonblocking writes even after cancellation, but keep
        // cleanup bounded if the emulator has stopped consuming output.
        let _ = self.write_inner(
            b"\x1b[r\x1b[0m\x1b[?25h\x1b[?1049l",
            Instant::now() + Duration::from_millis(250),
            false,
        );
        // SAFETY: the file remains open and saved came from tcgetattr.
        unsafe {
            libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.saved);
        }
        for id in self.signals.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn cursor_reply(bytes: &[u8]) -> bool {
    bytes.windows(2).enumerate().any(|(i, pair)| {
        if pair != b"\x1b[" {
            return false;
        }
        let rest = &bytes[i + 2..];
        let Some(end) = rest.iter().position(|&b| b == b'R') else {
            return false;
        };
        let mut parts = rest[..end].split(|&b| b == b';');
        let valid = |part: Option<&[u8]>| {
            part.is_some_and(|p| {
                !p.is_empty() && p.iter().all(u8::is_ascii_digit) && p.iter().any(|&b| b != b'0')
            })
        };
        valid(parts.next()) && valid(parts.next()) && parts.next().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fence_requires_complete_cursor_report() {
        assert!(cursor_reply(b"noise\x1b[24;80R"));
        for bad in
            [b"\x1b[24;80".as_slice(), b"\x1b[;R", b"\x1b[0;1R", b"\x1b[1;2;3R", b"\x1b[1;xR"]
        {
            assert!(!cursor_reply(bad));
        }
    }
}
