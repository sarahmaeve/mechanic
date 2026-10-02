//! Child-only zsh startup injection; user startup files are never modified.

use std::io;
use std::path::Path;

use alacritty_terminal::tty::{Options, Shell};
use mechanic_config::ShellConfig;

/// Keep the private startup directory alive until the PTY is dropped.
pub(crate) fn configure(
    config: &ShellConfig,
    options: &mut Options,
) -> io::Result<Option<tempfile::TempDir>> {
    options.env.insert("TERM_PROGRAM".into(), "mechanic".into());
    if !config.integration
        || Path::new(&config.program).file_name().is_none_or(|name| name != "zsh")
    {
        return Ok(None);
    }
    let directory = tempfile::Builder::new().prefix("mechanic-zsh-").tempdir()?;
    std::fs::write(directory.path().join(".zshenv"), ZSH_ENV)?;
    std::fs::write(directory.path().join(".zprofile"), ZSH_PROFILE)?;
    std::fs::write(directory.path().join(".zshrc"), ZSH_RC)?;
    std::fs::write(directory.path().join("integration.zsh"), ZSH_HOOKS)?;
    options
        .env
        .insert("MECHANIC_ZSH_DIRECTORY".into(), directory.path().to_string_lossy().into_owned());
    if let Some(original) = std::env::var_os("ZDOTDIR") {
        options
            .env
            .insert("MECHANIC_ORIGINAL_ZDOTDIR".into(), original.to_string_lossy().into_owned());
        options.env.insert("MECHANIC_ZDOTDIR_SET".into(), "1".into());
    } else {
        // Override values potentially inherited from an older Mechanic process.
        options.env.insert("MECHANIC_ZDOTDIR_SET".into(), "0".into());
    }
    options.env.insert("ZDOTDIR".into(), directory.path().to_string_lossy().into_owned());
    options.shell = Some(Shell::new(config.program.clone(), vec![]));
    Ok(Some(directory))
}

const ZSH_ENV: &str = r#"# Mechanic startup shim. Restore the user's ZDOTDIR before sourcing.
typeset -g _mechanic_zsh_directory=$MECHANIC_ZSH_DIRECTORY
typeset -g _mechanic_zdotdir_set=$MECHANIC_ZDOTDIR_SET
typeset -g _mechanic_zdotdir=$MECHANIC_ORIGINAL_ZDOTDIR
unset MECHANIC_ZSH_DIRECTORY MECHANIC_ZDOTDIR_SET MECHANIC_ORIGINAL_ZDOTDIR
_mechanic_restore_zdotdir() {
    if [[ $_mechanic_zdotdir_set == 1 ]]; then
        ZDOTDIR=$_mechanic_zdotdir
    else
        unset ZDOTDIR
    fi
}
_mechanic_save_zdotdir() {
    _mechanic_zdotdir_set=${+ZDOTDIR}
    _mechanic_zdotdir=${ZDOTDIR-}
}
_mechanic_restore_zdotdir
[[ -r ${ZDOTDIR-$HOME}/.zshenv ]] && source "${ZDOTDIR-$HOME}/.zshenv"
_mechanic_save_zdotdir
if [[ -o rcs ]]; then
    ZDOTDIR=$_mechanic_zsh_directory
else
    # User .zshenv can deliberately disable the remaining startup files.
    _mechanic_restore_zdotdir
    unfunction _mechanic_restore_zdotdir _mechanic_save_zdotdir
    unset _mechanic_zdotdir_set _mechanic_zdotdir _mechanic_zsh_directory
fi
"#;

const ZSH_PROFILE: &str = r#"_mechanic_restore_zdotdir
[[ -r ${ZDOTDIR-$HOME}/.zprofile ]] && source "${ZDOTDIR-$HOME}/.zprofile"
_mechanic_save_zdotdir
ZDOTDIR=$_mechanic_zsh_directory
"#;

const ZSH_RC: &str = r#"_mechanic_restore_zdotdir
[[ -r ${ZDOTDIR-$HOME}/.zshrc ]] && source "${ZDOTDIR-$HOME}/.zshrc"
source "$_mechanic_zsh_directory/integration.zsh"
unfunction _mechanic_restore_zdotdir _mechanic_save_zdotdir
unset _mechanic_zdotdir_set _mechanic_zdotdir _mechanic_zsh_directory
"#;

const ZSH_HOOKS: &str = r#"# OSC 133 contains boundaries and exit status only, never command text.
[[ -o interactive ]] || return
_mechanic_install_hooks() {
emulate -L zsh
typeset -g _mechanic_command_active=0
_mechanic_cwd() {
    emulate -L zsh
    local LC_ALL=C
    local working_directory=$PWD encoded='' char hex
    local -i index
    for ((index=1; index <= ${#working_directory}; index++)); do
        char=$working_directory[index]
        if [[ $char == [-a-zA-Z0-9/._~] ]]; then
            encoded+=$char
        else
            builtin printf -v hex '%02X' "'$char"
            encoded+="%$hex"
        fi
    done
    builtin printf '\e]7;file://localhost%s\a' "$encoded"
}
_mechanic_preexec() {
    emulate -L zsh
    _mechanic_command_active=1
    builtin printf '\e]133;C\a'
}
_mechanic_precmd() {
    emulate -L zsh
    local -i command_status=$1
    if (( _mechanic_command_active )); then
        builtin printf '\e]133;D;%d\a' "$command_status"
        _mechanic_command_active=0
    fi
    _mechanic_cwd
    builtin printf '\e]133;A\a'
}
# precmd runs before precmd_functions. Capture status there before user hooks
# can change it, then preserve the original status seen by the user's precmd.
_mechanic_return_status() { return "$1"; }
if (( ${+functions[precmd]} )); then
    functions[_mechanic_user_precmd]=$functions[precmd]
    precmd() {
        local -i mechanic_status=$?
        _mechanic_precmd "$mechanic_status"
        # The condition restores $? without ERR_RETURN aborting this wrapper
        # for a failed command. Both branches enter the user hook with that
        # status, while the user's hook retains its normal error semantics.
        if _mechanic_return_status "$mechanic_status"; then
            _mechanic_user_precmd "$@"
        else
            _mechanic_user_precmd "$@"
        fi
    }
else
    precmd() {
        local -i mechanic_status=$?
        _mechanic_precmd "$mechanic_status"
    }
fi
_mechanic_prompt_end() {
    emulate -L zsh
    local marker=$'%{\e]133;B\a%}'
    [[ $PS1 == *$marker ]] || PS1+=$marker
}
typeset -ga preexec_functions precmd_functions
preexec_functions=(_mechanic_preexec ${preexec_functions:#_mechanic_preexec})
precmd_functions=(${precmd_functions:#_mechanic_prompt_end} _mechanic_prompt_end)
}
_mechanic_install_hooks
unfunction _mechanic_install_hooks
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::tty::{self, EventedReadWrite};
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    fn read_until(pty: &mut tty::Pty, needle: &[u8]) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        while !output.windows(needle.len()).any(|window| window == needle) {
            assert!(
                Instant::now() < deadline,
                "PTY stalled: {:?}",
                String::from_utf8_lossy(&output)
            );
            match pty.reader().read(&mut buffer) {
                Ok(0) => panic!("PTY closed before marker"),
                Ok(length) => output.extend_from_slice(&buffer[..length]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => panic!("PTY read: {error}"),
            }
        }
        output
    }

    fn launch_zsh(user: &Path) -> (tempfile::TempDir, tty::Pty) {
        let config = ShellConfig { program: "/bin/zsh".into(), integration: true };
        let mut options = Options::default();
        let integration = configure(&config, &mut options).unwrap().unwrap();
        options.env.insert("MECHANIC_ZDOTDIR_SET".into(), "1".into());
        options.env.insert("MECHANIC_ORIGINAL_ZDOTDIR".into(), user.to_string_lossy().into_owned());
        let pty = {
            let _spawn = crate::pty::SPAWN_LOCK.lock().unwrap();
            tty::setup_env();
            tty::new(&options, crate::TerminalSize::default().to_window_size(), 0).unwrap()
        };
        (integration, pty)
    }

    fn wait_terminal(terminal: &mut crate::Terminal, ready: impl Fn(&crate::Terminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let outcome = terminal.process_input();
            assert!(outcome.io_error.is_none(), "{:?}", outcome.io_error);
            if ready(terminal) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shell metadata stalled: {:?}",
                terminal.shell_integration()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn automatic_zsh_hooks_drive_terminal_metadata_output_and_navigation() {
        use alacritty_terminal::grid::Dimensions;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Arc;

        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let user = fixture.path().join("startup");
        std::fs::create_dir(&user).unwrap();
        std::fs::write(user.join(".zshrc"), "PS1='TEST_PROMPT>'\n").unwrap();
        // A child-only launcher provides isolated startup files without
        // changing the process environment. Its basename exercises the same
        // automatic integration path used by a configured zsh executable.
        let executable = fixture.path().join("zsh");
        std::fs::write(&executable, format!("#!/bin/sh\nMECHANIC_ZDOTDIR_SET=1\nMECHANIC_ORIGINAL_ZDOTDIR='{}'\nexport MECHANIC_ZDOTDIR_SET MECHANIC_ORIGINAL_ZDOTDIR\nexec /bin/zsh\n", user.display())).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = mechanic_config::Config::default();
        config.shell.program = executable.to_string_lossy().into_owned();
        let size = crate::TerminalSize { columns: 80, rows: 6, ..Default::default() };
        let mut terminal = crate::Terminal::new(&config, size, Arc::new(|| {})).unwrap();
        wait_terminal(&mut terminal, |terminal| {
            terminal
                .shell_integration()
                .commands()
                .back()
                .is_some_and(|command| command.command_start.is_some())
        });

        terminal.write_to_pty(b"printf 'one\\ntwo\\n'\n").unwrap();
        wait_terminal(&mut terminal, |terminal| {
            terminal.shell_integration().last_exit_status() == Some(0)
                && terminal
                    .shell_integration()
                    .commands()
                    .back()
                    .is_some_and(|command| command.command_start.is_some())
        });
        assert_eq!(terminal.last_command_output().as_deref(), Some("one\ntwo"));
        terminal.write_to_pty(b"false\n").unwrap();
        wait_terminal(&mut terminal, |terminal| {
            terminal.shell_integration().last_exit_status() == Some(1)
                && terminal
                    .shell_integration()
                    .commands()
                    .back()
                    .is_some_and(|command| command.command_start.is_some())
        });
        assert_eq!(terminal.last_command_output().as_deref(), Some(""));

        terminal
            .write_to_pty("printf 'Grüße — 日本語 — مرحبًا\\n'; (exit 42)\n".as_bytes())
            .unwrap();
        wait_terminal(&mut terminal, |terminal| {
            terminal.shell_integration().last_exit_status() == Some(42)
                && terminal
                    .shell_integration()
                    .commands()
                    .back()
                    .is_some_and(|command| command.command_start.is_some())
        });
        assert_eq!(terminal.last_command_output().as_deref(), Some("Grüße — 日本語 — مرحبًا"));

        terminal
            .write_to_pty(b"sh -c 'printf \"INTERRUPT_%s\\n\" READY; exec sleep 30'\n")
            .unwrap();
        // Preexec runs before the child owns the foreground process group.
        // Wait for child output before delivering the terminal interrupt.
        wait_terminal(&mut terminal, |terminal| {
            terminal.shell_integration().is_running()
                && terminal
                    .grid()
                    .display_iter()
                    .map(|cell| cell.cell.c)
                    .collect::<String>()
                    .contains("INTERRUPT_READY")
        });
        terminal.write_to_pty(b"\x03").unwrap();
        wait_terminal(&mut terminal, |terminal| {
            !terminal.shell_integration().is_running()
                && terminal.shell_integration().last_exit_status() == Some(130)
                && terminal
                    .shell_integration()
                    .commands()
                    .back()
                    .is_some_and(|command| command.command_start.is_some())
        });

        let cwd = fixture.path().join("cwd é #%;");
        std::fs::create_dir(&cwd).unwrap();
        terminal.write_to_pty(format!("cd '{}'\n", cwd.display()).as_bytes()).unwrap();
        wait_terminal(&mut terminal, |terminal| {
            terminal.shell_integration().cwd() == Some(cwd.to_string_lossy().as_ref())
                && terminal
                    .shell_integration()
                    .commands()
                    .back()
                    .is_some_and(|command| command.command_start.is_some())
        });
        assert_eq!(terminal.shell_integration().last_exit_status(), Some(0));

        terminal.write_to_pty(b"printf 'row\\n%.0s' {1..20}\n").unwrap();
        wait_terminal(&mut terminal, |terminal| {
            terminal.last_command_output().is_some_and(|output| output.lines().count() == 20)
                && terminal
                    .shell_integration()
                    .commands()
                    .back()
                    .is_some_and(|command| command.command_start.is_some())
        });
        assert!(terminal.grid().history_size() > 0);
        terminal.scroll_up(2);
        assert_eq!(terminal.grid().display_offset(), 2);
        assert!(terminal.jump_to_previous_prompt());
        let previous_offset = terminal.grid().display_offset();
        assert!(previous_offset > 0);
        assert!(terminal.jump_to_next_prompt());
        assert!(terminal.grid().display_offset() < previous_offset);
        terminal.write_to_pty(b"exit\n").unwrap();
    }

    #[test]
    fn zsh_hooks_preserve_startup_status_and_encode_cwd() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let user = tempfile::tempdir().unwrap();
        let changed = user.path().join("changed startup");
        std::fs::create_dir(&changed).unwrap();
        // The original .zshenv changes ZDOTDIR; the shim must honor the change.
        std::fs::write(
            user.path().join(".zshenv"),
            format!("ZDOTDIR='{}'\nprint STARTUP_ENV\n", changed.display()),
        )
        .unwrap();
        std::fs::write(
            changed.join(".zshrc"),
            r#"print STARTUP_RC
PS1='TEST_PROMPT>'
precmd() { print USER_STATUS:$?; }
user_precmd() { print USER_PRECMD_ARRAY:$?; }
user_preexec() { print USER_PREEXEC; }
precmd_functions=(user_precmd)
preexec_functions=(user_preexec)
stty -echo
"#,
        )
        .unwrap();
        let (_integration, mut pty) = launch_zsh(user.path());
        let initial = read_until(&mut pty, b"\x1b]133;B\x07");
        let initial = String::from_utf8_lossy(&initial);
        assert!(initial.contains("STARTUP_ENV") && initial.contains("STARTUP_RC"));
        assert!(initial.contains("USER_PRECMD_ARRAY"));
        assert!(initial.contains("\x1b]133;A\x07"));
        assert!(!initial.contains("\x1b]133;D;"));

        pty.writer().write_all(b"false\n").unwrap();
        let failed = read_until(&mut pty, b"\x1b]133;B\x07");
        let failed = String::from_utf8_lossy(&failed);
        assert!(failed.contains("\x1b]133;C\x07"));
        assert!(failed.contains("\x1b]133;D;1\x07"), "{failed:?}");
        assert!(failed.contains("USER_STATUS:1"), "{failed:?}");
        assert!(failed.contains("USER_PRECMD_ARRAY:1"), "{failed:?}");
        assert!(failed.contains("USER_PREEXEC") && failed.contains("USER_PRECMD_ARRAY"));

        let cwd = user.path().join("a #%;é");
        std::fs::create_dir(&cwd).unwrap();
        pty.writer().write_all(format!("cd '{}'\n", cwd.display()).as_bytes()).unwrap();
        let success = read_until(&mut pty, b"\x1b]133;B\x07");
        let success = String::from_utf8_lossy(&success);
        assert!(success.contains("\x1b]133;D;0\x07"));
        assert!(success.contains("/a%20%23%25%3B%C3%A9\x07"), "{success:?}");
        // A leading space can suppress shell history; integration still must
        // avoid sending command text in its protocol payloads.
        pty.writer().write_all(b" SECRET_FOR_TEST=hidden\n").unwrap();
        let private = read_until(&mut pty, b"\x1b]133;B\x07");
        for sequence in String::from_utf8_lossy(&private).split("\x1b]").skip(1) {
            let payload = sequence.split('\x07').next().unwrap();
            assert!(!payload.contains("SECRET_FOR_TEST"));
        }

        pty.writer().write_all(b"print -r -- RESTORED:$ZDOTDIR PROGRAM:$TERM_PROGRAM\n").unwrap();
        let restored = read_until(&mut pty, b"\x1b]133;B\x07");
        let restored = String::from_utf8_lossy(&restored);
        assert!(restored.contains(&format!("RESTORED:{} PROGRAM:mechanic", changed.display())));
        pty.writer().write_all(b"exit\n").unwrap();
    }

    #[test]
    fn zsh_without_existing_precmd_reports_failure_and_prompt_end() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let user = tempfile::tempdir().unwrap();
        std::fs::write(user.path().join(".zshrc"), "PS1='TEST_PROMPT>'\nuser_array_precmd() { print ARRAY_STATUS:$?; }\nprecmd_functions=(user_array_precmd)\n").unwrap();
        let (_integration, mut pty) = launch_zsh(user.path());
        read_until(&mut pty, b"\x1b]133;B\x07");
        pty.writer().write_all(b"false\n").unwrap();
        let failed = read_until(&mut pty, b"\x1b]133;B\x07");
        assert!(String::from_utf8_lossy(&failed).contains("\x1b]133;D;1\x07"));
        assert!(
            String::from_utf8_lossy(&failed).contains("ARRAY_STATUS:1"),
            "{}",
            String::from_utf8_lossy(&failed)
        );
        pty.writer().write_all(b"exit\n").unwrap();
    }

    #[test]
    fn zsh_err_return_preserves_user_hooks_and_prompt_status() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let user = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join(".zshrc"),
            r#"PS1='TEST_STATUS:%?>'
precmd() { print -r -- USER_STATUS:$?; }
user_array_precmd() { print -r -- ARRAY_STATUS:$?; }
precmd_functions=(user_array_precmd)
setopt errreturn
stty -echo
"#,
        )
        .unwrap();
        let (_integration, mut pty) = launch_zsh(user.path());
        let initial = read_until(&mut pty, b"\x1b]133;B\x07");
        let initial = String::from_utf8_lossy(&initial);
        assert!(initial.contains("USER_STATUS:0"), "{initial:?}");
        assert!(initial.contains("ARRAY_STATUS:0"), "{initial:?}");
        assert!(initial.contains("TEST_STATUS:0>"), "{initial:?}");

        for (command, expected) in [("false\n", 1), ("true\n", 0), ("(exit 7)\n", 7)] {
            pty.writer().write_all(command.as_bytes()).unwrap();
            let output = read_until(&mut pty, b"\x1b]133;B\x07");
            let output = String::from_utf8_lossy(&output);
            for marker in [
                format!("\x1b]133;D;{expected}\x07"),
                format!("USER_STATUS:{expected}"),
                format!("ARRAY_STATUS:{expected}"),
                format!("TEST_STATUS:{expected}>"),
            ] {
                assert!(output.contains(&marker), "missing {marker:?}: {output:?}");
            }
        }
        // Zsh renders its PROMPT_SP end-of-line indicator before precmd runs.
        // Keep that user behavior: copied current-grid output can include it.
        pty.writer().write_all(b"printf 'NO_NEWLINE_OUTPUT'\n").unwrap();
        let output = read_until(&mut pty, b"\x1b]133;B\x07");
        let output = String::from_utf8_lossy(&output);
        let start = output.find("NO_NEWLINE_OUTPUT").expect("command output");
        let end = output.find("\x1b]133;D;0\x07").expect("completion after output");
        assert!(start < end, "{output:?}");
        assert!(output[start..end].contains('%'), "missing zsh PROMPT_SP indicator: {output:?}");
        pty.writer().write_all(b"exit\n").unwrap();
    }

    #[test]
    fn disabled_startup_restores_zdotdir_without_installing_hooks() {
        if !Path::new("/bin/zsh").exists() {
            return;
        }
        let user = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join(".zshenv"),
            "unsetopt rcs\nPS1='DISABLED_PROMPT>'\nprint -r -- RESTORED:$ZDOTDIR\n",
        )
        .unwrap();
        let (_integration, mut pty) = launch_zsh(user.path());
        let initial = read_until(&mut pty, b"DISABLED_PROMPT>");
        let initial = String::from_utf8_lossy(&initial);
        assert!(initial.contains(&format!("RESTORED:{}", user.path().display())));
        assert!(!initial.contains("\x1b]133;"));
        pty.writer().write_all(b"exit\n").unwrap();
    }

    #[test]
    fn opt_out_and_unsupported_shells_keep_their_launch_command() {
        for (program, integration) in [("/bin/zsh", false), ("/bin/bash", true)] {
            let config = ShellConfig { program: program.into(), integration };
            let mut options = Options::default();
            let shell = Shell::new(program.into(), vec![]);
            options.shell = Some(shell.clone());
            assert!(configure(&config, &mut options).unwrap().is_none());
            assert_eq!(options.shell, Some(shell));
            assert!(!options.env.contains_key("ZDOTDIR"));
            assert_eq!(options.env["TERM_PROGRAM"], "mechanic");
        }
    }
}
