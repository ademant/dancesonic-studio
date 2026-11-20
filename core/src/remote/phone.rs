//! A phone for tests: the client side of `docs/remote-protocol.md`, written
//! from the document, so the tests check Studio against the specification.

use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::json;

use super::devices::DeviceId;
use super::hub::{command_bytes, hello_bytes};
use super::identity::noise_builder;
use super::offer::{pair_transcript, proof};
use super::{b64u, from_b64u};

/// What the QR code says.
pub(crate) struct Scanned {
    pub fp: [u8; 32],
    pub secret: Vec<u8>,
    pub exp: i64,
    pub ble: String,
    pub name: String,
}

/// Reads a QR text.
pub(crate) fn scan(uri: &str) -> Scanned {
    let query = uri.strip_prefix("dancesonic-studio:pair?").unwrap();
    let mut fields = std::collections::HashMap::new();
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        fields.insert(k.into_owned(), v.into_owned());
    }
    assert_eq!(fields["v"], "1");
    Scanned {
        fp: from_b64u(&fields["fp"], 32).unwrap().try_into().unwrap(),
        secret: from_b64u(&fields["s"], 16).unwrap(),
        exp: fields["exp"].parse().unwrap(),
        ble: fields["ble"].clone(),
        name: fields["name"].clone(),
    }
}

pub(crate) struct Phone {
    pub signing: SigningKey,
    pub noise: snow::Keypair,
    pub counter: u64,
    pub device: Option<DeviceId>,
}

impl Phone {
    pub(crate) fn new() -> Self {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).unwrap();
        Self {
            signing: SigningKey::from_slice(&bytes).unwrap(),
            noise: noise_builder().unwrap().generate_keypair().unwrap(),
            counter: 0,
            device: None,
        }
    }

    pub(crate) fn device_key(&self) -> Vec<u8> {
        self.signing
            .verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .to_vec()
    }

    pub(crate) fn noise_public(&self) -> [u8; 32] {
        self.noise.public.as_slice().try_into().unwrap()
    }

    fn sign(&self, bytes: &[u8]) -> String {
        let sig: Signature = self.signing.sign(bytes);
        b64u(sig.to_der().as_bytes())
    }

    pub(crate) fn pair_request(
        &self,
        scanned: &Scanned,
        binding: &[u8; 32],
        name: &str,
    ) -> Vec<u8> {
        let t = pair_transcript(
            &scanned.fp,
            &self.device_key(),
            &self.noise_public(),
            binding,
        );
        json!({
            "type": "pair_request",
            "v": 1,
            "name": name,
            "device_key": b64u(&self.device_key()),
            "noise_key": b64u(&self.noise_public()),
            "proof": b64u(&proof(&scanned.secret, &t)),
            "sig": self.sign(&t),
        })
        .to_string()
        .into_bytes()
    }

    pub(crate) fn hello(&mut self, binding: &[u8; 32]) -> Vec<u8> {
        self.counter += 1;
        self.hello_at(self.counter, binding)
    }

    pub(crate) fn hello_at(&self, counter: u64, binding: &[u8; 32]) -> Vec<u8> {
        let id = self.device.clone().unwrap();
        json!({
            "type": "hello",
            "device": id.as_str(),
            "counter": counter,
            "sig": self.sign(&hello_bytes(&id, counter, binding)),
        })
        .to_string()
        .into_bytes()
    }

    pub(crate) fn command(&mut self, body: &str) -> (u64, Vec<u8>) {
        self.counter += 1;
        (self.counter, self.command_at(self.counter, body))
    }

    pub(crate) fn command_at(&self, counter: u64, body: &str) -> Vec<u8> {
        let id = self.device.clone().unwrap();
        json!({
            "type": "command",
            "counter": counter,
            "body": body,
            "sig": self.sign(&command_bytes(&id, counter, body)),
        })
        .to_string()
        .into_bytes()
    }
}
