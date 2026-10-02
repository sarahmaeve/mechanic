//! Command-line access to an existing Mechanic instance. Parsing never opens a window.

use crate::control::{
    self, AppearancePatch, InputMode, MoveEdge, Operation, Request, ResponseResult, SessionSelector,
};
use crate::panes::Axis;
use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSource {
    Text(String),
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    List,
    Read {
        pane: SessionSelector,
        lines: usize,
    },
    Send {
        pane: SessionSelector,
        source: TextSource,
        raw: bool,
        enter: bool,
    },
    Wait {
        pane: SessionSelector,
        after: u64,
        timeout_ms: u64,
    },
    Create {
        pane: Option<SessionSelector>,
        directory: Option<PathBuf>,
    },
    Split {
        pane: SessionSelector,
        axis: Axis,
        directory: Option<PathBuf>,
    },
    Focus {
        pane: SessionSelector,
    },
    Move {
        pane: SessionSelector,
        target: Option<SessionSelector>,
        edge: Option<MoveEdge>,
        new_window: bool,
    },
    Close {
        pane: SessionSelector,
    },
    Zoom {
        pane: SessionSelector,
    },
    Set {
        pane: SessionSelector,
        appearance: AppearancePatch,
    },
    SaveLoadout {
        name: String,
        include_directories: bool,
    },
    ListLoadouts,
    OpenLoadout {
        name: String,
        restore_directories: bool,
    },
    DeleteLoadout {
        name: String,
    },
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

/// Accept --socket before or after the command as well as at the app level.
pub fn parse(args: Vec<String>, mut socket: Option<PathBuf>) -> Result<Parsed, String> {
    let mut args = args.into_iter();
    let mut command = None;
    let mut loadout_action = None;
    let mut flags = BTreeMap::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--socket" => {
                if socket.is_some() {
                    return Err("--socket may only be supplied once".into());
                }
                let path = value(&mut args, "--socket")?;
                if path.is_empty() {
                    return Err("--socket requires a non-empty path".into());
                }
                socket = Some(PathBuf::from(path));
            }
            "--pane" | "--target" | "--lines" | "--text" | "--after" | "--timeout"
            | "--directory" | "--axis" | "--edge" | "--title" | "--text-color"
            | "--outline-color" | "--name" => {
                if flags.contains_key(&arg) {
                    return Err(format!("{arg} may only be supplied once"));
                }
                let supplied = value(&mut args, &arg)?;
                flags.insert(arg, Some(supplied));
            }
            "--stdin"
            | "--raw"
            | "--enter"
            | "--new-window"
            | "--clear-title"
            | "--clear-text-color"
            | "--clear-outline-color"
            | "--no-directories" => {
                if flags.insert(arg.clone(), None).is_some() {
                    return Err(format!("{arg} may only be supplied once"));
                }
            }
            "list" | "read" | "send" | "wait" | "instances" | "create" | "split" | "focus"
            | "move" | "close" | "zoom" | "set" | "loadout"
                if command.is_none() =>
            {
                command = Some(arg)
            }
            "save" | "list" | "open" | "delete"
                if command.as_deref() == Some("loadout") && loadout_action.is_none() =>
            {
                loadout_action = Some(arg)
            }
            other => {
                return Err(format!(
                    "unknown control argument '{other}'; try 'mechanic ctl --help'"
                ));
            }
        }
    }
    let command = command.ok_or("missing control command; try 'mechanic ctl --help'")?;
    let command = if command == "loadout" {
        format!(
            "loadout {}",
            loadout_action.ok_or("ctl loadout requires save, list, open, or delete")?
        )
    } else {
        command
    };
    let allowed: &[&str] = match command.as_str() {
        "list" | "instances" | "loadout list" => &[],
        "read" => &["--pane", "--lines"],
        "send" => &["--pane", "--text", "--stdin", "--raw", "--enter"],
        "wait" => &["--pane", "--after", "--timeout"],
        "create" => &["--pane", "--directory"],
        "split" => &["--pane", "--axis", "--directory"],
        "focus" | "close" | "zoom" => &["--pane"],
        "move" => &["--pane", "--target", "--edge", "--new-window"],
        "set" => &[
            "--pane",
            "--title",
            "--clear-title",
            "--text-color",
            "--clear-text-color",
            "--outline-color",
            "--clear-outline-color",
        ],
        "loadout save" | "loadout open" => &["--name", "--no-directories"],
        "loadout delete" => &["--name"],
        _ => unreachable!(),
    };
    for flag in flags.keys() {
        if !allowed.contains(&flag.as_str()) {
            return Err(format!("{flag} is not supported by ctl {command}"));
        }
    }
    if command == "instances" && socket.is_some() {
        return Err("ctl instances discovers all instances; --socket is not supported".into());
    }
    let pane = flags
        .get("--pane")
        .and_then(Option::as_deref)
        .map(str::parse::<SessionSelector>)
        .transpose()?;
    let require_pane =
        || pane.clone().ok_or_else(|| format!("ctl {command} requires --pane INSTANCE:SESSION"));
    let directory =
        flags.get("--directory").and_then(Option::as_deref).map(parse_directory).transpose()?;
    let supplied = |flag: &str| -> Result<&str, String> {
        flags
            .get(flag)
            .and_then(Option::as_deref)
            .ok_or_else(|| format!("ctl {command} requires {flag}"))
    };
    let name = || -> Result<String, String> {
        let name = supplied("--name")?;
        crate::session::validate_loadout_name(name)?;
        Ok(name.to_owned())
    };
    let command = match command.as_str() {
        "list" => Command::List,
        "instances" => Command::Instances,
        "read" => {
            let lines = match flags.get("--lines").and_then(Option::as_deref) {
                Some(value) => {
                    value.parse::<usize>().map_err(|_| "--lines must be a positive integer")?
                }
                None => 200,
            };
            if lines == 0 || lines > control::MAX_OUTPUT_LINES {
                return Err(format!("--lines must be between 1 and {}", control::MAX_OUTPUT_LINES));
            }
            Command::Read { pane: require_pane()?, lines }
        }
        "send" => {
            let source = match (
                flags.get("--text").and_then(Option::as_deref),
                flags.contains_key("--stdin"),
            ) {
                (Some(text), false) => {
                    validate_text_size(text.as_bytes())?;
                    TextSource::Text(text.to_owned())
                }
                (None, true) => TextSource::Stdin,
                _ => return Err("ctl send requires exactly one of --text or --stdin".into()),
            };
            Command::Send {
                pane: require_pane()?,
                source,
                raw: flags.contains_key("--raw"),
                enter: flags.contains_key("--enter"),
            }
        }
        "wait" => {
            let after = supplied("--after")?.parse::<u64>().map_err(
                |_| "--after must be a non-negative command ID returned by list or wait",
            )?;
            let timeout_ms = match flags.get("--timeout").and_then(Option::as_deref) {
                None => 30_000,
                Some(value) => {
                    let seconds = value
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
                    (seconds * 1000.0).ceil() as u64
                }
            };
            Command::Wait { pane: require_pane()?, after, timeout_ms }
        }
        "create" => Command::Create { pane, directory },
        "split" => {
            let axis = match supplied("--axis")? {
                "horizontal" => Axis::Horizontal,
                "vertical" => Axis::Vertical,
                _ => return Err("--axis must be horizontal or vertical".into()),
            };
            Command::Split { pane: require_pane()?, axis, directory }
        }
        "focus" => Command::Focus { pane: require_pane()? },
        "close" => Command::Close { pane: require_pane()? },
        "zoom" => Command::Zoom { pane: require_pane()? },
        "move" => {
            let target = flags
                .get("--target")
                .and_then(Option::as_deref)
                .map(str::parse::<SessionSelector>)
                .transpose()?;
            let edge = flags
                .get("--edge")
                .and_then(Option::as_deref)
                .map(|edge| match edge {
                    "left" => Ok(MoveEdge::Left),
                    "right" => Ok(MoveEdge::Right),
                    "top" => Ok(MoveEdge::Top),
                    "bottom" => Ok(MoveEdge::Bottom),
                    _ => Err("--edge must be left, right, top, or bottom".to_owned()),
                })
                .transpose()?;
            let new_window = flags.contains_key("--new-window");
            if (new_window && (target.is_some() || edge.is_some()))
                || (!new_window && (target.is_none() || edge.is_none()))
            {
                return Err("ctl move requires --target and --edge, or --new-window".into());
            }
            let pane = require_pane()?;
            if target.as_ref().is_some_and(|target| target.instance_id != pane.instance_id) {
                return Err("pane and target must belong to the same application instance".into());
            }
            Command::Move { pane, target, edge, new_window }
        }
        "set" => {
            let appearance = AppearancePatch {
                title: parse_patch(&flags, "--title", "--clear-title")?,
                text_color: parse_patch(&flags, "--text-color", "--clear-text-color")?,
                outline_color: parse_patch(&flags, "--outline-color", "--clear-outline-color")?,
            };
            if appearance.is_empty() {
                return Err("ctl set requires an appearance value or clear option".into());
            }
            let mut test = crate::session::PaneAppearance::default();
            appearance.apply(&mut test);
            test.validate()?;
            Command::Set { pane: require_pane()?, appearance }
        }
        "loadout save" => Command::SaveLoadout {
            name: name()?,
            include_directories: !flags.contains_key("--no-directories"),
        },
        "loadout list" => Command::ListLoadouts,
        "loadout open" => Command::OpenLoadout {
            name: name()?,
            restore_directories: !flags.contains_key("--no-directories"),
        },
        "loadout delete" => Command::DeleteLoadout { name: name()? },
        _ => unreachable!(),
    };
    Ok(Parsed::Command(Cli { socket, command }))
}

fn parse_patch(
    flags: &BTreeMap<String, Option<String>>,
    set: &str,
    clear: &str,
) -> Result<Option<Option<String>>, String> {
    match (flags.get(set).and_then(Option::as_ref), flags.contains_key(clear)) {
        (Some(_), true) => Err(format!("{set} and {clear} are mutually exclusive")),
        (Some(value), false) => Ok(Some(Some(value.clone()))),
        (None, true) => Ok(Some(None)),
        (None, false) => Ok(None),
    }
}
fn parse_directory(value: &str) -> Result<PathBuf, String> {
    let directory = PathBuf::from(value);
    if !directory.is_absolute()
        || value.len() > control::MAX_DIRECTORY_BYTES
        || value.contains('\0')
    {
        return Err("--directory must be an absolute path within the 4096 byte limit".into());
    }
    Ok(directory)
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
        Ok(true) => 0,
        Ok(false) => 1,
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
    let operation = match cli.command {
        Command::List => Operation::ListPanes,
        Command::Read { pane, lines } => Operation::ReadOutput {
            session: pane,
            max_lines: lines,
            max_bytes: control::MAX_OUTPUT_BYTES,
        },
        Command::Send { pane, source, raw, enter } => {
            let text = match source {
                TextSource::Text(text) => text,
                TextSource::Stdin => read_stdin(io::stdin().lock())?,
            };
            Operation::SendInput {
                session: pane,
                text,
                mode: if raw { InputMode::Raw } else { InputMode::Paste },
                enter,
            }
        }
        Command::Wait { pane, after, timeout_ms } => {
            Operation::Wait { session: pane, after_command_id: after, timeout_ms }
        }
        Command::Create { pane, directory } => Operation::CreatePane { session: pane, directory },
        Command::Split { pane, axis, directory } => {
            Operation::SplitPane { session: pane, axis, directory }
        }
        Command::Focus { pane } => Operation::FocusPane { session: pane },
        Command::Move { pane, target, edge, new_window } => {
            Operation::MovePane { session: pane, target, edge, new_window }
        }
        Command::Close { pane } => Operation::ClosePane { session: pane },
        Command::Zoom { pane } => Operation::ZoomPane { session: pane },
        Command::Set { pane, appearance } => Operation::SetPane { session: pane, appearance },
        Command::SaveLoadout { name, include_directories } => {
            Operation::SaveLoadout { name, include_directories }
        }
        Command::ListLoadouts => Operation::ListLoadouts,
        Command::OpenLoadout { name, restore_directories } => {
            Operation::OpenLoadout { name, restore_directories }
        }
        Command::DeleteLoadout { name } => Operation::DeleteLoadout { name },
        Command::Instances => unreachable!(),
    };
    let selector = operation.session().cloned();
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
        "mechanic ctl — control an existing terminal instance\n\nUSAGE:\n    mechanic [--socket PATH] ctl COMMAND [OPTIONS]\n\nCOMMANDS:\n    list                         List pane handles and command IDs\n    create [--pane HANDLE] [--directory PATH]  Open a window, optionally inherit a pane directory\n    split --pane HANDLE --axis horizontal|vertical [--directory PATH]\n    focus --pane HANDLE          Focus a pane and its window\n    move --pane HANDLE (--target HANDLE --edge left|right|top|bottom | --new-window)\n    close --pane HANDLE          Close a pane and its shell\n    zoom --pane HANDLE           Toggle pane zoom\n    set --pane HANDLE [--title TEXT | --clear-title]\n        [--text-color '#RRGGBB' | --clear-text-color]\n        [--outline-color '#RRGGBB' | --clear-outline-color]\n    loadout save --name NAME [--no-directories]\n    loadout list\n    loadout open --name NAME [--no-directories]\n    loadout delete --name NAME\n    read --pane HANDLE [--lines N]\n    send --pane HANDLE (--text TEXT | --stdin) [--raw] [--enter]\n    wait --pane HANDLE --after COMMAND_ID [--timeout SECONDS]\n    instances                    Discover instances and socket paths\n\nOPTIONS:\n    --socket PATH                Select an instance (also accepted after ctl)\n    --directory PATH             Absolute starting directory\n    --lines N                    Read up to N lines (default 200, maximum 5000)\n    --stdin                      Read UTF-8 input, at most 128 KiB\n    --raw                        Send UTF-8 bytes directly, including controls\n    --enter                      Send Enter separately after input\n    --timeout SECONDS            Wait limit (default 30, maximum 60)\n    --no-directories             Save or open a loadout without saved directories\n    -h, --help                   Show this help and exit\n\nHANDLE is INSTANCE:SESSION, returned by list and pane mutations.\nLoadouts open fresh shells in additional windows; saved directories are used by default.\nSend pastes text by default. Enter is sent only with --enter.\nResults and errors are JSON; errors and wait timeouts exit nonzero."
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
    fn existing_send_and_wait_semantics_are_preserved() {
        let Parsed::Command(cli) =
            parse_flags(&["send", "--pane", HANDLE, "--text", "--help"]).unwrap()
        else {
            panic!()
        };
        assert!(
            matches!(cli.command, Command::Send { source: TextSource::Text(text), raw: false, enter: false, .. } if text == "--help")
        );
        let Parsed::Command(cli) =
            parse_flags(&["wait", "--pane", HANDLE, "--after", "42", "--timeout", "0.125"])
                .unwrap()
        else {
            panic!()
        };
        assert!(matches!(cli.command, Command::Wait { after: 42, timeout_ms: 125, .. }));
    }
    #[test]
    fn socket_can_precede_or_follow_command() {
        let before = parse_flags(&["--socket", "/tmp/socket", "list"]).unwrap();
        assert_eq!(before, parse_flags(&["list", "--socket", "/tmp/socket"]).unwrap());
        assert_eq!(before, parse(vec!["list".into()], Some("/tmp/socket".into())).unwrap());
    }
    #[test]
    fn pane_creation_moves_and_appearance_are_explicit() {
        let Parsed::Command(cli) =
            parse_flags(&["split", "--pane", HANDLE, "--axis", "vertical", "--directory", "/tmp"])
                .unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            cli.command,
            Command::Split { axis: Axis::Vertical, directory: Some(_), .. }
        ));
        let Parsed::Command(cli) =
            parse_flags(&["move", "--pane", HANDLE, "--new-window"]).unwrap()
        else {
            panic!()
        };
        assert!(matches!(
            cli.command,
            Command::Move { new_window: true, target: None, edge: None, .. }
        ));
        let Parsed::Command(cli) =
            parse_flags(&["set", "--pane", HANDLE, "--clear-title", "--text-color", "#123AbC"])
                .unwrap()
        else {
            panic!()
        };
        let Command::Set { appearance, .. } = cli.command else { panic!() };
        assert_eq!(appearance.title, Some(None));
        assert_eq!(appearance.text_color, Some(Some("#123AbC".into())));
        assert_eq!(appearance.outline_color, None);
    }
    #[test]
    fn loadouts_use_directories_unless_disabled() {
        let Parsed::Command(cli) = parse_flags(&["loadout", "save", "--name", "work"]).unwrap()
        else {
            panic!()
        };
        assert!(matches!(cli.command, Command::SaveLoadout { include_directories: true, .. }));
        let Parsed::Command(cli) =
            parse_flags(&["loadout", "open", "--name", "work", "--no-directories"]).unwrap()
        else {
            panic!()
        };
        assert!(matches!(cli.command, Command::OpenLoadout { restore_directories: false, .. }));
    }
    #[test]
    fn invalid_options_and_ambiguous_mutations_are_rejected() {
        for flags in [
            vec![],
            vec!["bogus"],
            vec!["read"],
            vec!["read", "--pane", "7"],
            vec!["read", "--pane", "0123456789abcdef0123456789abcdef:0"],
            vec!["wait", "--pane", HANDLE],
            vec!["send", "--pane", HANDLE],
            vec!["send", "--pane", HANDLE, "--stdin", "--text", "x"],
            vec!["read", "--pane", HANDLE, "--lines", "0"],
            vec!["wait", "--pane", HANDLE, "--after", "3", "--timeout", "NaN"],
            vec!["wait", "--pane", HANDLE, "--after", "3", "--timeout", "61"],
            vec!["list", "--raw"],
            vec!["list", "--socket"],
            vec!["loadout"],
            vec!["create", "--directory", "relative"],
            vec!["split", "--pane", HANDLE, "--axis", "diagonal"],
            vec!["move", "--pane", HANDLE, "--target", HANDLE],
            vec!["move", "--pane", HANDLE, "--new-window", "--edge", "left"],
            vec!["set", "--pane", HANDLE],
            vec!["set", "--pane", HANDLE, "--title", "x", "--clear-title"],
            vec!["set", "--pane", HANDLE, "--text-color", "red"],
            vec!["set", "--pane", HANDLE, "--title", "bad\ntext"],
            vec!["loadout", "save", "--name", " bad"],
            vec!["loadout", "list", "--name", "work"],
            vec!["zoom", "--pane", HANDLE, "--pane", HANDLE],
            vec!["send", "--pane", HANDLE, "--stdin", "--raw", "--raw"],
        ] {
            assert!(parse_flags(&flags).is_err(), "accepted {flags:?}");
        }
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
