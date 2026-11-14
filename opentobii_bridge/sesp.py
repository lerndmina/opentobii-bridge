"""Stream Engine Services protocol (SESP), service side.

The game DLL's legacy TTP module has no head pose or display id of its own. It
asks "Stream Engine Services" for both over the named pipe
`\\\\.\\pipe\\streamengineservices`.

Framing: the bytes "sesp", a little-endian u32 size, the size XOR 0x70736573,
then a FlatBuffers buffer of that size. The root table has three fields:
0 sequence (u32), 1 type (u8), 2 payload table.

The message list and field numbers come from the protocol description in
Garulf/tobii-linux `docs/sesp.md`; no code is taken from that project. The
buffers built here are valid FlatBuffers but are not laid out byte for byte as
Tobii's builder does it. That has not been tested against the DLL's verifier.
"""

from __future__ import annotations

import struct
from dataclasses import dataclass, field
from typing import Union

MAGIC = 0x70736573  # "sesp" read as a little-endian u32
FRAME_HEAD = struct.Struct("<III")

CHANNEL_COMMAND = 0x2711
CHANNEL_SUBSCRIPTION = 0x2712

T_INVALID_OPERATION = 1
T_INITIALIZE = 2
T_INITIALIZE_RESPONSE = 3
T_HEAD_POSE_START = 4
T_HEAD_POSE_START_RESPONSE = 5
T_HEAD_POSE_STOP = 6
T_HEAD_POSE_STOP_RESPONSE = 7
T_HEAD_POSE = 8
T_DISPLAY_INFO = 9
T_DISPLAY_INFO_RESPONSE = 10
T_STREAM_STATUS = 0x1A
T_STREAM_STATUS_RESPONSE = 0x1B
T_STATISTICS = 0x32
T_STATISTICS_RESPONSE = 0x33

ERROR_NOT_SUPPORTED = 6

CAP_HEAD_POSE = 1
CAP_DISPLAY_INFO = 2
CAP_STREAM_STATUS = 3


# --- framing ---------------------------------------------------------------


def frame(buffer: bytes) -> bytes:
    return FRAME_HEAD.pack(MAGIC, len(buffer), len(buffer) ^ MAGIC) + buffer


def unframe(data: bytes) -> tuple[bytes | None, bytes]:
    """Split one buffer off the front of `data`.

    Returns (buffer, rest), or (None, data) when a whole frame has not arrived.
    """
    if len(data) < FRAME_HEAD.size:
        return None, data
    magic, size, check = FRAME_HEAD.unpack_from(data)
    if magic != MAGIC or size ^ MAGIC != check:
        raise ValueError("not a SESP frame")
    end = FRAME_HEAD.size + size
    if len(data) < end:
        return None, data
    return data[FRAME_HEAD.size : end], data[end:]


# --- a minimal FlatBuffers writer -------------------------------------------


@dataclass(frozen=True)
class U8:
    value: int


@dataclass(frozen=True)
class U32:
    value: int


@dataclass(frozen=True)
class F32:
    value: float


@dataclass
class Table:
    """Fields by number. A missing number means "left at its default"."""

    fields: dict[int, "Value"] = field(default_factory=dict)


Value = Union[U8, U32, F32, str, Table]


def _pad4(out: bytearray) -> None:
    out.extend(b"\x00" * (-len(out) % 4))


def _write_table(out: bytearray, table: Table) -> int:
    """Append a table, its vtable and everything it points to. Returns the
    table's position."""
    count = max(table.fields, default=-1) + 1
    _pad4(out)
    vtable_at = len(out)
    out.extend(b"\x00" * (4 + 2 * count))
    _pad4(out)
    table_at = len(out)
    out.extend(struct.pack("<i", table_at - vtable_at))

    slots: dict[int, int] = {}
    for number in sorted(table.fields):
        slots[number] = len(out)
        value = table.fields[number]
        if isinstance(value, U8):
            out.extend(struct.pack("<B3x", value.value))
        elif isinstance(value, U32):
            out.extend(struct.pack("<I", value.value))
        elif isinstance(value, F32):
            out.extend(struct.pack("<f", value.value))
        else:
            out.extend(b"\x00\x00\x00\x00")  # offset, filled in below
    table_size = len(out) - table_at

    struct.pack_into("<HH", out, vtable_at, 4 + 2 * count, table_size)
    for number, at in slots.items():
        struct.pack_into("<H", out, vtable_at + 4 + 2 * number, at - table_at)

    for number, at in slots.items():
        value = table.fields[number]
        if isinstance(value, str):
            _pad4(out)
            target = len(out)
            raw = value.encode()
            out.extend(struct.pack("<I", len(raw)) + raw + b"\x00")
            _pad4(out)
        elif isinstance(value, Table):
            target = _write_table(out, value)
        else:
            continue
        struct.pack_into("<I", out, at, target - at)
    return table_at


def build(sequence: int, kind: int, payload: Table | None = None) -> bytes:
    """One message as a FlatBuffers buffer (not framed)."""
    root = Table({0: U32(sequence), 1: U8(kind), 2: payload or Table()})
    out = bytearray(4)
    struct.pack_into("<I", out, 0, _write_table(out, root))
    return bytes(out)


# --- a minimal FlatBuffers reader -------------------------------------------


class View:
    """Read access to one table of a buffer."""

    def __init__(self, buf: bytes, at: int) -> None:
        if at + 4 > len(buf):
            raise ValueError("table outside the buffer")
        self.buf, self.at = buf, at
        self.vtable = at - struct.unpack_from("<i", buf, at)[0]
        if not 0 <= self.vtable <= len(buf) - 4:
            raise ValueError("vtable outside the buffer")
        self.vtable_size = struct.unpack_from("<H", buf, self.vtable)[0]

    def _slot(self, number: int) -> int | None:
        entry = 4 + 2 * number
        if entry + 2 > self.vtable_size:
            return None
        offset = struct.unpack_from("<H", self.buf, self.vtable + entry)[0]
        return self.at + offset if offset else None

    def u8(self, number: int, default: int = 0) -> int:
        at = self._slot(number)
        return default if at is None else self.buf[at]

    def u32(self, number: int, default: int = 0) -> int:
        at = self._slot(number)
        return default if at is None else struct.unpack_from("<I", self.buf, at)[0]

    def f32(self, number: int, default: float = 0.0) -> float:
        at = self._slot(number)
        return default if at is None else struct.unpack_from("<f", self.buf, at)[0]

    def string(self, number: int) -> str | None:
        at = self._slot(number)
        if at is None:
            return None
        target = at + struct.unpack_from("<I", self.buf, at)[0]
        (length,) = struct.unpack_from("<I", self.buf, target)
        return self.buf[target + 4 : target + 4 + length].decode(errors="replace")

    def table(self, number: int) -> "View | None":
        at = self._slot(number)
        if at is None:
            return None
        return View(self.buf, at + struct.unpack_from("<I", self.buf, at)[0])


@dataclass(frozen=True)
class Parsed:
    sequence: int
    kind: int
    payload: View | None


def parse(buffer: bytes) -> Parsed:
    root = View(buffer, struct.unpack_from("<I", buffer, 0)[0])
    return Parsed(root.u32(0), root.u8(1), root.table(2))


# --- the service's messages --------------------------------------------------


def initialize_response(sequence: int, capabilities: set[int], error: int = 0) -> bytes:
    fields: dict[int, Value] = {cap: U8(1) for cap in capabilities}
    if error:
        fields[0] = U32(error)
    return build(sequence, T_INITIALIZE_RESPONSE, Table(fields))


def error_response(sequence: int, kind: int, error: int = 0) -> bytes:
    """A response whose only payload field is the error word."""
    return build(sequence, kind, Table({0: U32(error)} if error else {}))


def display_info_response(sequence: int, monitor_id: str, a: int = 0, b: int = 0) -> bytes:
    """`a` and `b` are the response's two u32 fields; what they mean is unknown."""
    fields: dict[int, Value] = {1: monitor_id}
    if a:
        fields[2] = U32(a)
    if b:
        fields[3] = U32(b)
    return build(sequence, T_DISPLAY_INFO_RESPONSE, Table(fields))


def head_pose(
    sequence: int,
    micros: int,
    position_mm: tuple[float, float, float],
    rotation_rad: tuple[float, float, float],
) -> bytes:
    """A head pose with every validity left at its default of 1 (valid)."""
    fields: dict[int, Value] = {
        0: U32((micros >> 32) & 0xFFFFFFFF),
        1: U32(micros & 0xFFFFFFFF),
    }
    for i, value in enumerate(position_mm):
        fields[2 + i] = F32(value)
    for i, value in enumerate(rotation_rad):
        fields[5 + i] = F32(value)
    return build(sequence, T_HEAD_POSE, Table(fields))


def respond(request: Parsed, capabilities: set[int], monitor_id: str) -> bytes:
    """The service's answer to one command-channel request."""
    seq = request.sequence
    if request.kind == T_INITIALIZE:
        return initialize_response(seq, capabilities)
    if request.kind == T_HEAD_POSE_START:
        return error_response(seq, T_HEAD_POSE_START_RESPONSE)
    if request.kind == T_HEAD_POSE_STOP:
        return error_response(seq, T_HEAD_POSE_STOP_RESPONSE)
    if request.kind == T_DISPLAY_INFO:
        return display_info_response(seq, monitor_id)
    if request.kind == T_STREAM_STATUS:
        return build(seq, T_STREAM_STATUS_RESPONSE)
    if request.kind == T_STATISTICS:
        return error_response(seq, T_STATISTICS_RESPONSE)
    return error_response(seq, T_INVALID_OPERATION, ERROR_NOT_SUPPORTED)
