//! Smoothing for head pose.
//!
//! `tobiid`'s head pose comes straight from its estimator, one per IR image,
//! with a few tenths of a degree of frame-to-frame noise. A game scales head
//! rotation up several times, so that noise shows as a shaking view.
//!
//! The filter is the One Euro filter (Casiez, Roussel and Vogel, 2012): a
//! low-pass whose cutoff rises with the speed of the signal. A still head is
//! filtered hard, a turning head is followed with little lag.

use std::f64::consts::PI;

/// A gap this long, in seconds, restarts a filter: the old state says
/// nothing about now.
const RESTART_AFTER: f64 = 1.0;

fn alpha(cutoff_hz: f64, dt: f64) -> f64 {
    let tau = 1.0 / (2.0 * PI * cutoff_hz);
    1.0 / (1.0 + tau / dt)
}

/// Settings refused by [`OneEuro::new`]: a cutoff that is not positive, or
/// a negative or non-finite beta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadSettings;

/// One filtered value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OneEuro {
    min_cutoff: f64,
    beta: f64,
    derivative_cutoff: f64,
    state: Option<State>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct State {
    value: f64,
    speed: f64,
    time: f64,
}

impl OneEuro {
    /// `min_cutoff` (Hz) sets how hard a still signal is filtered: lower is
    /// steadier and slower. `beta` sets how fast the cutoff opens with
    /// speed, in Hz per unit per second: higher follows fast moves more
    /// closely.
    pub fn new(min_cutoff: f64, beta: f64) -> Result<Self, BadSettings> {
        let ok = min_cutoff.is_finite() && min_cutoff > 0.0 && beta.is_finite() && beta >= 0.0;
        ok.then_some(Self { min_cutoff, beta, derivative_cutoff: 1.0, state: None }).ok_or(BadSettings)
    }

    /// Forget the past: the next sample passes through as it is.
    pub fn reset(&mut self) {
        self.state = None;
    }

    /// Filter one sample taken at `seconds`.
    pub fn filter(&mut self, value: f64, seconds: f64) -> f64 {
        if !value.is_finite() {
            // Never let a bad sample into the state.
            return self.state.map_or(value, |s| s.value);
        }
        let restart = State { value, speed: 0.0, time: seconds };
        let Some(prev) = self.state else {
            self.state = Some(restart);
            return value;
        };
        let dt = seconds - prev.time;
        if !(dt > 0.0 && dt <= RESTART_AFTER) {
            // A clock that went back, or a gap too long to bridge.
            self.state = Some(restart);
            return value;
        }
        let raw_speed = (value - prev.value) / dt;
        let speed = prev.speed + alpha(self.derivative_cutoff, dt) * (raw_speed - prev.speed);
        let cutoff = self.min_cutoff + self.beta * speed.abs();
        let filtered = prev.value + alpha(cutoff, dt) * (value - prev.value);
        self.state = Some(State { value: filtered, speed, time: seconds });
        filtered
    }
}

/// Filter settings for a whole pose. Rotation is filtered in degrees and
/// position in mm, so the betas read as Hz per degree per second and Hz per
/// mm per second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub rotation_min_cutoff: f64,
    pub rotation_beta: f64,
    pub position_min_cutoff: f64,
    pub position_beta: f64,
}

impl Default for Settings {
    /// The values tuned against a real Eye Tracker 5 in ETS2 on 2026-11-14.
    fn default() -> Self {
        Self { rotation_min_cutoff: 0.8, rotation_beta: 0.1, position_min_cutoff: 0.8, position_beta: 0.02 }
    }
}

/// Six filters: three for position, three for rotation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoseFilter {
    position: [OneEuro; 3],
    rotation: [OneEuro; 3],
}

impl PoseFilter {
    pub fn new(settings: Settings) -> Result<Self, BadSettings> {
        let position = OneEuro::new(settings.position_min_cutoff, settings.position_beta)?;
        let rotation = OneEuro::new(settings.rotation_min_cutoff, settings.rotation_beta)?;
        Ok(Self { position: [position; 3], rotation: [rotation; 3] })
    }

    pub fn reset(&mut self) {
        for filter in self.position.iter_mut().chain(&mut self.rotation) {
            filter.reset();
        }
    }

    /// Filter one pose taken at `micros`. Returns position in mm and
    /// rotation in radians.
    pub fn filter(&mut self, micros: i64, position_mm: [f32; 3], rotation_rad: [f32; 3]) -> ([f32; 3], [f32; 3]) {
        let seconds = micros as f64 / 1e6;
        let mut position = [0.0; 3];
        let mut rotation = [0.0; 3];
        for i in 0..3 {
            position[i] = self.position[i].filter(f64::from(position_mm[i]), seconds) as f32;
            let degrees = self.rotation[i].filter(f64::from(rotation_rad[i]).to_degrees(), seconds);
            rotation[i] = degrees.to_radians() as f32;
        }
        (position, rotation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tracker's frame interval, seconds.
    const DT: f64 = 0.0302;

    /// Repeatable noise of roughly unit spread (sum of uniforms), so the
    /// tests need no random crate.
    struct Noise(u64);

    impl Noise {
        fn uniform(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self, sd: f64) -> f64 {
            let sum: f64 = (0..12).map(|_| self.uniform()).sum();
            (sum - 6.0) * sd
        }
    }

    fn spread(values: &[f64]) -> f64 {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64).sqrt()
    }

    fn mean(values: &[f64]) -> f64 {
        values.iter().sum::<f64>() / values.len() as f64
    }

    #[test]
    fn first_sample_passes_through() {
        assert_eq!(OneEuro::new(1.0, 0.0).unwrap().filter(5.0, 10.0), 5.0);
    }

    #[test]
    fn a_still_noisy_signal_is_steadied() {
        let mut noise = Noise(1);
        let mut f = OneEuro::new(0.8, 0.1).unwrap();
        let raw: Vec<f64> = (0..600).map(|_| 10.0 + noise.gauss(0.14)).collect();
        let out: Vec<f64> = raw.iter().enumerate().map(|(i, v)| f.filter(*v, i as f64 * DT)).collect();
        assert!(spread(&out[100..]) < spread(&raw[100..]) / 2.5);
        assert!((mean(&out[100..]) - 10.0).abs() < 0.05);
    }

    #[test]
    fn a_steady_turn_is_followed_with_little_lag() {
        let mut f = OneEuro::new(0.8, 0.1).unwrap();
        let speed = 40.0; // degrees per second
        let mut out = 0.0;
        for i in 0..60 {
            out = f.filter(speed * f64::from(i) * DT, f64::from(i) * DT);
        }
        let lag_seconds = (speed * 59.0 * DT - out) / speed;
        assert!(lag_seconds < 0.06, "lag {lag_seconds}");
    }

    #[test]
    fn a_long_gap_or_a_clock_going_back_restarts_the_filter() {
        let mut f = OneEuro::new(0.8, 0.0).unwrap();
        f.filter(0.0, 0.0);
        f.filter(0.0, DT);
        assert_eq!(f.filter(50.0, 5.0), 50.0);
        assert_eq!(f.filter(20.0, 1.0), 20.0);
    }

    #[test]
    fn not_a_number_does_not_poison_the_state() {
        let mut f = OneEuro::new(0.8, 0.0).unwrap();
        f.filter(3.0, 0.0);
        assert_eq!(f.filter(f64::NAN, DT), 3.0);
        assert!((f.filter(3.0, 2.0 * DT) - 3.0).abs() < 1e-12);
        assert_eq!(f.filter(3.0, f64::NAN), 3.0, "a bad time restarts, it does not spread");
    }

    #[test]
    fn bad_settings_are_refused() {
        assert_eq!(OneEuro::new(0.0, 0.1), Err(BadSettings));
        assert_eq!(OneEuro::new(1.0, -1.0), Err(BadSettings));
        assert_eq!(OneEuro::new(f64::NAN, 0.1), Err(BadSettings));
        assert!(PoseFilter::new(Settings { rotation_min_cutoff: 0.0, ..Settings::default() }).is_err());
    }

    #[test]
    fn filters_rotation_in_degrees_and_returns_radians() {
        let mut noise = Noise(2);
        let mut f = PoseFilter::new(Settings::default()).unwrap();
        let mut yaw = Vec::new();
        for i in 0..400 {
            let noisy = (5.0 + noise.gauss(0.14)).to_radians() as f32;
            let (_, rotation) = f.filter((f64::from(i) * DT * 1e6) as i64, [0.0, 0.0, 800.0], [0.0, noisy, 0.0]);
            yaw.push(f64::from(rotation[1]).to_degrees());
        }
        assert!((mean(&yaw[100..]) - 5.0).abs() < 0.05);
        assert!(spread(&yaw[100..]) < 0.06);
    }

    #[test]
    fn position_keeps_its_value_and_reset_forgets() {
        let mut f = PoseFilter::new(Settings::default()).unwrap();
        let mut position = [0.0; 3];
        for i in 0..200 {
            (position, _) = f.filter((f64::from(i) * DT * 1e6) as i64, [-10.0, 20.0, 800.0], [0.0; 3]);
        }
        assert_eq!(position, [-10.0, 20.0, 800.0]);
        f.reset();
        let (after_reset, _) = f.filter((200.0 * DT * 1e6) as i64, [50.0, 50.0, 500.0], [0.0; 3]);
        assert_eq!(after_reset, [50.0, 50.0, 500.0]);
    }
}
