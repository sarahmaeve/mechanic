//! Command-line access to an existing Mechanic instance. Parsing never opens a window.

use std::io::{self, Read};
use std::path::PathBuf;

use crate::control::{self, InputMode, Operation, Request, ResponseResult, SessionSelector};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSource {
    Text(String),
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    List,
    Read { pane: SessionSelector, lines: usize },
    Send { pane: SessionSelector, source: TextSource, raw: bool, enter: bool },
    Wait { pane: SessionSelector, after: u64, timeout_ms: u64 },
    Instances,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub socket: Option<PathBuf>,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Command(Cli),
    Help,
}

/// Accept --socket before or after the control command as well as at the app level.
pub fn parse(args: Vec<String>, mut socket: Option<PathBuf>) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let mut command = None;
    let mut pane = None;
    let mut lines = None;
    let mut source = None;
    let mut raw = false;
    let mut enter = false;
    let mut after = None;
    let mut timeout_ms = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--socket" => {
                if socket.is_some() {
                    return Err("--socket may only be supplied once".into());
                }
                socket = Some(PathBuf::from(value(&mut args, "--socket")?));
            }
            "--pane" => {
                if pane.is_some() {
                    return Err("--pane may only be supplied once".into());
                }
                let handle = value(&mut args, "--pane")?;
                pane = Some(handle.parse::<SessionSelector>().map_err(|err| err.to_string())?);
            }
            "--lines" => {
                if lines.is_some() {
                    return Err("--lines may only be supplied once".into());
                }
                let count = value(&mut args, "--lines")?
                    .parse::<usize>()
                    .map_err(|_| "--lines must be a positive integer")?;
                if count == 0 || count > control::MAX_OUTPUT_LINES {
                    return Err(format!(
                        "--lines must be between 1 and {}",
                        control::MAX_OUTPUT_LINES
                    ));
                }
                lines = Some(count);
            }
            "--text" => {
                if source.is_some() {
                    return Err("supply exactly one of --text or --stdin".into());
                }
                let text = value(&mut args, "--text")?;
                validate_text_size(text.as_bytes())?;
                source = Some(TextSource::Text(text));
            }
            "--stdin" => {
                if source.is_some() {
                    return Err("supply exactly one of --text or --stdin".into());
                }
                source = Some(TextSource::Stdin);
            }
            "--raw" => {
                if raw {
                    return Err("--raw may only be supplied once".into());
                }
                raw = true;
            }
            "--enter" => {
                if enter {
                    return Err("--enter may only be supplied once".into());
                }
                enter = true;
            }
            "--after" => {
                if after.is_some() {
                    return Err("--after may only be supplied once".into());
                }
                after = Some(value(&mut args, "--after")?.parse::<u64>().map_err(
                    |_| "--after must be a non-negative command ID returned by list or wait",
                )?);
            }
            "--timeout" => {
                if timeout_ms.is_some() {
                    return Err("--timeout may only be supplied once".into());
                }
                let seconds = value(&mut args, "--timeout")?
                    .parse::<f64>()
                    .map_err(|_| "--timeout must be a positive number of seconds")?;
                if !seconds.is_finite()
                    || seconds <= 0.0
                    || seconds * 1000.0 > control::MAX_WAIT_MS as f64
                {
                    return Err(format!(
                        "--timeout must be greater than zero and at most {} seconds",
                        control::MAX_WAIT_MS / 1000
                    ));
                }
                let milliseconds = (seconds * 1000.0).ceil() as u64;
                timeout_ms = Some(milliseconds);
            }
            "list" | "read" | "send" | "wait" | "instances" if command.is_none() => {
                command = Some(arg)
            }
            other => {
                return Err(format!(
                    "unknown control argument '{other}'; try 'mechanic ctl --help'"
                ));
            }
        }
    }
    let command = command.ok_or("missing control command; try 'mechanic ctl --help'")?;
    let needs_pane = matches!(command.as_str(), "read" | "send" | "wait");
    if !needs_pane && pane.is_some() {
        return Err(format!("--pane is not supported by ctl {command}"));
    }
    if command != "read" && lines.is_some() {
        return Err("--lines is only supported by ctl read".into());
    }
    if command != "send" && (source.is_some() || raw || enter) {
        return Err("--text, --stdin, --raw, and --enter are only supported by ctl send".into());
    }
    if command != "wait" && (after.is_some() || timeout_ms.is_some()) {
        return Err("--after and --timeout are only supported by ctl wait".into());
    }
    if command == "instances" && socket.is_some() {
        return Err("ctl instances discovers all instances; --socket is not supported".into());
    }
    let command = match command.as_str() {
        "list" => Command::List,
        "instances" => Command::Instances,
        "read" => Command::Read {
            pane: pane.ok_or("ctl read requires --pane INSTANCE:SESSION")?,
            lines: lines.unwrap_or(200),
        },
        "send" => Command::Send {
            pane: pane.ok_or("ctl send requires --pane INSTANCE:SESSION")?,
            source: source.ok_or("ctl send requires exactly one of --text or --stdin")?,
            raw,
            enter,
        },
        "wait" => Command::Wait {
            pane: pane.ok_or("ctl wait requires --pane INSTANCE:SESSION")?,
            after: after.ok_or("ctl wait requires --after COMMAND_ID")?,
            timeout_ms: timeout_ms.unwrap_or(30_000),
        },
        _ => unreachable!(),
    };
    Ok(Parsed::Command(Cli { socket, command }))
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} requires a value"))
}

fn validate_text_size(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > control::MAX_TEXT_BYTES {
        return Err(format!("input exceeds the {} byte limit", control::MAX_TEXT_BYTES));
    }
    Ok(())
}

fn read_stdin(mut reader: impl Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(control::MAX_TEXT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("failed to read stdin: {err}"))?;
    validate_text_size(&bytes)?;
    String::from_utf8(bytes).map_err(|_| "stdin must contain valid UTF-8".into())
}

pub fn print_error(code: &str, message: &str) {
    println!("{}", serde_json::json!({"status": "error", "code": code, "message": message}));
}

pub fn run(cli: Cli) -> i32 {
    match execute(cli) {
        Ok(success) => {
            if success {
                0
            } else {
                1
            }
        }
        Err(message) => {
            print_error("client_error", &message);
            1
        }
    }
}

fn execute(cli: Cli) -> Result<bool, String> {
    if cli.command == Command::Instances {
        let endpoints = control::discover_endpoints()
            .map_err(|err| format!("cannot discover instances: {err}"))?;
        println!("{}", serde_json::json!({"status": "instances", "instances": endpoints}));
        return Ok(true);
    }
    let (selector, operation) = match cli.command {
        Command::List => (None, Operation::ListPanes),
        Command::Read { pane, lines } => (
            Some(pane.clone()),
            Operation::ReadOutput {
                session: pane,
                max_lines: lines,
                max_bytes: control::MAX_OUTPUT_BYTES,
            },
        ),
        Command::Send { pane, source, raw, enter } => {
            let text = match source {
                TextSource::Text(text) => text,
                TextSource::Stdin => read_stdin(io::stdin().lock())?,
            };
            (
                Some(pane.clone()),
                Operation::SendInput {
                    session: pane,
                    text,
                    mode: if raw { InputMode::Raw } else { InputMode::Paste },
                    enter,
                },
            )
        }
        Command::Wait { pane, after, timeout_ms } => (
            Some(pane.clone()),
            Operation::Wait { session: pane, after_command_id: after, timeout_ms },
        ),
        Command::Instances => unreachable!(),
    };
    let socket =
        match cli.socket {
            Some(socket) => socket,
            None => {
                let mut endpoints = control::discover_endpoints()
                    .map_err(|err| format!("cannot discover instances: {err}"))?;
                if let Some(selector) = &selector {
                    endpoints.retain(|endpoint| endpoint.instance_id == selector.instance_id);
                }
                match endpoints.as_slice() {
                    [endpoint] => endpoint.socket_path.clone(),
                    [] => {
                        return Err(if selector.is_some() {
                            "pane handle is stale or its Mechanic instance is unavailable".into()
                        } else {
                            "no running Mechanic instance found".into()
                        });
                    }
                    _ => return Err(
                        "multiple Mechanic instances found; use ctl instances and --socket PATH"
                            .into(),
                    ),
                }
            }
        };
    let request = Request::new(selector.map(|selector| selector.instance_id), operation);
    let response = control::request(&socket, &request)
        .map_err(|err| format!("control request failed for {}: {err}", socket.display()))?;
    let success =
        !matches!(response.result, ResponseResult::Error { .. } | ResponseResult::Timeout { .. });
    println!(
        "{}",
        serde_json::to_string(&response).map_err(|err| format!("cannot encode response: {err}"))?
    );
    Ok(success)
}

pub fn print_help() {
    println!(
        "mechanic ctl — control an existing terminal instance\n\nUSAGE:\n    mechanic [--socket PATH] ctl COMMAND [OPTIONS]\n\nCOMMANDS:\n    list                         List pane handles and command IDs\n    read --pane INSTANCE:SESSION [--lines N]\n    send --pane INSTANCE:SESSION (--text TEXT | --stdin) [--raw] [--enter]\n    wait --pane INSTANCE:SESSION --after COMMAND_ID [--timeout SECONDS]\n    instances                    Discover running instances and socket paths\n\nOPTIONS:\n    --socket PATH                Select an instance (also accepted after ctl)\n    --lines N                    Read up to N lines (default 200)\n    --stdin                      Read UTF-8 input, at most 128 KiB\n    --raw                        Send UTF-8 bytes directly, including controls\n    --enter                      Send Enter separately after input\n    --timeout SECONDS            Wait limit (default 30, maximum 60)\n    -h, --help                   Show this help and exit\n\nSend pastes text by default. Enter is sent only with --enter.\nPane handles returned by list include the instance nonce; stale handles are rejected.\nControl results and errors are JSON; errors and wait timeouts exit nonzero."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDLE: &str = "0123456789abcdef0123456789abcdef:7";

    fn parse_flags(flags: &[&str]) -> Result<Parsed, String> {
        parse(flags.iter().map(|flag| (*flag).to_owned()).collect(), None)
    }

    #[test]
    fn send_defaults_to_paste_without_enter() {
        let Parsed::Command(cli) =
            parse_flags(&["send", "--pane", HANDLE, "--text", "echo ok"]).unwrap()
        else {
            panic!()
        };
        assert!(matches!(cli.command, Command::Send { raw: false, enter: false, .. }));
        let Parsed::Command(cli) =
            parse_flags(&["send", "--pane", HANDLE, "--stdin", "--raw", "--enter"]).unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            cli.command,
            Command::Send { source: TextSource::Stdin, raw: true, enter: true, .. }
        ));
    }

    #[test]
    fn socket_can_precede_or_follow_command() {
        let before = parse_flags(&["--socket", "/tmp/socket", "list"]).unwrap();
        let after = parse_flags(&["list", "--socket", "/tmp/socket"]).unwrap();
        assert_eq!(before, after);
        assert_eq!(before, parse(vec!["list".into()], Some("/tmp/socket".into())).unwrap());
    }

    #[test]
    fn text_values_that_look_like_flags_are_preserved() {
        let Parsed::Command(cli) =
            parse_flags(&["send", "--pane", HANDLE, "--text", "--help"]).unwrap()
        else {
            panic!()
        };
        assert!(
            matches!(cli.command, Command::Send { source: TextSource::Text(text), .. } if text == "--help")
        );
    }

    #[test]
    fn invalid_control_arguments_are_rejected_without_side_effects() {
        for flags in [
            vec![],
            vec!["bogus"],
            vec!["read"],
            vec!["read", "--pane", "7"],
            vec!["wait", "--pane", HANDLE],
            vec!["send", "--pane", HANDLE],
            vec!["send", "--pane", HANDLE, "--stdin", "--text", "x"],
            vec!["read", "--pane", HANDLE, "--lines", "0"],
            vec!["wait", "--pane", HANDLE, "--after", "3", "--timeout", "NaN"],
            vec!["wait", "--pane", HANDLE, "--after", "3", "--timeout", "61"],
            vec!["list", "--raw"],
            vec!["list", "--socket"],
        ] {
            assert!(parse_flags(&flags).is_err(), "accepted {flags:?}");
        }
    }

    #[test]
    fn wait_keeps_explicit_command_id_and_fractional_timeout() {
        let Parsed::Command(cli) =
            parse_flags(&["wait", "--pane", HANDLE, "--after", "42", "--timeout", "0.125"])
                .unwrap()
        else {
            panic!()
        };
        assert!(matches!(cli.command, Command::Wait { after: 42, timeout_ms: 125, .. }));
    }

    #[test]
    fn stdin_is_bounded_and_requires_utf8() {
        assert_eq!(read_stdin(&b"hello\n"[..]).unwrap(), "hello\n");
        assert!(read_stdin(&[0xff][..]).unwrap_err().contains("UTF-8"));
        assert!(
            read_stdin(vec![b'x'; control::MAX_TEXT_BYTES + 1].as_slice())
                .unwrap_err()
                .contains("limit")
        );
        assert_eq!(
            read_stdin(vec![b'x'; control::MAX_TEXT_BYTES].as_slice()).unwrap().len(),
            control::MAX_TEXT_BYTES
        );
    }
}
