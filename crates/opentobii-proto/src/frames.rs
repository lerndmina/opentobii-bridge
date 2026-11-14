//! Rebuild the tracker's TTP stream frames from live samples.
//!
//! `tobiid` hands out decoded samples, not the tracker's own frames, and
//! the game's DLL wants the frames. Every value in a gaze frame is
//! fixed-width, so a recorded frame serves as a template: the live values
//! are written over the recorded ones in place and the layout stays exactly
//! what a tracker sends.
//!
//! A frame is a run of keyed fields. Each is a field id item
//! ([`ttp::KEY_FIELD_ID`]), a `u32` key, then the value: one scalar item, or
//! a point header followed by two or three 32.32 fixed-point items. Lengths
//! are in 1/1024 mm and normalised values are scaled by 1024 as well, so
//! both are `value * 2^42` on the wire.

use std::collections::BTreeMap;

use crate::tobiid::{GazeRaw, RawEye};
use crate::ttp::{self, Item};

pub const TYPE_U32_ALT: u8 = 0x01;
pub const TYPE_U32: u8 = 0x02;
pub const TYPE_FIXED16: u8 = 0x03;
pub const TYPE_FIXED32: u8 = 0x04;
pub const TYPE_FIELD_ID: u8 = 0x05;
pub const TYPE_U64: u8 = 0x06;

const POINT_SCALE: f64 = (1u64 << 42) as f64;
const FIXED16_ONE: f64 = 65_536.0;

pub const KEY_TIMESTAMP: u32 = 0x01;
pub const KEY_PRESENCE: u32 = 0x02;
pub const KEY_FRAME_COUNTER: u32 = 0x14;
pub const KEY_COMBINED_VALID: u32 = 0x1B;
pub const KEY_COMBINED: u32 = 0x1C;
pub const KEY_RESPONSIVE_VALID: u32 = 0x1F;
pub const KEY_RESPONSIVE: u32 = 0x20;

/// The keys of one eye's fields in a gaze frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EyeKeys {
    pub origin_mm: u32,
    pub origin_in_track_box: u32,
    pub gaze_point_mm: u32,
    pub gaze_point_on_display: u32,
    pub pupil: u32,
    pub status: u32,
    pub origin_flag: u32,
    pub eyeball: u32,
    pub point_valid: u32,
    pub origin_display_valid: u32,
    pub origin_display: u32,
}

impl EyeKeys {
    const fn new(base: u32, origin_flag: u32, eyeball: u32, point_valid: u32, origin_display_valid: u32, origin_display: u32) -> Self {
        Self {
            origin_mm: base,
            origin_in_track_box: base + 1,
            gaze_point_mm: base + 2,
            gaze_point_on_display: base + 3,
            pupil: base + 4,
            status: base + 5,
            origin_flag,
            eyeball,
            point_valid,
            origin_display_valid,
            origin_display,
        }
    }
}

pub const LEFT: EyeKeys = EyeKeys::new(0x02, 0x16, 0x17, 0x1D, 0x21, 0x22);
pub const RIGHT: EyeKeys = EyeKeys::new(0x08, 0x15, 0x18, 0x1E, 0x23, 0x24);

/// Where one value item sits in a payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub typ: u8,
    /// Offset of the item's value bytes.
    pub offset: usize,
}

/// One decoded keyed value.
#[derive(Debug, Clone, PartialEq)]
pub enum Keyed {
    Int(u64),
    Fixed(f64),
    Point(Vec<f64>),
}

/// Map each key of a stream payload to the slots holding its value: one
/// for a scalar, two or three for a point. A malformed tail is ignored.
#[must_use]
pub fn keyed_slots(payload: &[u8]) -> BTreeMap<u32, Vec<Slot>> {
    let items: Vec<Item<'_>> = ttp::items(payload).map_while(Result::ok).collect();
    let key_id = ttp::KEY_FIELD_ID.to_be_bytes();
    let mut slots = BTreeMap::new();
    let mut i = 0;
    while i + 2 < items.len() {
        let (lead, key, value) = (items[i], items[i + 1], items[i + 2]);
        let is_keyed = lead.typ == TYPE_FIELD_ID && lead.value == key_id && key.typ == TYPE_U32 && key.value.len() == 4;
        if !is_keyed {
            i += 1;
            continue;
        }
        let key = u32::from_be_bytes([key.value[0], key.value[1], key.value[2], key.value[3]]);
        if value.typ == TYPE_FIELD_ID && value.value.len() == 4 {
            // 0x00021f40 is a 2-D point, 0x00031f41 a 3-D one.
            let count = usize::from(value.value[1]);
            let parts = items.get(i + 3..i + 3 + count).unwrap_or(&[]);
            slots.insert(key, parts.iter().map(|p| Slot { typ: p.typ, offset: p.offset }).collect());
            i += 3 + count;
        } else {
            slots.insert(key, vec![Slot { typ: value.typ, offset: value.offset }]);
            i += 3;
        }
    }
    slots
}

fn read_slot(payload: &[u8], slot: Slot) -> Option<Keyed> {
    let bytes = payload.get(slot.offset..)?;
    match slot.typ {
        TYPE_U32 | TYPE_U32_ALT => Some(Keyed::Int(u64::from(u32::from_be_bytes(*bytes.first_chunk()?)))),
        TYPE_U64 => Some(Keyed::Int(u64::from_be_bytes(*bytes.first_chunk()?))),
        TYPE_FIXED16 => Some(Keyed::Fixed(f64::from(i32::from_be_bytes(*bytes.first_chunk()?)) / FIXED16_ONE)),
        TYPE_FIXED32 => Some(Keyed::Fixed(i64::from_be_bytes(*bytes.first_chunk()?) as f64 / POINT_SCALE)),
        _ => None,
    }
}

/// Decode every keyed value of a stream payload: integers as they are,
/// fixed-point values and points as floats.
#[must_use]
pub fn keyed_values(payload: &[u8]) -> BTreeMap<u32, Keyed> {
    let mut out = BTreeMap::new();
    for (key, slots) in keyed_slots(payload) {
        let decoded: Vec<Keyed> = slots.iter().filter_map(|slot| read_slot(payload, *slot)).collect();
        match decoded.as_slice() {
            [] => {}
            [one] if slots.len() == 1 => {
                out.insert(key, one.clone());
            }
            many => {
                let point = many
                    .iter()
                    .map(|value| match value {
                        Keyed::Int(v) => *v as f64,
                        Keyed::Fixed(v) => *v,
                        Keyed::Point(_) => f64::NAN,
                    })
                    .collect();
                out.insert(key, Keyed::Point(point));
            }
        }
    }
    out
}

/// A recorded stream payload whose keyed values can be overwritten.
#[derive(Debug, Clone)]
pub struct Template {
    payload: Vec<u8>,
    slots: BTreeMap<u32, Vec<Slot>>,
}

impl Template {
    #[must_use]
    pub fn new(payload: Vec<u8>) -> Self {
        let slots = keyed_slots(&payload);
        Self { payload, slots }
    }

    /// The recorded payload, unchanged.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Start a copy to write live values into.
    #[must_use]
    pub fn fill(&self) -> Fill<'_> {
        Fill { out: self.payload.clone(), slots: &self.slots }
    }
}

/// A copy of a template being filled. A key the template lacks, or whose
/// recorded type differs, is left alone, and so is a value that is not a
/// finite number.
#[derive(Debug)]
pub struct Fill<'a> {
    out: Vec<u8>,
    slots: &'a BTreeMap<u32, Vec<Slot>>,
}

impl Fill<'_> {
    fn slots(&self, key: u32, types: &[u8], count: usize) -> Option<&[Slot]> {
        let found = self.slots.get(&key)?;
        let fits = found.len() == count
            && found.iter().all(|slot| types.contains(&slot.typ) && slot.offset + width(slot.typ) <= self.out.len());
        fits.then_some(found.as_slice())
    }

    pub fn word(&mut self, key: u32, value: Option<u32>) {
        let (Some(value), Some(&[slot])) = (value, self.slots(key, &[TYPE_U32, TYPE_U32_ALT], 1)) else {
            return;
        };
        self.out[slot.offset..slot.offset + 4].copy_from_slice(&value.to_be_bytes());
    }

    pub fn u64(&mut self, key: u32, value: u64) {
        if let Some(&[slot]) = self.slots(key, &[TYPE_U64], 1) {
            self.out[slot.offset..slot.offset + 8].copy_from_slice(&value.to_be_bytes());
        }
    }

    pub fn fixed16(&mut self, key: u32, value: f64) {
        if !value.is_finite() {
            return;
        }
        if let Some(&[slot]) = self.slots(key, &[TYPE_FIXED16], 1) {
            // `as` saturates at the i32 range.
            let raw = (value * FIXED16_ONE).round() as i32;
            self.out[slot.offset..slot.offset + 4].copy_from_slice(&raw.to_be_bytes());
        }
    }

    pub fn point(&mut self, key: u32, values: &[f64]) {
        if !values.iter().all(|v| v.is_finite()) {
            return;
        }
        let Some(found) = self.slots(key, &[TYPE_FIXED32], values.len()).map(<[Slot]>::to_vec) else {
            return;
        };
        for (slot, value) in found.iter().zip(values) {
            let raw = (value * POINT_SCALE).round() as i64;
            self.out[slot.offset..slot.offset + 8].copy_from_slice(&raw.to_be_bytes());
        }
    }

    #[must_use]
    pub fn done(self) -> Vec<u8> {
        self.out
    }
}

fn width(typ: u8) -> usize {
    match typ {
        TYPE_U64 | TYPE_FIXED32 => 8,
        _ => 4,
    }
}

/// The display frame of a display area given as three corners in the
/// tracker frame: origin at the display centre, +x right, +y up, +z towards
/// the user.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayFrame {
    centre: [f64; 3],
    x: [f64; 3],
    y: [f64; 3],
    z: [f64; 3],
}

impl DisplayFrame {
    /// `None` when a side of the area has no length.
    #[must_use]
    pub fn new(top_left: [f64; 3], top_right: [f64; 3], bottom_left: [f64; 3]) -> Option<Self> {
        let centre = [0, 1, 2].map(|i| (top_right[i] + bottom_left[i]) / 2.0);
        let x = unit(sub(top_right, top_left))?;
        let y = unit(sub(top_left, bottom_left))?;
        let z = [x[1] * y[2] - x[2] * y[1], x[2] * y[0] - x[0] * y[2], x[0] * y[1] - x[1] * y[0]];
        Some(Self { centre, x, y, z })
    }

    /// A tracker-frame point in the display frame.
    #[must_use]
    pub fn from_tracker(&self, point: [f64; 3]) -> [f64; 3] {
        let d = sub(point, self.centre);
        [dot(d, self.x), dot(d, self.y), dot(d, self.z)]
    }
}

fn sub(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn unit(v: [f64; 3]) -> Option<[f64; 3]> {
    let length = dot(v, v).sqrt();
    (length.is_finite() && length > 0.0).then(|| v.map(|c| c / length))
}

fn wide<const N: usize>(values: [f32; N]) -> [f64; N] {
    values.map(f64::from)
}

fn eye(fill: &mut Fill<'_>, keys: &EyeKeys, eye: &RawEye, origin_flag: Option<u32>, eyeball: Option<[f32; 3]>, display: Option<&DisplayFrame>) {
    let tracked = eye.status == 0;
    fill.point(keys.origin_mm, &wide(eye.gaze_origin_mm));
    fill.point(keys.origin_in_track_box, &wide(eye.gaze_origin_in_track_box));
    fill.point(keys.gaze_point_mm, &wide(eye.gaze_point_mm));
    fill.point(keys.gaze_point_on_display, &wide(eye.gaze_point_on_display));
    fill.fixed16(keys.pupil, f64::from(eye.pupil_diameter_mm));
    fill.word(keys.status, Some(eye.status));
    fill.word(keys.origin_flag, origin_flag);
    if let Some(centre) = eyeball {
        fill.point(keys.eyeball, &wide(centre));
    }
    // The raw sample does not carry these two flags. The tracker sets them
    // with the eye's status on every recorded frame, so follow the status.
    fill.word(keys.point_valid, Some(u32::from(tracked)));
    let mut origin_valid = origin_flag.map_or(tracked, |flag| flag != 0);
    match display {
        Some(display) => fill.point(keys.origin_display, &display.from_tracker(wide(eye.gaze_origin_mm))),
        // Without a display area the recorded display-frame origin would be
        // someone else's: mark it invalid rather than pass it on.
        None => origin_valid = false,
    }
    fill.word(keys.origin_display_valid, Some(u32::from(origin_valid)));
}

/// The gaze stream payload for one live sample, stamped `micros`.
///
/// Keys the sample does not carry keep the template's values: the user
/// position guide (0x25..0x28) and the fields an Eye Tracker 5 never fills
/// (0x19, 0x1a, 0x29..0x2c).
#[must_use]
pub fn gaze_frame(template: &Template, raw: &GazeRaw, micros: u64, display: Option<&DisplayFrame>) -> Vec<u8> {
    let mut fill = template.fill();
    fill.u64(KEY_TIMESTAMP, micros);
    fill.word(0x11, raw.key_11);
    fill.word(KEY_FRAME_COUNTER, raw.frame_counter);
    fill.word(KEY_COMBINED_VALID, Some(raw.combined_validity));
    fill.point(KEY_COMBINED, &wide(raw.combined_on_display));
    eye(&mut fill, &LEFT, &raw.left, raw.left_origin_flag, raw.left_eyeball_center_mm, display);
    eye(&mut fill, &RIGHT, &raw.right, raw.right_origin_flag, raw.right_eyeball_center_mm, display);
    match raw.responsive_on_display {
        Some(point) => {
            fill.word(KEY_RESPONSIVE_VALID, Some(raw.responsive_validity));
            fill.point(KEY_RESPONSIVE, &wide(point));
        }
        None => fill.word(KEY_RESPONSIVE_VALID, Some(0)),
    }
    fill.done()
}

/// The presence stream payload for a `tobiid` presence status.
#[must_use]
pub fn presence_frame(template: &Template, status: u8, micros: u64) -> Vec<u8> {
    let mut fill = template.fill();
    fill.u64(KEY_TIMESTAMP, micros);
    fill.word(KEY_PRESENCE, Some(u32::from(status)));
    fill.done()
}

fn corner_offsets(reply: &[u8]) -> Vec<usize> {
    ttp::items(reply)
        .map_while(Result::ok)
        .filter(|item| item.typ == TYPE_FIXED32 && item.value.len() == 8)
        .map(|item| item.offset)
        .collect()
}

/// Command 1430's reply with the three corners replaced. The reply is three
/// unkeyed 3-D points followed by fields that are left as recorded. `None`
/// when the template holds fewer than nine values or a corner is not finite.
#[must_use]
pub fn display_area_reply(template: &[u8], corners: [[f64; 3]; 3]) -> Option<Vec<u8>> {
    let offsets = corner_offsets(template);
    let values: Vec<f64> = corners.iter().flatten().copied().collect();
    if offsets.len() < values.len() || !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    let mut out = template.to_vec();
    for (at, value) in offsets.iter().zip(values) {
        out[*at..*at + 8].copy_from_slice(&((value * POINT_SCALE).round() as i64).to_be_bytes());
    }
    Some(out)
}

/// The three corners of a command 1430 reply.
#[must_use]
pub fn display_area_corners(reply: &[u8]) -> Option<[[f64; 3]; 3]> {
    let values: Vec<f64> = corner_offsets(reply)
        .iter()
        .filter_map(|at| Some(i64::from_be_bytes(*reply.get(*at..)?.first_chunk()?) as f64 / POINT_SCALE))
        .collect();
    let corner = |i: usize| Some([*values.get(i)?, *values.get(i + 1)?, *values.get(i + 2)?]);
    Some([corner(0)?, corner(3)?, corner(6)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::load;
    use crate::tobiid::{PRESENCE_AWAY, RawEye};

    fn gaze() -> Vec<u8> {
        load("session1-gaze-frame").payload
    }

    fn area() -> Vec<u8> {
        load("init-rsp-1430").payload
    }

    fn int(values: &BTreeMap<u32, Keyed>, key: u32) -> u64 {
        match values[&key] {
            Keyed::Int(v) => v,
            ref other => panic!("key {key:#x} is {other:?}"),
        }
    }

    fn point(values: &BTreeMap<u32, Keyed>, key: u32) -> Vec<f64> {
        match &values[&key] {
            Keyed::Point(v) => v.clone(),
            other => panic!("key {key:#x} is {other:?}"),
        }
    }

    fn fixed(values: &BTreeMap<u32, Keyed>, key: u32) -> f64 {
        match values[&key] {
            Keyed::Fixed(v) => v,
            ref other => panic!("key {key:#x} is {other:?}"),
        }
    }

    fn narrow<const N: usize>(values: &[f64]) -> [f32; N] {
        std::array::from_fn(|i| values[i] as f32)
    }

    /// The raw sample `tobiid` would send for a recorded gaze frame. Values
    /// pass through `f32`, as they do on the real socket.
    fn raw_from(payload: &[u8]) -> GazeRaw {
        let v = keyed_values(payload);
        let eye = |k: &EyeKeys| RawEye {
            gaze_origin_mm: narrow(&point(&v, k.origin_mm)),
            gaze_origin_in_track_box: narrow(&point(&v, k.origin_in_track_box)),
            gaze_point_mm: narrow(&point(&v, k.gaze_point_mm)),
            gaze_point_on_display: narrow(&point(&v, k.gaze_point_on_display)),
            pupil_diameter_mm: fixed(&v, k.pupil) as f32,
            status: int(&v, k.status) as u32,
        };
        GazeRaw {
            device_micros: int(&v, 1) as i64,
            left: eye(&LEFT),
            right: eye(&RIGHT),
            combined_on_display: narrow(&point(&v, 0x1C)),
            combined_validity: int(&v, 0x1B) as u32,
            key_0e: None,
            key_11: Some(int(&v, 0x11) as u32),
            frame_counter: Some(int(&v, 0x14) as u32),
            left_origin_flag: Some(int(&v, 0x16) as u32),
            right_origin_flag: Some(int(&v, 0x15) as u32),
            left_eyeball_center_mm: Some(narrow(&point(&v, 0x17))),
            right_eyeball_center_mm: Some(narrow(&point(&v, 0x18))),
            host_micros: None,
            responsive_on_display: Some(narrow(&point(&v, 0x20))),
            responsive_validity: int(&v, 0x1F) as u32,
        }
    }

    fn display() -> DisplayFrame {
        let [a, b, c] = display_area_corners(&area()).unwrap();
        DisplayFrame::new(a, b, c).unwrap()
    }

    fn close(a: &[f64], b: &[f64], tolerance: f64) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= tolerance)
    }

    #[test]
    fn finds_every_key_of_the_recorded_gaze_frame() {
        let slots = keyed_slots(&gaze());
        let keys: Vec<u32> = slots.keys().copied().collect();
        let expected: Vec<u32> = (0x01..0x0E).chain([0x11]).chain(0x14..0x2D).collect();
        assert_eq!(keys, expected);
        assert_eq!(slots[&0x02].len(), 3);
        assert_eq!(slots[&0x1C].len(), 2);
        assert_eq!(slots[&0x07].len(), 1);
    }

    #[test]
    fn decodes_known_values() {
        let v = keyed_values(&gaze());
        assert_eq!(int(&v, 0x14), 43780);
        assert!((fixed(&v, 0x06) - 6.24736).abs() < 1e-4);
        assert!((point(&v, 0x02)[2] - 673.5288).abs() < 1e-3);
        assert!((point(&v, 0x1C)[0] - 0.58729).abs() < 1e-4);
    }

    #[test]
    fn refilling_the_recording_with_its_own_values_reproduces_it() {
        let template = Template::new(gaze());
        let rebuilt = gaze_frame(&template, &raw_from(&gaze()), 9_613_320_391, Some(&display()));
        assert_eq!(rebuilt.len(), gaze().len());
        let (before, after) = (keyed_values(&gaze()), keyed_values(&rebuilt));
        for (key, was) in &before {
            match (was, &after[key]) {
                (Keyed::Int(a), Keyed::Int(b)) => assert_eq!(a, b, "{key:#x}"),
                // Through f32 and back: a few parts in ten million.
                (Keyed::Fixed(a), Keyed::Fixed(b)) => assert!((a - b).abs() < 1e-5, "{key:#x}"),
                (Keyed::Point(a), Keyed::Point(b)) => assert!(close(a, b, 1e-3), "{key:#x}: {a:?} {b:?}"),
                (a, b) => panic!("{key:#x} changed type: {a:?} {b:?}"),
            }
        }
    }

    #[test]
    fn live_values_land_in_the_frame() {
        let left = RawEye {
            gaze_origin_mm: [1.0, 2.0, 600.0],
            gaze_origin_in_track_box: [0.5, 0.5, 0.5],
            gaze_point_mm: [10.0, 20.0, 30.0],
            gaze_point_on_display: [0.25, 0.75],
            pupil_diameter_mm: 3.5,
            status: 4,
        };
        let raw = GazeRaw { left, combined_on_display: [0.125, 0.875], combined_validity: 0, frame_counter: Some(7), ..raw_from(&gaze()) };
        let v = keyed_values(&gaze_frame(&Template::new(gaze()), &raw, 123_456_789, Some(&display())));
        assert_eq!(int(&v, 0x01), 123_456_789);
        assert_eq!(point(&v, 0x02), [1.0, 2.0, 600.0]);
        assert_eq!(point(&v, 0x05), [0.25, 0.75]);
        assert_eq!(fixed(&v, 0x06), 3.5);
        assert_eq!(int(&v, 0x07), 4);
        assert_eq!(int(&v, 0x1D), 0, "a lost eye's gaze point is not valid");
        assert_eq!(int(&v, 0x1E), 1);
        assert_eq!(point(&v, 0x1C), [0.125, 0.875]);
        assert_eq!(int(&v, 0x1B), 0);
        assert_eq!(int(&v, 0x14), 7);
    }

    #[test]
    fn without_a_display_area_the_display_frame_origin_is_marked_invalid() {
        let v = keyed_values(&gaze_frame(&Template::new(gaze()), &raw_from(&gaze()), 1, None));
        assert_eq!((int(&v, 0x21), int(&v, 0x23)), (0, 0));
    }

    #[test]
    fn a_missing_responsive_point_is_marked_invalid() {
        let raw = GazeRaw { responsive_on_display: None, ..raw_from(&gaze()) };
        let v = keyed_values(&gaze_frame(&Template::new(gaze()), &raw, 1, Some(&display())));
        assert_eq!(int(&v, 0x1F), 0);
    }

    #[test]
    fn not_a_number_never_reaches_the_frame() {
        let raw = GazeRaw { combined_on_display: [f32::NAN, f32::INFINITY], ..raw_from(&gaze()) };
        let v = keyed_values(&gaze_frame(&Template::new(gaze()), &raw, 1, Some(&display())));
        assert_eq!(point(&v, 0x1C), point(&keyed_values(&gaze()), 0x1C));
    }

    #[test]
    fn away_from_the_present_template_matches_the_recorded_away_frame() {
        let away = load("session2-presence-away").payload;
        let micros = int(&keyed_values(&away), 1);
        let rebuilt = presence_frame(&Template::new(load("init-presence").payload), PRESENCE_AWAY, micros);
        assert_eq!(rebuilt, away);
    }

    #[test]
    fn display_area_round_trip() {
        let corners = [[-300.0, 340.0, 100.0], [300.0, 340.0, 100.0], [-300.0, 5.0, -2.0]];
        let reply = display_area_reply(&area(), corners).unwrap();
        assert_eq!(reply.len(), area().len());
        assert_eq!(display_area_corners(&reply), Some(corners));
        // Whatever follows the corners is left as recorded.
        assert_eq!(reply[reply.len() - 14..], area()[area().len() - 14..]);
        assert_eq!(display_area_reply(&[0, 0], corners), None);
        assert_eq!(display_area_reply(&area(), [[f64::NAN; 3]; 3]), None);
    }

    #[test]
    fn display_frame_axes() {
        let d = DisplayFrame::new([-300.0, 340.0, 0.0], [300.0, 340.0, 0.0], [-300.0, 0.0, 0.0]).unwrap();
        assert_eq!(d.from_tracker([0.0, 170.0, 0.0]), [0.0, 0.0, 0.0]);
        assert_eq!(d.from_tracker([10.0, 180.0, 600.0]), [10.0, 10.0, 600.0]);
        assert_eq!(DisplayFrame::new([0.0; 3], [0.0; 3], [0.0, -1.0, 0.0]), None);
    }

    #[test]
    fn a_truncated_template_fills_what_it_can_without_panicking() {
        let full = gaze();
        let raw = raw_from(&full);
        for cut in (0..full.len()).step_by(7) {
            let _ = gaze_frame(&Template::new(full[..cut].to_vec()), &raw, 1, Some(&display()));
        }
    }
}
