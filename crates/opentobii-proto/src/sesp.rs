//! Stream Engine Services protocol (SESP), service side.
//!
//! The game DLL's legacy TTP module has no head pose or display id of its
//! own. It asks "Stream Engine Services" for both over the named pipe
//! `\\.\pipe\streamengineservices`.
//!
//! Framing: the bytes `sesp`, a little-endian `u32` size, the size XOR
//! `0x70736573`, then a FlatBuffers buffer of that size. The root table has
//! three fields: 0 sequence (`u32`), 1 type (`u8`), 2 payload table.
//!
//! The message list and field numbers come from the protocol description in
//! Garulf/tobii-linux `docs/sesp.md`; no code is taken from that project.
//! The buffers built here are valid FlatBuffers but are not laid out byte
//! for byte as Tobii's builder does it. ETS2's DLL 7.3.0.14 accepts them.

use std::collections::BTreeMap;
use std::fmt;

/// `sesp` read as a little-endian `u32`.
pub const MAGIC: u32 = 0x7073_6573;
/// Bytes in a frame header.
pub const FRAME_HEAD_LEN: usize = 12;
/// Largest buffer a frame may announce. Real messages are under 200 bytes;
/// the cap keeps a corrupt size from being waited on forever.
pub const MAX_BUFFER: usize = 1 << 20;

pub const CHANNEL_COMMAND: u32 = 0x2711;
pub const CHANNEL_SUBSCRIPTION: u32 = 0x2712;

pub const T_INVALID_OPERATION: u8 = 1;
pub const T_INITIALIZE: u8 = 2;
pub const T_INITIALIZE_RESPONSE: u8 = 3;
pub const T_HEAD_POSE_START: u8 = 4;
pub const T_HEAD_POSE_START_RESPONSE: u8 = 5;
pub const T_HEAD_POSE_STOP: u8 = 6;
pub const T_HEAD_POSE_STOP_RESPONSE: u8 = 7;
pub const T_HEAD_POSE: u8 = 8;
pub const T_DISPLAY_INFO: u8 = 9;
pub const T_DISPLAY_INFO_RESPONSE: u8 = 10;
pub const T_STREAM_STATUS: u8 = 0x1A;
pub const T_STREAM_STATUS_RESPONSE: u8 = 0x1B;
pub const T_STATISTICS: u8 = 0x32;
pub const T_STATISTICS_RESPONSE: u8 = 0x33;

pub const ERROR_NOT_SUPPORTED: u32 = 6;

pub const CAP_HEAD_POSE: u16 = 1;
pub const CAP_DISPLAY_INFO: u16 = 2;
pub const CAP_STREAM_STATUS: u16 = 3;

/// Why bytes could not be read as a frame or buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The frame header's magic or check word is wrong, or its size is over
    /// [`MAX_BUFFER`].
    NotAFrame,
    /// A table, vtable or string lies outside the buffer.
    OutOfBounds,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAFrame => write!(f, "not a SESP frame"),
            Self::OutOfBounds => write!(f, "buffer points outside itself"),
        }
    }
}

impl std::error::Error for Error {}

// --- framing ---------------------------------------------------------------

/// `buffer` with its frame header in front.
#[must_use]
pub fn frame(buffer: &[u8]) -> Vec<u8> {
    let size = u32::try_from(buffer.len()).expect("buffer fits a u32 size");
    let mut out = Vec::with_capacity(FRAME_HEAD_LEN + buffer.len());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(size ^ MAGIC).to_le_bytes());
    out.extend_from_slice(buffer);
    out
}

/// Split one buffer off the front of `data`.
///
/// `Ok(Some((buffer, rest)))` for a whole frame, `Ok(None)` while one has
/// not fully arrived.
pub fn unframe(data: &[u8]) -> Result<Option<(&[u8], &[u8])>, Error> {
    if data.len() < FRAME_HEAD_LEN {
        return Ok(None);
    }
    let magic = le_u32(data, 0).ok_or(Error::NotAFrame)?;
    let size = le_u32(data, 4).ok_or(Error::NotAFrame)?;
    let check = le_u32(data, 8).ok_or(Error::NotAFrame)?;
    if magic != MAGIC || size ^ MAGIC != check || size as usize > MAX_BUFFER {
        return Err(Error::NotAFrame);
    }
    let end = FRAME_HEAD_LEN + size as usize;
    if data.len() < end {
        return Ok(None);
    }
    Ok(Some((&data[FRAME_HEAD_LEN..end], &data[end..])))
}

// --- a minimal FlatBuffers writer -------------------------------------------

/// One field's value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    U8(u8),
    U32(u32),
    F32(f32),
    Str(String),
    Table(Table),
}

/// Fields by number. A missing number means "left at its default".
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table(pub BTreeMap<u16, Value>);

impl Table {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style insert.
    #[must_use]
    pub fn with(mut self, number: u16, value: Value) -> Self {
        self.0.insert(number, value);
        self
    }
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

fn put_u16(out: &mut [u8], at: usize, value: usize) {
    let value = u16::try_from(value).expect("table layout fits u16 offsets");
    out[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], at: usize, value: usize) {
    let value = u32::try_from(value).expect("buffer fits u32 offsets");
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Append a table, its vtable and everything it points to. Returns the
/// table's position.
fn write_table(out: &mut Vec<u8>, table: &Table) -> usize {
    let count = table.0.keys().next_back().map_or(0, |last| usize::from(*last) + 1);
    pad4(out);
    let vtable_at = out.len();
    out.resize(vtable_at + 4 + 2 * count, 0);
    pad4(out);
    let table_at = out.len();
    let back = i32::try_from(table_at - vtable_at).expect("vtable is close to its table");
    out.extend_from_slice(&back.to_le_bytes());

    let mut slots: Vec<(u16, usize)> = Vec::with_capacity(table.0.len());
    for (number, value) in &table.0 {
        slots.push((*number, out.len()));
        match value {
            Value::U8(v) => out.extend_from_slice(&[*v, 0, 0, 0]),
            Value::U32(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::F32(v) => out.extend_from_slice(&v.to_le_bytes()),
            // An offset, filled in below.
            Value::Str(_) | Value::Table(_) => out.extend_from_slice(&[0; 4]),
        }
    }
    let table_size = out.len() - table_at;

    put_u16(out, vtable_at, 4 + 2 * count);
    put_u16(out, vtable_at + 2, table_size);
    for (number, at) in &slots {
        put_u16(out, vtable_at + 4 + 2 * usize::from(*number), at - table_at);
    }

    for (number, at) in slots {
        let target = match &table.0[&number] {
            Value::Str(text) => {
                pad4(out);
                let target = out.len();
                let len = u32::try_from(text.len()).expect("string fits a u32 length");
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(text.as_bytes());
                out.push(0);
                pad4(out);
                target
            }
            Value::Table(inner) => write_table(out, inner),
            Value::U8(_) | Value::U32(_) | Value::F32(_) => continue,
        };
        put_u32(out, at, target - at);
    }
    table_at
}

/// One message as a FlatBuffers buffer (not framed).
#[must_use]
pub fn build(sequence: u32, kind: u8, payload: Table) -> Vec<u8> {
    let root = Table::new()
        .with(0, Value::U32(sequence))
        .with(1, Value::U8(kind))
        .with(2, Value::Table(payload));
    let mut out = vec![0; 4];
    let root_at = write_table(&mut out, &root);
    put_u32(&mut out, 0, root_at);
    out
}

// --- a minimal FlatBuffers reader -------------------------------------------

/// Read access to one table of a buffer. Every read is bounds-checked: the
/// buffers come from a game process.
#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    buf: &'a [u8],
    at: usize,
    vtable: usize,
    vtable_size: usize,
}

impl<'a> View<'a> {
    fn new(buf: &'a [u8], at: usize) -> Result<Self, Error> {
        let back = le_i32(buf, at).ok_or(Error::OutOfBounds)?;
        let vtable = i64::try_from(at).map_err(|_| Error::OutOfBounds)? - i64::from(back);
        let vtable = usize::try_from(vtable).map_err(|_| Error::OutOfBounds)?;
        let vtable_size = usize::from(le_u16(buf, vtable).ok_or(Error::OutOfBounds)?);
        if vtable.checked_add(vtable_size).is_none_or(|end| end > buf.len()) {
            return Err(Error::OutOfBounds);
        }
        Ok(Self { buf, at, vtable, vtable_size })
    }

    fn slot(&self, number: u16) -> Option<usize> {
        let entry = 4 + 2 * usize::from(number);
        if entry + 2 > self.vtable_size {
            return None;
        }
        let offset = usize::from(le_u16(self.buf, self.vtable + entry)?);
        (offset != 0).then(|| self.at + offset)
    }

    /// Field `number` as a `u8`, or `default` when absent.
    #[must_use]
    pub fn u8(&self, number: u16, default: u8) -> u8 {
        self.slot(number).and_then(|at| self.buf.get(at).copied()).unwrap_or(default)
    }

    /// Field `number` as a `u32`, or `default` when absent.
    #[must_use]
    pub fn u32(&self, number: u16, default: u32) -> u32 {
        self.slot(number).and_then(|at| le_u32(self.buf, at)).unwrap_or(default)
    }

    /// Field `number` as an `f32`, or `default` when absent.
    #[must_use]
    pub fn f32(&self, number: u16, default: f32) -> f32 {
        self.u32_bits(number).map_or(default, f32::from_bits)
    }

    fn u32_bits(&self, number: u16) -> Option<u32> {
        self.slot(number).and_then(|at| le_u32(self.buf, at))
    }

    /// Field `number` as a string. `None` when absent or outside the buffer;
    /// bytes that are not UTF-8 are replaced.
    #[must_use]
    pub fn string(&self, number: u16) -> Option<String> {
        let at = self.slot(number)?;
        let target = at.checked_add(le_u32(self.buf, at)? as usize)?;
        let length = le_u32(self.buf, target)? as usize;
        let start = target.checked_add(4)?;
        let bytes = self.buf.get(start..start.checked_add(length)?)?;
        Some(String::from_utf8_lossy(bytes).into_owned())
    }

    /// Field `number` as a table. `None` when absent or outside the buffer.
    #[must_use]
    pub fn table(&self, number: u16) -> Option<View<'a>> {
        let at = self.slot(number)?;
        let target = at.checked_add(le_u32(self.buf, at)? as usize)?;
        View::new(self.buf, target).ok()
    }
}

/// One message read from a buffer.
#[derive(Debug, Clone, Copy)]
pub struct Parsed<'a> {
    pub sequence: u32,
    pub kind: u8,
    pub payload: Option<View<'a>>,
}

/// Read the root table of a buffer.
pub fn parse(buffer: &[u8]) -> Result<Parsed<'_>, Error> {
    let root_at = le_u32(buffer, 0).ok_or(Error::OutOfBounds)? as usize;
    let root = View::new(buffer, root_at)?;
    Ok(Parsed { sequence: root.u32(0, 0), kind: root.u8(1, 0), payload: root.table(2) })
}

// --- the service's messages --------------------------------------------------

/// The answer to an initialize request: one flag per capability offered.
#[must_use]
pub fn initialize_response(sequence: u32, capabilities: &[u16], error: u32) -> Vec<u8> {
    let mut table = Table::new();
    for capability in capabilities {
        table.0.insert(*capability, Value::U8(1));
    }
    if error != 0 {
        table.0.insert(0, Value::U32(error));
    }
    build(sequence, T_INITIALIZE_RESPONSE, table)
}

/// A response whose only payload field is the error word.
#[must_use]
pub fn error_response(sequence: u32, kind: u8, error: u32) -> Vec<u8> {
    let table = if error == 0 { Table::new() } else { Table::new().with(0, Value::U32(error)) };
    build(sequence, kind, table)
}

/// The display a tracker is mounted on, as the monitor's Windows device
/// instance id.
#[must_use]
pub fn display_info_response(sequence: u32, monitor_id: &str) -> Vec<u8> {
    let table = Table::new().with(1, Value::Str(monitor_id.to_owned()));
    build(sequence, T_DISPLAY_INFO_RESPONSE, table)
}

/// A head pose with every validity left at its default of 1 (valid).
#[must_use]
pub fn head_pose(sequence: u32, micros: u64, position_mm: [f32; 3], rotation_rad: [f32; 3]) -> Vec<u8> {
    let mut table = Table::new()
        .with(0, Value::U32((micros >> 32) as u32))
        .with(1, Value::U32(micros as u32));
    for (number, value) in (2..).zip(position_mm) {
        table.0.insert(number, Value::F32(value));
    }
    for (number, value) in (5..).zip(rotation_rad) {
        table.0.insert(number, Value::F32(value));
    }
    build(sequence, T_HEAD_POSE, table)
}

/// The service's answer to one command-channel request.
#[must_use]
pub fn respond(request: &Parsed<'_>, capabilities: &[u16], monitor_id: &str) -> Vec<u8> {
    let seq = request.sequence;
    match request.kind {
        T_INITIALIZE => initialize_response(seq, capabilities, 0),
        T_HEAD_POSE_START => error_response(seq, T_HEAD_POSE_START_RESPONSE, 0),
        T_HEAD_POSE_STOP => error_response(seq, T_HEAD_POSE_STOP_RESPONSE, 0),
        T_DISPLAY_INFO => display_info_response(seq, monitor_id),
        T_STREAM_STATUS => build(seq, T_STREAM_STATUS_RESPONSE, Table::new()),
        T_STATISTICS => error_response(seq, T_STATISTICS_RESPONSE, 0),
        _ => error_response(seq, T_INVALID_OPERATION, ERROR_NOT_SUPPORTED),
    }
}

fn le_u16(buf: &[u8], at: usize) -> Option<u16> {
    let bytes = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn le_u32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn le_i32(buf: &[u8], at: usize) -> Option<i32> {
    le_u32(buf, at).map(|v| v as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: [u16; 3] = [CAP_HEAD_POSE, CAP_DISPLAY_INFO, CAP_STREAM_STATUS];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn frame_starts_with_the_letters_sesp_and_carries_size_and_check() {
        let framed = frame(b"abcdefgh");
        assert_eq!(&framed[..4], b"sesp");
        assert_eq!(le_u32(&framed, 4), Some(8));
        assert_eq!(le_u32(&framed, 8), Some(8 ^ MAGIC));
    }

    #[test]
    fn unframe_returns_the_buffer_and_the_rest() {
        let mut data = frame(b"abcd");
        data.extend_from_slice(b"tail");
        assert_eq!(unframe(&data), Ok(Some((&b"abcd"[..], &b"tail"[..]))));
    }

    #[test]
    fn unframe_waits_for_a_whole_frame() {
        let data = frame(b"abcdefgh");
        for cut in [0, 5, 12, data.len() - 1] {
            assert_eq!(unframe(&data[..cut]), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn unframe_rejects_a_bad_check_word_and_an_absurd_size() {
        let mut data = frame(b"abcd");
        data[8] ^= 1;
        assert_eq!(unframe(&data), Err(Error::NotAFrame));

        let size = (MAX_BUFFER as u32) + 1;
        let mut huge = MAGIC.to_le_bytes().to_vec();
        huge.extend_from_slice(&size.to_le_bytes());
        huge.extend_from_slice(&(size ^ MAGIC).to_le_bytes());
        assert_eq!(unframe(&huge), Err(Error::NotAFrame));
    }

    #[test]
    fn root_fields_read_back() {
        let buf = build(7, T_HEAD_POSE_START, Table::new());
        let parsed = parse(&buf).unwrap();
        assert_eq!((parsed.sequence, parsed.kind), (7, T_HEAD_POSE_START));
        assert!(parsed.payload.is_some());
    }

    #[test]
    fn every_offset_is_four_byte_aligned() {
        let buf = display_info_response(3, "DISPLAY\\DEFAULT_MONITOR\\0000&0000");
        assert_eq!(buf.len() % 4, 0);
        assert_eq!(le_u32(&buf, 0).unwrap() % 4, 0);
    }

    #[test]
    fn absent_fields_read_as_their_default() {
        let buf = build(1, T_STREAM_STATUS_RESPONSE, Table::new());
        let payload = parse(&buf).unwrap().payload.unwrap();
        assert_eq!(payload.u32(0, 0), 0);
        assert_eq!(payload.u8(9, 1), 1);
        assert_eq!(payload.string(1), None);
    }

    #[test]
    fn string_and_scalar_fields_share_a_table() {
        let table = Table::new()
            .with(0, Value::Str("tet-tcp://127.0.0.1".into()))
            .with(1, Value::U32(42));
        let buf = build(2, T_INITIALIZE, table);
        let payload = parse(&buf).unwrap().payload.unwrap();
        assert_eq!(payload.string(0).as_deref(), Some("tet-tcp://127.0.0.1"));
        assert_eq!(payload.u32(1, 0), 42);
    }

    #[test]
    fn initialize_response_sets_only_the_asked_capabilities() {
        let buf = initialize_response(5, &[CAP_HEAD_POSE, CAP_STREAM_STATUS], 0);
        let parsed = parse(&buf).unwrap();
        assert_eq!((parsed.sequence, parsed.kind), (5, T_INITIALIZE_RESPONSE));
        let payload = parsed.payload.unwrap();
        let flags: Vec<u8> = (1..16).map(|n| payload.u8(n, 0)).collect();
        let mut expected = vec![1, 0, 1];
        expected.resize(15, 0);
        assert_eq!(flags, expected);
        assert_eq!(payload.u32(0, 0), 0);
    }

    #[test]
    fn head_pose_splits_the_timestamp_into_two_words() {
        let micros = (3u64 << 32) | 0x89AB_CDEF;
        let buf = head_pose(1, micros, [1.5, -2.0, 600.0], [0.1, 0.2, -0.3]);
        let payload = parse(&buf).unwrap().payload.unwrap();
        assert_eq!((payload.u32(0, 0), payload.u32(1, 0)), (3, 0x89AB_CDEF));
        assert_eq!([2, 3, 4].map(|n| payload.f32(n, 0.0)), [1.5, -2.0, 600.0]);
        assert_eq!([5, 6, 7].map(|n| payload.f32(n, 0.0)), [0.1, 0.2, -0.3]);
        assert_eq!([8, 9, 10, 11].map(|n| payload.u8(n, 1)), [1, 1, 1, 1]);
    }

    #[test]
    fn display_info_response_carries_the_monitor_id() {
        let monitor = "DISPLAY\\GSM5C7C\\5&1234";
        let buf = display_info_response(9, monitor);
        let payload = parse(&buf).unwrap().payload.unwrap();
        assert_eq!(payload.string(1).as_deref(), Some(monitor));
        assert_eq!(payload.u32(0, 0), 0);
    }

    #[test]
    fn each_request_gets_its_response_type_and_sequence() {
        let pairs = [
            (T_INITIALIZE, T_INITIALIZE_RESPONSE),
            (T_HEAD_POSE_START, T_HEAD_POSE_START_RESPONSE),
            (T_HEAD_POSE_STOP, T_HEAD_POSE_STOP_RESPONSE),
            (T_DISPLAY_INFO, T_DISPLAY_INFO_RESPONSE),
            (T_STREAM_STATUS, T_STREAM_STATUS_RESPONSE),
            (T_STATISTICS, T_STATISTICS_RESPONSE),
        ];
        for (request_kind, response_kind) in pairs {
            let request = build(11, request_kind, Table::new());
            let reply = respond(&parse(&request).unwrap(), &CAPS, "DISPLAY\\X\\1");
            let reply = parse(&reply).unwrap();
            assert_eq!((reply.sequence, reply.kind), (11, response_kind));
            assert_eq!(reply.payload.unwrap().u32(0, 0), 0);
        }
    }

    #[test]
    fn unknown_request_is_refused_as_not_supported() {
        let request = build(11, 0x25, Table::new());
        let reply = respond(&parse(&request).unwrap(), &CAPS, "DISPLAY\\X\\1");
        let reply = parse(&reply).unwrap();
        assert_eq!(reply.kind, T_INVALID_OPERATION);
        assert_eq!(reply.payload.unwrap().u32(0, 0), ERROR_NOT_SUPPORTED);
    }

    /// The Python proofs' output for the same messages, which ETS2's DLL
    /// accepted on 2026-11-14. Byte for byte, so the Rust daemon sends what
    /// was proven to work.
    #[test]
    fn output_matches_the_bytes_the_game_accepted() {
        let cases = include_str!("../testdata/sesp_golden.txt");
        let built = [
            ("initialize", initialize_response(1, &CAPS, 0)),
            ("display_info", display_info_response(2, "DISPLAY\\GSM5C7C\\5&1234&0&UID4353")),
            ("head_pose", head_pose(77, (3u64 << 32) | 0x89AB_CDEF, [1.5, -2.0, 600.0], [0.1, 0.2, -0.3])),
            ("head_pose_start", error_response(3, T_HEAD_POSE_START_RESPONSE, 0)),
            ("stream_status", build(4, T_STREAM_STATUS_RESPONSE, Table::new())),
            ("not_supported", error_response(5, T_INVALID_OPERATION, ERROR_NOT_SUPPORTED)),
        ];
        let mut seen = 0;
        for line in cases.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
            let (name, want) = line.split_once(' ').expect("name and hex");
            let (_, got) = built.iter().find(|(n, _)| *n == name).expect("known case");
            assert_eq!(hex(got), want, "{name}");
            seen += 1;
        }
        assert_eq!(seen, built.len());
    }

    #[test]
    fn hostile_buffers_are_errors_or_defaults_never_panics() {
        assert!(parse(&[]).is_err());
        assert!(parse(&[0xff; 4]).is_err());
        assert!(parse(&[4, 0, 0, 0, 0xff, 0xff, 0xff, 0x7f]).is_err());

        // Every truncation and every single-byte corruption of a real message.
        let good = display_info_response(9, "DISPLAY\\GSM5C7C\\5&1234");
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]).map(|p| p.payload.and_then(|v| v.string(1)));
        }
        for at in 0..good.len() {
            let mut bad = good.clone();
            bad[at] = 0xff;
            if let Ok(parsed) = parse(&bad) {
                if let Some(payload) = parsed.payload {
                    let _ = (payload.string(1), payload.u32(0, 0), payload.table(2));
                }
            }
        }
    }
}
