//! The login helper's place on the system: `opentobii login-helper`.
//!
//! `pam_exec` runs the helper as root after a face match, so it has to be a
//! file only root can change. This program carries the helper inside it and
//! writes it to [`SYSTEM_PATH`] when run as root. A package puts its own
//! copy at [`PACKAGED_PATH`].

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::install::write_atomic;

/// The helper built with this binary.
pub const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/opentobii-confirm"));

/// Where `opentobii login-helper install` puts it.
pub const SYSTEM_PATH: &str = "/usr/local/libexec/opentobii-confirm";
/// Where the rpm and deb packages put it.
pub const PACKAGED_PATH: &str = "/usr/libexec/opentobii-confirm";

/// What is on the system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    No,
    /// The same helper this program carries.
    Current(PathBuf),
    /// A helper from another version. It keeps working: the question it
    /// asks the daemon does not change between versions.
    Other(PathBuf),
    /// A file that someone other than root can change. PAM must not run it.
    Unsafe(PathBuf),
}

fn look(path: &Path, helper: &[u8]) -> Option<Installed> {
    let meta = std::fs::metadata(path).ok()?;
    let path = path.to_owned();
    if meta.uid() != 0 || meta.permissions().mode() & 0o022 != 0 {
        return Some(Installed::Unsafe(path));
    }
    Some(match std::fs::read(&path) {
        Ok(found) if found == helper => Installed::Current(path),
        _ => Installed::Other(path),
    })
}

/// The helper at the first of `places` that has one.
fn installed_at(places: &[&Path], helper: &[u8]) -> Installed {
    places
        .iter()
        .find_map(|place| look(place, helper))
        .unwrap_or(Installed::No)
}

#[must_use]
pub fn installed() -> Installed {
    installed_at(&[Path::new(SYSTEM_PATH), Path::new(PACKAGED_PATH)], HELPER)
}

/// The two lines that go in a PAM service file in place of a plain
/// `pam_howdy.so` line, for a helper at `path`.
#[must_use]
pub fn pam_lines(path: &Path) -> String {
    format!(
        "auth  [success=ok default=1]        pam_howdy.so\n\
         auth  [success=done default=ignore] pam_exec.so quiet {}\n",
        path.display()
    )
}

/// One line for `opentobii status` and the tuning window.
#[must_use]
pub fn describe(installed: &Installed) -> String {
    match installed {
        Installed::No => "not installed".to_owned(),
        Installed::Current(path) => format!("installed at {}", path.display()),
        Installed::Other(path) => format!(
            "installed at {}, from another version of opentobii",
            path.display()
        ),
        Installed::Unsafe(path) => format!(
            "{} can be changed by someone other than root: do not use it",
            path.display()
        ),
    }
}

/// Write the helper to `path`, owned by whoever runs this.
fn write(path: &Path, helper: &[u8]) -> Result<(), String> {
    if helper.is_empty() {
        return Err("this copy of opentobii was built without the login helper".into());
    }
    write_atomic(path, helper, 0o755)
}

/// Put the helper at [`SYSTEM_PATH`]. Needs root.
///
/// # Errors
/// When not run as root, or the file cannot be written.
pub fn install() -> Result<PathBuf, String> {
    if !rustix::process::geteuid().is_root() {
        return Err(format!(
            "this writes to {SYSTEM_PATH} and needs root. Run:\n  sudo {} login-helper install",
            std::env::current_exe()
                .map(|exe| exe.display().to_string())
                .unwrap_or_else(|_| "opentobii".into())
        ));
    }
    let path = PathBuf::from(SYSTEM_PATH);
    write(&path, HELPER)?;
    // A folder made just now does not have the label SELinux gives
    // programs; put it right where SELinux is in use.
    let _ = std::process::Command::new("restorecon")
        .arg("-RF")
        .arg(path.parent().unwrap_or(&path))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Ok(path)
}

/// Take the helper at [`SYSTEM_PATH`] away. Needs root.
///
/// # Errors
/// When not run as root, or the file cannot be removed.
pub fn remove() -> Result<bool, String> {
    match std::fs::remove_file(SYSTEM_PATH) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("{SYSTEM_PATH}: {err} (this needs root)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opentobii-login-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn this_binary_carries_a_helper() {
        assert!(HELPER.starts_with(b"\x7fELF"), "an ELF program");
    }

    #[test]
    fn a_file_the_user_owns_is_never_trusted() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = folder("unsafe");
        let path = dir.join("opentobii-confirm");
        write(&path, b"helper").unwrap();
        assert_eq!(
            installed_at(&[&path], b"helper"),
            Installed::Unsafe(path.clone())
        );
        assert!(describe(&Installed::Unsafe(path)).contains("do not use"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn nothing_there_is_not_installed_and_the_first_place_wins() {
        let dir = folder("none");
        let missing = dir.join("a");
        assert_eq!(installed_at(&[&missing], b"helper"), Installed::No);
        // /usr/bin/true belongs to root and is not the helper.
        let system = Path::new("/usr/bin/true");
        if system.exists() {
            assert_eq!(
                installed_at(&[&missing, system], b"helper"),
                Installed::Other(system.to_owned())
            );
            let same = std::fs::read(system).unwrap();
            assert_eq!(
                installed_at(&[system, &missing], &same),
                Installed::Current(system.to_owned())
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_pam_lines_run_the_helper_only_after_a_face_match() {
        let lines = pam_lines(Path::new(SYSTEM_PATH));
        let mut lines = lines.lines();
        // A failed match skips the next line; a match goes on to it.
        assert_eq!(
            lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
            ["auth", "[success=ok", "default=1]", "pam_howdy.so"]
        );
        // A nod ends authentication with success; anything else is ignored
        // and the password is asked for.
        assert_eq!(
            lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
            [
                "auth",
                "[success=done",
                "default=ignore]",
                "pam_exec.so",
                "quiet",
                SYSTEM_PATH
            ]
        );
    }

    #[test]
    fn an_empty_helper_is_not_written() {
        let dir = folder("empty");
        assert!(write(&dir.join("x"), b"").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
