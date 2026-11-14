//! The opentobii bridge daemon.
//!
//! Three sides, one thread per connection:
//!
//! - the `tobiid` socket, where live gaze, presence and head pose come from;
//! - a TCP listener speaking TTP to the game's Tobii DLL (port 4455);
//! - a TCP listener for the helper inside the game's prefix, which relays
//!   the services pipe that carries the display id and head pose (47455).

pub mod daemon;
mod recorded;

pub use daemon::{start, Config, Running};
