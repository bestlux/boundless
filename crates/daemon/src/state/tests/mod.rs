use super::*;
use chrono::Duration;

fn minimal_bmp_payload(red: u8) -> Vec<u8> {
    vec![
        b'B', b'M', 58, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1,
        0, 24, 0, 0, 0, 0, 0, 4, 0, 0, 0, 19, 11, 0, 0, 19, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        red, 0,
    ]
}

mod clipboard_replay;
mod input_and_outgoing;
mod input_broker;
mod layout_and_validation;
mod pairing_admission;
mod peer_and_capture;
mod trust_and_diagnostics;
