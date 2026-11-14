//! The stages a head pose passes through between the tracker and the game.
//!
//! In order: recentre, smoothing, dead zone, sensitivity, limit. Rotation is
//! handled in degrees (pitch, yaw, roll) and position in millimetres (x, y,
//! z). A default [`Profile`] changes nothing but the smoothing.
//!
//! Rotation comes out relative to the neutral pose: after a recentre,
//! looking the way you looked then is zero. Position comes out as the
//! neutral position plus the shaped movement away from it, so the game
//! still sees a plausible distance from the screen.

use serde::{Deserialize, Serialize};

use crate::smooth::{BadSettings, PoseFilter, Settings as SmoothSettings};

/// The movement at which a curve leaves the value unchanged: 30 degrees.
pub const ROTATION_REFERENCE: f64 = 30.0;
/// The same for position: 100 mm.
pub const POSITION_REFERENCE: f64 = 100.0;

/// How one axis is shaped.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Axis {
    /// Movement inside this distance from neutral is ignored. Degrees or mm.
    pub dead_zone: f64,
    /// Output per unit of input beyond the dead zone.
    pub sensitivity: f64,
    /// 1 is a straight line. Above 1, small movements are damped and large
    /// ones grow; the curve meets the straight line at the reference
    /// movement ([`ROTATION_REFERENCE`] or [`POSITION_REFERENCE`]).
    pub curve: f64,
    /// Flip the direction.
    pub invert: bool,
    /// Largest output either way. 0 means no limit.
    pub limit: f64,
}

impl Default for Axis {
    fn default() -> Self {
        Self {
            dead_zone: 0.0,
            sensitivity: 1.0,
            curve: 1.0,
            invert: false,
            limit: 0.0,
        }
    }
}

impl Axis {
    /// Shape one movement away from neutral. The result starts from zero at
    /// the edge of the dead zone, so nothing jumps when the head leaves it.
    #[must_use]
    pub fn shape(&self, delta: f64, reference: f64) -> f64 {
        if !delta.is_finite() {
            return 0.0;
        }
        let beyond = delta.abs() - self.dead_zone.max(0.0);
        if beyond <= 0.0 {
            return 0.0;
        }
        let curve = if self.curve.is_finite() && self.curve > 0.0 {
            self.curve
        } else {
            1.0
        };
        let mut out = reference * (beyond / reference).powf(curve) * self.sensitivity;
        if self.limit > 0.0 {
            out = out.clamp(-self.limit, self.limit);
        }
        let sign = if (delta < 0.0) != self.invert {
            -1.0
        } else {
            1.0
        };
        if out.is_finite() {
            sign * out
        } else {
            0.0
        }
    }

    /// True when the axis passes movement through unchanged.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// Smoothing as it appears in a profile.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Smoothing {
    pub enabled: bool,
    /// Rotation cutoff at rest, Hz. Lower is steadier and slower.
    pub rotation_steadiness: f64,
    /// How fast the rotation filter opens with speed.
    pub rotation_response: f64,
    /// Position cutoff at rest, Hz.
    pub position_steadiness: f64,
    pub position_response: f64,
}

impl Default for Smoothing {
    fn default() -> Self {
        let s = SmoothSettings::default();
        Self {
            enabled: true,
            rotation_steadiness: s.rotation_min_cutoff,
            rotation_response: s.rotation_beta,
            position_steadiness: s.position_min_cutoff,
            position_response: s.position_beta,
        }
    }
}

impl Smoothing {
    fn settings(&self) -> SmoothSettings {
        SmoothSettings {
            rotation_min_cutoff: self.rotation_steadiness,
            rotation_beta: self.rotation_response,
            position_min_cutoff: self.position_steadiness,
            position_beta: self.position_response,
        }
    }
}

/// Where "straight ahead" is: what a recentre captured.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Neutral {
    /// Pitch, yaw, roll in degrees.
    pub rotation: [f64; 3],
    /// Head position in mm. `None` until the first recentre: position then
    /// passes through unshaped.
    pub position: Option<[f64; 3]>,
}

/// Everything that tunes how head movement feels, for one game or as the
/// default for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub smoothing: Smoothing,
    pub pitch: Axis,
    pub yaw: Axis,
    pub roll: Axis,
    pub x: Axis,
    pub y: Axis,
    pub z: Axis,
    pub neutral: Neutral,
}

impl Profile {
    fn rotation_axes(&self) -> [Axis; 3] {
        [self.pitch, self.yaw, self.roll]
    }

    fn position_axes(&self) -> [Axis; 3] {
        [self.x, self.y, self.z]
    }

    /// Why this profile cannot be used, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        PoseFilter::new(self.smoothing.settings()).map_err(|BadSettings| {
            "smoothing steadiness must be above 0 and response not negative".to_owned()
        })?;
        let axes = [
            ("pitch", self.pitch),
            ("yaw", self.yaw),
            ("roll", self.roll),
            ("x", self.x),
            ("y", self.y),
            ("z", self.z),
        ];
        for (name, axis) in axes {
            let numbers = [axis.dead_zone, axis.sensitivity, axis.curve, axis.limit];
            if !numbers.iter().all(|v| v.is_finite()) {
                return Err(format!("{name}: every value must be a finite number"));
            }
            if axis.dead_zone < 0.0 || axis.sensitivity < 0.0 || axis.limit < 0.0 {
                return Err(format!(
                    "{name}: dead zone, sensitivity and limit cannot be negative"
                ));
            }
            if axis.curve <= 0.0 {
                return Err(format!("{name}: curve must be above 0"));
            }
        }
        let neutral = self
            .neutral
            .rotation
            .iter()
            .chain(self.neutral.position.iter().flatten());
        if !neutral.copied().all(f64::is_finite) {
            return Err("neutral pose must be finite numbers".to_owned());
        }
        Ok(())
    }
}

/// One pose in the pipeline's units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub position_mm: [f32; 3],
    pub rotation_rad: [f32; 3],
}

/// A profile and the filter state that goes with it.
#[derive(Debug, Clone)]
pub struct Pipeline {
    profile: Profile,
    filter: PoseFilter,
    /// The last smoothed input, for recentring on.
    last_smoothed: Option<Pose>,
}

impl Pipeline {
    pub fn new(profile: Profile) -> Result<Self, String> {
        profile.validate()?;
        let filter = PoseFilter::new(profile.smoothing.settings())
            .map_err(|BadSettings| "bad smoothing".to_owned())?;
        Ok(Self {
            profile,
            filter,
            last_smoothed: None,
        })
    }

    #[must_use]
    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Swap in new settings. The filter keeps its state when the smoothing
    /// is unchanged, so moving a dead-zone slider does not jolt the view.
    pub fn set_profile(&mut self, profile: Profile) -> Result<(), String> {
        profile.validate()?;
        if profile.smoothing != self.profile.smoothing {
            self.filter = PoseFilter::new(profile.smoothing.settings())
                .map_err(|BadSettings| "bad smoothing".to_owned())?;
        }
        self.profile = profile;
        Ok(())
    }

    /// Make the current head pose the neutral one. Returns the new neutral,
    /// or `None` when no pose has been seen yet.
    pub fn recentre(&mut self) -> Option<Neutral> {
        let pose = self.last_smoothed?;
        self.profile.neutral = Neutral {
            rotation: pose.rotation_rad.map(|r| f64::from(r).to_degrees()),
            position: Some(pose.position_mm.map(f64::from)),
        };
        Some(self.profile.neutral)
    }

    /// Run one pose taken at `micros` through every stage.
    pub fn process(&mut self, micros: i64, input: Pose) -> Pose {
        let smoothed = if self.profile.smoothing.enabled {
            let (position_mm, rotation_rad) =
                self.filter
                    .filter(micros, input.position_mm, input.rotation_rad);
            Pose {
                position_mm,
                rotation_rad,
            }
        } else {
            input
        };
        self.last_smoothed = Some(smoothed);

        let neutral = self.profile.neutral;
        let mut out = smoothed;
        for (i, axis) in self.profile.rotation_axes().iter().enumerate() {
            let degrees = f64::from(smoothed.rotation_rad[i]).to_degrees();
            let shaped = axis.shape(degrees - neutral.rotation[i], ROTATION_REFERENCE);
            out.rotation_rad[i] = shaped.to_radians() as f32;
        }
        if let Some(centre) = neutral.position {
            for (i, axis) in self.profile.position_axes().iter().enumerate() {
                let delta = f64::from(smoothed.position_mm[i]) - centre[i];
                out.position_mm[i] = (centre[i] + axis.shape(delta, POSITION_REFERENCE)) as f32;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: i64 = 30_200;

    fn pose(yaw_deg: f64) -> Pose {
        Pose {
            position_mm: [0.0, 0.0, 800.0],
            rotation_rad: [0.0, yaw_deg.to_radians() as f32, 0.0],
        }
    }

    fn unsmoothed(profile: Profile) -> Pipeline {
        let smoothing = Smoothing {
            enabled: false,
            ..Smoothing::default()
        };
        Pipeline::new(Profile {
            smoothing,
            ..profile
        })
        .unwrap()
    }

    fn yaw_out(pipeline: &mut Pipeline, yaw_deg: f64) -> f64 {
        f64::from(pipeline.process(0, pose(yaw_deg)).rotation_rad[1]).to_degrees()
    }

    #[test]
    fn a_default_axis_changes_nothing() {
        let axis = Axis::default();
        assert!(axis.is_identity());
        for delta in [-45.0, -0.01, 0.0, 0.3, 12.5, 90.0] {
            assert!(
                (axis.shape(delta, ROTATION_REFERENCE) - delta).abs() < 1e-9,
                "{delta}"
            );
        }
    }

    #[test]
    fn inside_the_dead_zone_is_zero_and_the_edge_does_not_jump() {
        let axis = Axis {
            dead_zone: 2.0,
            ..Axis::default()
        };
        assert_eq!(axis.shape(1.99, ROTATION_REFERENCE), 0.0);
        assert_eq!(axis.shape(-2.0, ROTATION_REFERENCE), 0.0);
        assert!((axis.shape(2.01, ROTATION_REFERENCE) - 0.01).abs() < 1e-9);
        assert!((axis.shape(-5.0, ROTATION_REFERENCE) + 3.0).abs() < 1e-9);
    }

    #[test]
    fn sensitivity_scales_what_is_beyond_the_dead_zone() {
        let axis = Axis {
            dead_zone: 1.0,
            sensitivity: 3.0,
            ..Axis::default()
        };
        assert!((axis.shape(11.0, ROTATION_REFERENCE) - 30.0).abs() < 1e-9);
        assert_eq!(
            Axis {
                sensitivity: 0.0,
                ..Axis::default()
            }
            .shape(20.0, ROTATION_REFERENCE),
            0.0
        );
    }

    #[test]
    fn a_curve_damps_small_moves_and_meets_the_line_at_the_reference() {
        let axis = Axis {
            curve: 2.0,
            ..Axis::default()
        };
        assert!(axis.shape(3.0, ROTATION_REFERENCE) < 3.0 / 2.0);
        assert!((axis.shape(30.0, ROTATION_REFERENCE) - 30.0).abs() < 1e-9);
        assert!(axis.shape(45.0, ROTATION_REFERENCE) > 45.0);
        assert!((axis.shape(-30.0, ROTATION_REFERENCE) + 30.0).abs() < 1e-9);
    }

    #[test]
    fn invert_flips_and_limit_clamps() {
        let axis = Axis {
            invert: true,
            limit: 20.0,
            sensitivity: 2.0,
            ..Axis::default()
        };
        assert!((axis.shape(5.0, ROTATION_REFERENCE) + 10.0).abs() < 1e-9);
        assert_eq!(axis.shape(50.0, ROTATION_REFERENCE), -20.0);
        assert_eq!(axis.shape(-50.0, ROTATION_REFERENCE), 20.0);
    }

    #[test]
    fn bad_numbers_never_come_out() {
        let axis = Axis {
            curve: f64::NAN,
            sensitivity: f64::INFINITY,
            ..Axis::default()
        };
        assert_eq!(axis.shape(f64::NAN, ROTATION_REFERENCE), 0.0);
        assert_eq!(axis.shape(10.0, ROTATION_REFERENCE), 0.0);
    }

    #[test]
    fn a_default_profile_only_smooths() {
        let mut smooth = Pipeline::new(Profile::default()).unwrap();
        let mut filter = PoseFilter::new(SmoothSettings::default()).unwrap();
        for i in 0..50i64 {
            let input = pose(if i % 2 == 0 { 3.0 } else { 3.4 });
            let (position, rotation) = filter.filter(i * DT, input.position_mm, input.rotation_rad);
            let out = smooth.process(i * DT, input);
            assert_eq!(out.position_mm, position);
            for axis in 0..3 {
                assert!((out.rotation_rad[axis] - rotation[axis]).abs() < 1e-6);
            }
        }
    }

    #[test]
    fn recentre_makes_the_current_pose_zero_and_keeps_distance() {
        let mut pipeline = unsmoothed(Profile::default());
        assert_eq!(pipeline.recentre(), None, "nothing seen yet");
        yaw_out(&mut pipeline, 12.0);
        let neutral = pipeline.recentre().unwrap();
        assert!((neutral.rotation[1] - 12.0).abs() < 1e-4);
        assert_eq!(neutral.position, Some([0.0, 0.0, 800.0]));
        assert!(yaw_out(&mut pipeline, 12.0).abs() < 1e-4);
        assert!((yaw_out(&mut pipeline, 20.0) - 8.0).abs() < 1e-4);
        assert_eq!(
            pipeline.process(0, pose(12.0)).position_mm,
            [0.0, 0.0, 800.0]
        );
    }

    #[test]
    fn position_is_shaped_around_the_neutral_position() {
        let profile = Profile {
            x: Axis {
                dead_zone: 10.0,
                sensitivity: 2.0,
                ..Axis::default()
            },
            neutral: Neutral {
                rotation: [0.0; 3],
                position: Some([0.0, 0.0, 800.0]),
            },
            ..Profile::default()
        };
        let mut pipeline = unsmoothed(profile);
        let lean = |pipeline: &mut Pipeline, x: f32| {
            pipeline
                .process(
                    0,
                    Pose {
                        position_mm: [x, 0.0, 800.0],
                        rotation_rad: [0.0; 3],
                    },
                )
                .position_mm
        };
        assert_eq!(lean(&mut pipeline, 8.0), [0.0, 0.0, 800.0]);
        assert_eq!(lean(&mut pipeline, 30.0), [40.0, 0.0, 800.0]);
        assert_eq!(lean(&mut pipeline, -30.0), [-40.0, 0.0, 800.0]);
    }

    #[test]
    fn position_passes_through_until_a_recentre() {
        let profile = Profile {
            x: Axis {
                sensitivity: 5.0,
                ..Axis::default()
            },
            ..Profile::default()
        };
        let mut pipeline = unsmoothed(profile);
        let input = Pose {
            position_mm: [30.0, -5.0, 700.0],
            rotation_rad: [0.0; 3],
        };
        assert_eq!(pipeline.process(0, input).position_mm, [30.0, -5.0, 700.0]);
    }

    #[test]
    fn changing_a_dead_zone_keeps_the_filter_state() {
        let mut pipeline = Pipeline::new(Profile::default()).unwrap();
        for i in 0..30i64 {
            pipeline.process(i * DT, pose(10.0));
        }
        let mut profile = *pipeline.profile();
        profile.roll.dead_zone = 1.0;
        pipeline.set_profile(profile).unwrap();
        // A jump right after the change is still smoothed, not passed through.
        let out = f64::from(pipeline.process(30 * DT, pose(11.0)).rotation_rad[1]).to_degrees();
        assert!(out > 10.0 && out < 10.9, "{out}");
    }

    #[test]
    fn bad_profiles_are_refused_with_a_reason() {
        let bad = |profile: Profile| profile.validate().unwrap_err();
        assert!(bad(Profile {
            yaw: Axis {
                dead_zone: -1.0,
                ..Axis::default()
            },
            ..Profile::default()
        })
        .contains("yaw"));
        assert!(bad(Profile {
            z: Axis {
                curve: 0.0,
                ..Axis::default()
            },
            ..Profile::default()
        })
        .contains("curve"));
        assert!(bad(Profile {
            pitch: Axis {
                limit: f64::NAN,
                ..Axis::default()
            },
            ..Profile::default()
        })
        .contains("pitch"));
        let smoothing = Smoothing {
            rotation_steadiness: 0.0,
            ..Smoothing::default()
        };
        assert!(bad(Profile {
            smoothing,
            ..Profile::default()
        })
        .contains("smoothing"));
        assert!(Pipeline::new(Profile {
            smoothing,
            ..Profile::default()
        })
        .is_err());
        let mut pipeline = Pipeline::new(Profile::default()).unwrap();
        assert!(pipeline
            .set_profile(Profile {
                smoothing,
                ..Profile::default()
            })
            .is_err());
        assert_eq!(
            *pipeline.profile(),
            Profile::default(),
            "a refused profile changes nothing"
        );
    }
}
