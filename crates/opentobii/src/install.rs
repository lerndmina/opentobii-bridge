//! Installing opentobii into the user's home folder, and removing it.
//!
//! `opentobii setup` copies this executable to `~/.local/bin`, and writes a
//! systemd unit, an app-menu entry and an icon beside it. Nothing needs
//! root, and a copy installed this way can update itself. A copy installed
//! from an rpm or deb lives under `/usr` and is left to the package manager.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::service;

const DESKTOP_ENTRY: &str = include_str!("../../../packaging/opentobii.desktop");
const ICON: &str = include_str!("../../../packaging/opentobii.svg");

/// Where this running executable came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Installed by `setup` in the home folder: it can update itself.
    Home,
    /// Under `/usr` or `/opt`: the package manager's to update.
    Packaged,
    /// Anywhere else: a download, or a build run from its folder.
    Loose,
}

/// The files an install consists of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub binary: PathBuf,
    pub unit: PathBuf,
    pub desktop_entry: PathBuf,
    pub icon: PathBuf,
}

impl Layout {
    /// The layout under a home folder, following the XDG folders when they
    /// are given.
    #[must_use]
    pub fn new(home: &Path, data_home: Option<&Path>, config_home: Option<&Path>) -> Self {
        let data = data_home.map_or_else(|| home.join(".local/share"), Path::to_owned);
        let config = config_home.map_or_else(|| home.join(".config"), Path::to_owned);
        Self {
            binary: home.join(".local/bin/opentobii"),
            unit: config.join("systemd/user").join(service::UNIT),
            desktop_entry: data.join("applications/opentobii.desktop"),
            icon: data.join("icons/hicolor/scalable/apps/opentobii.svg"),
        }
    }

    /// The layout for the current user.
    pub fn for_user() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or("HOME is not set")?;
        let folder = |name: &str| {
            std::env::var_os(name)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
        };
        Ok(Self::new(
            Path::new(&home),
            folder("XDG_DATA_HOME").as_deref(),
            folder("XDG_CONFIG_HOME").as_deref(),
        ))
    }
}

/// Classify an executable path against a layout.
#[must_use]
pub fn kind_of(exe: &Path, layout: &Layout) -> Kind {
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(exe, &layout.binary) {
        Kind::Home
    } else if exe.starts_with("/usr") || exe.starts_with("/opt") {
        Kind::Packaged
    } else {
        Kind::Loose
    }
}

/// How this running copy was installed.
pub fn kind() -> Result<Kind, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot find this program's own path: {err}"))?;
    Ok(kind_of(&exe, &Layout::for_user()?))
}

fn describe(path: &Path, err: &io::Error) -> String {
    format!("{}: {err}", path.display())
}

/// Write `contents` to `path` in one step, creating its folder.
pub(crate) fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<(), String> {
    let folder = path
        .parent()
        .ok_or_else(|| format!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(folder).map_err(|err| describe(folder, &err))?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".opentobii-new");
    let temporary = folder.join(name);
    std::fs::write(&temporary, contents).map_err(|err| describe(&temporary, &err))?;
    std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(mode))
        .map_err(|err| describe(&temporary, &err))?;
    std::fs::rename(&temporary, path).map_err(|err| {
        let _ = std::fs::remove_file(&temporary);
        describe(path, &err)
    })
}

/// The app-menu entry, pointing at `binary`.
#[must_use]
pub fn desktop_entry(binary: &Path) -> String {
    DESKTOP_ENTRY
        .lines()
        .map(|line| {
            if line.starts_with("Exec=") {
                format!("Exec=\"{}\" gui", binary.display())
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Put the files of an install in place, copying the executable at `source`.
/// Returns what was written. Does not touch systemd.
pub fn place(source: &Path, layout: &Layout) -> Result<Vec<PathBuf>, String> {
    let already_there = kind_of(source, layout) == Kind::Home;
    if !already_there {
        let binary = std::fs::read(source).map_err(|err| describe(source, &err))?;
        write_atomic(&layout.binary, &binary, 0o755)?;
    }
    write_atomic(
        &layout.unit,
        service::unit_text(&layout.binary).as_bytes(),
        0o644,
    )?;
    write_atomic(
        &layout.desktop_entry,
        desktop_entry(&layout.binary).as_bytes(),
        0o644,
    )?;
    write_atomic(&layout.icon, ICON.as_bytes(), 0o644)?;
    Ok(vec![
        layout.binary.clone(),
        layout.unit.clone(),
        layout.desktop_entry.clone(),
        layout.icon.clone(),
    ])
}

/// Take the files of an install away. Settings are left alone. Returns what
/// was removed.
pub fn unplace(layout: &Layout) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    let mut old = layout.binary.clone().into_os_string();
    old.push(".old");
    for path in [
        &layout.binary,
        &PathBuf::from(old),
        &layout.unit,
        &layout.desktop_entry,
        &layout.icon,
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => removed.push(path.clone()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(describe(path, &err)),
        }
    }
    Ok(removed)
}

/// A packaged copy that would sit alongside a home-folder install.
#[must_use]
pub fn packaged_copy() -> Option<PathBuf> {
    ["/usr/bin/opentobii", "/usr/local/bin/opentobii"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// Whether `folder` is one of the folders in a `PATH` value.
#[must_use]
pub fn on_path(folder: &Path, path_variable: &str) -> bool {
    path_variable
        .split(':')
        .any(|entry| !entry.is_empty() && Path::new(entry) == folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opentobii-install-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_layout_follows_the_xdg_folders_when_given() {
        let plain = Layout::new(Path::new("/home/u"), None, None);
        assert_eq!(plain.binary, Path::new("/home/u/.local/bin/opentobii"));
        assert_eq!(
            plain.unit,
            Path::new("/home/u/.config/systemd/user/opentobii.service")
        );
        assert_eq!(
            plain.desktop_entry,
            Path::new("/home/u/.local/share/applications/opentobii.desktop")
        );
        assert_eq!(
            plain.icon,
            Path::new("/home/u/.local/share/icons/hicolor/scalable/apps/opentobii.svg")
        );

        let moved = Layout::new(
            Path::new("/home/u"),
            Some(Path::new("/d")),
            Some(Path::new("/c")),
        );
        assert_eq!(moved.unit, Path::new("/c/systemd/user/opentobii.service"));
        assert_eq!(
            moved.icon,
            Path::new("/d/icons/hicolor/scalable/apps/opentobii.svg")
        );
        assert_eq!(
            moved.binary, plain.binary,
            "the binary always goes in ~/.local/bin"
        );
    }

    #[test]
    fn tells_a_home_install_from_a_package_and_a_loose_copy() {
        let layout = Layout::new(Path::new("/home/u"), None, None);
        assert_eq!(
            kind_of(Path::new("/home/u/.local/bin/opentobii"), &layout),
            Kind::Home
        );
        assert_eq!(
            kind_of(Path::new("/usr/bin/opentobii"), &layout),
            Kind::Packaged
        );
        assert_eq!(
            kind_of(Path::new("/opt/opentobii/opentobii"), &layout),
            Kind::Packaged
        );
        assert_eq!(
            kind_of(Path::new("/home/u/Downloads/opentobii"), &layout),
            Kind::Loose
        );
        assert_eq!(
            kind_of(Path::new("/home/u/git/x/target/release/opentobii"), &layout),
            Kind::Loose
        );
    }

    #[test]
    fn setup_places_every_file_pointing_at_the_installed_binary() {
        let home = home("place");
        let source = home.join("download/opentobii");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"the program").unwrap();
        let layout = Layout::new(&home, None, None);

        let written = place(&source, &layout).unwrap();
        assert_eq!(written.len(), 4);
        assert_eq!(std::fs::read(&layout.binary).unwrap(), b"the program");
        let mode = std::fs::metadata(&layout.binary)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);

        let unit = std::fs::read_to_string(&layout.unit).unwrap();
        assert!(
            unit.contains(&format!("ExecStart={} daemon", layout.binary.display())),
            "{unit}"
        );
        let entry = std::fs::read_to_string(&layout.desktop_entry).unwrap();
        assert!(
            entry.contains(&format!("Exec=\"{}\" gui", layout.binary.display())),
            "{entry}"
        );
        assert!(entry.contains("Name=opentobii") && entry.contains("Icon=opentobii"));
        assert!(std::fs::read_to_string(&layout.icon)
            .unwrap()
            .starts_with("<svg"));
        assert!(
            !layout
                .binary
                .with_file_name("opentobii.opentobii-new")
                .exists(),
            "no temporary file is left"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn running_setup_from_the_installed_copy_leaves_the_binary_alone() {
        let home = home("again");
        let layout = Layout::new(&home, None, None);
        std::fs::create_dir_all(layout.binary.parent().unwrap()).unwrap();
        std::fs::write(&layout.binary, b"installed").unwrap();
        place(&layout.binary, &layout).unwrap();
        assert_eq!(std::fs::read(&layout.binary).unwrap(), b"installed");
        assert!(layout.unit.is_file() && layout.desktop_entry.is_file());
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn remove_takes_away_what_setup_placed_and_nothing_else() {
        let home = home("remove");
        let source = home.join("opentobii-download");
        std::fs::write(&source, b"x").unwrap();
        let layout = Layout::new(&home, None, None);
        place(&source, &layout).unwrap();
        let settings = home.join(".config/opentobii/config.toml");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, "kept").unwrap();
        std::fs::write(layout.binary.with_file_name("opentobii.old"), b"previous").unwrap();

        let removed = unplace(&layout).unwrap();
        assert_eq!(removed.len(), 5, "four files and the kept previous binary");
        assert!(!layout.binary.exists() && !layout.unit.exists() && !layout.icon.exists());
        assert!(
            settings.is_file() && source.is_file(),
            "settings and the download are untouched"
        );
        assert!(
            unplace(&layout).unwrap().is_empty(),
            "removing twice is fine"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn knows_whether_a_folder_is_on_the_path() {
        let bin = Path::new("/home/u/.local/bin");
        assert!(on_path(bin, "/usr/bin:/home/u/.local/bin:/bin"));
        assert!(!on_path(bin, "/usr/bin:/home/u/.local/bin2"));
        assert!(!on_path(bin, ""));
    }
}
