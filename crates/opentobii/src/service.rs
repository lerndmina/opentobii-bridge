//! Starting and stopping the daemon, and starting it at login.
//!
//! The daemon is run by the user's own systemd, as the unit
//! `opentobii.service`. A package installs that unit; when there is none
//! (a build run from its folder, or a bare binary) one is written into the
//! user's own unit folder, pointing at this executable. Nothing here needs
//! root.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNIT: &str = "opentobii.service";

/// What systemd says about the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    /// `systemctl --user` works here.
    pub systemd: bool,
    /// A unit file exists, from a package or written by us.
    pub installed: bool,
    /// The daemon starts at login.
    pub enabled: bool,
    /// systemd is running the daemon now.
    pub active: bool,
}

/// The unit file for an executable at `exe`.
#[must_use]
pub fn unit_text(exe: &Path) -> String {
    // systemd reads the path as a command line, so one with a space in it
    // has to be quoted.
    let path = exe.display().to_string();
    let command = if path.contains(char::is_whitespace) {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        path
    };
    format!(
        "[Unit]\n\
         Description=opentobii bridge (Tobii Eye Tracker 5 for games under Proton)\n\
         Documentation=https://github.com/lerndmina/opentobii-bridge\n\
         After=tobiid.service\n\
         Wants=tobiid.socket\n\
         \n\
         [Service]\n\
         ExecStart={command} daemon\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// Where a unit written by us goes: `~/.config/systemd/user/`.
#[must_use]
pub fn user_unit_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?).join(".config")
        }
    };
    Some(base.join("systemd/user").join(UNIT))
}

fn systemctl(args: &[&str]) -> Option<std::process::Output> {
    Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .ok()
}

fn succeeds(args: &[&str]) -> bool {
    systemctl(args).is_some_and(|output| output.status.success())
}

/// Run `systemctl --user` with `args`; the error is what it printed.
fn run(args: &[&str]) -> Result<(), String> {
    let output = systemctl(args).ok_or("systemctl could not be run: is this a systemd system?")?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if said.is_empty() {
        format!("systemctl {} failed", args.join(" "))
    } else {
        said
    })
}

/// Read the daemon's state from systemd.
#[must_use]
pub fn state() -> State {
    let systemd = systemctl(&["--version"]).is_some_and(|output| output.status.success());
    if !systemd {
        return State::default();
    }
    State {
        systemd,
        installed: succeeds(&["cat", UNIT]),
        enabled: succeeds(&["is-enabled", "--quiet", UNIT]),
        active: succeeds(&["is-active", "--quiet", UNIT]),
    }
}

/// Make sure a unit exists, writing one for this executable if no package
/// installed one. Returns the path written, if any.
pub fn ensure_unit() -> Result<Option<PathBuf>, String> {
    if succeeds(&["cat", UNIT]) {
        return Ok(None);
    }
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot find this program's own path: {err}"))?;
    let path = user_unit_path().ok_or("HOME is not set")?;
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder).map_err(|err| format!("{}: {err}", folder.display()))?;
    }
    std::fs::write(&path, unit_text(&exe)).map_err(|err| format!("{}: {err}", path.display()))?;
    run(&["daemon-reload"])?;
    Ok(Some(path))
}

/// Start the daemon now.
pub fn start() -> Result<(), String> {
    ensure_unit()?;
    run(&["start", UNIT])
}

/// Stop the daemon.
pub fn stop() -> Result<(), String> {
    run(&["stop", UNIT])
}

/// Stop and start the daemon, as after an upgrade.
pub fn restart() -> Result<(), String> {
    ensure_unit()?;
    run(&["restart", UNIT])
}

/// Start the daemon at every login, or stop doing so. Does not start or
/// stop it now.
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled {
        ensure_unit()?;
        run(&["enable", UNIT])
    } else {
        run(&["disable", UNIT])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_runs_this_executable_as_the_daemon() {
        let text = unit_text(Path::new("/usr/bin/opentobii"));
        assert!(
            text.contains("\nExecStart=/usr/bin/opentobii daemon\n"),
            "{text}"
        );
        assert!(text.contains("[Install]\nWantedBy=default.target\n"));
        assert!(text.contains("Restart=on-failure"));
        assert!(text.starts_with("[Unit]\n"));
    }

    #[test]
    fn a_path_with_a_space_is_quoted() {
        let text = unit_text(Path::new("/home/a user/my \"apps\"/opentobii"));
        assert!(
            text.contains("\nExecStart=\"/home/a user/my \\\"apps\\\"/opentobii\" daemon\n"),
            "{text}"
        );
    }

    #[test]
    fn the_shipped_unit_matches_what_would_be_written() {
        let shipped = include_str!("../../../packaging/opentobii.service");
        assert_eq!(shipped, unit_text(Path::new("/usr/bin/opentobii")));
    }
}
