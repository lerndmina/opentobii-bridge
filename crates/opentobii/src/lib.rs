//! The opentobii bridge daemon.
//!
//! Three sides, one thread per connection:
//!
//! - the `tobiid` socket, where live gaze, presence and head pose come from;
//! - a TCP listener speaking TTP to the game's Tobii DLL (port 4455);
//! - a TCP listener for the helper inside the game's prefix, which relays
//!   the services pipe that carries the display id and head pose (47455).

pub mod daemon;
#[cfg(feature = "gui")]
pub mod gui;
#[cfg(feature = "overlay")]
pub mod overlay;
pub mod prefix;
mod recorded;
pub mod service;
pub mod steam;

pub use daemon::{start, Config, Running};
