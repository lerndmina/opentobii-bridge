//! What the opentobii bridge does to a head pose between the tracker and
//! the game.
//!
//! - [`smooth`]: the One Euro filter that steadies the driver's pose.
//! - [`camera`]: the tracker's infrared picture as a camera for face login.
//! - [`confirm`]: telling a nod from a head shake, to approve a face login.
//! - [`config`]: the settings file, with one profile per game.
//! - [`control`]: the messages of the daemon's control socket.
//! - [`overlay`]: the gaze overlay's settings and smoothing.
//! - [`pipeline`]: recentre, smoothing, dead zone, sensitivity and limit.

pub mod camera;
pub mod config;
pub mod confirm;
pub mod control;
pub mod overlay;
pub mod pipeline;
pub mod smooth;
