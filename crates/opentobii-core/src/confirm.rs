//! Telling a nod from a head shake, for the prompt that approves a face
//! login.
//!
//! A gesture is the head going one way and straight back: two opposite
//! swings on one axis, with no pause between them. Pitch is a nod, yaw is a
//! shake. The limits come from a recording of real ones (the test data):
//! every nod and shake there went out and back in under 0.9 seconds with at
//! least 14 degrees each way, while a look down at the keyboard, a look at
//! a second monitor and leaning back in the chair all took 1.6 seconds or
//! more, because the head rests at the far end.

use std::collections::VecDeque;

/// What the head did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// Up and down: yes.
    Nod,
    /// Side to side: no.
    Shake,
}

/// Each of the two swings must turn the head at least this far, degrees.
pub const MIN_SWING_DEG: f32 = 10.0;
/// From leaving the starting pose to being most of the way back, seconds.
pub const MAX_THERE_AND_BACK: f64 = 1.0;
/// The other axis may move at most this share of the smaller swing. It
/// keeps a diagonal movement, such as leaning back, from counting.
const MAX_OTHER_SHARE: f32 = 0.6;
/// A change of direction counts once the head has come back this far.
const TURN_DEG: f32 = 3.0;
/// A swing starts where the head has left its start by this share of the
/// swing, and ends where it has covered this share.
const START_SHARE: f32 = 0.2;
const END_SHARE: f32 = 0.8;
/// How much of the past is looked at, seconds.
const WINDOW: f64 = 2.5;
/// A hole in the poses this long means what came before is not continuous
/// with what comes after, seconds.
const MAX_GAP: f64 = 0.3;

#[derive(Debug, Clone, Copy)]
struct Sample {
    seconds: f64,
    /// Pitch, yaw.
    angle: [f32; 2],
}

/// Watches head poses for a nod or a shake. Start a new one for each
/// prompt: only movement after it was made counts.
#[derive(Debug, Default)]
pub struct Detector {
    samples: VecDeque<Sample>,
}

impl Detector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one valid head pose. Returns the gesture as soon as one is
    /// complete; the caller stops there.
    pub fn push(&mut self, micros: i64, pitch_deg: f32, yaw_deg: f32) -> Option<Gesture> {
        if !pitch_deg.is_finite() || !yaw_deg.is_finite() {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let seconds = micros as f64 / 1e6;
        if let Some(last) = self.samples.back() {
            let gap = seconds - last.seconds;
            if gap <= 0.0 {
                return None;
            }
            if gap > MAX_GAP {
                self.samples.clear();
            }
        }
        self.samples.push_back(Sample {
            seconds,
            angle: [pitch_deg, yaw_deg],
        });
        while self
            .samples
            .front()
            .is_some_and(|first| seconds - first.seconds > WINDOW)
        {
            self.samples.pop_front();
        }
        let samples = self.samples.make_contiguous();
        if there_and_back(samples, 1) {
            Some(Gesture::Shake)
        } else if there_and_back(samples, 0) {
            Some(Gesture::Nod)
        } else {
            None
        }
    }
}

/// The places where `axis` changed direction, as indexes into `samples`:
/// the first sample, each turn, and the furthest point of the swing still
/// under way.
fn turns(samples: &[Sample], axis: usize) -> Vec<usize> {
    let mut turns = vec![0];
    // 0 until the head has moved at all, then +1 or -1.
    let mut direction = 0i8;
    let mut furthest = 0;
    for (index, sample) in samples.iter().enumerate() {
        let value = sample.angle[axis];
        let far = samples[furthest].angle[axis];
        if (direction >= 0 && value > far) || (direction <= 0 && value < far) {
            furthest = index;
        }
        let far = samples[furthest].angle[axis];
        match direction {
            0 => {
                let start = samples[0].angle[axis];
                if value - start > TURN_DEG {
                    direction = 1;
                    furthest = index;
                } else if start - value > TURN_DEG {
                    direction = -1;
                    furthest = index;
                }
            }
            1 if far - value > TURN_DEG => {
                turns.push(furthest);
                direction = -1;
                furthest = index;
            }
            -1 if value - far > TURN_DEG => {
                turns.push(furthest);
                direction = 1;
                furthest = index;
            }
            _ => {}
        }
    }
    turns.push(furthest);
    turns
}

/// Whether the newest two swings on `axis` are a gesture.
fn there_and_back(samples: &[Sample], axis: usize) -> bool {
    let turns = turns(samples, axis);
    let [.., from, turn, to] = turns[..] else {
        return false;
    };
    let value = |index: usize| samples[index].angle[axis];
    let out = value(turn) - value(from);
    let back = value(to) - value(turn);
    let smaller = out.abs().min(back.abs());
    if smaller < MIN_SWING_DEG {
        return false;
    }
    // Where the head left its starting pose, and where it was most of the
    // way back. Time spent resting at either end is not part of the swing
    // out, but time resting at the turn is counted, and that is the point.
    let left = (from..=turn)
        .rev()
        .find(|&index| (value(index) - value(from)).abs() <= START_SHARE * out.abs())
        .unwrap_or(from);
    let Some(returned) =
        (turn..=to).find(|&index| (value(index) - value(turn)).abs() >= END_SHARE * back.abs())
    else {
        return false;
    };
    if samples[returned].seconds - samples[left].seconds > MAX_THERE_AND_BACK {
        return false;
    }
    let other = 1 - axis;
    let (low, high) = samples[left..=returned]
        .iter()
        .map(|sample| sample.angle[other])
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), value| {
            (low.min(value), high.max(value))
        });
    high - low <= MAX_OTHER_SHARE * smaller
}

#[cfg(test)]
mod tests {
    use super::*;

    /// phase, microseconds, pitch, yaw
    fn recording() -> Vec<(String, i64, f32, f32)> {
        include_str!("confirm_recording.csv")
            .lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .map(|line| {
                let mut fields = line.split(',');
                let phase = fields.next().unwrap().to_string();
                let millis: i64 = fields.next().unwrap().parse().unwrap();
                let pitch = fields.next().unwrap().parse().unwrap();
                let yaw = fields.next().unwrap().parse().unwrap();
                (phase, millis * 1000, pitch, yaw)
            })
            .collect()
    }

    /// What a fresh detector makes of each phase of the recording.
    fn phases() -> Vec<(String, Option<Gesture>)> {
        let mut found: Vec<(String, Option<Gesture>)> = Vec::new();
        let mut detector = Detector::new();
        for (phase, micros, pitch, yaw) in recording() {
            if found.last().is_none_or(|(last, _)| *last != phase) {
                found.push((phase, None));
                detector = Detector::new();
            }
            let slot = &mut found.last_mut().unwrap().1;
            if slot.is_none() {
                *slot = detector.push(micros, pitch, yaw);
            }
        }
        found
    }

    #[test]
    fn every_recorded_nod_and_shake_is_seen_and_nothing_else_is() {
        let phases = phases();
        assert_eq!(phases.len(), 20);
        for (phase, found) in phases {
            let wanted = if phase.starts_with("nod-") {
                Some(Gesture::Nod)
            } else if phase.starts_with("shake-") {
                Some(Gesture::Shake)
            } else {
                // Sitting still, looking at the keyboard, looking aside,
                // leaning back, reading.
                None
            };
            assert_eq!(found, wanted, "{phase}");
        }
    }

    /// A head that goes `degrees` one way over `out` seconds, rests for
    /// `rest` seconds, and comes back as fast as it went.
    fn move_and_return(axis: usize, degrees: f32, out: f64, rest: f64) -> Option<Gesture> {
        let mut detector = Detector::new();
        let total = 0.5 + out + rest + out + 0.5;
        let mut seconds = 0.0;
        let mut found = None;
        while seconds < total {
            let moved = if seconds < 0.5 {
                0.0
            } else if seconds < 0.5 + out {
                (seconds - 0.5) / out
            } else if seconds < 0.5 + out + rest {
                1.0
            } else if seconds < 0.5 + out + rest + out {
                1.0 - (seconds - 0.5 - out - rest) / out
            } else {
                0.0
            };
            #[allow(clippy::cast_possible_truncation)]
            let value = degrees * moved as f32;
            let (pitch, yaw) = if axis == 0 {
                (value, 0.0)
            } else {
                (0.0, value)
            };
            #[allow(clippy::cast_possible_truncation)]
            let micros = (seconds * 1e6) as i64;
            found = found.or(detector.push(micros, pitch, yaw));
            seconds += 0.03;
        }
        found
    }

    #[test]
    fn a_quick_out_and_back_is_a_gesture_in_either_direction() {
        assert_eq!(move_and_return(0, -15.0, 0.25, 0.0), Some(Gesture::Nod));
        assert_eq!(move_and_return(0, 15.0, 0.25, 0.0), Some(Gesture::Nod));
        assert_eq!(move_and_return(1, 15.0, 0.25, 0.0), Some(Gesture::Shake));
        assert_eq!(move_and_return(1, -15.0, 0.25, 0.0), Some(Gesture::Shake));
    }

    #[test]
    fn a_rest_at_the_far_end_a_small_movement_and_a_slow_one_are_not() {
        // Looking at something and back.
        assert_eq!(move_and_return(0, -16.0, 0.3, 1.0), None);
        assert_eq!(move_and_return(1, 30.0, 0.3, 1.2), None);
        // A twitch.
        assert_eq!(move_and_return(0, 6.0, 0.2, 0.0), None);
        // Drifting.
        assert_eq!(move_and_return(0, 20.0, 1.0, 0.0), None);
    }

    #[test]
    fn a_diagonal_movement_is_neither() {
        let mut detector = Detector::new();
        let mut found = None;
        for step in 0..60 {
            let seconds = f64::from(step) * 0.03;
            #[allow(clippy::cast_possible_truncation)]
            let wave = (15.0 * (seconds * 6.0).sin()) as f32;
            #[allow(clippy::cast_possible_truncation)]
            let micros = (seconds * 1e6) as i64;
            found = found.or(detector.push(micros, wave, wave));
        }
        assert_eq!(found, None);
    }

    #[test]
    fn a_hole_in_the_poses_starts_again() {
        let mut detector = Detector::new();
        // Out before the hole, back after it: not one movement.
        for step in 0..10 {
            let pitch = -1.5 * f32::from(u8::try_from(step).unwrap());
            assert_eq!(detector.push(i64::from(step) * 30_000, pitch, 0.0), None);
        }
        for step in 0..10 {
            let pitch = -15.0 + 1.5 * f32::from(u8::try_from(step).unwrap());
            let micros = 1_000_000 + i64::from(step) * 30_000;
            assert_eq!(detector.push(micros, pitch, 0.0), None);
        }
        // Not a number, and time going backwards, are dropped.
        assert_eq!(detector.push(2_000_000, f32::NAN, 0.0), None);
        assert_eq!(detector.push(0, 0.0, 0.0), None);
    }
}
