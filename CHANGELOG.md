# Changelog

The release workflow publishes the section for the version being released as that release's notes, and the tuning window shows its first lines in the update banner.

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
