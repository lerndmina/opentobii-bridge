"""Install the relay helper into a Wine or Proton prefix, and remove it again.

Two things go into the prefix: the helper executable under
`drive_c/opentobii/`, and one registry key in `system.reg` that registers it as
an auto-start Windows service. Nothing outside the prefix is touched.

`system.reg` is edited as text, which is only safe while no wineserver has the
prefix open: Wine rewrites the file from memory when it shuts down. Both
operations refuse to run if one does, and keep a backup of the file they edit.

    python3 -m opentobii_bridge.prefix install   <prefix> <sesp_relay.exe>
    python3 -m opentobii_bridge.prefix uninstall <prefix>
    python3 -m opentobii_bridge.prefix status    <prefix>
"""

from __future__ import annotations

import os
import pathlib
import shutil
import sys

SERVICE = "OpenTobiiRelay"
# Wine keeps services under ControlSet001; CurrentControlSet is a link to it.
KEY = f"[System\\\\ControlSet001\\\\Services\\\\{SERVICE}]"
INSTALL_DIR = pathlib.PurePosixPath("drive_c/opentobii")
EXE_NAME = "sesp_relay.exe"
BACKUP_SUFFIX = ".opentobii-backup"

KEY_BLOCK = "\n".join(
    [
        KEY,
        '"DisplayName"="OpenTobii relay"',
        '"ErrorControl"=dword:00000000',
        f'"ImagePath"=str(2):"C:\\\\opentobii\\\\{EXE_NAME}"',
        '"ObjectName"="LocalSystem"',
        '"Start"=dword:00000002',
        '"Type"=dword:00000010',
        "",
        "",
    ]
)


def add_service(registry: str) -> str:
    """Return `registry` with the service key present exactly once, last."""
    return remove_service(registry).rstrip("\n") + "\n\n" + KEY_BLOCK


def remove_service(registry: str) -> str:
    """Return `registry` without the service key. A key runs from its
    `[header]` line to the blank line after it, or to the next header."""
    out: list[str] = []
    skipping = False
    cut: int | None = None
    for line in registry.splitlines(keepends=True):
        if skipping:
            if line.strip() == "":
                skipping = False
                continue
            if not line.startswith("["):
                continue
            skipping = False
        if line.startswith(KEY):
            skipping = True
            cut = len(out)
            continue
        out.append(line)
    # If our key was the last one, also drop the blank line that separated it
    # from the key before, so the file ends as it did before the install.
    if cut is not None and cut == len(out) and out and out[-1].strip() == "":
        out.pop()
    return "".join(out)


def has_service(registry: str) -> bool:
    return any(line.startswith(KEY) for line in registry.splitlines())


def prefix_in_use(prefix: pathlib.Path) -> bool:
    """True if a wineserver is running with this prefix."""
    want = os.path.realpath(prefix)
    for entry in pathlib.Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if b"wineserver" not in (entry / "cmdline").read_bytes():
                continue
            env = (entry / "environ").read_bytes().split(b"\0")
        except OSError:
            continue
        for item in env:
            if item.startswith(b"WINEPREFIX=") and os.path.realpath(item[11:].decode()) == want:
                return True
    return False


def _system_reg(prefix: pathlib.Path) -> pathlib.Path:
    path = prefix / "system.reg"
    if not path.is_file():
        raise SystemExit(f"{path} not found: is {prefix} a Wine prefix?")
    return path


def _require_idle(prefix: pathlib.Path) -> None:
    if prefix_in_use(prefix):
        raise SystemExit(f"{prefix} is in use by a running wineserver. Quit the game first.")


def _write(path: pathlib.Path, text: str) -> None:
    backup = path.with_name(path.name + BACKUP_SUFFIX)
    if not backup.exists():
        shutil.copy2(path, backup)
    tmp = path.with_name(path.name + ".opentobii-tmp")
    tmp.write_text(text, encoding="utf-8", newline="")
    os.replace(tmp, path)


def install(prefix: pathlib.Path, exe: pathlib.Path) -> None:
    registry = _system_reg(prefix)
    _require_idle(prefix)
    target = prefix / INSTALL_DIR
    target.mkdir(parents=True, exist_ok=True)
    shutil.copy2(exe, target / EXE_NAME)
    _write(registry, add_service(registry.read_text(encoding="utf-8", newline="")))
    print(f"installed {target / EXE_NAME} and service {SERVICE}")


def uninstall(prefix: pathlib.Path) -> None:
    registry = _system_reg(prefix)
    _require_idle(prefix)
    text = registry.read_text(encoding="utf-8", newline="")
    if has_service(text):
        _write(registry, remove_service(text))
    target = prefix / INSTALL_DIR
    if (target / EXE_NAME).exists():
        (target / EXE_NAME).unlink()
    if target.is_dir() and not any(target.iterdir()):
        target.rmdir()
    print(f"removed service {SERVICE} and {target / EXE_NAME}")


def status(prefix: pathlib.Path) -> None:
    registry = _system_reg(prefix)
    text = registry.read_text(encoding="utf-8", newline="")
    print(f"service key present: {has_service(text)}")
    print(f"helper present:      {(prefix / INSTALL_DIR / EXE_NAME).exists()}")
    print(f"prefix in use:       {prefix_in_use(prefix)}")


def main(argv: list[str]) -> None:
    if len(argv) == 4 and argv[1] == "install":
        install(pathlib.Path(argv[2]), pathlib.Path(argv[3]))
    elif len(argv) == 3 and argv[1] == "uninstall":
        uninstall(pathlib.Path(argv[2]))
    elif len(argv) == 3 and argv[1] == "status":
        status(pathlib.Path(argv[2]))
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
