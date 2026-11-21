//! The pairing offer behind the QR code: a one-time secret that expires after
//! five minutes, is consumed by the first valid proof and is void after five
//! wrong ones.

use hmac::{KeyInit, Mac};
use secrecy::{ExposeSecret, SecretSlice, SecretString};

use super::identity::Identity;

/// How long a shown code is valid, in seconds.
pub const OFFER_LIFETIME_SECS: i64 = 5 * 60;
/// Wrong proofs after which the secret is void.
pub const MAX_WRONG_PROOFS: u8 = 5;
/// Longest Studio name in the code.
pub const MAX_NAME_CHARS: usize = 64;

const PAIR_LABEL: &[u8] = b"dancesonic-pair-v1\0";
const PAIR_CODE_LABEL: &[u8] = b"dancesonic-pair-code-v1\0";
const COMPARE_LABEL: &[u8] = b"dancesonic-compare-v1\0";

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

/// Why a pairing proof was refused. `code()` is the wire error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OfferError {
    #[error("no pairing code is shown")]
    NoOffer,
    #[error("the pairing code has expired")]
    Expired,
    #[error("the pairing code was already used")]
    Used,
    #[error("the pairing proof is wrong")]
    BadProof,
}

impl OfferError {
    /// The wire error code.
    pub fn code(self) -> &'static str {
        match self {
            OfferError::NoOffer | OfferError::Expired => "expired",
            OfferError::Used => "secret_used",
            OfferError::BadProof => "bad_proof",
        }
    }
}

/// One shown pairing code. `Debug` never prints the secret.
pub struct Offer {
    secret: SecretSlice<u8>,
    expires_at: i64,
    wrong: u8,
    used: bool,
}

impl std::fmt::Debug for Offer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Offer")
            .field("expires_at", &self.expires_at)
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}

impl Offer {
    /// A new offer with a fresh 128-bit secret, valid from `now` (Unix s).
    pub fn new(now: i64) -> Result<Self, getrandom::Error> {
        let mut secret = vec![0u8; 16];
        getrandom::fill(&mut secret)?;
        Ok(Self::with_secret(secret.into(), now))
    }

    pub(crate) fn with_secret(secret: SecretSlice<u8>, now: i64) -> Self {
        Self {
            secret,
            expires_at: now + OFFER_LIFETIME_SECS,
            wrong: 0,
            used: false,
        }
    }

    /// When the code expires (Unix s).
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// Whether the code can still be used at `now`.
    pub fn is_open(&self, now: i64) -> bool {
        !self.used && self.wrong < MAX_WRONG_PROOFS && now < self.expires_at
    }

    /// The text of the QR code. It holds the secret: show it, never log it.
    pub fn uri(&self, identity: &Identity, name: &str) -> SecretString {
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        let name: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
        format!(
            "dancesonic-studio:pair?v={}&fp={}&s={}&exp={}&ble={}&name={}",
            super::PROTOCOL_VERSION,
            super::b64u(identity.fingerprint()),
            super::b64u(self.secret.expose_secret()),
            self.expires_at,
            identity.ble_service_text(),
            name
        )
        .into()
    }

    /// Checks a proof over `transcript` at `now`. The first valid proof uses
    /// the code up; wrong ones count towards [`MAX_WRONG_PROOFS`].
    pub fn check(&mut self, transcript: &[u8], proof: &[u8], now: i64) -> Result<(), OfferError> {
        if self.used || self.wrong >= MAX_WRONG_PROOFS {
            return Err(OfferError::Used);
        }
        if now >= self.expires_at {
            return Err(OfferError::Expired);
        }
        let mut mac = HmacSha256::new_from_slice(self.secret.expose_secret())
            .map_err(|_| OfferError::BadProof)?;
        mac.update(transcript);
        // `verify_slice` compares in constant time.
        if mac.verify_slice(proof).is_err() {
            self.wrong = self.wrong.saturating_add(1);
            return Err(OfferError::BadProof);
        }
        self.used = true;
        Ok(())
    }
}

/// The bytes a pairing proof and signature cover.
pub fn pair_transcript(
    fingerprint: &[u8; 32],
    device_key: &[u8],
    noise_key: &[u8],
    binding: &[u8; 32],
) -> Vec<u8> {
    labelled_transcript(PAIR_LABEL, fingerprint, device_key, noise_key, binding)
}

/// The bytes a `pair_code_request` signature covers (pairing through
/// folksonic, #38). A different label from [`pair_transcript`], so a
/// signature made for one kind of pairing is never valid for the other.
pub fn pair_code_transcript(
    fingerprint: &[u8; 32],
    device_key: &[u8],
    noise_key: &[u8],
    binding: &[u8; 32],
) -> Vec<u8> {
    labelled_transcript(PAIR_CODE_LABEL, fingerprint, device_key, noise_key, binding)
}

fn labelled_transcript(
    label: &[u8],
    fingerprint: &[u8; 32],
    device_key: &[u8],
    noise_key: &[u8],
    binding: &[u8; 32],
) -> Vec<u8> {
    let mut t = Vec::with_capacity(label.len() + 32 + device_key.len() + noise_key.len() + 32);
    t.extend_from_slice(label);
    t.extend_from_slice(fingerprint);
    t.extend_from_slice(device_key);
    t.extend_from_slice(noise_key);
    t.extend_from_slice(binding);
    t
}

/// The 6-digit comparison code both screens show when a phone pairs through
/// folksonic (#38), from the Noise handshake hash of the session.
///
/// A man in the middle, folksonic included, runs two handshakes, one with
/// each side, and so gets two different hashes: the codes on the two
/// screens differ, and the user rejects the phone.
pub fn comparison_code(binding: &[u8; 32]) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::new()
        .chain_update(COMPARE_LABEL)
        .chain_update(binding)
        .finalize();
    let n = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) % 1_000_000;
    format!("{n:06}")
}

/// HMAC-SHA256 of `transcript` with `secret`: what the phone sends as proof.
#[cfg(test)]
pub(crate) fn proof(secret: &[u8], transcript: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).unwrap_or_else(|_| unreachable!());
    mac.update(transcript);
    mac.finalize().into_bytes().to_vec()
}
