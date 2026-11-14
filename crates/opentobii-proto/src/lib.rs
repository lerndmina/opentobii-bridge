//! Wire formats the opentobii bridge speaks.
//!
//! - [`ttp`]: the tracker protocol, as the Tobii game integration DLL speaks
//!   it over TCP.

pub mod ttp;

#[cfg(test)]
pub(crate) mod fixtures {
    use crate::ttp::Message;

    /// One recorded Eye Tracker 5 message from `fixtures/et5`.
    pub(crate) fn load(name: &str) -> Message {
        let path = format!("{}/../../fixtures/et5/{name}.hex", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        Message::from_usb_hex(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    }
}
