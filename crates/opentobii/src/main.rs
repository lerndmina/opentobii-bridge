use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use opentobii::steam::{self, Game};
use opentobii::{prefix, service, start, Config};
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
    let found = steam::games(&home()?);
    if found.is_empty() {
        println!("No installed Steam game with Tobii's game integration DLL was found.");
        return Ok(());
    }
    for game in found {
        let state = if game.prefix_exists {
            describe(prefix::status(&game.prefix))
        } else {
            "no prefix yet: run the game once first".to_owned()
        };
        println!("{:>8}  {}  ({state})", game.app_id, game.name);
    }
    Ok(())
}

/// Turn what the user typed into a prefix folder and a name for it.
fn resolve(wanted: &str) -> Result<(PathBuf, String), String> {
    let path = Path::new(wanted);
    for candidate in [path.to_owned(), path.join("pfx")] {
        if candidate.join("system.reg").is_file() {
            return Ok((candidate.clone(), candidate.display().to_string()));
        }
    }
    let found = steam::games(&home()?);
    match steam::find(&found, wanted) {
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
                .map(|game| format!("{} ({})", game.name, game.app_id))
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
