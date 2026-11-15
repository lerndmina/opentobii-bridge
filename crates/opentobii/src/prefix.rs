//! Install the helper into a Wine or Proton prefix, and remove it again.
//!
//! Two things go into the prefix: the helper executable under
//! `drive_c/opentobii/`, and one registry key in `system.reg` that
//! registers it as an auto-start Windows service. Nothing outside the
//! prefix is touched.
//!
//! `system.reg` is edited as text, which is only safe while no wineserver
//! has the prefix open: Wine rewrites the file from memory when it shuts
//! down. Both operations refuse to run if one does, and keep a backup of
//! the file they edit.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// The helper built with this binary; empty when it was built without the
/// MinGW compiler.
pub const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sesp_relay.exe"));

pub const SERVICE: &str = "OpenTobiiRelay";
/// Wine keeps services under `ControlSet001`; `CurrentControlSet` is a link
/// to it.
const KEY: &str = "[System\\\\ControlSet001\\\\Services\\\\OpenTobiiRelay]";
const INSTALL_DIR: &str = "drive_c/opentobii";
const EXE_NAME: &str = "sesp_relay.exe";
const BACKUP_SUFFIX: &str = ".opentobii-backup";

const KEY_BLOCK: &str = concat!(
    "[System\\\\ControlSet001\\\\Services\\\\OpenTobiiRelay]\n",
    "\"DisplayName\"=\"OpenTobii relay\"\n",
    "\"ErrorControl\"=dword:00000000\n",
    "\"ImagePath\"=str(2):\"C:\\\\opentobii\\\\sesp_relay.exe\"\n",
    "\"ObjectName\"=\"LocalSystem\"\n",
    "\"Start\"=dword:00000002\n",
    "\"Type\"=dword:00000010\n",
    "\n",
);

/// Why an install or uninstall did not happen.
#[derive(Debug)]
pub enum Error {
    /// The folder has no `system.reg`.
    NotAPrefix(PathBuf),
    /// A wineserver has the prefix open.
    InUse(PathBuf),
    /// This binary was built without the helper and none was given.
    NoHelper,
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAPrefix(path) => {
                write!(f, "{} has no system.reg: not a Wine prefix", path.display())
            }
            Self::InUse(path) => write!(
                f,
                "{} is in use by a running wineserver. Quit the game first.",
                path.display()
            ),
            Self::NoHelper => write!(
                f,
                "this build has no helper inside it; pass --helper PATH to a sesp_relay.exe"
            ),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// `registry` with the service key present exactly once, last.
#[must_use]
pub fn add_service(registry: &str) -> String {
    let mut out = remove_service(registry).trim_end_matches('\n').to_owned();
    out.push_str("\n\n");
    out.push_str(KEY_BLOCK);
    out
}

/// `registry` without the service key. A key runs from its `[header]` line
/// to the blank line after it, or to the next header.
#[must_use]
pub fn remove_service(registry: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    let mut cut = None;
    for line in registry.split_inclusive('\n') {
        if skipping {
            if line.trim().is_empty() {
                skipping = false;
                continue;
            }
            if !line.starts_with('[') {
                continue;
            }
            skipping = false;
        }
        if line.starts_with(KEY) {
            skipping = true;
            cut = Some(out.len());
            continue;
        }
        out.push(line);
    }
    // If our key was the last one, also drop the blank line that separated
    // it from the key before, so the file ends as it did before the install.
    if cut == Some(out.len()) && out.last().is_some_and(|line| line.trim().is_empty()) {
        out.pop();
    }
    out.concat()
}

#[must_use]
pub fn has_service(registry: &str) -> bool {
    registry.lines().any(|line| line.starts_with(KEY))
}

/// True if a wineserver is running with this prefix.
#[must_use]
pub fn prefix_in_use(prefix: &Path) -> bool {
    let Ok(want) = std::fs::canonicalize(prefix) else {
        return false;
    };
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in processes.flatten() {
        let path = entry.path();
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        let Ok(cmdline) = std::fs::read(path.join("cmdline")) else {
            continue;
        };
        if !contains(&cmdline, b"wineserver") {
            continue;
        }
        let Ok(environ) = std::fs::read(path.join("environ")) else {
            continue;
        };
        for item in environ.split(|b| *b == 0) {
            if let Some(value) = item.strip_prefix(b"WINEPREFIX=") {
                let value = PathBuf::from(String::from_utf8_lossy(value).into_owned());
                if std::fs::canonicalize(value).is_ok_and(|have| have == want) {
                    return true;
                }
            }
        }
    }
    false
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn system_reg(prefix: &Path) -> Result<PathBuf, Error> {
    let path = prefix.join("system.reg");
    if path.is_file() {
        Ok(path)
    } else {
        Err(Error::NotAPrefix(prefix.to_owned()))
    }
}

fn require_idle(prefix: &Path) -> Result<(), Error> {
    if prefix_in_use(prefix) {
        Err(Error::InUse(prefix.to_owned()))
    } else {
        Ok(())
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Replace `path` in one step, keeping the first original as a backup.
fn write_registry(path: &Path, text: &str) -> io::Result<()> {
    let backup = with_suffix(path, BACKUP_SUFFIX);
    if !backup.exists() {
        std::fs::copy(path, &backup)?;
    }
    let temporary = with_suffix(path, ".opentobii-tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

/// What is in a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    pub service_key: bool,
    pub helper: bool,
    /// The helper there is the one built into this binary.
    pub helper_current: bool,
    pub in_use: bool,
}

/// Put `helper` and the service key into `prefix`.
pub fn install(prefix: &Path, helper: &[u8]) -> Result<PathBuf, Error> {
    if helper.is_empty() {
        return Err(Error::NoHelper);
    }
    let registry = system_reg(prefix)?;
    require_idle(prefix)?;
    let folder = prefix.join(INSTALL_DIR);
    std::fs::create_dir_all(&folder)?;
    let target = folder.join(EXE_NAME);
    std::fs::write(&target, helper)?;
    let text = std::fs::read_to_string(&registry)?;
    write_registry(&registry, &add_service(&text))?;
    Ok(target)
}

/// Take the helper and the service key out of `prefix`.
pub fn uninstall(prefix: &Path) -> Result<(), Error> {
    let registry = system_reg(prefix)?;
    require_idle(prefix)?;
    let text = std::fs::read_to_string(&registry)?;
    if has_service(&text) {
        write_registry(&registry, &remove_service(&text))?;
    }
    let folder = prefix.join(INSTALL_DIR);
    let target = folder.join(EXE_NAME);
    if target.exists() {
        std::fs::remove_file(&target)?;
    }
    // Only an empty folder goes; anything else in it is not ours.
    let _ = std::fs::remove_dir(&folder);
    Ok(())
}

/// Look at a prefix without changing it.
pub fn status(prefix: &Path) -> Result<Installed, Error> {
    let registry = system_reg(prefix)?;
    let text = std::fs::read_to_string(&registry)?;
    let helper = std::fs::read(prefix.join(INSTALL_DIR).join(EXE_NAME)).ok();
    Ok(Installed {
        service_key: has_service(&text),
        helper: helper.is_some(),
        helper_current: !HELPER.is_empty() && helper.as_deref() == Some(HELPER),
        in_use: prefix_in_use(prefix),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = concat!(
        "WINE REGISTRY Version 2\n",
        ";; All keys relative to \\\\Machine\n",
        "\n",
        "#arch=win64\n",
        "\n",
        "[System\\\\ControlSet001\\\\Services\\\\Winmgmt] 1700000000\n",
        "#time=1d0000000000000\n",
        "\"Start\"=dword:00000003\n",
        "\n",
        "[System\\\\ControlSet001\\\\Services\\\\wuauserv] 1700000000\n",
        "\"Start\"=dword:00000003\n",
    );

    fn temp_prefix(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opentobii-prefix-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pfx")).unwrap();
        std::fs::write(dir.join("pfx/system.reg"), REGISTRY).unwrap();
        dir
    }

    #[test]
    fn add_appends_the_key_once() {
        let once = add_service(REGISTRY);
        assert_eq!(add_service(&once), once);
        assert_eq!(once.matches(KEY).count(), 1);
    }

    #[test]
    fn added_key_is_an_auto_start_own_process_service() {
        let added = add_service(REGISTRY);
        let block = &added[added.find(KEY).unwrap()..];
        assert!(block.contains("\"Start\"=dword:00000002"));
        assert!(block.contains("\"Type\"=dword:00000010"));
        assert!(block.contains("\"ImagePath\"=str(2):\"C:\\\\opentobii\\\\sesp_relay.exe\""));
    }

    #[test]
    fn add_keeps_every_existing_line() {
        let added = add_service(REGISTRY);
        let lines: Vec<&str> = added.lines().collect();
        for line in REGISTRY.lines() {
            assert!(lines.contains(&line), "{line}");
        }
    }

    #[test]
    fn remove_restores_the_other_keys_byte_for_byte() {
        assert_eq!(remove_service(&add_service(REGISTRY)), REGISTRY);
        assert_eq!(
            remove_service(REGISTRY),
            REGISTRY,
            "a no-op without the key"
        );
    }

    #[test]
    fn remove_takes_only_our_key_when_it_sits_in_the_middle() {
        let mut middle = add_service(REGISTRY).trim_end_matches('\n').to_owned();
        middle
            .push_str("\n[System\\\\ControlSet001\\\\Services\\\\Zzz]\n\"Start\"=dword:00000004\n");
        let removed = remove_service(&middle);
        assert!(!has_service(&removed));
        assert!(removed.contains("Services\\\\Zzz]"));
        assert!(removed.contains("\"Start\"=dword:00000004"));
    }

    #[test]
    fn the_key_matches_what_first_worked_in_a_real_prefix() {
        // Nine lines were appended to the real ETS2 prefix on 2026-11-14 and
        // Wine started the service from them.
        let appended = &add_service(REGISTRY)[REGISTRY.len()..];
        assert_eq!(
            appended,
            concat!(
                "\n",
                "[System\\\\ControlSet001\\\\Services\\\\OpenTobiiRelay]\n",
                "\"DisplayName\"=\"OpenTobii relay\"\n",
                "\"ErrorControl\"=dword:00000000\n",
                "\"ImagePath\"=str(2):\"C:\\\\opentobii\\\\sesp_relay.exe\"\n",
                "\"ObjectName\"=\"LocalSystem\"\n",
                "\"Start\"=dword:00000002\n",
                "\"Type\"=dword:00000010\n",
                "\n",
            )
        );
    }

    #[test]
    fn install_then_uninstall_leaves_the_prefix_as_it_was() {
        let dir = temp_prefix("roundtrip");
        let prefix = dir.join("pfx");
        let target = install(&prefix, b"MZ-not-really").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"MZ-not-really");
        let found = status(&prefix).unwrap();
        assert!(found.service_key && found.helper && !found.helper_current && !found.in_use);
        assert_eq!(
            std::fs::read_to_string(prefix.join("system.reg.opentobii-backup")).unwrap(),
            REGISTRY
        );

        uninstall(&prefix).unwrap();
        assert!(!prefix.join("drive_c/opentobii").exists());
        assert_eq!(
            std::fs::read_to_string(prefix.join("system.reg")).unwrap(),
            REGISTRY
        );
        let found = status(&prefix).unwrap();
        assert!(!found.service_key && !found.helper);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn backup_keeps_the_original_across_reinstalls() {
        let dir = temp_prefix("backup");
        let prefix = dir.join("pfx");
        install(&prefix, b"one").unwrap();
        install(&prefix, b"two").unwrap();
        assert_eq!(
            std::fs::read_to_string(prefix.join("system.reg.opentobii-backup")).unwrap(),
            REGISTRY
        );
        assert_eq!(
            std::fs::read(prefix.join("drive_c/opentobii/sesp_relay.exe")).unwrap(),
            b"two"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refuses_a_folder_that_is_not_a_prefix_and_an_empty_helper() {
        let dir = temp_prefix("refuse");
        assert!(matches!(
            install(&dir.join("nope"), b"x"),
            Err(Error::NotAPrefix(_))
        ));
        assert!(matches!(
            uninstall(&dir.join("nope")),
            Err(Error::NotAPrefix(_))
        ));
        assert!(matches!(
            install(&dir.join("pfx"), b""),
            Err(Error::NoHelper)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.join("pfx/system.reg")).unwrap(),
            REGISTRY,
            "untouched"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uninstall_leaves_other_files_in_the_folder() {
        let dir = temp_prefix("others");
        let prefix = dir.join("pfx");
        install(&prefix, b"x").unwrap();
        std::fs::write(prefix.join("drive_c/opentobii/notes.txt"), "mine").unwrap();
        uninstall(&prefix).unwrap();
        assert!(prefix.join("drive_c/opentobii/notes.txt").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_built_in_helper_is_a_windows_executable_when_present() {
        if !HELPER.is_empty() {
            assert_eq!(&HELPER[..2], b"MZ");
            assert!(HELPER.len() > 100_000);
        }
    }
}
