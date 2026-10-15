//! The four places the owner's client has to know which system it is on.
//!
//! The client is one download per machine and the machines are not alike. Most
//! of the crate does not care: the wire is bytes, the screen is a parser, and
//! the pseudo-terminal comes from a crate that already knows both systems. What
//! is left is this file — the shell, the flag that shell takes, the home
//! directory, and (in `consent`) the keyboard.
//!
//! Everything here is decided at compile time. There is no runtime branch on
//! the current system, so the Unix build contains no Windows code and the
//! Windows build contains no Unix code.

/// The shell a task runs in when the operator names no command.
///
/// `TETHER_SHELL` overrides it everywhere, which is the escape hatch for a
/// machine whose shell is somewhere this does not look.
pub fn default_shell() -> String {
    if let Ok(shell) = std::env::var("TETHER_SHELL") {
        if !shell.is_empty() {
            return shell;
        }
    }
    platform_shell()
}

#[cfg(unix)]
fn platform_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

#[cfg(windows)]
fn platform_shell() -> String {
    // PowerShell where it exists, the command processor where it does not.
    // PowerShell has shipped with Windows since 7, but a stripped install or a
    // policy that removed it still answers with the command processor — and a
    // shell that is not there is a task that cannot start at all. The command
    // processor is the one program guaranteed to be present.
    for name in ["pwsh.exe", "powershell.exe"] {
        if let Some(found) = on_path(name) {
            return found;
        }
    }
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
}

#[cfg(not(any(unix, windows)))]
fn platform_shell() -> String {
    "/bin/sh".into()
}

#[cfg(windows)]
fn on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

/// The flag that hands a shell one command line to run.
///
/// It travels with the shell rather than being a constant because the two have
/// to agree. A command processor given a POSIX flag treats it as a file name
/// and reports that it cannot find it, which reads to the operator like the
/// command failing rather than like the shell never seeing it.
pub fn command_flag(shell: &str) -> &'static str {
    #[cfg(unix)]
    {
        let _ = shell;
        "-c"
    }

    #[cfg(windows)]
    {
        let name = shell.to_ascii_lowercase();
        if name.contains("powershell") || name.contains("pwsh") {
            "-Command"
        } else {
            "/C"
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = shell;
        "-c"
    }
}

/// Where an interactive shell starts, which is where the owner expects to find
/// themselves. Two systems, two names for one directory.
pub fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|home| !home.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_matches_the_shell() {
        #[cfg(unix)]
        {
            assert_eq!(command_flag("/bin/sh"), "-c");
            assert_eq!(command_flag("/usr/bin/zsh"), "-c");
        }

        #[cfg(windows)]
        {
            assert_eq!(
                command_flag(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
                "-Command"
            );
            assert_eq!(
                command_flag(r"C:\Program Files\PowerShell\7\pwsh.exe"),
                "-Command"
            );
            assert_eq!(command_flag(r"C:\Windows\system32\cmd.exe"), "/C");
        }
    }

    #[test]
    fn the_shell_can_be_named_outright() {
        // The override is spelled the same on every system, which is what makes
        // it usable as an instruction read out over the phone.
        std::env::set_var("TETHER_SHELL", "/somewhere/else");
        assert_eq!(default_shell(), "/somewhere/else");
        std::env::remove_var("TETHER_SHELL");
    }
}
