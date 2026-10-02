//! Shell metadata emitted by OSC 7 and FinalTerm-compatible OSC 133 hooks.
//!
//! Positions use a monotonically increasing main-screen row coordinate. The
//! terminal maps them back to live-grid coordinates when navigating/extracting.
//! Reflow and destructive grid edits invalidate positions but preserve cwd and
//! the last exit status. Metadata is bounded independently of scrollback size.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use alacritty_terminal::index::Point;

const MAX_COMMANDS: usize = 1024;

/// A point in the shell's main-screen scroll coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShellPosition {
    pub line: i64,
    pub column: usize,
}

impl ShellPosition {
    pub(crate) fn at(point: Point, scroll: u64) -> Self {
        Self {
            line: point.line.0 as i64 + scroll.min(i64::MAX as u64) as i64,
            column: point.column.0,
        }
    }
}

/// A prompt and the command entered after it, as reported by shell hooks.
#[derive(Debug, Clone)]
pub struct ShellCommand {
    pub prompt: Option<ShellPosition>,
    pub command_start: Option<ShellPosition>,
    pub output_start: Option<ShellPosition>,
    /// Exclusive output endpoint.
    pub output_end: Option<ShellPosition>,
    pub exit_status: Option<i32>,
    pub completed: bool,
}

/// One execution completed by a matching OSC 133 C/D pair.
/// Independent of grid positions, so clearing and reflow cannot lose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCompletion {
    /// Monotonically increasing execution identifier within this terminal.
    pub id: u64,
    pub duration: Duration,
    pub exit_status: Option<i32>,
    /// Directory metadata at execution start; this may name a remote host.
    pub cwd: Option<String>,
    pub cwd_host: Option<String>,
}

#[derive(Debug)]
struct RunningCommand {
    id: u64,
    started: Instant,
    cwd: Option<String>,
    cwd_host: Option<String>,
}

/// Event-driven metadata for the foreground shell.
#[derive(Debug, Default)]
pub struct ShellIntegration {
    cwd: Option<String>,
    cwd_host: Option<String>,
    last_exit_status: Option<i32>,
    commands: VecDeque<ShellCommand>,
    generation: u64,
    running: Option<RunningCommand>,
    next_command_id: u64,
    completions: VecDeque<CommandCompletion>,
    oldest_retained: i64,
}

impl ShellIntegration {
    /// Decoded absolute directory path from the last valid OSC 7 URI.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Host named by OSC 7; an empty authority is represented as an empty string.
    pub fn cwd_host(&self) -> Option<&str> {
        self.cwd_host.as_deref()
    }

    /// Existing absolute directory reported by a local shell.
    /// Remote OSC 7 metadata remains visible through [`Self::cwd`] but cannot
    /// select a directory for a new local shell.
    pub fn local_working_directory(&self) -> Option<&Path> {
        let host = self.cwd_host()?;
        if !host.is_empty()
            && !host.eq_ignore_ascii_case("localhost")
            && !local_hostname().is_some_and(|local| host.eq_ignore_ascii_case(&local))
        {
            return None;
        }
        let path = Path::new(self.cwd()?);
        (path.is_absolute() && path.is_dir()).then_some(path)
    }

    pub fn last_exit_status(&self) -> Option<i32> {
        self.last_exit_status
    }

    /// Whether command execution has begun without a completion marker yet.
    /// This remains accurate when clearing or reflow invalidates grid positions.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Retained markers, from oldest to newest, in absolute scroll coordinates.
    pub fn commands(&self) -> &VecDeque<ShellCommand> {
        &self.commands
    }

    pub(crate) fn drain_completions(&mut self) -> impl Iterator<Item = CommandCompletion> + '_ {
        self.completions.drain(..)
    }

    pub(crate) fn invalidate(&mut self) {
        self.commands.clear();
    }

    pub(crate) fn erase(&mut self, point: Point, above: bool, scroll: u64, generation: u64) {
        if self.generation != generation {
            self.invalidate();
            self.generation = generation;
        }
        let cursor = ShellPosition::at(point, scroll);
        let first_visible = scroll.min(i64::MAX as u64) as i64;
        self.commands.retain_mut(|command| {
            if above {
                // Erase-above affects only the visible screen, not history.
                if command.prompt.is_some_and(|p| p.line >= first_visible && p <= cursor) {
                    command.prompt = None;
                }
                if command.output_start.is_some_and(|start| start <= cursor)
                    && command.output_end.is_none_or(|end| end.line >= first_visible)
                {
                    command.output_start = None;
                }
            } else {
                // ZLE regularly erases below the command-input boundary. Keep
                // that unfinished prompt; invalidate only recorded output or
                // later prompts whose cells are actually erased.
                if command.completed {
                    if command.prompt.is_some_and(|p| p >= cursor) {
                        command.prompt = None;
                    }
                    if command.output_end.is_some_and(|end| end > cursor) {
                        command.output_start = None;
                    }
                } else if command.output_start.is_some_and(|start| start >= cursor) {
                    command.output_start = None;
                }
            }
            command.prompt.is_some() || command.output_start.is_some()
        });
    }

    pub(crate) fn cells_changed(&mut self, start: Point, end: Point, scroll: u64, generation: u64) {
        if self.generation != generation {
            self.invalidate();
            self.generation = generation;
        }
        let start = ShellPosition::at(start, scroll);
        let end = ShellPosition::at(end, scroll);
        if start >= end {
            return;
        }
        for command in &mut self.commands {
            if !command.completed {
                continue;
            }
            if command.prompt.is_some_and(|p| start <= p && p < end) {
                command.prompt = None;
            }
            if let (Some(output_start), Some(output_end)) =
                (command.output_start, command.output_end)
                && start < output_end
                && output_start < end
            {
                command.output_start = None;
            }
        }
    }

    pub(crate) fn synchronize(&mut self, generation: u64, oldest: i64) {
        if self.generation != generation {
            self.invalidate();
            self.generation = generation;
        } else if self.oldest_retained == oldest {
            return;
        }
        self.oldest_retained = oldest;
        for command in &mut self.commands {
            for point in
                [&mut command.prompt, &mut command.command_start, &mut command.output_start]
            {
                if point.is_some_and(|p| p.line < oldest) {
                    *point = None;
                }
            }
        }
        self.commands.retain(|c| !c.completed || c.prompt.is_some() || c.output_start.is_some());
    }

    pub(crate) fn marker(
        &mut self,
        params: &[Vec<u8>],
        point: Point,
        scroll: u64,
        generation: u64,
    ) {
        self.marker_at(params, point, scroll, generation, Instant::now());
    }

    fn marker_at(
        &mut self,
        params: &[Vec<u8>],
        point: Point,
        scroll: u64,
        generation: u64,
        now: Instant,
    ) {
        if self.generation != generation {
            self.invalidate();
            self.generation = generation;
        }
        match params.first().map(Vec::as_slice) {
            Some(b"7") if params.len() >= 2 => {
                // Semicolons are legal filename bytes; VTE splits OSC parameters.
                let uri = params[1..].join(&b';');
                if let Some((host, path)) = parse_directory(&uri) {
                    self.cwd_host = Some(host);
                    self.cwd = Some(path);
                }
            }
            Some(b"133") if params.len() >= 2 => {
                let position = ShellPosition::at(point, scroll);
                match params[1].as_slice() {
                    b"A" => {
                        self.running = None;
                        // Repainted prompts replace an unfinished prompt; a new
                        // prompt cannot manufacture a successful completion.
                        if self
                            .commands
                            .back()
                            .is_some_and(|c| !c.completed && c.output_start.is_none())
                        {
                            self.commands.pop_back();
                        }
                        self.commands.push_back(ShellCommand {
                            prompt: Some(position),
                            command_start: None,
                            output_start: None,
                            output_end: None,
                            exit_status: None,
                            completed: false,
                        });
                        if self.commands.len() > MAX_COMMANDS {
                            self.commands.pop_front();
                        }
                    }
                    b"B" => {
                        if let Some(command) = self.commands.back_mut()
                            && !command.completed
                            && command.output_start.is_none()
                        {
                            command.command_start = Some(position);
                        }
                    }
                    b"C" => {
                        if self.running.is_none() {
                            // Refuse new executions only after exhausting the
                            // identifier space; IDs must never wrap or repeat.
                            let Some(id) = self.next_command_id.checked_add(1) else { return };
                            self.next_command_id = id;
                            self.running = Some(RunningCommand {
                                id,
                                started: now,
                                cwd: self.cwd.clone(),
                                cwd_host: self.cwd_host.clone(),
                            });
                        }
                        if let Some(command) = self.commands.back_mut()
                            && !command.completed
                            && command.output_start.is_none()
                        {
                            command.output_start = Some(position);
                        }
                    }
                    b"D" => {
                        let status = match params.get(2) {
                            None => None,
                            Some(raw) => {
                                let Some(status) = parse_status(raw) else { return };
                                Some(status)
                            }
                        };
                        let Some(running) = self.running.take() else { return };
                        self.last_exit_status = status;
                        self.completions.push_back(CommandCompletion {
                            id: running.id,
                            duration: now.saturating_duration_since(running.started),
                            exit_status: status,
                            cwd: running.cwd,
                            cwd_host: running.cwd_host,
                        });
                        if self.completions.len() > MAX_COMMANDS {
                            self.completions.pop_front();
                        }
                        if let Some(command) = self.commands.back_mut()
                            && !command.completed
                            && command.output_start.is_some()
                        {
                            command.output_end = Some(position);
                            command.exit_status = status;
                            command.completed = true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn local_hostname() -> Option<String> {
    let mut bytes = [0u8; 256];
    // Leave a trailing zero even on platforms which truncate without one.
    let result = unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len() - 1) };
    if result != 0 {
        return None;
    }
    let end = bytes.iter().position(|&byte| byte == 0)?;
    let host = std::str::from_utf8(&bytes[..end]).ok()?;
    (!host.is_empty()).then(|| host.to_owned())
}

fn parse_status(raw: &[u8]) -> Option<i32> {
    if raw.is_empty() || !raw.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(raw).ok()?.parse().ok()
}

fn parse_directory(raw: &[u8]) -> Option<(String, String)> {
    let uri = std::str::from_utf8(raw).ok()?.strip_prefix("file://")?;
    let slash = uri.find('/')?;
    let (host, path) = uri.split_at(slash);
    if host
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '@' | '?' | '#' | '%'))
    {
        return None;
    }
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            // URI query/fragment characters must be percent encoded in a path.
            if matches!(byte, b'?' | b'#') {
                return None;
            }
            byte
        });
    }
    let path = String::from_utf8(decoded).ok()?;
    if path.chars().any(char::is_control) {
        return None;
    }
    Some((host.to_owned(), path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker_at(integration: &mut ShellIntegration, marker: &[u8], now: Instant) {
        integration.marker_at(&[b"133".to_vec(), marker.to_vec()], Point::default(), 0, 0, now);
    }

    fn report_directory(integration: &mut ShellIntegration, host: &str, path: &Path) {
        let uri = format!("file://{host}{}", path.display());
        integration.marker(&[b"7".to_vec(), uri.into_bytes()], Point::default(), 0, 0);
    }

    #[test]
    fn directory_inheritance_requires_a_local_existing_absolute_directory() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("a space 雪");
        std::fs::create_dir(&directory).unwrap();
        let mut integration = ShellIntegration::default();
        let local = local_hostname().unwrap();
        for host in ["", "localhost", "LOCALHOST", &local] {
            report_directory(&mut integration, host, &directory);
            assert_eq!(integration.local_working_directory(), Some(directory.as_path()));
        }
        report_directory(&mut integration, "remote.invalid", &directory);
        assert_eq!(integration.cwd(), directory.to_str());
        assert!(integration.local_working_directory().is_none());
        report_directory(&mut integration, "", &directory.join("missing"));
        assert!(integration.local_working_directory().is_none());
        let file = temporary.path().join("file");
        std::fs::write(&file, b"file").unwrap();
        report_directory(&mut integration, "", &file);
        assert!(integration.local_working_directory().is_none());
        report_directory(&mut integration, "", &directory);
        std::fs::remove_dir(&directory).unwrap();
        assert!(integration.local_working_directory().is_none());
        integration.cwd = Some("relative".into());
        assert!(integration.local_working_directory().is_none());
    }

    #[test]
    fn completions_measure_execution_and_survive_position_invalidation() {
        let mut integration = ShellIntegration::default();
        report_directory(&mut integration, "localhost", Path::new("/original"));
        let start = Instant::now();
        marker_at(&mut integration, b"A", start);
        marker_at(&mut integration, b"C", start);
        // A duplicate C must not restart the clock.
        marker_at(&mut integration, b"C", start + Duration::from_secs(3));
        integration.invalidate();
        integration.synchronize(1, 40);
        report_directory(&mut integration, "localhost", Path::new("/later"));
        integration.marker_at(
            &[b"133".to_vec(), b"D".to_vec(), b"130".to_vec()],
            Point::default(),
            0,
            1,
            start + Duration::from_secs(5),
        );
        assert!(!integration.is_running());
        let completion = integration.drain_completions().next().unwrap();
        assert_eq!(completion.id, 1);
        assert_eq!(completion.duration, Duration::from_secs(5));
        assert_eq!(completion.exit_status, Some(130));
        assert_eq!(completion.cwd.as_deref(), Some("/original"));
        assert_eq!(completion.cwd_host.as_deref(), Some("localhost"));
        assert!(integration.commands().is_empty());
        assert!(integration.drain_completions().next().is_none());
    }

    #[test]
    fn completions_ignore_no_start_duplicate_and_cancelled_markers() {
        let mut integration = ShellIntegration::default();
        let now = Instant::now();
        for marker in [b"D".as_slice(), b"A", b"D", b"C", b"A", b"D"] {
            marker_at(&mut integration, marker, now);
        }
        assert!(integration.drain_completions().next().is_none());
        // C/D remains usable when prompt hooks are unavailable.
        for marker in [b"C".as_slice(), b"D", b"D", b"C", b"D"] {
            marker_at(&mut integration, marker, now);
        }
        let completions = integration.drain_completions().collect::<Vec<_>>();
        assert_eq!(completions.iter().map(|c| c.id).collect::<Vec<_>>(), [2, 3]);
        assert!(completions.iter().all(|c| c.exit_status.is_none()));
        assert!(completions.iter().all(|c| c.duration.is_zero()));
    }

    #[test]
    fn malformed_completion_keeps_execution_until_valid_status() {
        let mut integration = ShellIntegration::default();
        let now = Instant::now();
        marker_at(&mut integration, b"C", now);
        integration.marker_at(
            &[b"133".to_vec(), b"D".to_vec(), b"-1".to_vec()],
            Point::default(),
            0,
            0,
            now + Duration::from_secs(1),
        );
        assert!(integration.is_running());
        assert!(integration.drain_completions().next().is_none());
        marker_at(&mut integration, b"D", now + Duration::from_secs(2));
        assert_eq!(
            integration.drain_completions().next().unwrap().duration,
            Duration::from_secs(2)
        );
    }

    #[test]
    fn directory_decoding_rejects_malformed_uris() {
        assert_eq!(
            parse_directory(b"file://host/tmp/a%20b%3Bc"),
            Some(("host".into(), "/tmp/a b;c".into()))
        );
        for invalid in [
            b"/tmp".as_slice(),
            b"file://host",
            b"file:///tmp/%",
            b"file:///tmp/%ff",
            b"file:///tmp/%00",
            b"file:///tmp/a#b",
            b"file://user@host/tmp",
        ] {
            assert!(parse_directory(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn command_metadata_is_bounded() {
        let mut integration = ShellIntegration::default();
        for line in 0..MAX_COMMANDS * 2 {
            let p = Point::new(
                alacritty_terminal::index::Line(line as i32),
                alacritty_terminal::index::Column(0),
            );
            for marker in [b"A", b"C", b"D"] {
                integration.marker(&[b"133".to_vec(), marker.to_vec()], p, 0, 0);
            }
        }
        assert_eq!(integration.commands().len(), MAX_COMMANDS);
        let completions = integration.drain_completions().collect::<Vec<_>>();
        assert_eq!(completions.len(), MAX_COMMANDS);
        assert_eq!(completions.first().unwrap().id, MAX_COMMANDS as u64 + 1);
        assert_eq!(completions.last().unwrap().id, MAX_COMMANDS as u64 * 2);
    }
}
