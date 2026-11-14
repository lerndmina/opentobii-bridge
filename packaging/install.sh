#!/bin/sh
# Install opentobii for the current user, from the folder this script is in.
# Nothing here needs root, and nothing is started.
#
#   ./install.sh                    install
#   ./install.sh --enable-service   install, and start the daemon now and at login
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
units="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
enable=no

for arg in "$@"; do
    case "$arg" in
        --enable-service) enable=yes ;;
        -h|--help) sed -n '2,7p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "install.sh: unknown argument '$arg'" >&2; exit 2 ;;
    esac
done

if [ ! -x "$here/opentobii" ]; then
    echo "install.sh: no opentobii binary beside this script" >&2
    exit 1
fi

mkdir -p "$bin" "$units" "$data/applications" "$data/icons/hicolor/scalable/apps"
install -m 755 "$here/opentobii" "$bin/opentobii"
# The packaged unit runs /usr/bin/opentobii; here it lives in the home folder.
sed "s|^ExecStart=.*|ExecStart=$bin/opentobii daemon|" "$here/opentobii.service" > "$units/opentobii.service"
sed "s|^Exec=.*|Exec=$bin/opentobii gui|" "$here/opentobii.desktop" > "$data/applications/opentobii.desktop"
install -m 644 "$here/opentobii.svg" "$data/icons/hicolor/scalable/apps/opentobii.svg"

echo "Installed:"
echo "  $bin/opentobii"
echo "  $units/opentobii.service"
echo "  $data/applications/opentobii.desktop"

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload 2>/dev/null || true
    if [ "$enable" = yes ]; then
        systemctl --user enable --now opentobii.service
        echo "The daemon is running and starts at login."
    else
        echo
        echo "To start the daemon now and at every login:"
        echo "  systemctl --user enable --now opentobii"
    fi
fi

case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo; echo "Note: $bin is not on your PATH. Use the full path, or add it." ;;
esac

if ! command -v tobiid >/dev/null 2>&1; then
    echo
    echo "The tracker's driver (tobiid) was not found. opentobii needs it:"
    echo "  https://github.com/Skyggedans/tobii-linux"
fi

echo
echo "Next: set up a game, with the game closed:"
echo "  opentobii games"
echo "  opentobii install <app id or name>"
