"""TTP message framing, as the Tobii game integration DLL speaks it over TCP.

A message is six big-endian u32 words followed by the payload:

    marker, seq, status, id, error, payload_len

`id` is the command for a request or response and the stream id for a stream
message. Over USB the tracker puts an 8-byte prefix in front of this; over the
DLL's `tet-tcp://` route there is none.

A payload is two zero bytes followed by TLV items: a type byte, a big-endian
u32 length, then the value.
"""

from __future__ import annotations

import struct
from dataclasses import dataclass
from typing import Iterator

HEADER = struct.Struct(">6I")
HEADER_LEN = HEADER.size  # 24
USB_PREFIX_LEN = 8

MARKER_COMMAND = 0x51
MARKER_RESPONSE = 0x52
MARKER_STREAM = 0x53
MARKER_NOTIFICATION = 0x4E

STATUS_OK = 1
ERROR_NONE = 0
ERROR_UNSUPPORTED = 0x2000_0501

TYPE_U32 = 0x02
TYPE_FIELD_ID = 0x05
TYPE_U64 = 0x06
TYPE_STRING = 0x14
TYPE_LIST = 0x17

KEY_FIELD_ID = 0x0002_0BB9


@dataclass(frozen=True)
class Message:
    marker: int
    seq: int
    status: int
    id: int
    error: int
    payload: bytes

    def encode(self) -> bytes:
        return (
            HEADER.pack(
                self.marker, self.seq, self.status, self.id, self.error, len(self.payload)
            )
            + self.payload
        )


def decode_header(buf: bytes) -> tuple[int, int, int, int, int, int]:
    """Return (marker, seq, status, id, error, payload_len)."""
    if len(buf) < HEADER_LEN:
        raise ValueError(f"need {HEADER_LEN} header bytes, got {len(buf)}")
    return HEADER.unpack_from(buf)


def decode(buf: bytes) -> Message:
    """Decode one whole message with no USB prefix."""
    marker, seq, status, ident, error, length = decode_header(buf)
    payload = bytes(buf[HEADER_LEN : HEADER_LEN + length])
    if len(payload) != length:
        raise ValueError(f"payload truncated: header says {length}, have {len(payload)}")
    return Message(marker, seq, status, ident, error, payload)


def from_usb_hex(text: str) -> Message:
    """Decode a fixture line: hex of a USB message, 8-byte prefix included."""
    raw = bytes.fromhex(text.strip())
    return decode(raw[USB_PREFIX_LEN:])


def response(request: Message, payload: bytes = b"", error: int = ERROR_NONE) -> Message:
    return Message(MARKER_RESPONSE, request.seq, STATUS_OK, request.id, error, payload)


def tlvs(payload: bytes) -> Iterator[tuple[int, bytes]]:
    """Yield (type, value) for each item of a payload."""
    pos = 2  # every payload opens with two zero bytes
    while pos + 5 <= len(payload):
        typ = payload[pos]
        (length,) = struct.unpack_from(">I", payload, pos + 1)
        value = payload[pos + 5 : pos + 5 + length]
        if len(value) != length:
            raise ValueError("TLV runs past the end of the payload")
        yield typ, value
        pos += 5 + length


def tlv(typ: int, value: bytes) -> bytes:
    return bytes([typ]) + struct.pack(">I", len(value)) + value


def u32(value: int) -> bytes:
    return tlv(TYPE_U32, struct.pack(">I", value))


def payload(*items: bytes) -> bytes:
    return b"\x00\x00" + b"".join(items)


def first_u32(body: bytes) -> int | None:
    for typ, value in tlvs(body):
        if typ == TYPE_U32 and len(value) == 4:
            return struct.unpack(">I", value)[0]
    return None


# A keyed stream field with key 1 holding a u64: the device timestamp in
# microseconds. Layout: field id KEY_FIELD_ID, u32 key, u64 value.
_TIMESTAMP_LEAD = (
    tlv(TYPE_FIELD_ID, struct.pack(">I", KEY_FIELD_ID))
    + u32(1)
    + bytes([TYPE_U64])
    + struct.pack(">I", 8)
)


def timestamp_offset(body: bytes) -> int | None:
    """Offset of the 8 timestamp bytes inside a stream payload, if present."""
    at = body.find(_TIMESTAMP_LEAD)
    return None if at < 0 else at + len(_TIMESTAMP_LEAD)


def with_timestamp(body: bytes, micros: int) -> bytes:
    at = timestamp_offset(body)
    if at is None:
        return body
    return body[:at] + struct.pack(">Q", micros) + body[at + 8 :]
