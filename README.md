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
| 3 | Helper in the prefix supplies display id and head pose | Helper, Linux service and prefix installer written and unit tested; not yet run with the game |
| 4 | The helper starts by itself as a service in the prefix | Built in: the helper is a Windows service and the installer registers it. Not yet run |

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

## Tests

```sh
python3 -m unittest discover -s tests
```

## Licence

Not chosen yet.
