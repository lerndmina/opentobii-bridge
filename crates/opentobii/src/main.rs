use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use opentobii::{start, Config};

const USAGE: &str = "\
opentobii daemon [--no-smooth] [--monitor N] [--socket PATH] [--config PATH]

Runs the bridge between tobiid and games that use Tobii's game integration
DLL under Proton. Start it before the game.

  --no-smooth    send head poses unfiltered
  --monitor N    report monitor N to the game instead of the primary
  --socket PATH  tobiid's socket (default: $XDG_RUNTIME_DIR/tobiid.sock)
  --config PATH  settings file (default: ~/.config/opentobii/config.toml)
";

fn parse(mut args: impl Iterator<Item = String>) -> Result<Config, String> {
    if args.next().as_deref() != Some("daemon") {
        return Err("expected the `daemon` command".into());
    }
    let mut config = Config::default();
    let mut settings_path = opentobii_core::config::default_path();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--no-smooth" => config.force_unsmoothed = true,
            "--config" => {
                settings_path = Some(PathBuf::from(args.next().ok_or("--config needs a path")?))
            }
            "--monitor" => {
                let value = args.next().ok_or("--monitor needs a number")?;
                config.monitor_index = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--monitor: `{value}` is not a number"))?,
                );
            }
            "--socket" => {
                config.tobiid_socket = PathBuf::from(args.next().ok_or("--socket needs a path")?)
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if let Some(path) = settings_path {
        config.tuning = opentobii_core::config::Config::load(&path)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        config.settings_path = Some(path);
    }
    Ok(config)
}

fn main() -> ExitCode {
    let config = match parse(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(reason) => {
            eprintln!("opentobii: {reason}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match start(config) {
        Ok(running) => {
            eprintln!(
                "opentobii: TTP on {}, helper relay on {}",
                running.ttp_addr, running.relay_addr
            );
            loop {
                thread::park();
            }
        }
        Err(err) => {
            eprintln!("opentobii: cannot start: {err}");
            ExitCode::FAILURE
        }
    }
}
