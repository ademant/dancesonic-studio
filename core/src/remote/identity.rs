//! Studio's long-term remote control identity: a P-256 TLS key, an X25519
//! Noise key and a BLE service UUID, bound together by the fingerprint the QR
//! code carries. Kept as one secret in the Secret Service.

use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData};
use secrecy::{ExposeSecret, SecretSlice};
use sha2::{Digest, Sha256};

/// The Noise protocol of the Bluetooth transport.
pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

const FINGERPRINT_LABEL: &[u8] = b"dancesonic-studio-id-v1\0";
const MDNS_LABEL: &[u8] = b"dancesonic-mdns-v1\0";
/// Format byte of the stored secret.
const STORED_VERSION: u8 = 1;
/// The certificate's name. The phone pins the key, never checks the name.
const CERT_NAME: &str = "dancesonic-studio.local";

/// Why an identity could not be made or read back.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("cannot create a key: {0}")]
    Key(String),
    #[error("the stored remote control identity is damaged")]
    Damaged,
    #[error("cannot create the certificate: {0}")]
    Certificate(String),
}

/// Studio's identity. `Debug` prints only the fingerprint.
pub struct Identity {
    tls_pkcs8: SecretSlice<u8>,
    tls_spki: Vec<u8>,
    noise_private: SecretSlice<u8>,
    noise_public: [u8; 32],
    ble_service: [u8; 16],
    fingerprint: [u8; 32],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &super::b64u(&self.fingerprint))
            .finish_non_exhaustive()
    }
}

/// A TLS certificate and its private key, both DER. `Debug` hides the key.
#[derive(Debug)]
pub struct TlsCertificate {
    /// The self-signed certificate.
    pub certificate: Vec<u8>,
    /// The PKCS#8 private key.
    pub key: SecretSlice<u8>,
}

impl Identity {
    /// Creates a new identity with fresh keys.
    pub fn generate() -> Result<Self, IdentityError> {
        let tls = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|e| IdentityError::Key(e.to_string()))?;
        let noise = noise_builder()?
            .generate_keypair()
            .map_err(|e| IdentityError::Key(e.to_string()))?;
        let noise_public: [u8; 32] = noise
            .public
            .as_slice()
            .try_into()
            .map_err(|_| IdentityError::Key("noise key size".into()))?;
        let mut ble_service = [0u8; 16];
        getrandom::fill(&mut ble_service).map_err(|e| IdentityError::Key(e.to_string()))?;
        // A random (version 4, RFC 4122 variant) UUID.
        ble_service[6] = (ble_service[6] & 0x0f) | 0x40;
        ble_service[8] = (ble_service[8] & 0x3f) | 0x80;
        Ok(Self::assemble(
            tls.serialize_der().into(),
            tls.subject_public_key_info(),
            noise.private.into(),
            noise_public,
            ble_service,
        ))
    }

    fn assemble(
        tls_pkcs8: SecretSlice<u8>,
        tls_spki: Vec<u8>,
        noise_private: SecretSlice<u8>,
        noise_public: [u8; 32],
        ble_service: [u8; 16],
    ) -> Self {
        let fingerprint = fingerprint(&tls_spki, &noise_public);
        Self {
            tls_pkcs8,
            tls_spki,
            noise_private,
            noise_public,
            ble_service,
            fingerprint,
        }
    }

    /// The identity as one secret for the Secret Service.
    pub fn to_secret(&self) -> SecretSlice<u8> {
        let pkcs8 = self.tls_pkcs8.expose_secret();
        let mut out = Vec::with_capacity(3 + pkcs8.len() + 32 + 32 + 16);
        out.push(STORED_VERSION);
        out.extend_from_slice(&u16::try_from(pkcs8.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(pkcs8);
        out.extend_from_slice(self.noise_private.expose_secret());
        out.extend_from_slice(&self.noise_public);
        out.extend_from_slice(&self.ble_service);
        out.into()
    }

    /// Reads an identity back from [`Identity::to_secret`].
    pub fn from_secret(secret: &SecretSlice<u8>) -> Result<Self, IdentityError> {
        let bytes = secret.expose_secret();
        let (&version, rest) = bytes.split_first().ok_or(IdentityError::Damaged)?;
        if version != STORED_VERSION || rest.len() < 2 {
            return Err(IdentityError::Damaged);
        }
        let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
        let rest = &rest[2..];
        if rest.len() != len + 32 + 32 + 16 {
            return Err(IdentityError::Damaged);
        }
        let (pkcs8, rest) = rest.split_at(len);
        let (noise_private, rest) = rest.split_at(32);
        let (noise_public, ble) = rest.split_at(32);
        let tls = KeyPair::try_from(pkcs8).map_err(|_| IdentityError::Damaged)?;
        if tls.algorithm() != &PKCS_ECDSA_P256_SHA256 {
            return Err(IdentityError::Damaged);
        }
        Ok(Self::assemble(
            pkcs8.to_vec().into(),
            tls.subject_public_key_info(),
            noise_private.to_vec().into(),
            noise_public
                .try_into()
                .map_err(|_| IdentityError::Damaged)?,
            ble.try_into().map_err(|_| IdentityError::Damaged)?,
        ))
    }

    /// The fingerprint the phone pins.
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    /// The DER `SubjectPublicKeyInfo` of the TLS key.
    pub fn tls_spki(&self) -> &[u8] {
        &self.tls_spki
    }

    /// The X25519 public key.
    pub fn noise_public(&self) -> &[u8; 32] {
        &self.noise_public
    }

    /// The X25519 private key, for the Noise handshake.
    pub(crate) fn noise_private(&self) -> &SecretSlice<u8> {
        &self.noise_private
    }

    /// The BLE service UUID, big-endian bytes.
    pub fn ble_service(&self) -> &[u8; 16] {
        &self.ble_service
    }

    /// The BLE service UUID as text (`8-4-4-4-12`).
    pub fn ble_service_text(&self) -> String {
        uuid_text(&self.ble_service)
    }

    /// The short tag in the mDNS TXT record.
    pub fn mdns_tag(&self) -> String {
        let hash = Sha256::new()
            .chain_update(MDNS_LABEL)
            .chain_update(self.fingerprint)
            .finalize();
        super::b64u(&hash[..8])
    }

    /// A self-signed certificate for the network transport. Made fresh on
    /// every start; only its key is pinned.
    pub fn certificate(&self) -> Result<TlsCertificate, IdentityError> {
        let pkcs8 = self.tls_pkcs8.expose_secret();
        let key = KeyPair::try_from(pkcs8).map_err(|_| IdentityError::Damaged)?;
        let params = CertificateParams::new(vec![CERT_NAME.to_owned()])
            .map_err(|e| IdentityError::Certificate(e.to_string()))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| IdentityError::Certificate(e.to_string()))?;
        Ok(TlsCertificate {
            certificate: cert.der().to_vec(),
            key: pkcs8.to_vec().into(),
        })
    }
}

/// `SHA-256(label || spki || noise_public)`.
pub fn fingerprint(tls_spki: &[u8], noise_public: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(FINGERPRINT_LABEL)
        .chain_update(tls_spki)
        .chain_update(noise_public)
        .finalize()
        .into()
}

/// A Noise builder for [`NOISE_PARAMS`].
pub(crate) fn noise_builder() -> Result<snow::Builder<'static>, IdentityError> {
    let params = NOISE_PARAMS
        .parse()
        .map_err(|e: snow::Error| IdentityError::Key(e.to_string()))?;
    Ok(snow::Builder::new(params))
}

/// `8-4-4-4-12` hex text of a UUID.
pub fn uuid_text(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
