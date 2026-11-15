//! Find installed Steam games that ship Tobii's game integration DLL, and
//! the Proton prefix each one runs in.

use std::path::{Path, PathBuf};

/// The DLL a game loads to talk to a tracker.
const DLL_NAMES: [&str; 2] = [
    "tobii_gameintegration_x64.dll",
    "tobii_gameintegration_x86.dll",
];
/// How deep under a game's folder the DLL is looked for. ETS2 keeps it two
/// levels down; games with a launcher keep it a little deeper.
const SEARCH_DEPTH: usize = 6;

/// One installed game that can use a tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub app_id: String,
    pub name: String,
    pub dll: PathBuf,
    /// `<library>/steamapps/compatdata/<appid>/pfx`.
    pub prefix: PathBuf,
    /// The prefix exists: the game has been run under Proton at least once.
    pub prefix_exists: bool,
}

/// Steam's own folders, wherever this user has one.
#[must_use]
pub fn steam_roots(home: &Path) -> Vec<PathBuf> {
    [
        ".local/share/Steam",
        ".steam/steam",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ]
    .iter()
    .map(|relative| home.join(relative))
    .filter(|root| root.join("steamapps").is_dir())
    .collect()
}

/// Every quoted string of a line of Valve's text format, in order.
fn quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (&mut current, c) {
            (Some(text), '\\') => text.extend(chars.next()),
            (Some(_), '"') => out.extend(current.take()),
            (Some(text), other) => text.push(other),
            (None, '"') => current = Some(String::new()),
            (None, _) => {}
        }
    }
    out
}

/// The value of the first top-level-looking `"key" "value"` line.
fn value_of(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(quoted)
        .find(|parts| parts.len() == 2 && parts[0].eq_ignore_ascii_case(key))
        .map(|parts| parts[1].clone())
}

/// The library folders named in a `libraryfolders.vdf`.
#[must_use]
pub fn library_paths(vdf: &str) -> Vec<PathBuf> {
    vdf.lines()
        .map(quoted)
        .filter(|parts| parts.len() == 2 && parts[0].eq_ignore_ascii_case("path"))
        .map(|parts| PathBuf::from(&parts[1]))
        .collect()
}

/// Tobii's DLL at or under `folder`, no more than `depth` folders down.
#[must_use]
pub fn find_dll(folder: &Path, depth: usize) -> Option<PathBuf> {
    let entries = std::fs::read_dir(folder).ok()?;
    let mut folders = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if DLL_NAMES.contains(&name.as_str()) {
            return Some(path);
        }
        // `file_type` does not follow symlinks, so a link cannot loop us.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            folders.push(path);
        }
    }
    if depth == 0 {
        return None;
    }
    folders.sort();
    folders
        .iter()
        .find_map(|folder| find_dll(folder, depth - 1))
}

/// The games in one library folder that ship the DLL.
#[must_use]
pub fn games_in_library(library: &Path) -> Vec<Game> {
    let steamapps = library.join("steamapps");
    let Ok(entries) = std::fs::read_dir(&steamapps) else {
        return Vec::new();
    };
    let mut games = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !(file.starts_with("appmanifest_") && file.ends_with(".acf")) {
            continue;
        }
        let Ok(manifest) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let (Some(app_id), Some(folder)) = (
            value_of(&manifest, "appid"),
            value_of(&manifest, "installdir"),
        ) else {
            continue;
        };
        let Some(dll) = find_dll(&steamapps.join("common").join(&folder), SEARCH_DEPTH) else {
            continue;
        };
        let prefix = steamapps.join("compatdata").join(&app_id).join("pfx");
        games.push(Game {
            name: value_of(&manifest, "name").unwrap_or(folder),
            prefix_exists: prefix.join("system.reg").is_file(),
            app_id,
            dll,
            prefix,
        });
    }
    games
}

/// Every installed game that ships the DLL, across all of this user's Steam
/// libraries, by name.
#[must_use]
pub fn games(home: &Path) -> Vec<Game> {
    let mut libraries: Vec<PathBuf> = Vec::new();
    for root in steam_roots(home) {
        let listed = std::fs::read_to_string(root.join("steamapps/libraryfolders.vdf"))
            .map(|vdf| library_paths(&vdf));
        for library in std::iter::once(root).chain(listed.unwrap_or_default()) {
            let library = std::fs::canonicalize(&library).unwrap_or(library);
            if !libraries.contains(&library) {
                libraries.push(library);
            }
        }
    }
    let mut games: Vec<Game> = libraries
        .iter()
        .flat_map(|library| games_in_library(library))
        .collect();
    games.sort_by_key(|game| game.name.to_lowercase());
    games.dedup_by(|a, b| a.app_id == b.app_id);
    games
}

/// Pick a game by app id, or by a part of its name. `Err` lists what
/// matched when it was not exactly one.
pub fn find<'a>(games: &'a [Game], wanted: &str) -> Result<&'a Game, Vec<&'a Game>> {
    if let Some(game) = games.iter().find(|game| game.app_id == wanted) {
        return Ok(game);
    }
    let needle = wanted.to_lowercase();
    let matches: Vec<&Game> = games
        .iter()
        .filter(|game| game.name.to_lowercase().contains(&needle))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        _ => Err(matches),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opentobii-steam-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("steamapps/common")).unwrap();
        dir
    }

    fn add_game(
        library: &Path,
        app_id: &str,
        name: &str,
        folder: &str,
        dll: Option<&str>,
        prefix: bool,
    ) {
        let manifest = format!(
            "\"AppState\"\n{{\n\t\"appid\"\t\t\"{app_id}\"\n\t\"name\"\t\t\"{name}\"\n\t\"installdir\"\t\t\"{folder}\"\n}}\n"
        );
        std::fs::write(
            library.join(format!("steamapps/appmanifest_{app_id}.acf")),
            manifest,
        )
        .unwrap();
        let game = library.join("steamapps/common").join(folder);
        std::fs::create_dir_all(&game).unwrap();
        if let Some(relative) = dll {
            let path = game.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"").unwrap();
        }
        if prefix {
            let pfx = library
                .join("steamapps/compatdata")
                .join(app_id)
                .join("pfx");
            std::fs::create_dir_all(&pfx).unwrap();
            std::fs::write(pfx.join("system.reg"), "").unwrap();
        }
    }

    #[test]
    fn reads_quoted_values_with_escapes() {
        assert_eq!(
            quoted("\t\"path\"\t\t\"/home/u/4TB HDD/SteamLibrary\""),
            ["path", "/home/u/4TB HDD/SteamLibrary"]
        );
        assert_eq!(
            quoted(r#""name" "Say \"hi\" C:\\x""#),
            ["name", r#"Say "hi" C:\x"#]
        );
        assert!(quoted("{").is_empty());
    }

    #[test]
    fn finds_library_paths_and_ignores_the_rest() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/a/Steam\"\n\t\t\"apps\"\n\t\t{\n\t\t\t\"227300\"\t\t\"41\"\n\t\t}\n\t}\n\t\"1\"\n\t{\n\t\t\"path\"\t\t\"/b/4TB HDD/SteamLibrary\"\n\t}\n}\n";
        assert_eq!(
            library_paths(vdf),
            [
                PathBuf::from("/a/Steam"),
                PathBuf::from("/b/4TB HDD/SteamLibrary")
            ]
        );
    }

    #[test]
    fn lists_only_games_that_ship_the_dll() {
        let lib = library("list");
        add_game(
            &lib,
            "227300",
            "Euro Truck Simulator 2",
            "Euro Truck Simulator 2",
            Some("bin/win_x64/tobii_gameintegration_x64.dll"),
            true,
        );
        add_game(&lib, "111", "No Tracker Here", "Plain", None, true);
        add_game(
            &lib,
            "222",
            "Never Run",
            "Fresh",
            Some("Tobii_GameIntegration_x64.dll"),
            false,
        );
        let mut games = games_in_library(&lib);
        games.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        assert_eq!(games.len(), 2);
        assert_eq!(
            (games[0].app_id.as_str(), games[0].prefix_exists),
            ("222", false)
        );
        assert_eq!(games[1].name, "Euro Truck Simulator 2");
        assert!(games[1].prefix_exists);
        assert!(games[1].prefix.ends_with("steamapps/compatdata/227300/pfx"));
        assert!(games[1]
            .dll
            .ends_with("bin/win_x64/tobii_gameintegration_x64.dll"));
        let _ = std::fs::remove_dir_all(lib);
    }

    #[test]
    fn picks_a_game_by_id_or_by_part_of_its_name() {
        let lib = library("find");
        add_game(
            &lib,
            "227300",
            "Euro Truck Simulator 2",
            "ETS2",
            Some("tobii_gameintegration_x64.dll"),
            true,
        );
        add_game(
            &lib,
            "270880",
            "American Truck Simulator",
            "ATS",
            Some("tobii_gameintegration_x64.dll"),
            true,
        );
        let games = games_in_library(&lib);
        assert_eq!(
            find(&games, "227300").unwrap().name,
            "Euro Truck Simulator 2"
        );
        assert_eq!(find(&games, "american").unwrap().app_id, "270880");
        assert_eq!(find(&games, "truck").unwrap_err().len(), 2);
        assert!(find(&games, "flight").unwrap_err().is_empty());
        let _ = std::fs::remove_dir_all(lib);
    }
}
