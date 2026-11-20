//! Remote control by the DanceSonic phone app (#19): pairing by QR code,
//! signed and counted commands, and one session layer shared by Bluetooth LE
//! (#24) and the local network (#20). The protocol is specified in
//! `docs/remote-protocol.md`.
//!
//! - [`identity`]: Studio's long-term keys and fingerprint.
//! - [`offer`]: the QR code with its one-time secret.
//! - [`messages`]: the wire messages, parsed strictly.
//! - [`devices`]: paired phones and their counters, in the database.
//! - [`hub`]: what every connection does, whatever its transport.
//! - [`ble`]: framing over GATT and the Noise session for Bluetooth.
//! - [`network`]: HTTPS and WebSocket on the local network, with mDNS.
//! - [`settings`]: the switches, off by default.

pub mod ble;
pub mod devices;
pub mod hub;
pub mod identity;
pub mod messages;
pub mod network;
pub mod offer;
pub mod settings;

#[cfg(test)]
pub(crate) mod phone;
#[cfg(test)]
mod tests;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u8 = 1;

/// Base64url without padding, as every binary value on the wire.
pub fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes base64url without padding; `None` for anything else or for more
/// than `max` bytes.
pub fn from_b64u(text: &str, max: usize) -> Option<Vec<u8>> {
    if text.len() > max.div_ceil(3) * 4 {
        return None;
    }
    URL_SAFE_NO_PAD.decode(text).ok().filter(|b| b.len() <= max)
}
