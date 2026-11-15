use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use opentobii::games::{self, Game, Source};
use opentobii::{install, prefix, service, start, update, Config};
use opentobii_core::control::{self, Line, Request};

const USAGE: &str = "\
opentobii: a Tobii Eye Tracker 5 for games under Proton

  opentobii daemon [--no-smooth] [--monitor N] [--socket PATH] [--config PATH]
      Run the bridge between tobiid and games. Start it before the game.
        --no-smooth    send head poses unsmoothed
        --monitor N    report monitor N to the game instead of the primary
        --socket PATH  tobiid's socket (default: $XDG_RUNTIME_DIR/tobiid.sock)
        --config PATH  settings file (default: ~/.config/opentobii/config.toml)

  opentobii games
      List installed Steam games that can use the tracker.

  opentobii install GAME [--helper PATH]
  opentobii uninstall GAME
      Put the helper into a game's prefix, or take it out. GAME is a Steam
      app id, part of a game's name, or the path of a prefix. The game must
      be closed.

  opentobii status
      Show what the running daemon is connected to.

  opentobii setup [--autostart]
      Install opentobii for your user: the program in ~/.local/bin, an
      app-menu entry, and a background service. No root. With --autostart
      the daemon is started now and at every login.

  opentobii remove
      Take that install out again. Settings are kept.

  opentobii update [--check]
      Update the installed copy to the newest release. Each release is
      checked against the project's signing key before it is used. With
      --check, only say whether there is one.

  opentobii service [start|stop|restart|enable|disable]
      Run the daemon in the background through your user's systemd, and
      choose whether it starts at login. With no word, show its state.

  opentobii gui
      Open the tuning window: dead zone, smoothing, sensitivity, recentre.

  opentobii overlay
      Draw the gaze mark on screen. The daemon runs this by itself when the
      overlay is switched on; there is no need to start it by hand.
";

type CommandResult = Result<(), String>;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("daemon") => daemon(args),
        Some("games") => games(),
        Some("install") => install(args),
        Some("uninstall") => uninstall(args),
        Some("status") => status(),
        Some("service") => service_command(args),
        Some("setup") => setup(args),
        Some("update") => update_command(args),
        Some("remove") => remove(args),
        Some("version" | "--version" | "-V") => {
            println!("opentobii {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        #[cfg(feature = "gui")]
        Some("gui") => opentobii::gui::run(),
        #[cfg(feature = "overlay")]
        Some("overlay") => opentobii::overlay::run(),
        Some("help" | "--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
        None => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("opentobii: {reason}");
            ExitCode::FAILURE
        }
    }
}

fn daemon(mut args: impl Iterator<Item = String>) -> CommandResult {
    let mut config = Config::default();
    let mut settings_path = opentobii_core::config::default_path();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-smooth" => config.force_unsmoothed = true,
            "--monitor" => {
                let value = args.next().ok_or("--monitor needs a number")?;
                let index = value
                    .parse()
                    .map_err(|_| format!("--monitor: `{value}` is not a number"))?;
                config.monitor_index = Some(index);
            }
            "--socket" => {
                config.tobiid_socket = PathBuf::from(args.next().ok_or("--socket needs a path")?);
            }
            "--config" => {
                settings_path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?));
            }
            other => return Err(format!("daemon: unknown argument `{other}`")),
        }
    }
    if let Some(path) = settings_path {
        config.tuning = opentobii_core::config::Config::load(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        config.settings_path = Some(path);
    }
    // An update replaces only the program. If this is the installed copy,
    // bring its launcher entry, icon and unit up to this version's.
    if let (Ok(install::Kind::Home), Ok(layout)) = (install::kind(), install::Layout::for_user()) {
        match install::refresh(&layout) {
            Ok(changed) if changed.contains(&layout.unit) => {
                // Tell systemd the unit on disk is new; it applies at the next start.
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "daemon-reload"])
                    .status();
            }
            Ok(_) => {}
            Err(reason) => eprintln!("opentobii: could not refresh the installed files: {reason}"),
        }
    }
    let running = start(config).map_err(|err| format!("cannot start: {err}"))?;
    eprintln!(
        "opentobii: TTP on {}, helper relay on {}",
        running.ttp_addr, running.relay_addr
    );
    if let Some(path) = &running.control_socket {
        eprintln!("opentobii: control socket at {}", path.display());
    }
    loop {
        thread::park();
    }
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_owned())
}

fn describe(installed: Result<prefix::Installed, prefix::Error>) -> String {
    match installed {
        Ok(found) if found.service_key && found.helper_current => "helper installed".to_owned(),
        Ok(found) if found.service_key && found.helper => {
            "helper installed, older version".to_owned()
        }
        Ok(found) if found.service_key || found.helper => "helper partly installed".to_owned(),
        Ok(_) => "helper not installed".to_owned(),
        Err(err) => err.to_string(),
    }
}

fn games() -> CommandResult {
    let found = all_games()?;
    if found.is_empty() {
        println!("No installed game with Tobii's game integration DLL was found.");
        println!("For a game in a Wine prefix somewhere unusual: opentobii install PREFIX");
        return Ok(());
    }
    for game in found {
        let state = if game.prefix_exists {
            describe(prefix::status(&game.prefix))
        } else {
            "no prefix yet: run the game once first".to_owned()
        };
        match game.source {
            Source::Steam => println!("{:>8}  {}  ({state})", game.id, game.name),
            // A game outside Steam has no number: it is named by its prefix.
            _ => println!(
                "{:>8}  {}  ({state})\n          {}",
                "-", game.name, game.id
            ),
        }
    }
    Ok(())
}

/// Every game that can use the tracker: Steam's, the ones found in other
/// launchers' prefixes, and the ones added in the tuning window.
fn all_games() -> Result<Vec<Game>, String> {
    let added = games::added_path()
        .map(|file| games::load_added(&file))
        .unwrap_or_default();
    Ok(games::all(&home()?, &added))
}

/// Turn what the user typed into a prefix folder and a name for it.
fn resolve(wanted: &str) -> Result<(PathBuf, String), String> {
    let path = Path::new(wanted);
    for candidate in [path.to_owned(), path.join("pfx")] {
        if candidate.join("system.reg").is_file() {
            return Ok((candidate.clone(), candidate.display().to_string()));
        }
    }
    let found = all_games()?;
    match games::find(&found, wanted) {
        Ok(Game {
            prefix_exists: false,
            name,
            ..
        }) => Err(format!(
            "{name} has no prefix yet: run it once under Proton, close it, then try again"
        )),
        Ok(game) => Ok((game.prefix.clone(), game.name.clone())),
        Err(matches) if matches.is_empty() => Err(format!(
            "`{wanted}` is not a prefix and matches no game; `opentobii games` lists them"
        )),
        Err(matches) => {
            let names: Vec<String> = matches
                .iter()
                .map(|game| format!("{} ({})", game.name, game.id))
                .collect();
            Err(format!(
                "`{wanted}` matches more than one game: {}",
                names.join(", ")
            ))
        }
    }
}

fn install(mut args: impl Iterator<Item = String>) -> CommandResult {
    let mut wanted = None;
    let mut helper_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--helper" => {
                helper_path = Some(PathBuf::from(args.next().ok_or("--helper needs a path")?));
            }
            other if wanted.is_none() => wanted = Some(other.to_owned()),
            other => return Err(format!("install: unexpected argument `{other}`")),
        }
    }
    let (target, name) = resolve(&wanted.ok_or("install: which game?")?)?;
    let helper = match &helper_path {
        Some(path) => std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?,
        None => prefix::HELPER.to_vec(),
    };
    let written = prefix::install(&target, &helper).map_err(|err| err.to_string())?;
    println!("Installed the helper for {name}:");
    println!("  {}", written.display());
    println!(
        "  one service key added to {}",
        target.join("system.reg").display()
    );
    println!("In the game, switch eye tracking on in the options if it has such a switch.");
    Ok(())
}

fn uninstall(mut args: impl Iterator<Item = String>) -> CommandResult {
    let (target, name) = resolve(&args.next().ok_or("uninstall: which game?")?)?;
    if let Some(extra) = args.next() {
        return Err(format!("uninstall: unexpected argument `{extra}`"));
    }
    prefix::uninstall(&target).map_err(|err| err.to_string())?;
    println!("Removed the helper for {name}.");
    Ok(())
}

fn setup(mut args: impl Iterator<Item = String>) -> CommandResult {
    let mut autostart = false;
    for arg in args.by_ref() {
        match arg.as_str() {
            "--autostart" => autostart = true,
            other => return Err(format!("setup: unknown argument `{other}`")),
        }
    }
    let layout = install::Layout::for_user()?;
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot find this program's own path: {err}"))?;
    if install::kind_of(&exe, &layout) == install::Kind::Packaged {
        return Err(
            "this copy was installed by a package and is already set up; \
                    use `opentobii service enable` to start it at login"
                .into(),
        );
    }
    // A running daemon keeps using the old file until it is restarted.
    let was_running = service::state().active;
    let written = install::place(&exe, &layout)?;
    println!("Installed:");
    for path in written {
        println!("  {}", path.display());
    }
    if let Some(other) = install::packaged_copy() {
        println!(
            "Note: a packaged copy is also installed at {}.",
            other.display()
        );
        println!("      Which one runs depends on your PATH; removing one avoids confusion.");
    }
    let state = service::state();
    if state.systemd {
        // The unit file changed on disk: have systemd read it again.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
        if autostart {
            service::set_enabled(true)?;
            if was_running {
                service::restart()?
            } else {
                service::start()?
            }
            println!("The daemon is running and starts at login.");
        } else if was_running {
            service::restart()?;
            println!("The daemon was restarted on the installed copy.");
        } else {
            println!("To start the daemon now and at every login:  opentobii service enable && opentobii service start");
        }
    } else {
        println!(
            "This system has no systemd for your user: start the daemon with `opentobii daemon`."
        );
    }
    let bin = layout.binary.parent().unwrap_or(&layout.binary);
    if !install::on_path(bin, &std::env::var("PATH").unwrap_or_default()) {
        println!(
            "Note: {} is not on your PATH. Use the full path, or add it.",
            bin.display()
        );
    }
    println!("Next: `opentobii games`, then `opentobii install <game>` with that game closed.");
    Ok(())
}

fn update_command(mut args: impl Iterator<Item = String>) -> CommandResult {
    let mut check_only = false;
    for arg in args.by_ref() {
        match arg.as_str() {
            "--check" => check_only = true,
            other => return Err(format!("update: unknown argument `{other}`")),
        }
    }
    let fetch = update::Http;
    let have = update::current();
    let Some(release) = update::newer_release(&fetch, have)? else {
        println!("opentobii {have} is the newest version.");
        return Ok(());
    };
    println!(
        "opentobii {} is available (you have {have}).",
        release.version
    );
    if !release.notes.trim().is_empty() {
        println!();
        println!("{}", release.notes.trim());
        println!();
    }
    if check_only {
        return Ok(());
    }
    println!("Downloading and checking the release's signature...");
    println!("{}", update::install_release(&fetch, &release)?);
    Ok(())
}

fn remove(mut args: impl Iterator<Item = String>) -> CommandResult {
    if let Some(extra) = args.next() {
        return Err(format!("remove: unexpected argument `{extra}`"));
    }
    let layout = install::Layout::for_user()?;
    if service::state().systemd {
        let _ = service::set_enabled(false);
        let _ = service::stop();
    }
    let removed = install::unplace(&layout)?;
    if service::state().systemd {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
    if removed.is_empty() {
        println!("Nothing to remove: opentobii is not installed in your home folder.");
    } else {
        println!("Removed:");
        for path in removed {
            println!("  {}", path.display());
        }
        println!("Settings in ~/.config/opentobii were kept. The helper inside game prefixes is separate:");
        println!("take it out first with `opentobii uninstall <game>` if you want it gone.");
    }
    Ok(())
}

fn service_command(mut args: impl Iterator<Item = String>) -> CommandResult {
    match args.next().as_deref() {
        Some("start") => service::start()?,
        Some("stop") => service::stop()?,
        Some("restart") => service::restart()?,
        Some("enable") => service::set_enabled(true)?,
        Some("disable") => service::set_enabled(false)?,
        Some(other) => return Err(format!("service: unknown word `{other}`")),
        None => {}
    }
    let state = service::state();
    if !state.systemd {
        return Err(
            "this system has no systemd for your user; run `opentobii daemon` instead".into(),
        );
    }
    let yes_no = |flag: bool| if flag { "yes" } else { "no" };
    println!("Running:         {}", yes_no(state.active));
    println!("Starts at login: {}", yes_no(state.enabled));
    Ok(())
}

fn status() -> CommandResult {
    let path = control::socket_path();
    let stream = UnixStream::connect(&path)
        .map_err(|err| format!("the daemon is not running ({}: {err})", path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|err| err.to_string())?;
    let mut writer = stream.try_clone().map_err(|err| err.to_string())?;
    let request = serde_json::to_string(&Request::Get).map_err(|err| err.to_string())?;
    writeln!(writer, "{request}").map_err(|err| err.to_string())?;
    let mut text = String::new();
    BufReader::new(stream)
        .read_line(&mut text)
        .map_err(|err| format!("no answer from the daemon: {err}"))?;
    let answer = serde_json::from_str(&text).map_err(|err| format!("unexpected answer: {err}"))?;
    let Line::State { config, status } = answer else {
        return Err("unexpected answer from the daemon".into());
    };

    let yes_no = |flag: bool| if flag { "yes" } else { "no" };
    println!(
        "Driver (tobiid) connected: {}",
        yes_no(status.driver_connected)
    );
    println!("Tracking a head:           {}", yes_no(status.tracking));
    let presence = match status.presence {
        1 => "nobody in front of the tracker",
        2 => "somebody in front of the tracker",
        _ => "not reported yet",
    };
    println!("Presence:                  {presence}");
    match status.display_width_mm {
        Some(width) => println!("Display:                   {width:.0} mm wide"),
        None => println!("Display:                   not reported by the driver"),
    }
    if !status.camera.is_empty() {
        println!("Face login camera:         {}", status.camera);
    }
    match &status.settings_path {
        Some(path) => println!("Settings:                  {}", path.display()),
        None => println!("Settings:                  not saved to a file"),
    }
    let profiles: Vec<&str> = config.games.keys().map(String::as_str).collect();
    let profiles = if profiles.is_empty() {
        "none".to_owned()
    } else {
        profiles.join(", ")
    };
    println!("Game profiles:             {profiles}");
    if status.games.is_empty() {
        println!("Games connected:           none");
    }
    for game in &status.games {
        let name = if game.game.is_empty() {
            "unknown (older helper)"
        } else {
            &game.game
        };
        let profile = if game.own_profile {
            "own profile"
        } else {
            "default profile"
        };
        let pose = if game.head_pose {
            "head pose on"
        } else {
            "head pose off"
        };
        println!(
            "Game connected:            {name}, process {}, {profile}, {pose}",
            game.pid
        );
    }
    Ok(())
}
