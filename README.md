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
