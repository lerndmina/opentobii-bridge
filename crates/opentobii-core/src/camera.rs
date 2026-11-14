//! The tracker's infrared picture as a camera, for face login.
//!
//! The tracker films the user in infrared to find their eyes. The daemon
//! can publish that picture on a `v4l2loopback` device, where a face login
//! program such as Howdy reads it like any webcam. The picture is small and
//! dark as it leaves the tracker, so it is enlarged and its brightness is
//! stretched on the way: [`Levels`] and [`render`].

use serde::{Deserialize, Serialize};

/// Width of the published picture, pixels.
pub const WIDTH: usize = 560;
/// Height of the published picture, pixels.
pub const HEIGHT: usize = 560;
/// Bytes in one published frame: YUYV, two bytes a pixel.
pub const FRAME_BYTES: usize = WIDTH * HEIGHT * 2;

/// The `[camera]` section of the settings file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Camera {
    /// Publish the infrared picture.
    pub enabled: bool,
    /// The `v4l2loopback` device to publish on.
    pub device: String,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            enabled: false,
            device: "/dev/video10".to_owned(),
        }
    }
}

impl Camera {
    /// Refuse a device that is not a device file's path.
    pub fn validate(&self) -> Result<(), String> {
        let name = self.device.strip_prefix("/dev/").unwrap_or("");
        if name.is_empty() || name.contains("..") || name.contains(char::is_whitespace) {
            return Err(format!(
                "camera.device must be a path under /dev, such as /dev/video10, not {:?}",
                self.device
            ));
        }
        Ok(())
    }
}

/// How far a frame's brightness range moves towards the newest picture's.
/// Low enough that the picture does not flicker, high enough to settle
/// within a third of a second at the tracker's 33 pictures a second.
const FOLLOW: f32 = 0.25;
/// The narrowest brightness range that is stretched to full. A picture of
/// an empty, dark room has almost no range; stretching that would publish
/// sensor noise as a bright picture.
const MIN_RANGE: f32 = 24.0;
/// Lifts the middle tones: skin in infrared sits well below the highlights
/// on glasses and eyes.
const GAMMA: f32 = 0.7;

/// The brightness stretch, carried from one picture to the next.
#[derive(Debug, Clone)]
pub struct Levels {
    low: f32,
    high: f32,
    primed: bool,
    table: [u8; 256],
}

impl Default for Levels {
    fn default() -> Self {
        let mut levels = Self {
            low: 0.0,
            high: 255.0,
            primed: false,
            table: [0; 256],
        };
        levels.rebuild();
        levels
    }
}

impl Levels {
    /// Take in a picture's brightness range: the darkest 1% and the
    /// brightest 0.3% of pixels are left to clip.
    pub fn update(&mut self, pixels: &[u8]) {
        if pixels.is_empty() {
            return;
        }
        let mut counts = [0usize; 256];
        for pixel in pixels {
            counts[usize::from(*pixel)] += 1;
        }
        let low = percentile(&counts, pixels.len() / 100);
        let high = percentile(&counts, pixels.len() - pixels.len() * 3 / 1000);
        if self.primed {
            self.low += (low - self.low) * FOLLOW;
            self.high += (high - self.high) * FOLLOW;
        } else {
            self.low = low;
            self.high = high;
            self.primed = true;
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let range = (self.high - self.low).max(MIN_RANGE);
        for (value, slot) in self.table.iter_mut().enumerate() {
            let unit = ((value as f32 - self.low) / range).clamp(0.0, 1.0);
            *slot = (unit.powf(GAMMA) * 255.0).round() as u8;
        }
    }

    /// The published brightness of a tracker brightness.
    #[must_use]
    pub fn map(&self, value: u8) -> u8 {
        self.table[usize::from(value)]
    }
}

/// The brightness below which `rank` pixels lie.
fn percentile(counts: &[usize; 256], rank: usize) -> f32 {
    let mut seen = 0;
    for (value, count) in counts.iter().enumerate() {
        seen += count;
        if seen > rank {
            return value as f32;
        }
    }
    255.0
}

/// Draw a tracker picture, `width` by `height` pixels of one byte each,
/// into `out` as a [`WIDTH`] by [`HEIGHT`] YUYV frame: enlarged smoothly,
/// brightness stretched by `levels`, no colour. Returns false, leaving
/// `out` alone, when `pixels` is not a picture of that size.
pub fn render(
    pixels: &[u8],
    width: usize,
    height: usize,
    levels: &Levels,
    out: &mut Vec<u8>,
) -> bool {
    if width == 0 || height == 0 || pixels.len() != width * height {
        return false;
    }
    out.clear();
    out.reserve(FRAME_BYTES);
    // Where each output column samples the source: the left neighbour and
    // how much of the right one to mix in, out of 256.
    let columns: Vec<(usize, usize, u32)> = (0..WIDTH).map(|x| sample(x, WIDTH, width)).collect();
    for y in 0..HEIGHT {
        let (top, bottom, down) = sample(y, HEIGHT, height);
        let upper = &pixels[top * width..(top + 1) * width];
        let lower = &pixels[bottom * width..(bottom + 1) * width];
        for (left, right, across) in &columns {
            let mix = |row: &[u8]| {
                u32::from(row[*left]) * (256 - across) + u32::from(row[*right]) * across
            };
            let value = (mix(upper) * (256 - down) + mix(lower) * down + (1 << 15)) >> 16;
            out.push(levels.map(value.min(255) as u8));
            // Neutral chroma: a grey picture.
            out.push(128);
        }
    }
    true
}

/// For output position `at` of `out_len`, the two source positions it lies
/// between and the weight of the second, out of 256.
fn sample(at: usize, out_len: usize, src_len: usize) -> (usize, usize, u32) {
    let position = ((at as f32 + 0.5) * src_len as f32 / out_len as f32 - 0.5)
        .clamp(0.0, (src_len - 1) as f32);
    let first = position.floor() as usize;
    let second = (first + 1).min(src_len - 1);
    (first, second, ((position - first as f32) * 256.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_must_be_under_dev() {
        assert!(Camera::default().validate().is_ok());
        for bad in [
            "",
            "video10",
            "/dev/",
            "/dev/../etc/passwd",
            "/tmp/video10",
            "/dev/video 1",
        ] {
            let camera = Camera {
                enabled: true,
                device: bad.to_owned(),
            };
            assert!(camera.validate().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_dark_picture_is_stretched_to_the_full_range() {
        // The tracker's picture: mostly near black, a face around 40.
        let mut pixels = vec![4u8; 10_000];
        for pixel in &mut pixels[2_000..6_000] {
            *pixel = 40;
        }
        let mut levels = Levels::default();
        levels.update(&pixels);
        assert_eq!(levels.map(4), 0);
        assert_eq!(levels.map(40), 255);
        // Middle tones are lifted above a straight line.
        assert!(levels.map(22) > 128, "{}", levels.map(22));
    }

    #[test]
    fn a_flat_dark_picture_stays_dark() {
        let mut levels = Levels::default();
        levels.update(&[3u8; 10_000]);
        assert_eq!(levels.map(3), 0);
        assert!(levels.map(5) < 60, "{}", levels.map(5));
    }

    #[test]
    fn the_range_follows_a_change_without_jumping() {
        let mut levels = Levels::default();
        levels.update(&[10u8; 1000]);
        let before = levels.map(40);
        let mut bright = vec![10u8; 1000];
        bright[500..].fill(200);
        levels.update(&bright);
        let after_one = levels.map(40);
        for _ in 0..40 {
            levels.update(&bright);
        }
        let settled = levels.map(40);
        assert!(
            before > after_one && after_one > settled,
            "{before} {after_one} {settled}"
        );
    }

    #[test]
    fn a_picture_is_enlarged_to_the_published_size() {
        // Left half black, right half white, 4 by 2.
        let pixels = [0, 0, 255, 255, 0, 0, 255, 255];
        let mut out = Vec::new();
        assert!(render(&pixels, 4, 2, &Levels::default(), &mut out));
        assert_eq!(out.len(), FRAME_BYTES);
        let luma = |x: usize, y: usize| out[(y * WIDTH + x) * 2];
        assert_eq!(luma(0, 0), 0);
        assert_eq!(luma(WIDTH - 1, HEIGHT - 1), 255);
        // The edge between the halves is a ramp, not a step.
        let middle = luma(WIDTH / 2, HEIGHT / 2);
        assert!((64..=200).contains(&middle), "{middle}");
        // No colour anywhere.
        assert!(out.iter().skip(1).step_by(2).all(|chroma| *chroma == 128));
    }

    #[test]
    fn a_picture_of_the_wrong_size_is_refused() {
        let mut out = vec![7];
        assert!(!render(&[0; 5], 2, 2, &Levels::default(), &mut out));
        assert!(!render(&[], 0, 0, &Levels::default(), &mut out));
        assert_eq!(out, [7]);
    }
}
