//! The Bluetooth LE transport without BlueZ: framing of messages over GATT
//! writes and notifications, and the Noise XX session (#24). The app moves
//! frames between BlueZ and [`serve_link`]; everything else happens here.

use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret;
use tokio::sync::mpsc;

use super::hub::{self, Hub, Link};
use super::identity::noise_builder;
use super::messages::{Outgoing, Transport};

/// The phone-to-Studio characteristic (write, write without response).
pub const RX_CHARACTERISTIC: &str = "7c3b0001-5b9e-4c2c-9c6a-2d1f5e0a4b10";
/// The Studio-to-phone characteristic (notify).
pub const TX_CHARACTERISTIC: &str = "7c3b0002-5b9e-4c2c-9c6a-2d1f5e0a4b10";

/// Header bit: the last frame of a message.
const FIN: u8 = 0x01;
/// Bytes of Noise overhead on a transport message (the tag).
const NOISE_TAG: usize = 16;
/// The largest reassembled message: a protocol message plus Noise overhead.
pub const MAX_FRAMED: usize = 16 * 1024 + NOISE_TAG;
/// The smallest ATT MTU (BLE 4.0 default).
pub const MIN_MTU: usize = 23;
/// How long each handshake step may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PROLOGUE_LABEL: &[u8] = b"dancesonic-ble-v1\0";

/// Why a Bluetooth connection ended early.
#[derive(Debug, thiserror::Error)]
pub enum BleError {
    #[error("all remote control connections are in use")]
    Busy,
    #[error("a message frame is malformed or too large")]
    Framing,
    #[error("the phone closed the connection")]
    Closed,
    #[error("the handshake took too long")]
    Timeout,
    #[error("the Noise session failed: {0}")]
    Noise(String),
}

impl From<snow::Error> for BleError {
    fn from(e: snow::Error) -> Self {
        BleError::Noise(e.to_string())
    }
}

/// Splits a message into frames for an ATT MTU of `mtu`.
pub fn frames(message: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    // ATT notification header (3) and our header (1).
    let chunk = mtu.max(MIN_MTU) - 4;
    let mut out: Vec<Vec<u8>> = message
        .chunks(chunk)
        .map(|c| {
            let mut f = Vec::with_capacity(c.len() + 1);
            f.push(0);
            f.extend_from_slice(c);
            f
        })
        .collect();
    match out.last_mut() {
        Some(last) => last[0] = FIN,
        None => out.push(vec![FIN]),
    }
    out
}

/// Puts frames back together.
#[derive(Debug, Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    /// Adds a frame; returns the message when it is complete.
    pub fn push(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, BleError> {
        let (&header, chunk) = frame.split_first().ok_or(BleError::Framing)?;
        if header & !FIN != 0 || self.buf.len() + chunk.len() > MAX_FRAMED {
            return Err(BleError::Framing);
        }
        self.buf.extend_from_slice(chunk);
        Ok((header & FIN != 0).then(|| std::mem::take(&mut self.buf)))
    }
}

/// The Noise prologue for a service UUID.
pub fn prologue(service: &[u8; 16]) -> Vec<u8> {
    let mut p = PROLOGUE_LABEL.to_vec();
    p.extend_from_slice(service);
    p
}

async fn next_message(
    frames_in: &mut mpsc::Receiver<Vec<u8>>,
    reassembler: &mut Reassembler,
) -> Result<Vec<u8>, BleError> {
    loop {
        let frame = frames_in.recv().await.ok_or(BleError::Closed)?;
        if let Some(message) = reassembler.push(&frame)? {
            return Ok(message);
        }
    }
}

async fn send_message(
    frames_out: &mpsc::Sender<Vec<u8>>,
    message: &[u8],
    mtu: usize,
) -> Result<(), BleError> {
    for frame in frames(message, mtu) {
        frames_out.send(frame).await.map_err(|_| BleError::Closed)?;
    }
    Ok(())
}

/// Serves one Bluetooth connection: the Noise XX handshake as responder,
/// Studio's `identity`, and then the session of [`hub::run`]. `frames_in`
/// carries the phone's writes to the rx characteristic, `frames_out` the
/// notifications for the tx characteristic.
pub async fn serve_link(
    hub: Arc<Hub>,
    mtu: usize,
    mut frames_in: mpsc::Receiver<Vec<u8>>,
    frames_out: mpsc::Sender<Vec<u8>>,
) -> Result<(), BleError> {
    let _slot = hub.admit().ok_or(BleError::Busy)?;
    let identity = Arc::clone(hub.identity());
    let prologue = prologue(identity.ble_service());
    let mut handshake = noise_builder()
        .map_err(|e| BleError::Noise(e.to_string()))?
        .local_private_key(identity.noise_private().expose_secret())?
        .prologue(&prologue)?
        .build_responder()?;
    let mut reassembler = Reassembler::default();
    let mut buf = vec![0u8; MAX_FRAMED + 1024];

    // -> e
    let m1 = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        next_message(&mut frames_in, &mut reassembler),
    )
    .await
    .map_err(|_| BleError::Timeout)??;
    handshake.read_message(&m1, &mut buf)?;
    // <- e, ee, s, es
    let n = handshake.write_message(&[], &mut buf)?;
    send_message(&frames_out, &buf[..n], mtu).await?;
    // -> s, se
    let m3 = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        next_message(&mut frames_in, &mut reassembler),
    )
    .await
    .map_err(|_| BleError::Timeout)??;
    handshake.read_message(&m3, &mut buf)?;

    let peer: [u8; 32] = handshake
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| BleError::Noise("no static key".into()))?;
    let binding: [u8; 32] = handshake
        .get_handshake_hash()
        .try_into()
        .map_err(|_| BleError::Noise("handshake hash size".into()))?;
    let mut noise = handshake.into_transport_mode()?;

    let identity_message = Outgoing::Identity {
        tls_spki: super::b64u(identity.tls_spki()),
    }
    .to_json();
    let n = noise.write_message(&identity_message, &mut buf)?;
    send_message(&frames_out, &buf[..n], mtu).await?;

    let (to_hub, hub_in) = mpsc::channel(8);
    let (hub_out, mut from_hub) = mpsc::channel(32);
    let link = Link {
        transport: Transport::Ble,
        binding,
        peer_noise: Some(peer),
    };
    let session = hub::run(hub, link, hub_in, hub_out);
    tokio::pin!(session);
    let mut to_hub = Some(to_hub);
    loop {
        tokio::select! {
            () = &mut session => break,
            frame = frames_in.recv(), if to_hub.is_some() => {
                let Some(frame) = frame else {
                    // The phone left: let the session end.
                    to_hub = None;
                    continue;
                };
                let Some(sealed) = reassembler.push(&frame)? else { continue };
                let n = noise.read_message(&sealed, &mut buf)?;
                if let Some(tx) = &to_hub
                    && tx.send(buf[..n].to_vec()).await.is_err()
                {
                    break;
                }
            }
            message = from_hub.recv() => {
                let Some(message) = message else { break };
                if message.len() > Transport::Ble.max_message() {
                    tracing::warn!("a message too large for Bluetooth was dropped");
                    continue;
                }
                let n = noise.write_message(&message, &mut buf)?;
                send_message(&frames_out, &buf[..n], mtu).await?;
            }
        }
    }
    // Send what the session wrote last (a `bye`, for example).
    while let Ok(message) = from_hub.try_recv() {
        let n = noise.write_message(&message, &mut buf)?;
        send_message(&frames_out, &buf[..n], mtu).await?;
    }
    Ok(())
}
