//! Client side of the `tobiid` socket protocol (Skyggedans/tobii-linux 0.6.0).
//!
//! `tobiid` owns the tracker's USB connection and serves decoded samples on
//! a Unix socket. The layouts here follow that project's `tobii-ipc` crate;
//! this is an independent reading of its documented wire format, covering
//! only what the bridge needs: subscribing, the raw gaze, presence and head
//! pose samples, and the display area request.
//!
//! A frame is a little-endian `u32` length, then a body whose first byte is
//! a tag.

use std::path::PathBuf;

pub const TAG_SUBSCRIBE: u8 = 0x01;
pub const TAG_REQUEST: u8 = 0x03;
pub const TAG_SUBSCRIBED: u8 = 0x10;
pub const TAG_REPLY: u8 = 0x11;
pub const TAG_PRESENCE: u8 = 0x22;
/// A device notification, such as a changed display area.
/// One infrared picture from the tracker's camera.
pub const TAG_IMAGE: u8 = 0x26;
pub const TAG_NOTIFICATION: u8 = 0x27;
pub const TAG_GAZE_RAW: u8 = 0x28;
pub const TAG_HEAD_POSE: u8 = 0x29;

pub const STREAM_PRESENCE: u32 = 1 << 2;
pub const STREAM_IMAGE: u32 = 1 << 6;
pub const STREAM_NOTIFICATIONS: u32 = 1 << 7;
pub const STREAM_GAZE_RAW: u32 = 1 << 8;
pub const STREAM_HEAD_POSE: u32 = 1 << 9;

pub const KIND_DISPLAY_AREA_GET: u8 = 3;
pub const STATUS_OK: u8 = 0;

pub const PRESENCE_UNKNOWN: u8 = 0;
pub const PRESENCE_AWAY: u8 = 1;
pub const PRESENCE_PRESENT: u8 = 2;

/// Largest frame body accepted. The biggest body the bridge subscribes to is
/// under 300 bytes; the cap keeps a corrupt length from being waited on.
pub const MAX_BODY: usize = 1 << 20;

/// Where `tobiid` listens: under the XDG runtime dir, else `/tmp`.
#[must_use]
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("tobiid.sock"),
        _ => PathBuf::from("/tmp/tobiid.sock"),
    }
}

/// `body` with its length in front.
#[must_use]
pub fn frame(body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).expect("body fits a u32 length");
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// The frame announces a body over [`MAX_BODY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Oversized(pub usize);

/// One whole frame body and the bytes after it, or `None` while a frame is
/// incomplete.
pub type Split<'a> = Option<(&'a [u8], &'a [u8])>;

/// Split one frame body off the front of `buffer`: `Ok(Some((body, rest)))`,
/// or `Ok(None)` while the frame is incomplete.
pub fn unframe(buffer: &[u8]) -> Result<Split<'_>, Oversized> {
    let Some(head) = buffer.get(..4) else {
        return Ok(None);
    };
    let length = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if length > MAX_BODY {
        return Err(Oversized(length));
    }
    match buffer.get(4..4 + length) {
        Some(body) => Ok(Some((body, &buffer[4 + length..]))),
        None => Ok(None),
    }
}

/// SUBSCRIBE body for a `STREAM_*` mask.
#[must_use]
pub fn subscribe(streams: u32) -> Vec<u8> {
    let mut body = vec![TAG_SUBSCRIBE];
    body.extend_from_slice(&streams.to_le_bytes());
    body
}

/// REQUEST body.
#[must_use]
pub fn request(request_id: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = vec![TAG_REQUEST];
    body.extend_from_slice(&request_id.to_le_bytes());
    body.push(kind);
    body.extend_from_slice(payload);
    body
}

/// A head pose, absolute, in the display frame: origin at the display
/// centre, +x right, +y up, +z towards the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeadPose {
    /// When the IR image was taken, `CLOCK_MONOTONIC` microseconds.
    pub host_micros: i64,
    pub position_valid: bool,
    pub position_mm: [f32; 3],
    pub rotation_valid: [bool; 3],
    pub rotation_rad: [f32; 3],
}

impl HeadPose {
    /// Position and all three angles hold a measurement.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.position_valid && self.rotation_valid.iter().all(|v| *v)
    }
}

/// One eye's block of a raw gaze sample, the device's values as sent.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RawEye {
    pub gaze_origin_mm: [f32; 3],
    pub gaze_origin_in_track_box: [f32; 3],
    pub gaze_point_mm: [f32; 3],
    pub gaze_point_on_display: [f32; 2],
    pub pupil_diameter_mm: f32,
    /// 0 tracked, 4 lost.
    pub status: u32,
}

/// One gaze frame as the tracker sent it. `None` marks a field the frame
/// did not carry.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct GazeRaw {
    pub device_micros: i64,
    pub left: RawEye,
    pub right: RawEye,
    pub combined_on_display: [f32; 2],
    pub combined_validity: u32,
    pub key_0e: Option<u32>,
    pub key_11: Option<u32>,
    pub frame_counter: Option<u32>,
    pub left_origin_flag: Option<u32>,
    pub right_origin_flag: Option<u32>,
    pub left_eyeball_center_mm: Option<[f32; 3]>,
    pub right_eyeball_center_mm: Option<[f32; 3]>,
    /// `CLOCK_MONOTONIC` microseconds; absent from a daemon before 0.6.
    pub host_micros: Option<i64>,
    pub responsive_on_display: Option<[f32; 2]>,
    pub responsive_validity: u32,
}

/// One daemon-to-client message.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMsg {
    Subscribed {
        ok: bool,
    },
    Reply {
        request_id: u32,
        status: u8,
        payload: Vec<u8>,
    },
    Presence {
        host_micros: i64,
        status: u8,
    },
    HeadPose(HeadPose),
    GazeRaw(Box<GazeRaw>),
    /// The tracker reported a change. What changed is not decoded: the
    /// bridge asks again for what it cares about.
    Notification,
}

/// Reads little-endian scalars off a body; a short body is `None`.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take().map(i64::from_le_bytes)
    }
    fn f32(&mut self) -> Option<f32> {
        self.take().map(f32::from_le_bytes)
    }
    fn f32s<const N: usize>(&mut self) -> Option<[f32; N]> {
        let mut out = [0.0; N];
        for slot in &mut out {
            *slot = self.f32()?;
        }
        Some(out)
    }
    fn eye(&mut self) -> Option<RawEye> {
        Some(RawEye {
            gaze_origin_mm: self.f32s()?,
            gaze_origin_in_track_box: self.f32s()?,
            gaze_point_mm: self.f32s()?,
            gaze_point_on_display: self.f32s()?,
            pupil_diameter_mm: self.f32()?,
            status: self.u32()?,
        })
    }
    fn optional_u32(&mut self) -> Option<Option<u32>> {
        let present = self.u8()? != 0;
        let value = self.u32()?;
        Some(present.then_some(value))
    }
    fn optional_point(&mut self) -> Option<Option<[f32; 3]>> {
        let present = self.u8()? != 0;
        let value = self.f32s()?;
        Some(present.then_some(value))
    }
}

fn gaze_raw(r: &mut Reader<'_>) -> Option<GazeRaw> {
    let mut raw = GazeRaw {
        device_micros: r.i64()?,
        left: r.eye()?,
        right: r.eye()?,
        combined_on_display: r.f32s()?,
        combined_validity: r.u32()?,
        key_0e: r.optional_u32()?,
        key_11: r.optional_u32()?,
        frame_counter: r.optional_u32()?,
        left_origin_flag: r.optional_u32()?,
        right_origin_flag: r.optional_u32()?,
        left_eyeball_center_mm: r.optional_point()?,
        right_eyeball_center_mm: r.optional_point()?,
        ..GazeRaw::default()
    };
    // The host time and the responsive point are an optional tail.
    if let Some(host) = r.i64() {
        raw.host_micros = Some(host);
        if let (Some(point), Some(validity)) = (r.f32s(), r.u32()) {
            raw.responsive_on_display = Some(point);
            raw.responsive_validity = validity;
        }
    }
    Some(raw)
}

/// Decode one frame body. `None` for a tag this module does not read, or a
/// body too short for its tag.
#[must_use]
pub fn decode(body: &[u8]) -> Option<ServerMsg> {
    let (tag, rest) = body.split_first()?;
    let mut r = Reader(rest);
    match *tag {
        TAG_SUBSCRIBED => Some(ServerMsg::Subscribed { ok: r.u8()? != 0 }),
        TAG_REPLY => Some(ServerMsg::Reply {
            request_id: r.u32()?,
            status: r.u8()?,
            payload: r.0.to_vec(),
        }),
        TAG_PRESENCE => Some(ServerMsg::Presence {
            host_micros: r.i64()?,
            status: r.u8()?,
        }),
        TAG_HEAD_POSE => {
            let host_micros = r.i64()?;
            let flags = r.u8()?;
            Some(ServerMsg::HeadPose(HeadPose {
                host_micros,
                position_valid: flags & 1 != 0,
                position_mm: r.f32s()?,
                rotation_valid: [flags & 2 != 0, flags & 4 != 0, flags & 8 != 0],
                rotation_rad: r.f32s()?,
            }))
        }
        TAG_NOTIFICATION => Some(ServerMsg::Notification),
        TAG_GAZE_RAW => gaze_raw(&mut r).map(|raw| ServerMsg::GazeRaw(Box::new(raw))),
        _ => None,
    }
}

/// One infrared picture, borrowed from the message it arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image<'a> {
    /// When the picture was taken, `CLOCK_MONOTONIC` microseconds.
    pub host_micros: i64,
    pub width: u32,
    pub height: u32,
    /// Bits in a pixel; the ET5 sends 8.
    pub bits: u8,
    /// Rows from the top, `width * height * bits / 8` bytes.
    pub pixels: &'a [u8],
}

/// Read an IMAGE message. [`decode`] leaves these alone: a picture is
/// large, and only the camera feed wants it.
#[must_use]
pub fn image(body: &[u8]) -> Option<Image<'_>> {
    let (tag, rest) = body.split_first()?;
    if *tag != TAG_IMAGE {
        return None;
    }
    let mut r = Reader(rest);
    let image = Image {
        host_micros: r.i64()?,
        width: r.u32()?,
        height: r.u32()?,
        bits: r.u8()?,
        pixels: r.0,
    };
    let bytes = u64::from(image.width) * u64::from(image.height) * u64::from(image.bits) / 8;
    (image.pixels.len() as u64 == bytes).then_some(image)
}

/// Reply payload of `DISPLAY_AREA_GET`: top-left, top-right and bottom-left
/// corners in the tracker frame, mm.
#[must_use]
pub fn display_area(payload: &[u8]) -> Option<[[f32; 3]; 3]> {
    let mut r = Reader(payload);
    Some([r.f32s()?, r.f32s()?, r.f32s()?])
}

/// Encoders for the daemon's side of the protocol, for tests here and in
/// the crates that talk to a fake daemon.
pub mod testing {
    use super::{GazeRaw, HeadPose, RawEye, TAG_GAZE_RAW, TAG_HEAD_POSE, TAG_IMAGE, TAG_PRESENCE};

    fn eye(out: &mut Vec<u8>, e: &RawEye) {
        let floats = e
            .gaze_origin_mm
            .iter()
            .chain(&e.gaze_origin_in_track_box)
            .chain(&e.gaze_point_mm)
            .chain(&e.gaze_point_on_display)
            .chain(std::iter::once(&e.pupil_diameter_mm));
        for value in floats {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&e.status.to_le_bytes());
    }

    /// A `GAZE_RAW` body as `tobiid` 0.6.0 writes it.
    #[must_use]
    pub fn gaze_raw_body(raw: &GazeRaw) -> Vec<u8> {
        let mut out = vec![TAG_GAZE_RAW];
        out.extend_from_slice(&raw.device_micros.to_le_bytes());
        eye(&mut out, &raw.left);
        eye(&mut out, &raw.right);
        for value in raw.combined_on_display {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&raw.combined_validity.to_le_bytes());
        for word in [
            raw.key_0e,
            raw.key_11,
            raw.frame_counter,
            raw.left_origin_flag,
            raw.right_origin_flag,
        ] {
            out.push(u8::from(word.is_some()));
            out.extend_from_slice(&word.unwrap_or(0).to_le_bytes());
        }
        for point in [raw.left_eyeball_center_mm, raw.right_eyeball_center_mm] {
            out.push(u8::from(point.is_some()));
            for value in point.unwrap_or_default() {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        if let Some(host) = raw.host_micros {
            out.extend_from_slice(&host.to_le_bytes());
            if let Some(point) = raw.responsive_on_display {
                for value in point {
                    out.extend_from_slice(&value.to_le_bytes());
                }
                out.extend_from_slice(&raw.responsive_validity.to_le_bytes());
            }
        }
        out
    }

    /// A `HEAD_POSE` body.
    #[must_use]
    pub fn head_pose_body(pose: &HeadPose) -> Vec<u8> {
        let [x, y, z] = pose.rotation_valid;
        let flags =
            u8::from(pose.position_valid) | u8::from(x) << 1 | u8::from(y) << 2 | u8::from(z) << 3;
        let mut out = vec![TAG_HEAD_POSE];
        out.extend_from_slice(&pose.host_micros.to_le_bytes());
        out.push(flags);
        for value in pose.position_mm.iter().chain(&pose.rotation_rad) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out
    }

    /// A `PRESENCE` body.
    #[must_use]
    pub fn presence_body(host_micros: i64, status: u8) -> Vec<u8> {
        let mut out = vec![TAG_PRESENCE];
        out.extend_from_slice(&host_micros.to_le_bytes());
        out.push(status);
        out
    }

    /// An IMAGE message of one byte a pixel.
    #[must_use]
    pub fn image_body(host_micros: i64, width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = vec![TAG_IMAGE];
        out.extend_from_slice(&host_micros.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.push(8);
        out.extend_from_slice(pixels);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{gaze_raw_body, head_pose_body, image_body, presence_body};

    #[test]
    fn a_picture_is_read_and_a_short_one_refused() {
        let body = image_body(77, 3, 2, &[1, 2, 3, 4, 5, 6]);
        let picture = image(&body).expect("a picture");
        assert_eq!(
            (picture.host_micros, picture.width, picture.height),
            (77, 3, 2)
        );
        assert_eq!(picture.pixels, [1, 2, 3, 4, 5, 6]);
        assert!(decode(&body).is_none());
        assert!(image(&body[..body.len() - 1]).is_none());
        assert!(image(&presence_body(1, 2)).is_none());
    }
    use super::*;

    fn sample() -> GazeRaw {
        let eye = |seed: f32, status| RawEye {
            gaze_origin_mm: [seed, seed + 1.0, seed + 2.0],
            gaze_origin_in_track_box: [seed + 3.0, seed + 4.0, seed + 5.0],
            gaze_point_mm: [seed + 6.0, seed + 7.0, seed + 8.0],
            gaze_point_on_display: [seed + 9.0, seed + 10.0],
            pupil_diameter_mm: seed + 11.0,
            status,
        };
        GazeRaw {
            device_micros: 1_000_000,
            left: eye(100.0, 0),
            right: eye(200.0, 4),
            combined_on_display: [0.5, 0.25],
            combined_validity: 1,
            key_0e: None,
            key_11: Some(4),
            frame_counter: Some(777),
            left_origin_flag: Some(1),
            right_origin_flag: Some(0),
            left_eyeball_center_mm: Some([1.0, 2.0, 3.0]),
            right_eyeball_center_mm: None,
            host_micros: None,
            responsive_on_display: None,
            responsive_validity: 0,
        }
    }

    #[test]
    fn frame_round_trip_and_partial_input() {
        let mut wire = frame(b"abc");
        wire.extend_from_slice(&frame(b"de"));
        assert_eq!(unframe(&wire[..5]), Ok(None));
        let (body, rest) = unframe(&wire).unwrap().unwrap();
        assert_eq!(body, b"abc");
        assert_eq!(unframe(rest), Ok(Some((&b"de"[..], &b""[..]))));
    }

    #[test]
    fn an_absurd_length_is_refused() {
        assert_eq!(
            unframe(&[0xff, 0xff, 0xff, 0x7f, 0]),
            Err(Oversized(0x7fff_ffff))
        );
    }

    #[test]
    fn subscribe_and_request_bodies() {
        assert_eq!(
            subscribe(STREAM_GAZE_RAW | STREAM_HEAD_POSE),
            [1, 0, 3, 0, 0]
        );
        assert_eq!(request(7, KIND_DISPLAY_AREA_GET, &[]), [3, 7, 0, 0, 0, 3]);
    }

    #[test]
    fn subscribed_and_reply() {
        assert_eq!(decode(&[0x10, 1]), Some(ServerMsg::Subscribed { ok: true }));
        assert_eq!(
            decode(b"\x11\x07\x00\x00\x00\x00xyz"),
            Some(ServerMsg::Reply {
                request_id: 7,
                status: 0,
                payload: b"xyz".to_vec()
            })
        );
    }

    #[test]
    fn a_notification_is_recognised_whatever_it_carries() {
        assert_eq!(decode(&[TAG_NOTIFICATION]), Some(ServerMsg::Notification));
        assert_eq!(
            decode(&[TAG_NOTIFICATION, 3, 9, 9, 9]),
            Some(ServerMsg::Notification)
        );
    }

    #[test]
    fn presence() {
        assert_eq!(
            decode(&presence_body(42, PRESENCE_PRESENT)),
            Some(ServerMsg::Presence {
                host_micros: 42,
                status: 2
            })
        );
    }

    #[test]
    fn head_pose_flags() {
        let pose = HeadPose {
            host_micros: 5,
            position_valid: true,
            position_mm: [1.0, 2.0, 600.0],
            rotation_valid: [true, false, true],
            rotation_rad: [0.1, 0.2, 0.3],
        };
        let body = head_pose_body(&pose);
        assert_eq!(body.len(), 34, "the size the real daemon sent");
        assert_eq!(body[9], 0b1011);
        assert_eq!(decode(&body), Some(ServerMsg::HeadPose(pose)));
        assert!(!pose.valid());
    }

    #[test]
    fn gaze_raw_without_the_tail() {
        let raw = sample();
        assert_eq!(
            decode(&gaze_raw_body(&raw)),
            Some(ServerMsg::GazeRaw(Box::new(raw)))
        );
    }

    #[test]
    fn gaze_raw_with_host_time_and_responsive_point() {
        let raw = GazeRaw {
            host_micros: Some(2_000_000),
            responsive_on_display: Some([0.75, 0.5]),
            responsive_validity: 1,
            ..sample()
        };
        let body = gaze_raw_body(&raw);
        assert_eq!(body.len(), 196, "the size the real daemon sent");
        assert_eq!(decode(&body), Some(ServerMsg::GazeRaw(Box::new(raw))));
    }

    #[test]
    fn short_and_unknown_bodies_decode_to_none() {
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[0x7f, 0]), None);
        assert_eq!(decode(&gaze_raw_body(&sample())[..50]), None);
        assert_eq!(decode(&[TAG_HEAD_POSE, 0, 0, 0]), None);
    }

    #[test]
    fn display_area_reply() {
        let corners = [
            [-300.0, 340.0, 100.0],
            [300.0, 340.0, 100.0],
            [-300.0, 5.0, -2.0],
        ];
        let payload: Vec<u8> = corners
            .iter()
            .flatten()
            .flat_map(|v: &f32| v.to_le_bytes())
            .collect();
        assert_eq!(display_area(&payload), Some(corners));
        assert_eq!(display_area(&payload[..20]), None);
    }
}
