#!/bin/sh
# Remove what install.sh put in the home folder. Settings in
# ~/.config/opentobii and the helper inside game prefixes are left alone:
# take the helper out first with `opentobii uninstall <game>`.
set -eu

bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now opentobii.service 2>/dev/null || true
fi
rm -f "$bin/opentobii" "$units/opentobii.service" \
    "$data/applications/opentobii.desktop" \
    "$data/icons/hicolor/scalable/apps/opentobii.svg"
if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload 2>/dev/null || true
fi
echo "opentobii removed. Settings in ~/.config/opentobii were kept."
