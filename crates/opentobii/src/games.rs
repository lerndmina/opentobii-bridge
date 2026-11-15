//! Every game on this machine that can use the tracker: Steam's, the ones
//! found in other launchers' Wine prefixes, and the ones the user added.
//!
//! A Steam game is found through Steam's own lists ([`crate::steam`]).
//! Games from Lutris, Bottles, Heroic and hand-made prefixes have no such
//! list, so the usual places are searched for Wine prefixes that hold
//! Tobii's DLL. A prefix somewhere else is added by hand and remembered in
//! `~/.config/opentobii/games.txt`, one folder a line.

use std::path::{Path, PathBuf};

use crate::steam;

/// How deep under a search root a prefix may sit. Heroic's are deepest:
/// `~/Games/Heroic/Prefixes/default/<game>`.
const PREFIX_DEPTH: usize = 4;
/// How deep under `drive_c` the DLL is looked for. Star Citizen keeps it
/// five folders down.
const DLL_DEPTH: usize = 7;

/// Where a game in the list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Steam,
    /// Found in one of the usual places for Wine prefixes.
    Found,
    /// A prefix the user pointed at.
    Added,
}

/// One game that can use the tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    /// Names the game for good: a Steam app id, else the prefix's path.
    pub id: String,
    pub name: String,
    /// The folder holding `system.reg`.
    pub prefix: PathBuf,
    /// False only for a Steam game that has never been run.
    pub prefix_exists: bool,
    pub source: Source,
}

/// Is `folder` a Wine prefix?
#[must_use]
pub fn is_prefix(folder: &Path) -> bool {
    folder.join("system.reg").is_file() && folder.join("drive_c").is_dir()
}

/// The prefix a folder the user picked belongs to: the folder itself, the
/// `pfx` folder inside it (Proton's layout), or the prefix it lies within,
/// for someone who picked the game's own folder under `drive_c`.
#[must_use]
pub fn prefix_of(picked: &Path) -> Option<PathBuf> {
    let inside = picked.join("pfx");
    if is_prefix(&inside) {
        return Some(inside);
    }
    picked
        .ancestors()
        .find(|folder| is_prefix(folder))
        .map(Path::to_owned)
}

/// A name for a prefix with no list to take one from: its folder's name,
/// tidied. `star-citizen` becomes `Star Citizen`.
#[must_use]
pub fn name_of(prefix: &Path) -> String {
    // Proton keeps the prefix in `<game>/pfx`: the name is one level up.
    let folder = match prefix.file_name() {
        Some(name) if name == "pfx" => prefix.parent().unwrap_or(prefix),
        _ => prefix,
    };
    let raw = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.display().to_string());
    let words: Vec<String> = raw
        .split(['-', '_', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut letters = word.chars();
            match letters.next() {
                Some(first) => first.to_uppercase().chain(letters).collect(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        raw
    } else {
        words.join(" ")
    }
}

/// The places other launchers keep their prefixes, under `home`.
#[must_use]
pub fn search_roots(home: &Path) -> Vec<PathBuf> {
    [
        // Lutris's and Heroic's default, and where hand-made ones tend to go.
        "Games",
        ".wine",
        ".local/share/bottles/bottles",
        ".var/app/com.usebottles.bottles/data/bottles/bottles",
        ".local/share/lutris/prefixes",
        ".var/app/net.lutris.Lutris/data/lutris/prefixes",
        ".var/app/com.heroicgameslauncher.hgl/config/heroic/Prefixes",
    ]
    .iter()
    .map(|relative| home.join(relative))
    .filter(|root| root.is_dir())
    .collect()
}

/// Every Wine prefix at or under `folder`, no deeper than `depth`. A
/// prefix is not searched for more prefixes inside it.
fn prefixes_under(folder: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if is_prefix(folder) {
        out.push(folder.to_owned());
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        // `file_type` does not follow symlinks, so a link cannot loop us.
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    folders.sort();
    for inner in folders {
        prefixes_under(&inner, depth - 1, out);
    }
}

/// Does this prefix hold a game that ships Tobii's DLL?
#[must_use]
pub fn has_tobii(prefix: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(prefix.join("drive_c")) else {
        return false;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        // Wine's own folder is large and never holds a game.
        .filter(|entry| !entry.file_name().eq_ignore_ascii_case("windows"))
        .any(|entry| steam::find_dll(&entry.path(), DLL_DEPTH - 1).is_some())
}

/// Prefixes outside Steam, under the usual places, that hold Tobii's DLL.
#[must_use]
pub fn found(home: &Path) -> Vec<PathBuf> {
    let mut prefixes = Vec::new();
    for root in search_roots(home) {
        prefixes_under(&root, PREFIX_DEPTH, &mut prefixes);
    }
    prefixes.retain(|prefix| has_tobii(prefix));
    prefixes
}

/// Where the added folders are remembered.
#[must_use]
pub fn added_path() -> Option<PathBuf> {
    opentobii_core::config::default_path().map(|config| config.with_file_name("games.txt"))
}

/// The prefixes the user added, from `file`. A missing file is none.
#[must_use]
pub fn load_added(file: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(PathBuf::from)
        .collect()
}

fn save_added(file: &Path, prefixes: &[PathBuf]) -> std::io::Result<()> {
    if let Some(folder) = file.parent() {
        std::fs::create_dir_all(folder)?;
    }
    let mut text = String::from("# Wine prefixes added to opentobii's game list, one a line.\n");
    for prefix in prefixes {
        text.push_str(&prefix.display().to_string());
        text.push('\n');
    }
    std::fs::write(file, text)
}

/// Remember the prefix that `picked` belongs to. Returns the prefix, or
/// why the folder cannot be used.
pub fn add(file: &Path, picked: &Path) -> Result<PathBuf, String> {
    let prefix = prefix_of(picked).ok_or_else(|| {
        format!(
            "{} is not a Wine prefix: pick the folder that holds system.reg and drive_c",
            picked.display()
        )
    })?;
    let prefix = std::fs::canonicalize(&prefix).unwrap_or(prefix);
    let mut prefixes = load_added(file);
    if !prefixes.contains(&prefix) {
        prefixes.push(prefix.clone());
        save_added(file, &prefixes).map_err(|err| format!("{}: {err}", file.display()))?;
    }
    Ok(prefix)
}

/// Stop remembering an added prefix. Nothing in the prefix is touched.
pub fn forget(file: &Path, prefix: &Path) -> Result<(), String> {
    let mut prefixes = load_added(file);
    prefixes.retain(|kept| kept != prefix);
    save_added(file, &prefixes).map_err(|err| format!("{}: {err}", file.display()))
}

/// Every game that can use the tracker: Steam's first, then the found and
/// the added, each group by name. A prefix is listed once.
#[must_use]
pub fn all(home: &Path, added: &[PathBuf]) -> Vec<Game> {
    let mut games: Vec<Game> = steam::games(home)
        .into_iter()
        .map(|game| Game {
            id: game.app_id,
            name: game.name,
            prefix: game.prefix,
            prefix_exists: game.prefix_exists,
            source: Source::Steam,
        })
        .collect();
    let same = |a: &Path, b: &Path| {
        std::fs::canonicalize(a).unwrap_or_else(|_| a.to_owned())
            == std::fs::canonicalize(b).unwrap_or_else(|_| b.to_owned())
    };
    let mut others: Vec<Game> = Vec::new();
    let candidates = added
        .iter()
        .map(|prefix| (prefix.clone(), Source::Added))
        .chain(
            found(home)
                .into_iter()
                .map(|prefix| (prefix, Source::Found)),
        );
    for (prefix, source) in candidates {
        let listed = games
            .iter()
            .chain(&others)
            .any(|game| same(&game.prefix, &prefix));
        if listed {
            continue;
        }
        others.push(Game {
            id: prefix.display().to_string(),
            name: name_of(&prefix),
            prefix_exists: is_prefix(&prefix),
            prefix,
            source,
        });
    }
    others.sort_by_key(|game| game.name.to_lowercase());
    games.extend(others);
    games
}

/// Pick a game by id, or by a part of its name. `Err` lists what matched
/// when it was not exactly one.
pub fn find<'a>(games: &'a [Game], wanted: &str) -> Result<&'a Game, Vec<&'a Game>> {
    if let Some(game) = games.iter().find(|game| game.id == wanted) {
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

    fn home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opentobii-games-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A Wine prefix at `folder`, with Tobii's DLL at `dll` under `drive_c`
    /// when given.
    fn prefix(folder: &Path, dll: Option<&str>) {
        std::fs::create_dir_all(folder.join("drive_c/windows/system32")).unwrap();
        std::fs::write(folder.join("system.reg"), "WINE REGISTRY Version 2\n").unwrap();
        if let Some(dll) = dll {
            let file = folder.join("drive_c").join(dll);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, b"MZ").unwrap();
        }
    }

    const STAR_CITIZEN: &str =
        "Program Files/Roberts Space Industries/StarCitizen/LIVE/Bin64/tobii_gameintegration_x64.dll";

    #[test]
    fn a_prefix_with_the_dll_is_found_and_one_without_is_not() {
        let home = home("found");
        prefix(&home.join("Games/star-citizen"), Some(STAR_CITIZEN));
        prefix(&home.join("Games/some-other-game"), Some("Game/game.exe"));
        prefix(
            &home.join("Games/Heroic/Prefixes/default/Racer"),
            Some("Racer/TOBII_GameIntegration_x64.dll"),
        );
        // Not a prefix: no system.reg.
        std::fs::create_dir_all(home.join("Games/PS5/drive_c")).unwrap();

        let games = all(&home, &[]);
        let names: Vec<&str> = games.iter().map(|game| game.name.as_str()).collect();
        assert_eq!(names, ["Racer", "Star Citizen"]);
        assert!(games.iter().all(|game| game.source == Source::Found));
        assert_eq!(games[1].prefix, home.join("Games/star-citizen"));
        assert_eq!(
            games[1].id,
            home.join("Games/star-citizen").display().to_string()
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn the_dll_in_wines_own_folder_does_not_count() {
        let home = home("windows");
        prefix(
            &home.join("Games/odd"),
            Some("windows/system32/tobii_gameintegration_x64.dll"),
        );
        assert!(all(&home, &[]).is_empty());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_added_prefix_is_listed_without_the_dll_and_only_once() {
        let home = home("added");
        let elsewhere = home.join("mnt/big/prefixes/space-game");
        prefix(&elsewhere, None);
        prefix(&home.join("Games/star-citizen"), Some(STAR_CITIZEN));
        let file = home.join("config/games.txt");

        // Picking the game's folder inside the prefix finds the prefix.
        let picked = elsewhere.join("drive_c/windows");
        assert_eq!(
            add(&file, &picked).unwrap(),
            elsewhere.canonicalize().unwrap()
        );
        // Adding one that is found anyway, twice, changes nothing.
        add(&file, &home.join("Games/star-citizen")).unwrap();
        add(&file, &home.join("Games/star-citizen")).unwrap();
        assert_eq!(load_added(&file).len(), 2);

        let games = all(&home, &load_added(&file));
        let listed: Vec<(&str, Source)> = games
            .iter()
            .map(|game| (game.name.as_str(), game.source))
            .collect();
        assert_eq!(
            listed,
            [
                ("Space Game", Source::Added),
                ("Star Citizen", Source::Added)
            ]
        );

        forget(&file, &elsewhere.canonicalize().unwrap()).unwrap();
        assert_eq!(load_added(&file).len(), 1);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_folder_that_is_no_prefix_is_refused() {
        let home = home("refused");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        let reason = add(&home.join("games.txt"), &home.join("Documents")).unwrap_err();
        assert!(reason.contains("not a Wine prefix"), "{reason}");
        assert!(!home.join("games.txt").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn protons_layout_is_understood() {
        let home = home("proton");
        prefix(&home.join("compat/12345/pfx"), None);
        assert_eq!(
            prefix_of(&home.join("compat/12345")),
            Some(home.join("compat/12345/pfx"))
        );
        assert_eq!(name_of(&home.join("compat/my_game/pfx")), "My Game");
        assert_eq!(name_of(Path::new("/x/star-citizen")), "Star Citizen");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn a_game_is_picked_by_id_or_part_of_its_name() {
        let home = home("find");
        prefix(&home.join("Games/star-citizen"), Some(STAR_CITIZEN));
        prefix(
            &home.join("Games/star-trucker"),
            Some("g/tobii_gameintegration_x64.dll"),
        );
        let games = all(&home, &[]);
        assert_eq!(find(&games, "citizen").unwrap().name, "Star Citizen");
        assert_eq!(find(&games, "star").unwrap_err().len(), 2);
        assert!(find(&games, "nothing").unwrap_err().is_empty());
        let id = home.join("Games/star-trucker").display().to_string();
        assert_eq!(find(&games, &id).unwrap().name, "Star Trucker");
        std::fs::remove_dir_all(&home).unwrap();
    }
}
