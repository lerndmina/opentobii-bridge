//! What the opentobii bridge does to a head pose between the tracker and
//! the game.
//!
//! - [`smooth`]: the One Euro filter that steadies the driver's pose.
//! - [`config`]: the settings file, with one profile per game.
//! - [`pipeline`]: recentre, smoothing, dead zone, sensitivity and limit.

pub mod config;
pub mod pipeline;
pub mod smooth;
