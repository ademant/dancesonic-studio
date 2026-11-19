//! Device keys and DPoP proofs (RFC 9449) for folksonic's device pairing.
//!
//! A paired device has no bearer secret. It holds a P-256 key and signs each
//! request with a proof: a short ES256 JWT naming the method and the URL
//! (without its query), with a fresh `iat` and a unique `jti`. The server
//! checks the proof against the public key it stored at pairing.
//!
//! The private key is the only secret. It is kept in the Secret Service by
//! the caller; this module never writes it anywhere.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use secrecy::{ExposeSecret, SecretSlice};
use serde::Serialize;
use sha2::{Digest, Sha256};
use url::Url;

/// Bytes in a P-256 private key.
pub const KEY_LEN: usize = 32;

/// Longest `jti` the server accepts.
const JTI_BYTES: usize = 16;

/// Errors from device keys and proofs. Carry no key material.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DpopError {
    #[error("the device key is not a valid P-256 key")]
    BadKey,
    #[error("the system random source failed")]
    Random,
    #[error("the request address cannot be signed")]
    BadUrl,
}

/// A paired device's private key, P-256.
#[derive(Clone)]
pub struct DeviceKey {
    signing: SigningKey,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeviceKey([REDACTED])")
    }
}

/// The public part of a key, as sent to the server (RFC 7517).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PublicJwk {
    pub kty: &'static str,
    pub crv: &'static str,
    pub x: String,
    pub y: String,
}

impl DeviceKey {
    /// A new key from the system random source.
    pub fn generate() -> Result<Self, DpopError> {
        // A P-256 scalar is invalid only when it is zero or not below the
        // group order, which is vanishingly rare; retry instead of failing.
        for _ in 0..8 {
            let mut bytes = [0u8; KEY_LEN];
            getrandom::fill(&mut bytes).map_err(|_| DpopError::Random)?;
            if let Ok(signing) = SigningKey::from_slice(&bytes) {
                return Ok(Self { signing });
            }
        }
        Err(DpopError::BadKey)
    }

    /// Restores a key from its 32-byte secret, as kept in the Secret Service.
    pub fn from_secret(secret: &SecretSlice<u8>) -> Result<Self, DpopError> {
        let signing =
            SigningKey::from_slice(secret.expose_secret()).map_err(|_| DpopError::BadKey)?;
        Ok(Self { signing })
    }

    /// The 32-byte secret, for the Secret Service. Handle with care.
    pub fn secret(&self) -> SecretSlice<u8> {
        SecretSlice::from(self.signing.to_bytes().to_vec())
    }

    /// The public key in JWK form.
    pub fn public_jwk(&self) -> PublicJwk {
        let (x, y) = self.coordinates();
        PublicJwk {
            kty: "EC",
            crv: "P-256",
            x,
            y,
        }
    }

    /// The RFC 7638 thumbprint of the public key (`jkt`), which the
    /// verification code is derived from.
    pub fn thumbprint(&self) -> String {
        // RFC 7638: the required members in lexicographic order, no spaces.
        let (x, y) = self.coordinates();
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes()))
    }

    /// A DPoP proof for one request. `url` is the request URL; its query and
    /// fragment are not part of the proof.
    pub fn proof(&self, method: &str, url: &Url, now_secs: i64) -> Result<String, DpopError> {
        let mut htu = url.clone();
        htu.set_query(None);
        htu.set_fragment(None);
        let jti = random_jti()?;
        let header = Header {
            typ: "dpop+jwt",
            alg: "ES256",
            jwk: self.public_jwk(),
        };
        let claims = Claims {
            jti: &jti,
            htm: method,
            htu: htu.as_str(),
            iat: now_secs,
        };
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).map_err(|_| DpopError::BadUrl)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).map_err(|_| DpopError::BadUrl)?),
        );
        let signature: Signature = self.signing.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }

    fn coordinates(&self) -> (String, String) {
        let point = self.signing.verifying_key().to_sec1_point(false);
        let x = point
            .x()
            .map(|v| URL_SAFE_NO_PAD.encode(v))
            .unwrap_or_default();
        let y = point
            .y()
            .map(|v| URL_SAFE_NO_PAD.encode(v))
            .unwrap_or_default();
        (x, y)
    }

    /// The public key, for verifying proofs in tests.
    #[cfg(test)]
    fn verifying_key(&self) -> p256::ecdsa::VerifyingKey {
        *self.signing.verifying_key()
    }
}

#[derive(Serialize)]
struct Header {
    typ: &'static str,
    alg: &'static str,
    jwk: PublicJwk,
}

#[derive(Serialize)]
struct Claims<'a> {
    jti: &'a str,
    htm: &'a str,
    htu: &'a str,
    iat: i64,
}

fn random_jti() -> Result<String, DpopError> {
    let mut bytes = [0u8; JTI_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| DpopError::Random)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The six-digit verification code both sides show during pairing:
/// `SHA-256(token_hash ":" jkt)`, the first four bytes big-endian, modulo
/// 1,000,000, zero-padded. `token_hash` is the lower-case hex SHA-256 of the
/// pairing token and `jkt` the thumbprint of the device key.
pub fn verification_code(token: &str, jkt: &str) -> String {
    let token_hash: String = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let digest = Sha256::digest(format!("{token_hash}:{jkt}").as_bytes());
    let first = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", first % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;

    fn key_from(byte: u8) -> DeviceKey {
        DeviceKey::from_secret(&SecretSlice::from(vec![byte; KEY_LEN])).unwrap()
    }

    #[test]
    fn verification_code_matches_the_published_test_vector() {
        // From folksonic's README, "Accounts and pairing".
        assert_eq!(
            verification_code(
                "tsp_example-pairing-token",
                "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs"
            ),
            "446284"
        );
    }

    #[test]
    fn a_different_key_gives_a_different_code() {
        let token = "tsp_example-pairing-token";
        let a = verification_code(token, &key_from(1).thumbprint());
        let b = verification_code(token, &key_from(2).thumbprint());
        assert_ne!(a, b);
        assert_eq!(a.len(), 6);
    }

    #[test]
    fn thumbprint_is_43_url_safe_characters() {
        let jkt = key_from(7).thumbprint();
        assert_eq!(jkt.len(), 43);
        assert!(
            jkt.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn a_secret_round_trips_to_the_same_key() {
        let key = key_from(9);
        let restored = DeviceKey::from_secret(&key.secret()).unwrap();
        assert_eq!(key.thumbprint(), restored.thumbprint());
    }

    #[test]
    fn zero_is_not_a_key() {
        assert_eq!(
            DeviceKey::from_secret(&SecretSlice::from(vec![0u8; KEY_LEN])).err(),
            Some(DpopError::BadKey)
        );
        assert_eq!(
            DeviceKey::from_secret(&SecretSlice::from(vec![1u8; 5])).err(),
            Some(DpopError::BadKey)
        );
    }

    #[test]
    fn a_proof_names_the_request_and_verifies_with_the_public_key() {
        let key = key_from(3);
        let url = Url::parse(
            "https://music.example/folksonic/rest/getLibraryChanges.view?since=5&size=500",
        )
        .unwrap();
        let proof = key.proof("GET", &url, 1_700_000_000).unwrap();

        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header["typ"], "dpop+jwt");
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["jwk"]["crv"], "P-256");
        assert_eq!(claims["htm"], "GET");
        assert_eq!(
            claims["htu"],
            "https://music.example/folksonic/rest/getLibraryChanges.view"
        );
        assert_eq!(claims["iat"], 1_700_000_000);
        assert!(claims["jti"].as_str().unwrap().len() <= 128);

        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).unwrap()).unwrap();
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        key.verifying_key()
            .verify(signing_input.as_bytes(), &signature)
            .expect("the proof must verify with the device's public key");
    }

    #[test]
    fn each_proof_has_a_fresh_jti() {
        let key = key_from(4);
        let url = Url::parse("https://music.example/rest/ping.view").unwrap();
        let a = key.proof("GET", &url, 1).unwrap();
        let b = key.proof("GET", &url, 1).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_debug_output_hides_the_key() {
        let text = format!("{:?}", key_from(5));
        assert!(!text.contains("05"), "{text}");
    }
}
