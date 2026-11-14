"""Rebuild the tracker's TTP stream frames from live samples.

`tobiid` hands out decoded samples, not the tracker's own frames, and the
game's DLL wants the frames. Every value in a gaze frame is fixed-width, so a
recorded frame serves as a template: the live values are written over the
recorded ones in place and the layout stays exactly what a tracker sends.

A frame is a run of keyed fields. Each is a field id item (`KEY_FIELD_ID`), a
`u32` key, then the value: one scalar item, or a point header followed by two
or three 32.32 fixed-point items. Lengths are in 1/1024 mm and normalised
values are scaled by 1024 as well, so both are `value * 2**42` on the wire.
"""

from __future__ import annotations

import math
import struct
from typing import Sequence

from . import ttp
from .tobiid import GazeRaw, RawEye

TYPE_U32_ALT = 0x01
TYPE_U32 = 0x02
TYPE_FIXED16 = 0x03
TYPE_FIXED32 = 0x04
TYPE_FIELD_ID = 0x05
TYPE_U64 = 0x06

_KEY_ID = struct.pack(">I", ttp.KEY_FIELD_ID)
_POINT_SCALE = float(1 << 42)

KEY_TIMESTAMP = 0x01
KEY_FRAME_COUNTER = 0x14
KEY_COMBINED_VALID = 0x1B
KEY_COMBINED = 0x1C
KEY_RESPONSIVE_VALID = 0x1F
KEY_RESPONSIVE = 0x20
KEY_PRESENCE = 0x02


class EyeKeys:
    def __init__(self, base: int, origin_flag: int, eyeball: int, point_valid: int,
                 origin_display_valid: int, origin_display: int) -> None:
        self.origin_mm = base
        self.origin_in_track_box = base + 1
        self.gaze_point_mm = base + 2
        self.gaze_point_on_display = base + 3
        self.pupil = base + 4
        self.status = base + 5
        self.origin_flag = origin_flag
        self.eyeball = eyeball
        self.point_valid = point_valid
        self.origin_display_valid = origin_display_valid
        self.origin_display = origin_display


LEFT = EyeKeys(0x02, 0x16, 0x17, 0x1D, 0x21, 0x22)
RIGHT = EyeKeys(0x08, 0x15, 0x18, 0x1E, 0x23, 0x24)

Slot = tuple[int, int]  # item type, offset of its value bytes


def _items(payload: bytes):
    """Yield (type, value offset, length) for each item of a payload."""
    pos = 2
    while pos + 5 <= len(payload):
        typ = payload[pos]
        (length,) = struct.unpack_from(">I", payload, pos + 1)
        if pos + 5 + length > len(payload):
            raise ValueError("item runs past the end of the payload")
        yield typ, pos + 5, length
        pos += 5 + length


def keyed_slots(payload: bytes) -> dict[int, list[Slot]]:
    """Map each key of a stream payload to the slots holding its value: one
    for a scalar, two or three for a point."""
    items = list(_items(payload))
    slots: dict[int, list[Slot]] = {}
    i = 0
    while i + 2 < len(items):
        typ, at, length = items[i]
        key_typ, key_at, key_len = items[i + 1]
        if not (typ == TYPE_FIELD_ID and payload[at : at + length] == _KEY_ID
                and key_typ == TYPE_U32 and key_len == 4):
            i += 1
            continue
        (key,) = struct.unpack_from(">I", payload, key_at)
        value_typ, value_at, value_len = items[i + 2]
        if value_typ == TYPE_FIELD_ID and value_len == 4:
            count = payload[value_at + 1]  # 0x00021f40 is 2-D, 0x00031f41 3-D
            parts = items[i + 3 : i + 3 + count]
            slots[key] = [(t, a) for t, a, _ in parts]
            i += 3 + count
        else:
            slots[key] = [(value_typ, value_at)]
            i += 3
    return slots


def keyed_values(payload: bytes) -> dict[int, int | float | list[float]]:
    """Decode every keyed value of a stream payload: integers as they are,
    fixed-point values and points as floats."""
    out: dict[int, int | float | list[float]] = {}
    for key, found in keyed_slots(payload).items():
        decoded: list[int | float] = []
        for typ, at in found:
            if typ in (TYPE_U32, TYPE_U32_ALT):
                decoded.append(struct.unpack_from(">I", payload, at)[0])
            elif typ == TYPE_U64:
                decoded.append(struct.unpack_from(">Q", payload, at)[0])
            elif typ == TYPE_FIXED16:
                decoded.append(struct.unpack_from(">i", payload, at)[0] / 65536)
            elif typ == TYPE_FIXED32:
                decoded.append(struct.unpack_from(">q", payload, at)[0] / _POINT_SCALE)
        if len(found) == 1 and decoded:
            out[key] = decoded[0]
        elif decoded:
            out[key] = [float(v) for v in decoded]
    return out


class Template:
    """A recorded stream payload whose keyed values can be overwritten."""

    def __init__(self, payload: bytes) -> None:
        self.payload = payload
        self.slots = keyed_slots(payload)

    def fill(self) -> "Fill":
        return Fill(self)


class Fill:
    def __init__(self, template: Template) -> None:
        self.out = bytearray(template.payload)
        self.slots = template.slots

    def _slots(self, key: int, types: tuple[int, ...], count: int) -> list[Slot] | None:
        found = self.slots.get(key)
        if found is None or len(found) != count or any(t not in types for t, _ in found):
            return None
        return found

    def word(self, key: int, value: int | None) -> None:
        found = self._slots(key, (TYPE_U32, TYPE_U32_ALT), 1)
        if found and value is not None:
            struct.pack_into(">I", self.out, found[0][1], value & 0xFFFFFFFF)

    def u64(self, key: int, value: int) -> None:
        found = self._slots(key, (TYPE_U64,), 1)
        if found:
            struct.pack_into(">Q", self.out, found[0][1], value & 0xFFFFFFFFFFFFFFFF)

    def fixed16(self, key: int, value: float) -> None:
        found = self._slots(key, (TYPE_FIXED16,), 1)
        if found and math.isfinite(value):
            raw = max(-(1 << 31), min((1 << 31) - 1, round(value * 65536)))
            struct.pack_into(">i", self.out, found[0][1], raw)

    def point(self, key: int, values: Sequence[float] | None) -> None:
        if values is None:
            return
        found = self._slots(key, (TYPE_FIXED32,), len(values))
        if not found or not all(math.isfinite(v) for v in values):
            return
        for (_, at), value in zip(found, values):
            raw = max(-(1 << 63), min((1 << 63) - 1, round(value * _POINT_SCALE)))
            struct.pack_into(">q", self.out, at, raw)

    def done(self) -> bytes:
        return bytes(self.out)


Vec3 = Sequence[float]


class DisplayFrame:
    """The display frame of a display area given as three corners in the
    tracker frame: origin at the display centre, +x right, +y up, +z towards
    the user."""

    def __init__(self, top_left: Vec3, top_right: Vec3, bottom_left: Vec3) -> None:
        self.centre = [(a + b) / 2 for a, b in zip(top_right, bottom_left)]
        self.x = _unit([a - b for a, b in zip(top_right, top_left)])
        self.y = _unit([a - b for a, b in zip(top_left, bottom_left)])
        self.z = [
            self.x[1] * self.y[2] - self.x[2] * self.y[1],
            self.x[2] * self.y[0] - self.x[0] * self.y[2],
            self.x[0] * self.y[1] - self.x[1] * self.y[0],
        ]

    def from_tracker(self, point: Vec3) -> tuple[float, float, float]:
        d = [a - b for a, b in zip(point, self.centre)]
        return (_dot(d, self.x), _dot(d, self.y), _dot(d, self.z))


def _dot(a: Vec3, b: Vec3) -> float:
    return sum(x * y for x, y in zip(a, b))


def _unit(v: Vec3) -> list[float]:
    length = math.sqrt(_dot(v, v))
    if length == 0:
        raise ValueError("display area has a side of no length")
    return [x / length for x in v]


def _eye(fill: Fill, keys: EyeKeys, eye: RawEye, origin_flag: int | None,
         eyeball: Vec3 | None, display: DisplayFrame | None) -> None:
    tracked = eye.status == 0
    fill.point(keys.origin_mm, eye.gaze_origin_mm)
    fill.point(keys.origin_in_track_box, eye.gaze_origin_in_track_box)
    fill.point(keys.gaze_point_mm, eye.gaze_point_mm)
    fill.point(keys.gaze_point_on_display, eye.gaze_point_on_display)
    fill.fixed16(keys.pupil, eye.pupil_diameter_mm)
    fill.word(keys.status, eye.status)
    fill.word(keys.origin_flag, origin_flag)
    fill.point(keys.eyeball, eyeball)
    # The raw sample does not carry these two flags. The tracker sets them
    # with the eye's status on every recorded frame, so follow the status.
    fill.word(keys.point_valid, int(tracked))
    origin_valid = int(tracked if origin_flag is None else bool(origin_flag))
    if display is None:
        # Without a display area the recorded display-frame origin would be
        # someone else's: mark it invalid rather than pass it on.
        origin_valid = 0
    else:
        fill.point(keys.origin_display, display.from_tracker(eye.gaze_origin_mm))
    fill.word(keys.origin_display_valid, origin_valid)


def gaze_frame(template: Template, raw: GazeRaw, micros: int,
               display: DisplayFrame | None) -> bytes:
    """The gaze stream payload for one live sample, stamped `micros`.

    Keys the sample does not carry keep the template's values: the user
    position guide (0x25..0x28) and the fields an Eye Tracker 5 never fills
    (0x19, 0x1a, 0x29..0x2c)."""
    fill = template.fill()
    fill.u64(KEY_TIMESTAMP, micros)
    fill.word(0x11, raw.key_11)
    fill.word(KEY_FRAME_COUNTER, raw.frame_counter)
    fill.word(KEY_COMBINED_VALID, raw.combined_validity)
    fill.point(KEY_COMBINED, raw.combined_on_display)
    _eye(fill, LEFT, raw.left, raw.left_origin_flag, raw.left_eyeball_center_mm, display)
    _eye(fill, RIGHT, raw.right, raw.right_origin_flag, raw.right_eyeball_center_mm, display)
    if raw.responsive_on_display is not None:
        fill.word(KEY_RESPONSIVE_VALID, raw.responsive_validity)
        fill.point(KEY_RESPONSIVE, raw.responsive_on_display)
    else:
        fill.word(KEY_RESPONSIVE_VALID, 0)
    return fill.done()


def presence_frame(template: Template, status: int, micros: int) -> bytes:
    fill = template.fill()
    fill.u64(KEY_TIMESTAMP, micros)
    fill.word(KEY_PRESENCE, status)
    return fill.done()


def display_area_reply(template: bytes, top_left: Vec3, top_right: Vec3,
                       bottom_left: Vec3) -> bytes:
    """Command 1430's reply with the three corners replaced. The reply is
    three unkeyed 3-D points followed by fields that are left as recorded."""
    out = bytearray(template)
    offsets = [at for typ, at, length in _items(template) if typ == TYPE_FIXED32 and length == 8]
    values = [*top_left, *top_right, *bottom_left]
    if len(offsets) < len(values):
        raise ValueError("display area template holds fewer than nine values")
    for at, value in zip(offsets, values):
        struct.pack_into(">q", out, at, round(value * _POINT_SCALE))
    return bytes(out)


def display_area_corners(reply: bytes) -> tuple[list[float], list[float], list[float]]:
    values = [
        struct.unpack_from(">q", reply, at)[0] / _POINT_SCALE
        for typ, at, length in _items(reply)
        if typ == TYPE_FIXED32 and length == 8
    ]
    return values[0:3], values[3:6], values[6:9]
