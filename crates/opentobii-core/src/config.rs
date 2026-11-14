//! The settings file: one default profile and one profile per game.
//!
//! ```toml
//! [default.yaw]
//! sensitivity = 1.5
//!
//! [games."eurotrucks2.exe".yaw]
//! dead_zone = 1.0
//! sensitivity = 2.0
//! ```
//!
//! A game is named by its executable's file name, lower case. A game with no
//! section of its own uses the default profile. Values left out of a profile
//! take their built-in defaults, not the default profile's values: each
//! profile is complete in itself.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::overlay::Overlay;
use crate::pipeline::Profile;

/// Everything in the settings file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default: Profile,
    /// Profiles by executable name, as [`game_key`] spells it.
    pub games: BTreeMap<String, Profile>,
    /// The on-screen gaze mark.
    pub overlay: Overlay,
}

/// Why the settings file could not be used.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The file is not valid TOML, or has a name the program does not know.
    Parse(String),
    /// A profile holds a value that cannot work; the text says which.
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Parse(reason) | Self::Invalid(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for Error {}

/// How a game is named in the file: the file name of its executable, lower
/// case. `C:\Games\ETS2\bin\win_x64\eurotrucks2.exe` is `eurotrucks2.exe`.
#[must_use]
pub fn game_key(executable: &str) -> String {
    executable
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(executable)
        .trim()
        .to_lowercase()
}

/// Where the settings file lives: `$XDG_CONFIG_HOME/opentobii/config.toml`,
/// else `~/.config/opentobii/config.toml`.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?).join(".config")
        }
    };
    Some(base.join("opentobii").join("config.toml"))
}

impl Config {
    /// Read settings from TOML text.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let config: Self = toml::from_str(text).map_err(|err| Error::Parse(err.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// The file's text for these settings.
    pub fn to_toml(&self) -> Result<String, Error> {
        toml::to_string_pretty(self).map_err(|err| Error::Parse(err.to_string()))
    }

    /// Check every profile, and that every game is named as [`game_key`]
    /// would name it.
    pub fn validate(&self) -> Result<(), Error> {
        self.overlay.validate().map_err(Error::Invalid)?;
        self.default
            .validate()
            .map_err(|reason| Error::Invalid(format!("default profile: {reason}")))?;
        for (game, profile) in &self.games {
            if game.is_empty() || *game != game_key(game) {
                return Err(Error::Invalid(format!(
                    "game `{game}` must be an executable's file name in lower case"
                )));
            }
            profile
                .validate()
                .map_err(|reason| Error::Invalid(format!("profile for {game}: {reason}")))?;
        }
        Ok(())
    }

    /// Load the file at `path`. A file that does not exist is the built-in
    /// defaults, not an error.
    pub fn load(path: &Path) -> Result<Self, Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(Error::Io(err)),
        }
    }

    /// Write the file at `path`, creating its folder. The old file is
    /// replaced in one step, so a crash cannot leave half a file.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        self.validate()?;
        let text = self.to_toml()?;
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder).map_err(Error::Io)?;
        }
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        let temporary = PathBuf::from(temporary);
        std::fs::write(&temporary, text).map_err(Error::Io)?;
        std::fs::rename(&temporary, path).map_err(Error::Io)
    }

    /// The profile for a game, by any spelling of its executable.
    #[must_use]
    pub fn profile_for(&self, executable: &str) -> &Profile {
        self.games
            .get(&game_key(executable))
            .unwrap_or(&self.default)
    }

    /// Whether a game has a profile of its own.
    #[must_use]
    pub fn has_profile(&self, executable: &str) -> bool {
        self.games.contains_key(&game_key(executable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Axis;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opentobii-config-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn games_are_named_by_lower_case_file_name() {
        assert_eq!(
            game_key("S:\\steamapps\\common\\ETS2\\bin\\win_x64\\EuroTrucks2.exe"),
            "eurotrucks2.exe"
        );
        assert_eq!(game_key("/home/u/game/Game.EXE"), "game.exe");
        assert_eq!(game_key("eurotrucks2.exe"), "eurotrucks2.exe");
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn a_partial_file_fills_the_rest_with_defaults() {
        let config = Config::parse(
            r#"
            [default.yaw]
            sensitivity = 1.5

            [games."eurotrucks2.exe".yaw]
            dead_zone = 1.0
            sensitivity = 2.0

            [games."eurotrucks2.exe".smoothing]
            rotation_steadiness = 0.4
            "#,
        )
        .unwrap();
        assert_eq!(config.default.yaw.sensitivity, 1.5);
        assert_eq!(config.default.pitch, Axis::default());
        let ets2 = config.profile_for("C:\\x\\EuroTrucks2.exe");
        assert_eq!((ets2.yaw.dead_zone, ets2.yaw.sensitivity), (1.0, 2.0));
        assert_eq!(ets2.smoothing.rotation_steadiness, 0.4);
        assert!(ets2.smoothing.enabled);
        assert!(config.has_profile("eurotrucks2.exe"));
        assert_eq!(config.profile_for("other.exe"), &config.default);
    }

    #[test]
    fn what_is_written_reads_back_the_same() {
        let mut config = Config::default();
        config.default.pitch = Axis {
            dead_zone: 0.5,
            sensitivity: 1.25,
            curve: 1.5,
            invert: true,
            limit: 60.0,
        };
        config.default.neutral.position = Some([1.0, -2.0, 750.0]);
        config.games.insert(
            "eurotrucks2.exe".into(),
            Profile {
                yaw: Axis {
                    sensitivity: 3.0,
                    ..Axis::default()
                },
                ..Profile::default()
            },
        );
        let text = config.to_toml().unwrap();
        assert_eq!(Config::parse(&text).unwrap(), config);
    }

    #[test]
    fn mistakes_are_refused_with_a_reason() {
        let unknown = Config::parse("[default.yaw]\nsensitivty = 2.0\n").unwrap_err();
        assert!(
            matches!(unknown, Error::Parse(ref reason) if reason.contains("sensitivty")),
            "{unknown}"
        );

        let negative = Config::parse("[default.yaw]\ndead_zone = -1.0\n").unwrap_err();
        assert!(
            matches!(negative, Error::Invalid(ref reason) if reason.contains("default profile: yaw")),
            "{negative}"
        );

        let named = Config::parse("[games.\"C:\\\\Games\\\\A.exe\"]\n").unwrap_err();
        assert!(
            matches!(named, Error::Invalid(ref reason) if reason.contains("lower case")),
            "{named}"
        );

        assert!(matches!(Config::parse("not toml ["), Err(Error::Parse(_))));
    }

    #[test]
    fn a_missing_file_is_the_defaults_and_save_creates_the_folder() {
        let dir = temp_dir("save");
        let path = dir.join("nested").join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        let mut config = Config::default();
        config.default.yaw.sensitivity = 2.5;
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "no temporary file is left behind"
        );

        config.default.yaw.sensitivity = -1.0;
        assert!(matches!(config.save(&path), Err(Error::Invalid(_))));
        assert_eq!(
            Config::load(&path).unwrap().default.yaw.sensitivity,
            2.5,
            "a refused save leaves the file alone"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
