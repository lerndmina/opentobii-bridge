# Changelog

The release workflow publishes the section for the version being released as that release's notes, and the tuning window shows its first lines in the update banner.

## 0.3.2

- "Match Windows" in the Profile card sets a profile to hand the game what it gets on Windows: no extra smoothing, dead zone or curve, with yaw scaled by 0.80 and pitch by 1.10. The driver already smooths head pose as Tobii's software does, so opentobii's own smoothing on top made the view feel damped.

## 0.3.1

- The tracker sleeps when nothing is using it, and its infrared lights go out. It wakes in under a tenth of a second for a game, the tuning window, the gaze overlay, face login or a nod prompt. Before, the lights stayed on for as long as the daemon ran.
- A "Tracker lights" switch in the tuning window turns this off, for use alongside other tracker programs. `[tracker]` in the settings file holds the switch and the 20-second wait.
- `opentobii status` says whether the tracker is awake.

## 0.3.0

- Nod to approve: after Howdy recognises your face on a sudo prompt or a system "Authentication Required" dialog, a banner asks for a nod. A nod approves, a head shake denies, and no answer in 15 seconds falls back to the password. The tracker's head pose decides.
- `opentobii login-helper install` (with sudo) installs the small root-owned program PAM runs for this. `opentobii login-helper` shows the two PAM lines. The README's "Nod to approve" section has the steps.
- `opentobii status` and the Face login camera card say whether the helper is installed.

## 0.2.4

- The Games card lists games outside Steam: Wine prefixes under `~/Games`, Bottles and `~/.wine` that hold Tobii's DLL, such as Star Citizen from the LUG helper.
- "Add a game folder…" adds a prefix from anywhere with your desktop's folder picker. `opentobii games` and `opentobii install` know these games too.
- The Python proofs are removed and the README is rewritten for users.

## 0.2.3

- Face login camera: the daemon can publish the tracker's infrared picture on a v4l2loopback device, for Howdy. It feeds pictures only while a program reads the device.

## 0.2.2

- The tuning window looks for a newer release every hour while it is open, not only when it opens. A window that was already open when a release came out never showed the Update banner.
- The About box has a "Check for updates" button and says how the last check went, including why it failed.
- The update banner shows release notes as plain text.

## 0.2.1

- The launcher entry has one main category.
- An update refreshes the installed launcher entry, icon and service file.

## 0.2.0

- `opentobii setup` installs the program for the current user, with a launcher entry and a service that can start at login.
- Signed releases, and an installed copy that updates itself.
- Gaze overlay with bubble, solid and heatmap styles.
- The tuning window starts, stops and restarts the daemon.
