//! TTP message framing, as the Tobii game integration DLL speaks it over TCP.
//!
//! A message is six big-endian `u32` words followed by the payload:
//!
//! ```text
//! marker, seq, status, id, error, payload_len
//! ```
//!
//! `id` is the command for a request or response and the stream id for a
//! stream message. Over USB the tracker puts an 8-byte prefix in front of
//! this; over the DLL's `tet-tcp://` route there is none.
//!
//! A payload is two zero bytes followed by TLV items: a type byte, a
//! big-endian `u32` length, then the value.

use std::fmt;

/// Bytes in a message header.
pub const HEADER_LEN: usize = 24;
/// Bytes the tracker puts in front of a message over USB.
pub const USB_PREFIX_LEN: usize = 8;

pub const MARKER_COMMAND: u32 = 0x51;
pub const MARKER_RESPONSE: u32 = 0x52;
pub const MARKER_STREAM: u32 = 0x53;
pub const MARKER_NOTIFICATION: u32 = 0x4E;

pub const STATUS_OK: u32 = 1;
pub const ERROR_NONE: u32 = 0;
/// The tracker's answer to a command it does not have.
pub const ERROR_UNSUPPORTED: u32 = 0x2000_0501;

pub const TYPE_U32: u8 = 0x02;
pub const TYPE_FIELD_ID: u8 = 0x05;
pub const TYPE_U64: u8 = 0x06;
pub const TYPE_STRING: u8 = 0x14;
pub const TYPE_LIST: u8 = 0x17;

/// The field id that opens a keyed stream field.
pub const KEY_FIELD_ID: u32 = 0x0002_0BB9;

/// Why bytes could not be read as a message or payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Fewer than [`HEADER_LEN`] bytes.
    ShortHeader { have: usize },
    /// The header promises more payload than there is.
    TruncatedPayload { want: usize, have: usize },
    /// A TLV item's length runs past the end of the payload.
    TruncatedItem { at: usize },
    /// A fixture line is not hex.
    BadHex,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortHeader { have } => {
                write!(f, "need {HEADER_LEN} header bytes, got {have}")
            }
            Self::TruncatedPayload { want, have } => {
                write!(f, "payload truncated: header says {want}, have {have}")
            }
            Self::TruncatedItem { at } => {
                write!(f, "item at offset {at} runs past the end of the payload")
            }
            Self::BadHex => write!(f, "not a line of hex"),
        }
    }
}

impl std::error::Error for Error {}

/// The six header words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub marker: u32,
    pub seq: u32,
    pub status: u32,
    pub id: u32,
    pub error: u32,
    pub payload_len: u32,
}

impl Header {
    /// Read a header from the front of `buf`.
    pub fn decode(buf: &[u8]) -> Result<Self, Error> {
        if buf.len() < HEADER_LEN {
            return Err(Error::ShortHeader { have: buf.len() });
        }
        let word = |i: usize| be_u32(&buf[i * 4..]);
        Ok(Self {
            marker: word(0),
            seq: word(1),
            status: word(2),
            id: word(3),
            error: word(4),
            payload_len: word(5),
        })
    }
}

/// One whole message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub marker: u32,
    pub seq: u32,
    pub status: u32,
    pub id: u32,
    pub error: u32,
    pub payload: Vec<u8>,
}

impl Message {
    /// Decode one whole message with no USB prefix. Bytes after the payload
    /// are ignored.
    pub fn decode(buf: &[u8]) -> Result<Self, Error> {
        let header = Header::decode(buf)?;
        let want = header.payload_len as usize;
        let body = &buf[HEADER_LEN..];
        if body.len() < want {
            return Err(Error::TruncatedPayload { want, have: body.len() });
        }
        Ok(Self {
            marker: header.marker,
            seq: header.seq,
            status: header.status,
            id: header.id,
            error: header.error,
            payload: body[..want].to_vec(),
        })
    }

    /// Decode a fixture line: hex of a USB message, 8-byte prefix included.
    pub fn from_usb_hex(text: &str) -> Result<Self, Error> {
        let raw = decode_hex(text.trim()).ok_or(Error::BadHex)?;
        if raw.len() < USB_PREFIX_LEN {
            return Err(Error::ShortHeader { have: 0 });
        }
        Self::decode(&raw[USB_PREFIX_LEN..])
    }

    /// The bytes that go on the wire.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        let len = u32::try_from(self.payload.len()).expect("payload fits a u32 length");
        for word in [self.marker, self.seq, self.status, self.id, self.error, len] {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&self.payload);
        out
    }

    /// The response to this request: same sequence number and command.
    #[must_use]
    pub fn response(&self, payload: Vec<u8>, error: u32) -> Self {
        Self {
            marker: MARKER_RESPONSE,
            seq: self.seq,
            status: STATUS_OK,
            id: self.id,
            error,
            payload,
        }
    }

    /// A stream message carrying `payload` for stream `id`.
    #[must_use]
    pub fn stream(id: u32, payload: Vec<u8>) -> Self {
        Self { marker: MARKER_STREAM, seq: 0, status: 0, id, error: ERROR_NONE, payload }
    }
}

/// One TLV item of a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item<'a> {
    pub typ: u8,
    /// Offset of `value` inside the payload, for in-place rewrites.
    pub offset: usize,
    pub value: &'a [u8],
}

/// Iterate the items of a payload. Stops at the first item that runs past
/// the end, yielding an error for it.
#[must_use]
pub fn items(payload: &[u8]) -> Items<'_> {
    // Every payload opens with two zero bytes.
    Items { payload, pos: 2, failed: false }
}

/// Iterator returned by [`items`].
#[derive(Debug, Clone)]
pub struct Items<'a> {
    payload: &'a [u8],
    pos: usize,
    failed: bool,
}

impl<'a> Iterator for Items<'a> {
    type Item = Result<Item<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos + 5 > self.payload.len() {
            return None;
        }
        let at = self.pos;
        let typ = self.payload[at];
        let length = be_u32(&self.payload[at + 1..]) as usize;
        let start = at + 5;
        let Some(end) = start.checked_add(length).filter(|end| *end <= self.payload.len()) else {
            self.failed = true;
            return Some(Err(Error::TruncatedItem { at }));
        };
        self.pos = end;
        Some(Ok(Item { typ, offset: start, value: &self.payload[start..end] }))
    }
}

/// One item as bytes.
#[must_use]
pub fn tlv(typ: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + value.len());
    out.push(typ);
    let len = u32::try_from(value.len()).expect("value fits a u32 length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(value);
    out
}

/// A `u32` item.
#[must_use]
pub fn u32_item(value: u32) -> Vec<u8> {
    tlv(TYPE_U32, &value.to_be_bytes())
}

/// A payload made of `items`, with its two leading zero bytes.
#[must_use]
pub fn payload(items: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![0, 0];
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// The first `u32` item of a payload: the stream id of a stream start, the
/// chosen version of a protocol upgrade reply.
#[must_use]
pub fn first_u32(body: &[u8]) -> Option<u32> {
    items(body)
        .map_while(Result::ok)
        .find(|item| item.typ == TYPE_U32 && item.value.len() == 4)
        .map(|item| be_u32(item.value))
}

/// Offset of the 8 timestamp bytes inside a stream payload, if it has one.
///
/// The timestamp is the keyed field with key 1: field id [`KEY_FIELD_ID`],
/// `u32` key 1, then a `u64` of device microseconds.
#[must_use]
pub fn timestamp_offset(body: &[u8]) -> Option<usize> {
    let mut lead = tlv(TYPE_FIELD_ID, &KEY_FIELD_ID.to_be_bytes());
    lead.extend_from_slice(&u32_item(1));
    lead.push(TYPE_U64);
    lead.extend_from_slice(&8u32.to_be_bytes());
    let at = body.windows(lead.len()).position(|window| window == lead.as_slice())?;
    let value = at + lead.len();
    (value + 8 <= body.len()).then_some(value)
}

/// `body` with its timestamp replaced by `micros`. A payload without a
/// timestamp comes back unchanged.
#[must_use]
pub fn with_timestamp(body: &[u8], micros: u64) -> Vec<u8> {
    let mut out = body.to_vec();
    if let Some(at) = timestamp_offset(body) {
        out[at..at + 8].copy_from_slice(&micros.to_be_bytes());
    }
    out
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.is_ascii() {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::load;

    /// The 71 bytes ETS2's Tobii DLL 7.3.0.14 sent to 127.0.0.1:4455 on
    /// 2026-11-14.
    const GAME_UPGRADE_REQUEST: &str = concat!(
        "000000510000000100000000000003e8000000000000002f",
        "0000170000002800000009000100000001000100010002000100030001000400010005",
        "000100060001000700010008",
    );

    fn upgrade_request() -> Vec<u8> {
        decode_hex(GAME_UPGRADE_REQUEST).expect("valid hex")
    }

    #[test]
    fn decodes_the_request_the_game_sent() {
        let msg = Message::decode(&upgrade_request()).unwrap();
        assert_eq!(msg.marker, MARKER_COMMAND);
        assert_eq!(msg.seq, 1);
        assert_eq!(msg.id, 1000);
        assert_eq!(msg.payload.len(), 47);
    }

    #[test]
    fn round_trips() {
        let raw = upgrade_request();
        assert_eq!(Message::decode(&raw).unwrap().encode(), raw);
    }

    #[test]
    fn rejects_a_truncated_payload_and_a_short_header() {
        let raw = upgrade_request();
        assert_eq!(
            Message::decode(&raw[..raw.len() - 1]),
            Err(Error::TruncatedPayload { want: 47, have: 46 })
        );
        assert_eq!(Message::decode(&raw[..10]), Err(Error::ShortHeader { have: 10 }));
    }

    #[test]
    fn response_copies_seq_and_command() {
        let reply = Message::decode(&upgrade_request()).unwrap().response(Vec::new(), ERROR_UNSUPPORTED);
        assert_eq!(
            (reply.marker, reply.seq, reply.status, reply.id, reply.error),
            (MARKER_RESPONSE, 1, STATUS_OK, 1000, ERROR_UNSUPPORTED)
        );
    }

    #[test]
    fn upgrade_request_lists_nine_versions() {
        let msg = Message::decode(&upgrade_request()).unwrap();
        let all: Vec<_> = items(&msg.payload).collect::<Result<_, _>>().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].typ, TYPE_LIST);
        let words: Vec<u32> = all[0].value.chunks(4).map(be_u32).collect();
        assert_eq!(words[0], 9);
        let expected: Vec<u32> = (0..9).map(|i| 0x10000 + i).collect();
        assert_eq!(&words[1..], expected.as_slice());
    }

    #[test]
    fn recorded_upgrade_reply_picks_the_newest_version() {
        let reply = load("init-rsp-1000");
        assert_eq!(reply.marker, MARKER_RESPONSE);
        assert_eq!(first_u32(&reply.payload), Some(0x10008));
    }

    #[test]
    fn builds_a_stream_start_payload_that_parses_back() {
        let body = payload(&[&u32_item(0x500), &tlv(TYPE_LIST, &[0, 0, 0, 0])]);
        assert_eq!(first_u32(&body), Some(0x500));
    }

    #[test]
    fn an_item_running_past_the_end_is_an_error_not_a_panic() {
        let mut body = payload(&[&u32_item(7)]);
        body.extend_from_slice(&[TYPE_U32, 0, 0, 0, 9, 1]);
        let all: Vec<_> = items(&body).collect();
        assert_eq!(all.len(), 2);
        assert!(all[0].is_ok());
        assert_eq!(all[1], Err(Error::TruncatedItem { at: 11 }));
    }

    #[test]
    fn gaze_frame_timestamp_is_rewritten_in_place() {
        let frame = load("session1-gaze-frame");
        assert_eq!(frame.id, 0x500);
        let at = timestamp_offset(&frame.payload).expect("gaze frames carry a timestamp");
        let rewritten = with_timestamp(&frame.payload, 123_456_789);
        assert_eq!(rewritten.len(), frame.payload.len());
        assert_eq!(rewritten[at..at + 8], 123_456_789u64.to_be_bytes());
        assert_eq!(rewritten[..at], frame.payload[..at]);
        assert_eq!(rewritten[at + 8..], frame.payload[at + 8..]);
    }

    #[test]
    fn presence_frame_has_a_timestamp_too() {
        assert!(timestamp_offset(&load("init-presence").payload).is_some());
    }

    #[test]
    fn payload_without_a_timestamp_is_left_alone() {
        let body = payload(&[&u32_item(7)]);
        assert_eq!(timestamp_offset(&body), None);
        assert_eq!(with_timestamp(&body, 1), body);
    }

    #[test]
    fn every_recorded_message_decodes() {
        for name in [
            "init-rsp-1000", "init-rsp-1200", "init-rsp-1330", "init-rsp-1400", "init-rsp-1420",
            "init-rsp-1430", "init-rsp-1490", "init-rsp-1650", "init-rsp-2110", "init-presence",
            "session1-gaze-frame", "session2-presence-away",
        ] {
            let msg = load(name);
            assert!(items(&msg.payload).all(|item| item.is_ok()), "{name}");
        }
    }

    #[test]
    fn bad_hex_is_refused() {
        assert_eq!(Message::from_usb_hex("zz"), Err(Error::BadHex));
        assert_eq!(Message::from_usb_hex("abc"), Err(Error::BadHex));
    }
}
