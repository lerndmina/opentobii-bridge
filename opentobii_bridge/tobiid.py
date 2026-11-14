"""Client side of the `tobiid` socket protocol (Skyggedans/tobii-linux 0.6.0).

`tobiid` owns the tracker's USB connection and serves decoded samples on a
Unix socket. The layouts here follow that project's `tobii-ipc` crate; this is
an independent reading of its documented wire format, covering only what the
bridge needs: subscribing, the raw gaze, presence and head pose samples, and
the display area request.

A frame is a little-endian `u32` length, then a body whose first byte is a tag.
"""

from __future__ import annotations

import os
import pathlib
import struct
from dataclasses import dataclass

TAG_SUBSCRIBE = 0x01
TAG_REQUEST = 0x03
TAG_SUBSCRIBED = 0x10
TAG_REPLY = 0x11
TAG_PRESENCE = 0x22
TAG_GAZE_RAW = 0x28
TAG_HEAD_POSE = 0x29

STREAM_PRESENCE = 1 << 2
STREAM_GAZE_RAW = 1 << 8
STREAM_HEAD_POSE = 1 << 9

KIND_DISPLAY_AREA_GET = 3
STATUS_OK = 0

PRESENCE_UNKNOWN = 0
PRESENCE_AWAY = 1
PRESENCE_PRESENT = 2

Vec3 = tuple[float, float, float]
Vec2 = tuple[float, float]


def socket_path() -> pathlib.Path:
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    return pathlib.Path(runtime) / "tobiid.sock" if runtime else pathlib.Path("/tmp/tobiid.sock")


def frame(body: bytes) -> bytes:
    return struct.pack("<I", len(body)) + body


def unframe(buffer: bytes) -> tuple[bytes | None, bytes]:
    """Split one frame body off the front of `buffer`. Returns `(None,
    buffer)` while the frame is incomplete."""
    if len(buffer) < 4:
        return None, buffer
    (length,) = struct.unpack_from("<I", buffer)
    if len(buffer) < 4 + length:
        return None, buffer
    return buffer[4 : 4 + length], buffer[4 + length :]


def subscribe(streams: int) -> bytes:
    return bytes([TAG_SUBSCRIBE]) + struct.pack("<I", streams)


def request(request_id: int, kind: int, payload: bytes = b"") -> bytes:
    return bytes([TAG_REQUEST]) + struct.pack("<IB", request_id, kind) + payload


@dataclass(frozen=True)
class Subscribed:
    ok: bool


@dataclass(frozen=True)
class Reply:
    request_id: int
    status: int
    payload: bytes


@dataclass(frozen=True)
class Presence:
    host_micros: int
    status: int


@dataclass(frozen=True)
class HeadPose:
    """Absolute, in the display frame: origin at the display centre, +x right,
    +y up, +z towards the user. Position in mm, rotation in radians."""

    host_micros: int
    position_valid: bool
    position_mm: Vec3
    rotation_valid: tuple[bool, bool, bool]
    rotation_rad: Vec3

    @property
    def valid(self) -> bool:
        return self.position_valid and all(self.rotation_valid)


@dataclass(frozen=True)
class RawEye:
    gaze_origin_mm: Vec3
    gaze_origin_in_track_box: Vec3
    gaze_point_mm: Vec3
    gaze_point_on_display: Vec2
    pupil_diameter_mm: float
    status: int  # 0 tracked, 4 lost


@dataclass(frozen=True)
class GazeRaw:
    """One gaze frame as the tracker sent it. `None` marks a field the frame
    did not carry."""

    device_micros: int
    left: RawEye
    right: RawEye
    combined_on_display: Vec2
    combined_validity: int
    key_0e: int | None
    key_11: int | None
    frame_counter: int | None
    left_origin_flag: int | None
    right_origin_flag: int | None
    left_eyeball_center_mm: Vec3 | None
    right_eyeball_center_mm: Vec3 | None
    host_micros: int | None
    responsive_on_display: Vec2 | None
    responsive_validity: int


_EYE = struct.Struct("<12fI")
_HEAD_POSE = struct.Struct("<qB6f")
_PRESENCE = struct.Struct("<qB")
_OPT_U32 = struct.Struct("<BI")
_OPT_VEC3 = struct.Struct("<B3f")


def _eye(body: bytes, at: int) -> RawEye:
    v = _EYE.unpack_from(body, at)
    return RawEye(v[0:3], v[3:6], v[6:9], v[9:11], v[11], v[12])


def _gaze_raw(body: bytes) -> GazeRaw | None:
    fixed = 1 + 8 + 2 * _EYE.size + 12 + 5 * _OPT_U32.size + 2 * _OPT_VEC3.size
    if len(body) < fixed:
        return None
    at = 1
    (device_micros,) = struct.unpack_from("<q", body, at)
    at += 8
    left = _eye(body, at)
    right = _eye(body, at + _EYE.size)
    at += 2 * _EYE.size
    cx, cy, validity = struct.unpack_from("<2fI", body, at)
    at += 12
    words: list[int | None] = []
    for _ in range(5):
        present, value = _OPT_U32.unpack_from(body, at)
        words.append(value if present else None)
        at += _OPT_U32.size
    points: list[Vec3 | None] = []
    for _ in range(2):
        present, *xyz = _OPT_VEC3.unpack_from(body, at)
        points.append(tuple(xyz) if present else None)
        at += _OPT_VEC3.size
    host_micros = None
    responsive = None
    responsive_validity = 0
    if len(body) >= at + 8:
        (host_micros,) = struct.unpack_from("<q", body, at)
        at += 8
        if len(body) >= at + 12:
            rx, ry, responsive_validity = struct.unpack_from("<2fI", body, at)
            responsive = (rx, ry)
    return GazeRaw(
        device_micros, left, right, (cx, cy), validity,
        words[0], words[1], words[2], words[3], words[4],
        points[0], points[1], host_micros, responsive, responsive_validity,
    )


def decode(body: bytes) -> Subscribed | Reply | Presence | HeadPose | GazeRaw | None:
    """Decode one daemon-to-client frame body. `None` for a tag this module
    does not read, or a body too short for its tag."""
    if not body:
        return None
    tag = body[0]
    if tag == TAG_SUBSCRIBED and len(body) >= 2:
        return Subscribed(bool(body[1]))
    if tag == TAG_REPLY and len(body) >= 6:
        request_id, status = struct.unpack_from("<IB", body, 1)
        return Reply(request_id, status, body[6:])
    if tag == TAG_PRESENCE and len(body) >= 1 + _PRESENCE.size:
        return Presence(*_PRESENCE.unpack_from(body, 1))
    if tag == TAG_HEAD_POSE and len(body) >= 1 + _HEAD_POSE.size:
        micros, flags, *v = _HEAD_POSE.unpack_from(body, 1)
        return HeadPose(
            micros,
            bool(flags & 1),
            tuple(v[0:3]),
            (bool(flags & 2), bool(flags & 4), bool(flags & 8)),
            tuple(v[3:6]),
        )
    if tag == TAG_GAZE_RAW:
        return _gaze_raw(body)
    return None


def display_area(payload: bytes) -> tuple[Vec3, Vec3, Vec3] | None:
    """Reply payload of DISPLAY_AREA_GET: top-left, top-right and bottom-left
    corners in the tracker frame, mm."""
    if len(payload) < 36:
        return None
    v = struct.unpack_from("<9f", payload)
    return v[0:3], v[3:6], v[6:9]
