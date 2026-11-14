//! The gaze overlay's settings and its smoothing.
//!
//! The overlay is a mark drawn on screen where the user is looking, in the
//! manner of Tobii Ghost. These are the settings the daemon stores and the
//! overlay program draws from.

use serde::{Deserialize, Serialize};

use crate::smooth::OneEuro;

/// What the mark looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    /// A glowing hollow ring.
    #[default]
    Bubble,
    /// A filled disc with a soft edge.
    Solid,
    /// A wide, soft glow with no hard edge.
    Heatmap,
}

impl Style {
    pub const ALL: [Self; 3] = [Self::Bubble, Self::Solid, Self::Heatmap];

    /// The name shown in the tuning window.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Bubble => "Bubble",
            Self::Solid => "Solid",
            Self::Heatmap => "Heatmap",
        }
    }
}

pub const SIZE_RANGE: (f64, f64) = (20.0, 600.0);
pub const OPACITY_RANGE: (f64, f64) = (0.05, 1.0);

/// The overlay's settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overlay {
    pub enabled: bool,
    pub style: Style,
    /// Diameter in pixels.
    pub size: f64,
    /// `#rrggbb`.
    pub color: String,
    /// 0 is invisible, 1 is solid.
    pub opacity: f64,
    /// 0 follows the eyes exactly, 100 is very steady and slow.
    pub smoothing: f64,
    /// How much of the path just travelled stays visible behind the mark:
    /// 0 is none, 100 is about half a second.
    pub trail: f64,
    /// The monitor to draw on, by connector name such as `DP-4`. `None`
    /// uses the one the driver was last calibrated on.
    pub monitor: Option<String>,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            enabled: false,
            style: Style::Bubble,
            size: 72.0,
            color: "#60cdff".to_owned(),
            opacity: 0.9,
            smoothing: 45.0,
            trail: 50.0,
            monitor: None,
        }
    }
}

/// Read `#rrggbb` as red, green, blue.
#[must_use]
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

impl Overlay {
    /// Why these settings cannot be used, if they cannot.
    pub fn validate(&self) -> Result<(), String> {
        let within = |value: f64, (low, high): (f64, f64)| {
            value.is_finite() && value >= low && value <= high
        };
        if !within(self.size, SIZE_RANGE) {
            return Err(format!(
                "overlay size must be between {} and {} pixels",
                SIZE_RANGE.0, SIZE_RANGE.1
            ));
        }
        if !within(self.opacity, OPACITY_RANGE) {
            return Err(format!(
                "overlay opacity must be between {} and {}",
                OPACITY_RANGE.0, OPACITY_RANGE.1
            ));
        }
        if !within(self.smoothing, (0.0, 100.0)) {
            return Err("overlay smoothing must be between 0 and 100".to_owned());
        }
        if !within(self.trail, (0.0, 100.0)) {
            return Err("overlay trail must be between 0 and 100".to_owned());
        }
        if parse_color(&self.color).is_none() {
            return Err(format!(
                "overlay colour `{}` must look like #60cdff",
                self.color
            ));
        }
        if self
            .monitor
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("overlay monitor cannot be an empty name".to_owned());
        }
        Ok(())
    }

    /// The colour as red, green, blue; the default colour if it is unreadable.
    #[must_use]
    pub fn rgb(&self) -> [u8; 3] {
        parse_color(&self.color).unwrap_or([0x60, 0xcd, 0xff])
    }
}

/// How long the trail behind the mark lasts for a trail setting, seconds.
#[must_use]
pub fn trail_seconds(trail: f64) -> f64 {
    trail.clamp(0.0, 100.0) / 100.0 * 0.5
}

/// The filter cutoff at rest for a smoothing setting: 20 Hz at 0, 0.3 Hz at
/// 100, evenly spaced by ratio.
#[must_use]
pub fn cutoff_for(smoothing: f64) -> f64 {
    20.0 * (0.3f64 / 20.0).powf(smoothing.clamp(0.0, 100.0) / 100.0)
}

/// How fast the filter opens when the eyes jump, in Hz per screen-width per
/// second. A jump across half the screen in a tenth of a second opens it by
/// 20 Hz, so the mark lands quickly and then settles.
const JUMP_RESPONSE: f64 = 4.0;

/// Smooths a gaze point given as fractions of the screen.
#[derive(Debug, Clone, Copy)]
pub struct GazeFilter {
    x: OneEuro,
    y: OneEuro,
}

impl GazeFilter {
    #[must_use]
    pub fn new(smoothing: f64) -> Self {
        let axis = OneEuro::new(cutoff_for(smoothing), JUMP_RESPONSE)
            .expect("cutoff_for is always above zero");
        Self { x: axis, y: axis }
    }

    pub fn reset(&mut self) {
        self.x.reset();
        self.y.reset();
    }

    pub fn filter(&mut self, micros: i64, point: [f32; 2]) -> [f32; 2] {
        let seconds = micros as f64 / 1e6;
        [
            self.x.filter(f64::from(point[0]), seconds) as f32,
            self.y.filter(f64::from(point[1]), seconds) as f32,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_usable() {
        let overlay = Overlay::default();
        assert!(!overlay.enabled);
        assert_eq!(overlay.style, Style::Bubble);
        assert!(overlay.validate().is_ok());
        assert_eq!(overlay.rgb(), [0x60, 0xcd, 0xff]);
    }

    #[test]
    fn colours_are_six_hex_digits() {
        assert_eq!(parse_color("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_color("#FF8000"), Some([255, 128, 0]));
        for bad in ["ff8000", "#ff80", "#ff80000", "#gg0000", "", "#ffé000"] {
            assert_eq!(parse_color(bad), None, "{bad}");
        }
    }

    #[test]
    fn unusable_settings_are_refused_with_a_reason() {
        let bad = |overlay: Overlay| overlay.validate().unwrap_err();
        assert!(bad(Overlay {
            size: 5.0,
            ..Overlay::default()
        })
        .contains("size"));
        assert!(bad(Overlay {
            size: f64::NAN,
            ..Overlay::default()
        })
        .contains("size"));
        assert!(bad(Overlay {
            opacity: 0.0,
            ..Overlay::default()
        })
        .contains("opacity"));
        assert!(bad(Overlay {
            smoothing: 101.0,
            ..Overlay::default()
        })
        .contains("smoothing"));
        assert!(bad(Overlay {
            color: "blue".into(),
            ..Overlay::default()
        })
        .contains("colour"));
        assert!(bad(Overlay {
            monitor: Some(" ".into()),
            ..Overlay::default()
        })
        .contains("monitor"));
    }

    #[test]
    fn styles_read_and_write_as_lower_case_words() {
        let text = toml::to_string(&Overlay {
            style: Style::Solid,
            ..Overlay::default()
        })
        .unwrap();
        assert!(text.contains("style = \"solid\""), "{text}");
        let back: Overlay = toml::from_str("style = \"heatmap\"\nsize = 40.0\n").unwrap();
        assert_eq!((back.style, back.size), (Style::Heatmap, 40.0));
        assert!(toml::from_str::<Overlay>("style = \"square\"\n").is_err());
        assert!(
            toml::from_str::<Overlay>("colour = \"#ffffff\"\n").is_err(),
            "unknown keys are refused"
        );
    }

    #[test]
    fn the_trail_setting_is_checked_and_maps_to_half_a_second() {
        let bad = Overlay {
            trail: -1.0,
            ..Overlay::default()
        };
        assert!(bad.validate().unwrap_err().contains("trail"));
        assert_eq!(trail_seconds(0.0), 0.0);
        assert_eq!(trail_seconds(100.0), 0.5);
    }

    #[test]
    fn more_smoothing_means_a_lower_cutoff() {
        assert!((cutoff_for(0.0) - 20.0).abs() < 1e-9);
        assert!((cutoff_for(100.0) - 0.3).abs() < 1e-9);
        assert!(cutoff_for(60.0) < cutoff_for(30.0));
    }

    #[test]
    fn a_still_gaze_is_steadied_and_a_jump_lands_quickly() {
        let mut filter = GazeFilter::new(60.0);
        let dt = 30_200i64;
        // Jitter of half a percent of the screen around the centre.
        let mut out = [0.0f32; 2];
        let mut swing = 0.0f32;
        for i in 0..200i64 {
            let jitter = if i % 2 == 0 { 0.005 } else { -0.005 };
            out = filter.filter(i * dt, [0.5 + jitter, 0.5]);
            if i > 100 {
                swing = swing.max((out[0] - 0.5).abs());
            }
        }
        assert!(swing < 0.002, "swing {swing}");
        // A jump to the right edge: most of the way there within 150 ms.
        for i in 200..205i64 {
            out = filter.filter(i * dt, [0.95, 0.5]);
        }
        assert!(out[0] > 0.85, "after the jump {}", out[0]);
    }
}
