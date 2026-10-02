//! Shell metadata emitted by OSC 7 and FinalTerm-compatible OSC 133 hooks.
//!
//! Positions use a monotonically increasing main-screen row coordinate. The
//! terminal maps them back to live-grid coordinates when navigating/extracting.
//! Reflow and destructive grid edits invalidate positions but preserve cwd and
//! the last exit status. Metadata is bounded independently of scrollback size.

use std::collections::VecDeque;

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

/// Event-driven metadata for the foreground shell.
#[derive(Debug, Default)]
pub struct ShellIntegration {
    cwd: Option<String>,
    cwd_host: Option<String>,
    last_exit_status: Option<i32>,
    commands: VecDeque<ShellCommand>,
    generation: u64,
    executing: bool,
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

    pub fn last_exit_status(&self) -> Option<i32> {
        self.last_exit_status
    }

    /// Whether command execution has begun without a completion marker yet.
    /// This remains accurate when clearing or reflow invalidates grid positions.
    pub fn is_running(&self) -> bool {
        self.executing
    }

    /// Retained markers, from oldest to newest, in absolute scroll coordinates.
    pub fn commands(&self) -> &VecDeque<ShellCommand> {
        &self.commands
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
                        self.executing = false;
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
                        self.executing = true;
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
                        if !self.executing {
                            return;
                        }
                        self.executing = false;
                        self.last_exit_status = status;
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
    }
}
