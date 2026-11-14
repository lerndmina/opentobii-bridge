//! What the daemon and its clients (the GUI, the command line) say to each
//! other over the control socket.
//!
//! One JSON object per line, both ways. A client sends [`Request`]s; the
//! daemon answers each with one [`Line`], in order. After a
//! [`Request::Subscribe`] the daemon also sends [`Line::Pose`] and
//! [`Line::Status`] lines as things happen, between the answers.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::overlay::Overlay;
use crate::pipeline::{Neutral, Profile};

/// Where the daemon listens: `$XDG_RUNTIME_DIR/opentobii.sock`, else
/// `/tmp/opentobii.sock`.
#[must_use]
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("opentobii.sock"),
        _ => PathBuf::from("/tmp/opentobii.sock"),
    }
}

/// A client's request. `game` names a profile by the game's executable, as
/// [`crate::config::game_key`] spells it; leaving it out means the default
/// profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
// One of these exists per message and is dropped at once, so the size
// difference between variants costs nothing worth boxing for.
#[allow(clippy::large_enum_variant)]
pub enum Request {
    /// Answered with [`Line::State`].
    Get,
    /// Replace a profile, creating a game's profile if it has none. Takes
    /// effect on the next pose and is saved.
    SetProfile {
        #[serde(default)]
        game: Option<String>,
        profile: Profile,
    },
    /// Remove a game's profile; the game goes back to the default.
    DeleteProfile { game: String },
    /// Make the current head pose a profile's neutral. Answered with
    /// [`Line::Recentred`].
    Recentre {
        #[serde(default)]
        game: Option<String>,
    },
    /// Replace the gaze overlay's settings. Subscribers are sent
    /// [`Line::Overlay`]; the daemon starts or stops the overlay to match.
    SetOverlay { overlay: Overlay },
    /// Start receiving [`Line::Status`] and [`Line::Overlay`], and the
    /// feeds asked for. A bare `{"cmd":"subscribe"}` asks for head poses.
    Subscribe {
        /// Send [`Line::Pose`].
        #[serde(default = "yes")]
        poses: bool,
        /// Send [`Line::Gaze`].
        #[serde(default)]
        gaze: bool,
    },
}

fn yes() -> bool {
    true
}

/// One line from the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// One of these exists per message and is dropped at once, so the size
// difference between variants costs nothing worth boxing for.
#[allow(clippy::large_enum_variant)]
pub enum Line {
    /// The settings and what is connected.
    State { config: Config, status: Status },
    /// The request was carried out.
    Ok,
    /// The neutral pose a recentre stored.
    Recentred { neutral: Neutral },
    /// The request was not carried out, and why.
    Error { reason: String },
    /// A head pose as the tracker reported it, before any shaping.
    Pose(RawPose),
    /// Where the user is looking, unsmoothed.
    Gaze(GazePoint),
    /// The overlay's settings changed.
    Overlay(Overlay),
    /// Sent about once a second to subscribers.
    Status(Status),
}

/// A head pose in the units the settings use.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RawPose {
    /// `CLOCK_MONOTONIC` microseconds.
    pub micros: i64,
    pub position_mm: [f32; 3],
    /// Pitch, yaw, roll.
    pub rotation_deg: [f32; 3],
    /// False when the tracker marked any part of the pose as not measured.
    pub valid: bool,
}

/// A gaze point on the screen the tracker is mounted on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GazePoint {
    /// `CLOCK_MONOTONIC` microseconds.
    pub micros: i64,
    /// Across the screen: 0 is the left edge, 1 the right.
    pub x: f32,
    /// Down the screen: 0 is the top edge, 1 the bottom.
    pub y: f32,
    /// False when the tracker has no gaze for this sample.
    pub valid: bool,
}

/// What the daemon is connected to.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Status {
    /// The driver's socket is open.
    pub driver_connected: bool,
    /// A head pose arrived within the last second.
    pub tracking: bool,
    /// 0 unknown, 1 nobody in front of the tracker, 2 somebody.
    pub presence: u8,
    /// Width of the screen the driver says the tracker is on, mm.
    pub display_width_mm: Option<f64>,
    pub games: Vec<GameStatus>,
    /// Where settings are saved; `None` when the daemon was started without
    /// a settings file.
    pub settings_path: Option<PathBuf>,
}

/// One connected game process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameStatus {
    pub pid: u32,
    /// The executable as the helper reported it; empty from an old helper.
    pub executable: String,
    /// The profile name for this game.
    pub game: String,
    /// Whether it has a profile of its own.
    pub own_profile: bool,
    /// It asked for head pose.
    pub head_pose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_plain_json() {
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"get"}"#).unwrap(),
            Request::Get
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"recentre"}"#).unwrap(),
            Request::Recentre { game: None }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"subscribe"}"#).unwrap(),
            Request::Subscribe {
                poses: true,
                gaze: false
            }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"subscribe","poses":false,"gaze":true}"#)
                .unwrap(),
            Request::Subscribe {
                poses: false,
                gaze: true
            }
        );
        let overlay: Request = serde_json::from_str(
            r#"{"cmd":"set_overlay","overlay":{"enabled":true,"style":"ring"}}"#,
        )
        .unwrap();
        let Request::SetOverlay { overlay } = overlay else {
            panic!("a set_overlay request")
        };
        assert!(overlay.enabled);
        assert_eq!(overlay.style, crate::overlay::Style::Ring);
        assert_eq!(overlay.size, Overlay::default().size);
        let set: Request = serde_json::from_str(
            r#"{"cmd":"set_profile","game":"eurotrucks2.exe","profile":{"yaw":{"sensitivity":2.0}}}"#,
        )
        .unwrap();
        let Request::SetProfile { game, profile } = set else {
            panic!("a set_profile request")
        };
        assert_eq!(game.as_deref(), Some("eurotrucks2.exe"));
        assert_eq!(profile.yaw.sensitivity, 2.0);
        assert_eq!(profile.pitch.sensitivity, 1.0);
    }

    #[test]
    fn unknown_commands_and_fields_are_refused() {
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"reboot"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"recentre","gmae":"x.exe"}"#).is_err());
        assert!(serde_json::from_str::<Request>(
            r#"{"cmd":"set_profile","profile":{"yaw":{"sensitivty":2.0}}}"#
        )
        .is_err());
    }

    #[test]
    fn lines_round_trip() {
        let lines = [
            Line::Ok,
            Line::Gaze(GazePoint {
                micros: 9,
                x: 0.25,
                y: 0.75,
                valid: true,
            }),
            Line::Overlay(Overlay::default()),
            Line::Error {
                reason: "no".into(),
            },
            Line::Recentred {
                neutral: Neutral {
                    rotation: [1.0, 2.0, 3.0],
                    position: Some([0.0, 0.0, 700.0]),
                },
            },
            Line::Pose(RawPose {
                micros: 5,
                position_mm: [1.0, 2.0, 700.0],
                rotation_deg: [0.5, -3.0, 0.0],
                valid: true,
            }),
            Line::State {
                config: Config::default(),
                status: Status::default(),
            },
        ];
        for line in lines {
            let text = serde_json::to_string(&line).unwrap();
            assert!(!text.contains('\n'));
            assert_eq!(serde_json::from_str::<Line>(&text).unwrap(), line);
        }
    }
}
