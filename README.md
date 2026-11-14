# opentobii-bridge

Work in progress. Not affiliated with Tobii.

Goal: make the Tobii Eye Tracker 5 usable on Linux in two places where it is not today.

1. Games under Proton that ship Tobii's own `tobii_gameintegration_x64.dll`, starting with Euro Truck Simulator 2. No game files are modified.
2. Howdy face login, by exposing the tracker's IR camera as a V4L2 device.

This repo does not contain a USB driver. It builds on an existing Linux driver for the tracker.

## Status

| Proof | What it shows | State |
|---|---|---|
| 1 | The game's Tobii DLL connects to a Linux listener on `127.0.0.1:4455` under Proton | Passed 2026-11-14, ETS2 with Tobii DLL 7.3.0.14, Proton-CachyOS |
| 2 | Gaze reaches the game | Partly. On a fresh start the DLL ran its whole device start against the responder and accepted every reply (2026-11-14). The game then did not select the tracker: it matches trackers to monitors by a display id that only the helper can supply |
| 3 | Helper in the prefix supplies display id and head pose | Passed 2026-11-14, Proton-CachyOS. The game selected the tracker, subscribed to gaze and head pose, and the cabin view followed a synthetic yaw sweep |
| 4 | The helper starts by itself as a service in the prefix | Passed 2026-11-14, Proton-CachyOS. Wine started the helper from the service key the installer adds to `system.reg` |
| 5 | Live gaze and head pose from a tracker reach the game | Passed 2026-11-14, Proton-CachyOS, Skyggedans tobii-linux 0.6.0. The cabin view follows real head movement with no noticeable lag. Unfiltered it was jittery, so head pose is now smoothed |

Not yet done: gaze features in the game, tuning of the head pose smoothing, and a run on Valve's stock Proton.

## The daemon

The Python proofs are being replaced by one Rust program. Build and run it with:

```sh
cargo build --release
./target/release/opentobii daemon
```

It needs `tobiid` running and the helper installed in the game's prefix (see below). It does what proof 5 does: live gaze, presence and smoothed head pose to the game, with the driver's display area. `--no-smooth`, `--monitor N` and `--socket PATH` are the only options so far; the tuning settings and GUI are not built yet.

### Setting up a game

```sh
opentobii games              # installed Steam games that ship Tobii's DLL
opentobii install 227300     # by app id, part of the name, or a prefix path
opentobii uninstall 227300
opentobii status             # what the running daemon is connected to
```

`install` puts the helper (built into the binary) under `drive_c/opentobii/` in the game's prefix and adds one service key to `system.reg`, keeping a backup beside it. `uninstall` removes both and leaves `system.reg` byte for byte as it was. Both refuse to run while the game is open. Building needs `mingw64-gcc` for the helper; without it the binary builds with no helper inside and `install` takes `--helper PATH`.

To start the daemon at login, copy the binary to `~/.local/bin/` and `packaging/opentobii.service` to `~/.config/systemd/user/`, then `systemctl --user enable --now opentobii`.

### The tuning window

```sh
opentobii gui
```

![The opentobii tuning window](docs/gui.png)

Shows what the tracker sees against what is sent to the game, and edits the selected profile live: smoothing, and per axis a dead zone, sensitivity, curve, limit and invert. "Recentre" makes your current head pose the neutral one. The Calibration card runs the driver's `tobii-calibrate` on the monitor you pick, which sets up the screen the tracker sits on and then calibrates your eyes; the daemon picks up a changed screen setup by itself. It talks to the running daemon, so start that first. Building it needs `fontconfig-devel`; `cargo build --no-default-features` builds the daemon and commands without the window.

### Settings

Tuning lives in `~/.config/opentobii/config.toml` (or `--config PATH`): one `[default]` profile and one per game under `[games."name.exe"]`. Each axis (`pitch`, `yaw`, `roll`, `x`, `y`, `z`) has `dead_zone`, `sensitivity`, `curve`, `invert` and `limit`; `[default.smoothing]` holds the filter settings. A file with a mistake in it stops the daemon from starting and says what is wrong.

Per-game profiles need the helper built from this version (hello `OTB2`), which reports the game's executable. With an older helper every game uses the default profile.

### Control socket

The daemon listens on `$XDG_RUNTIME_DIR/opentobii.sock`, one JSON object per line. Requests: `{"cmd":"get"}`, `{"cmd":"set_profile","game":"name.exe","profile":{...}}` (leave `game` out for the default), `{"cmd":"delete_profile","game":"name.exe"}`, `{"cmd":"recentre"}` and `{"cmd":"subscribe"}` for a feed of raw head poses and a status line every second. Changes apply on the next pose and are saved to the settings file. The tuning GUI will use this; it is not built yet.

```sh
cargo test
```

runs the Rust tests, including an end-to-end test with a fake driver, game and helper.

## Proof 1

```sh
python3 proofs/proof1_listen.py 40
```

Start it while the game is running. The DLL rescans every 10 seconds. Expected output is an empty discovery connection, then a second connection carrying a 71-byte TTP message with command 1000 (protocol upgrade). The script only logs; it sends nothing and exits after the given number of seconds.

## Proof 2

```sh
python3 proofs/proof2_replay.py 300
```

Start it before the game. It answers the DLL with replies recorded from a real tracker (`fixtures/et5`) and replays one recorded gaze frame at 30 Hz once the DLL subscribes. The DLL tries to open the device once per game run, so a responder started after a failed attempt only sees discovery probes.

## Proof 3

Build the helper (needs `mingw64-gcc`), install it into the game's prefix while the game is closed, start the Linux side, then start the game:

```sh
make -C helper
python3 -m opentobii_bridge.prefix install <prefix> helper/sesp_relay.exe
python3 proofs/proof3_service.py 600
```

`<prefix>` is the folder holding `system.reg`, for a Steam game `steamapps/compatdata/<appid>/pfx`. The installer copies the helper to `drive_c/opentobii/` and adds one service key to `system.reg`, keeping a backup beside it. `python3 -m opentobii_bridge.prefix uninstall <prefix>` removes both and leaves `system.reg` byte for byte as it was.

In ETS2, eye tracking also has to be switched on in the game's options. With that on, the interior view swings slowly left and right while the service runs. The game writes no `[Tobii]` line to `game.log.txt`, so the log does not show whether it worked.

## Proof 5

```sh
python3 proofs/proof5_live.py 1800
```

As proof 3, with the data taken from `tobiid`, the daemon of [Skyggedans/tobii-linux](https://github.com/Skyggedans/tobii-linux), on its socket at `$XDG_RUNTIME_DIR/tobiid.sock`. It subscribes to raw gaze, presence and head pose and asks for the display area. Gaze and presence frames are rebuilt in the tracker's own layout from the live values, using a recorded frame as the template. Head poses are passed on as they come.

Head pose is smoothed with a One Euro filter, because `tobiid`'s estimate has a few tenths of a degree of frame-to-frame noise and the game scales rotation up. `--rot-cutoff` (default 0.8 Hz) sets how steady a still head is: lower is steadier and slower to settle. `--rot-beta` (default 0.1) sets how closely fast turns are followed. `--no-smooth` turns it off.

After its last client leaves, `tobiid` restarts the tracker, and a client that connects during the restart waits about 12 seconds for its first sample.

Known gaps:

- Device identity and the other start-up replies still come from the recordings, so the game sees the recorded serial number.
- The user position guide fields of a gaze frame keep the recorded values; `tobiid`'s raw gaze sample does not carry them.
- A head pose with any part marked invalid is dropped, because the width of the validity fields in the services message is not confirmed.

## Tests

```sh
python3 -m unittest discover -s tests
```

## Licence

MIT, see `LICENSE`. The recorded messages in `fixtures/et5` are from Skyggedans/tobii-linux, also MIT, see `fixtures/et5/LICENSE-skyggedans`.
