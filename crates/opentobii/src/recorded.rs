//! The recorded Eye Tracker 5 messages built into the binary.
//!
//! The game's DLL runs a fixed start-up conversation with a tracker. The
//! replies here are what a real tracker gave (see `fixtures/et5`). Gaze and
//! presence frames are used as templates and filled with live values.

use std::collections::HashMap;

use opentobii_proto::ttp::Message;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!("../../../fixtures/et5/", $name, ".hex"))
    };
}

/// Recorded reply payloads by command number.
pub(crate) fn replies() -> HashMap<u32, Vec<u8>> {
    [
        (1000, fixture!("init-rsp-1000")),
        (1200, fixture!("init-rsp-1200")),
        (1330, fixture!("init-rsp-1330")),
        (1400, fixture!("init-rsp-1400")),
        (1420, fixture!("init-rsp-1420")),
        (1430, fixture!("init-rsp-1430")),
        (1490, fixture!("init-rsp-1490")),
        (1650, fixture!("init-rsp-1650")),
        (2110, fixture!("init-rsp-2110")),
    ]
    .into_iter()
    .map(|(command, hex)| (command, payload(hex)))
    .collect()
}

pub(crate) fn gaze_frame() -> Vec<u8> {
    payload(fixture!("session1-gaze-frame"))
}

pub(crate) fn presence_frame() -> Vec<u8> {
    payload(fixture!("init-presence"))
}

fn payload(hex: &str) -> Vec<u8> {
    Message::from_usb_hex(hex).expect("built-in recording is valid").payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_recording_loads() {
        assert_eq!(replies().len(), 9);
        assert!(gaze_frame().len() > 1000);
        assert!(!presence_frame().is_empty());
    }
}
