//! Remote control through folksonic's relay (#38, ademant/folksonic#159).
//!
//! While the relay is on and Studio is signed in, Studio publishes a public
//! record on folksonic (its keys and Bluetooth service) and keeps one
//! WebSocket to `GET /api/relay/connect`. folksonic forwards binary frames
//! between devices of the same account and sees only ciphertext: every phone
//! runs its own `Noise_XX_25519_ChaChaPoly_BLAKE2s` session with Studio, as on
//! Bluetooth, and every command is still signed and counted by [`hub`].
//!
//! Layers, outside in:
//!
//! 1. **WebSocket** ([`RelayRemote`]): one connection, reconnected with
//!    backoff. Binary messages are relay frames, text messages are notes
//!    from folksonic (`undelivered`).
//! 2. **Relay frame** ([`encode_frame`], [`decode_frame`]):
//!    `[1-byte id length][device id][payload]`, the id being the recipient
//!    when sending and the sender when receiving.
//! 3. **Payload**: `[kind][Noise message]`: kind 1 is the first handshake
//!    message (it starts a session, replacing one the phone may have), kind 3
//!    the later handshake messages, kind 2 a transport message.
//! 4. **Plaintext** of a transport message: `[flags][chunk]`, bit 0 of
//!    `flags` = last chunk of a protocol message. Chunks keep every relay
//!    frame under folksonic's 16 KiB limit, so library answers and covers fit.
//!
//! [`serve`] multiplexes the phones over one relay connection and is driven
//! over channels, so tests run it without a server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls_pki_types::ServerName;
use secrecy::ExposeSecret;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};

use super::hub::{self, Hub, Link};
use super::identity::{Identity, noise_builder};
use super::messages::{Outgoing, Transport};
use crate::http::{CONNECT_TIMEOUT, HttpSettings, pinned_certificate_ders};
use crate::subsonic::{ApiError, SubsonicClient, credential, map_transport, read_limited_to};

/// Version byte at the start of Studio's public record.
pub const RECORD_VERSION: u8 = 1;
/// folksonic's limit for one relay message, header included.
pub const MAX_RELAY_MESSAGE: usize = 16 * 1024;
/// Longest device id folksonic accepts.
pub const MAX_DEVICE_ID: usize = 64;
/// Bytes of protocol message per chunk. With the chunk flags, the Noise tag,
/// the payload kind and the longest frame header, a frame stays below
/// [`MAX_RELAY_MESSAGE`].
pub const CHUNK: usize = 15 * 1024;
/// Largest protocol message Studio sends over the relay: library answers and
/// covers are big, but nothing is near this.
pub const MAX_OUTGOING: usize = 1024 * 1024;
/// Most phones with a session or a handshake on one relay connection.
pub const MAX_PEERS: usize = 8;
/// A session with no message from the phone for this long ends. The phone
/// sends `get_state` at least every 30 seconds while it stays connected.
pub const PEER_IDLE: Duration = Duration::from_secs(90);
/// How long each handshake step may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A ping this often keeps the relay connection open: folksonic drops a
/// device after 60 seconds of silence.
const PING_EVERY: Duration = Duration::from_secs(25);
/// Frames sent per second, below folksonic's 30 (burst 60).
const SEND_RATE: u32 = 25;
/// Waits between reconnection attempts, growing to the last one.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// Largest answer read from the record endpoints.
const MAX_RECORD_ANSWER: usize = 16 * 1024;

/// The phone's first handshake message: starts a session.
const KIND_START: u8 = 1;
/// A transport message.
const KIND_TRANSPORT: u8 = 2;
/// Any later handshake message, in either direction.
const KIND_HANDSHAKE: u8 = 3;
const FIN: u8 = 0x01;
const PROLOGUE_LABEL: &[u8] = b"dancesonic-relay-v1\0";
const DEVICE_ID_LABEL: &[u8] = b"dancesonic-relay-id-v1\0";

/// Why the relay could not be used. Carries no key, URL or frame content.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("the server does not offer the folksonic relay")]
    NotOffered,
    #[error("the server refused the relay: {0}")]
    Server(#[from] ApiError),
    #[error("the relay connection failed: {0}")]
    Connection(String),
    #[error("a relay frame is malformed or too large")]
    Framing,
    #[error("the Noise session failed: {0}")]
    Noise(String),
    #[error("the phone took too long")]
    Timeout,
    #[error("all remote control connections are in use")]
    Busy,
    #[error("the phone left")]
    Closed,
}

impl From<snow::Error> for RelayError {
    fn from(e: snow::Error) -> Self {
        RelayError::Noise(e.to_string())
    }
}

// ---- Record and ids ----

/// Studio's device id on the relay: derived from its fingerprint, so it is
/// stable for one identity and changes when the user resets it. Letters,
/// digits, `-` and `_` only, as folksonic requires.
pub fn studio_device_id(identity: &Identity) -> String {
    let hash = Sha256::new()
        .chain_update(DEVICE_ID_LABEL)
        .chain_update(identity.fingerprint())
        .finalize();
    format!("studio-{}", super::b64u(&hash[..12]))
}

/// The public key material of Studio's record, as the `publicKey` field:
/// `b64u(version || noise_public (32) || ble_service (16) || tls_spki)`.
/// Only public values; the phone computes the fingerprint from it.
pub fn studio_record(identity: &Identity) -> String {
    let mut bytes = Vec::with_capacity(1 + 32 + 16 + identity.tls_spki().len());
    bytes.push(RECORD_VERSION);
    bytes.extend_from_slice(identity.noise_public());
    bytes.extend_from_slice(identity.ble_service());
    bytes.extend_from_slice(identity.tls_spki());
    super::b64u(&bytes)
}

/// A Studio record read back, as the phone reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudioRecord {
    pub noise_public: [u8; 32],
    pub ble_service: [u8; 16],
    pub tls_spki: Vec<u8>,
}

impl StudioRecord {
    /// Parses `publicKey`; `None` for another version or a malformed value.
    pub fn parse(text: &str) -> Option<Self> {
        // folksonic stores at most 1024 characters.
        let bytes = super::from_b64u(text, 768)?;
        let (&version, rest) = bytes.split_first()?;
        if version != RECORD_VERSION || rest.len() <= 48 {
            return None;
        }
        Some(Self {
            noise_public: rest[..32].try_into().ok()?,
            ble_service: rest[32..48].try_into().ok()?,
            tls_spki: rest[48..].to_vec(),
        })
    }

    /// The fingerprint the record claims; the Noise handshake and the
    /// comparison code show whether the Studio behind it really holds it.
    pub fn fingerprint(&self) -> [u8; 32] {
        super::identity::fingerprint(&self.tls_spki, &self.noise_public)
    }
}

/// The Noise prologue on the relay: binds the session to the Studio the
/// phone meant to reach.
pub fn prologue(fingerprint: &[u8; 32]) -> Vec<u8> {
    let mut p = PROLOGUE_LABEL.to_vec();
    p.extend_from_slice(fingerprint);
    p
}

/// Whether `id` is a device id folksonic accepts.
pub fn valid_device_id(id: &str) -> bool {
    (1..=MAX_DEVICE_ID).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

// ---- Relay frames ----

/// `[len][id][payload]`.
pub fn encode_frame(id: &str, payload: &[u8]) -> Result<Vec<u8>, RelayError> {
    if !valid_device_id(id) || 1 + id.len() + payload.len() > MAX_RELAY_MESSAGE {
        return Err(RelayError::Framing);
    }
    let mut out = Vec::with_capacity(1 + id.len() + payload.len());
    // valid_device_id bounds the length to 64.
    out.push(id.len() as u8);
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Splits a frame into the device id and the payload.
pub fn decode_frame(frame: &[u8]) -> Result<(String, &[u8]), RelayError> {
    if frame.len() > MAX_RELAY_MESSAGE {
        return Err(RelayError::Framing);
    }
    let (&len, rest) = frame.split_first().ok_or(RelayError::Framing)?;
    let len = usize::from(len);
    if rest.len() < len {
        return Err(RelayError::Framing);
    }
    let (id, payload) = rest.split_at(len);
    let id = std::str::from_utf8(id).map_err(|_| RelayError::Framing)?;
    if !valid_device_id(id) || payload.is_empty() {
        return Err(RelayError::Framing);
    }
    Ok((id.to_owned(), payload))
}

/// Splits a protocol message into chunk plaintexts `[flags][chunk]`.
pub fn chunks(message: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = message
        .chunks(CHUNK)
        .map(|c| {
            let mut p = Vec::with_capacity(1 + c.len());
            p.push(0);
            p.extend_from_slice(c);
            p
        })
        .collect();
    match out.last_mut() {
        Some(last) => last[0] = FIN,
        None => out.push(vec![FIN]),
    }
    out
}

/// Joins chunk plaintexts back into protocol messages of at most `max` bytes.
#[derive(Debug)]
pub struct Joiner {
    buf: Vec<u8>,
    max: usize,
}

impl Joiner {
    pub fn new(max: usize) -> Self {
        Self {
            buf: Vec::new(),
            max,
        }
    }

    /// Adds a chunk plaintext; returns the message when it is complete.
    pub fn push(&mut self, plain: &[u8]) -> Result<Option<Vec<u8>>, RelayError> {
        let (&flags, chunk) = plain.split_first().ok_or(RelayError::Framing)?;
        if flags & !FIN != 0 || self.buf.len() + chunk.len() > self.max {
            return Err(RelayError::Framing);
        }
        self.buf.extend_from_slice(chunk);
        Ok((flags & FIN != 0).then(|| std::mem::take(&mut self.buf)))
    }
}

/// What a payload carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The phone's first handshake message.
    Start,
    /// A later handshake message.
    Handshake,
    /// A transport message.
    Transport,
}

impl Kind {
    fn byte(self) -> u8 {
        match self {
            Kind::Start => KIND_START,
            Kind::Handshake => KIND_HANDSHAKE,
            Kind::Transport => KIND_TRANSPORT,
        }
    }
}

/// A payload with its kind byte.
pub fn payload(kind: Kind, noise_message: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(1 + noise_message.len());
    p.push(kind.byte());
    p.extend_from_slice(noise_message);
    p
}

// ---- Multiplexing phones over one relay connection ----

/// What arrives from the relay connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayIn {
    /// A frame from device `from`.
    Frame { from: String, payload: Vec<u8> },
    /// folksonic could not deliver a frame to `to`: the phone is gone.
    Undelivered { to: String },
}

/// A frame to send to device `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOut {
    pub to: String,
    pub payload: Vec<u8>,
}

/// A phone's session task. Dropping `tx` closes the session's input, so it
/// ends the normal way and the hub reports the disconnection.
struct Peer {
    generation: u64,
    tx: mpsc::Sender<Vec<u8>>,
}

/// Serves every phone that reaches Studio over one relay connection, until
/// `incoming` closes. Each phone gets its own Noise session and hub session;
/// a phone that starts a new handshake replaces its old session.
pub async fn serve(
    hub: Arc<Hub>,
    mut incoming: mpsc::Receiver<RelayIn>,
    outgoing: mpsc::Sender<RelayOut>,
) {
    let mut peers: HashMap<String, Peer> = HashMap::new();
    let mut generation = 0u64;
    let (done_tx, mut done) = mpsc::channel::<(String, u64)>(MAX_PEERS);
    loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break };
                match message {
                    RelayIn::Undelivered { to } => {
                        peers.remove(&to);
                    }
                    RelayIn::Frame { from, payload } => {
                        let Some(&kind) = payload.first() else { continue };
                        if kind == KIND_START {
                            peers.remove(&from);
                            if peers.len() >= MAX_PEERS {
                                tracing::debug!("too many phones on the relay; handshake ignored");
                                continue;
                            }
                            generation += 1;
                            let (tx, rx) = mpsc::channel(16);
                            tokio::spawn(peer_task(
                                Arc::clone(&hub),
                                from.clone(),
                                generation,
                                rx,
                                outgoing.clone(),
                                done_tx.clone(),
                            ));
                            peers.insert(from.clone(), Peer { generation, tx });
                        }
                        let Some(peer) = peers.get(&from) else {
                            tracing::debug!("relay frame from a phone without a session dropped");
                            continue;
                        };
                        // A phone that does not keep up loses its session
                        // rather than holding up the others.
                        if peer.tx.try_send(payload).is_err() {
                            peers.remove(&from);
                        }
                    }
                }
            }
            ended = done.recv() => {
                let Some((id, ended_generation)) = ended else { break };
                if peers.get(&id).is_some_and(|p| p.generation == ended_generation) {
                    peers.remove(&id);
                }
            }
        }
    }
    // Dropping `peers` closes every session's input.
}

async fn peer_task(
    hub: Arc<Hub>,
    id: String,
    generation: u64,
    incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<RelayOut>,
    done: mpsc::Sender<(String, u64)>,
) {
    if let Err(error) = serve_peer(hub, &id, incoming, &outgoing).await {
        tracing::debug!(%error, "relay session ended");
    }
    // The multiplexer may be gone already.
    let _ = done.send((id, generation)).await;
}

async fn recv_payload(
    incoming: &mut mpsc::Receiver<Vec<u8>>,
    timeout: Duration,
) -> Result<Vec<u8>, RelayError> {
    tokio::time::timeout(timeout, incoming.recv())
        .await
        .map_err(|_| RelayError::Timeout)?
        .ok_or(RelayError::Closed)
}

fn expect_kind(payload: &[u8], kind: u8) -> Result<&[u8], RelayError> {
    match payload.split_first() {
        Some((&k, rest)) if k == kind => Ok(rest),
        _ => Err(RelayError::Framing),
    }
}

async fn send_out(
    outgoing: &mpsc::Sender<RelayOut>,
    to: &str,
    payload: Vec<u8>,
) -> Result<(), RelayError> {
    outgoing
        .send(RelayOut {
            to: to.to_owned(),
            payload,
        })
        .await
        .map_err(|_| RelayError::Closed)
}

/// Encrypts and sends one protocol message in chunks.
async fn send_message(
    noise: &mut snow::TransportState,
    outgoing: &mpsc::Sender<RelayOut>,
    to: &str,
    message: &[u8],
    buf: &mut [u8],
) -> Result<(), RelayError> {
    for plain in chunks(message) {
        let n = noise.write_message(&plain, buf)?;
        send_out(outgoing, to, payload(Kind::Transport, &buf[..n])).await?;
    }
    Ok(())
}

/// One phone: the Noise XX handshake as responder, Studio's `identity`, then
/// the hub session.
async fn serve_peer(
    hub: Arc<Hub>,
    id: &str,
    mut incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: &mpsc::Sender<RelayOut>,
) -> Result<(), RelayError> {
    let _slot = hub.admit().ok_or(RelayError::Busy)?;
    let identity = Arc::clone(hub.identity());
    let prologue = prologue(identity.fingerprint());
    let mut handshake = noise_builder()
        .map_err(|e| RelayError::Noise(e.to_string()))?
        .local_private_key(identity.noise_private().expose_secret())?
        .prologue(&prologue)?
        .build_responder()?;
    let mut buf = vec![0u8; MAX_RELAY_MESSAGE + 1024];

    // -> e
    let m1 = recv_payload(&mut incoming, HANDSHAKE_TIMEOUT).await?;
    handshake.read_message(expect_kind(&m1, KIND_START)?, &mut buf)?;
    // <- e, ee, s, es
    let n = handshake.write_message(&[], &mut buf)?;
    send_out(outgoing, id, payload(Kind::Handshake, &buf[..n])).await?;
    // -> s, se
    let m3 = recv_payload(&mut incoming, HANDSHAKE_TIMEOUT).await?;
    handshake.read_message(expect_kind(&m3, KIND_HANDSHAKE)?, &mut buf)?;

    let peer: [u8; 32] = handshake
        .get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| RelayError::Noise("no static key".into()))?;
    let binding: [u8; 32] = handshake
        .get_handshake_hash()
        .try_into()
        .map_err(|_| RelayError::Noise("handshake hash size".into()))?;
    let mut noise = handshake.into_transport_mode()?;

    let identity_message = Outgoing::Identity {
        tls_spki: super::b64u(identity.tls_spki()),
    }
    .to_json();
    send_message(&mut noise, outgoing, id, &identity_message, &mut buf).await?;

    let (to_hub, hub_in) = mpsc::channel(8);
    let (hub_out, mut from_hub) = mpsc::channel(32);
    let link = Link {
        transport: Transport::Relay,
        binding,
        peer_noise: Some(peer),
    };
    let session = hub::run(hub, link, hub_in, hub_out);
    tokio::pin!(session);
    let mut to_hub = Some(to_hub);
    let mut joiner = Joiner::new(Transport::Relay.max_message());
    // Only the phone's own messages keep the session alive; Studio's state
    // updates do not.
    let idle = tokio::time::sleep(PEER_IDLE);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            () = &mut session => break,
            () = &mut idle, if to_hub.is_some() => {
                tracing::debug!("relay phone silent too long");
                to_hub = None;
            }
            received = incoming.recv(), if to_hub.is_some() => {
                let Some(received) = received else {
                    // Replaced, undelivered or the relay is gone.
                    to_hub = None;
                    continue;
                };
                idle.as_mut().reset(tokio::time::Instant::now() + PEER_IDLE);
                // Any tampered, replayed or reordered message fails here:
                // Noise counts its nonces.
                let sealed = expect_kind(&received, KIND_TRANSPORT)?;
                let n = noise.read_message(sealed, &mut buf)?;
                let Some(message) = joiner.push(&buf[..n])? else { continue };
                if let Some(tx) = &to_hub
                    && tx.send(message).await.is_err()
                {
                    break;
                }
            }
            message = from_hub.recv() => {
                let Some(message) = message else { break };
                if message.len() > MAX_OUTGOING {
                    tracing::warn!("a message too large for the relay was dropped");
                    continue;
                }
                send_message(&mut noise, outgoing, id, &message, &mut buf).await?;
            }
        }
    }
    // Send what the session wrote last (a `bye`, for example).
    while let Ok(message) = from_hub.try_recv() {
        send_message(&mut noise, outgoing, id, &message, &mut buf).await?;
    }
    Ok(())
}

// ---- folksonic: the record and the WebSocket ----

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordBody<'a> {
    kind: &'static str,
    name: &'a str,
    public_key: &'a str,
}

impl SubsonicClient {
    fn relay_url(&self, relative: &str) -> Result<url::Url, ApiError> {
        self.server()
            .endpoint(relative)
            .map_err(|_| ApiError::Invalid("the endpoint address"))
    }

    /// Publishes or updates Studio's record (`PUT /api/relay/devices/{id}`).
    pub async fn publish_relay_record(
        &self,
        identity: &Identity,
        name: &str,
    ) -> Result<(), RelayError> {
        let id = studio_device_id(identity);
        let url = self.relay_url(&format!("api/relay/devices/{id}"))?;
        let (header, value) = credential(self.auth(), "PUT", &url)?;
        let record = studio_record(identity);
        let response = self
            .http()
            .put(url)
            .header(header, value)
            .json(&RecordBody {
                kind: "studio",
                name,
                public_key: &record,
            })
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status().as_u16();
        // Read (and bound) the body so the connection can be reused.
        let _ = read_limited_to(response, MAX_RECORD_ANSWER).await;
        match status {
            200..=299 => Ok(()),
            404 | 405 => Err(RelayError::NotOffered),
            other => Err(RelayError::Server(ApiError::Http(other))),
        }
    }

    /// Removes Studio's record (`DELETE /api/relay/devices/{id}`); folksonic
    /// closes the relay connection with it. A record that is already gone is
    /// not an error.
    pub async fn withdraw_relay_record(&self, identity: &Identity) -> Result<(), RelayError> {
        let id = studio_device_id(identity);
        let url = self.relay_url(&format!("api/relay/devices/{id}"))?;
        let (header, value) = credential(self.auth(), "DELETE", &url)?;
        let response = self
            .http()
            .delete(url)
            .header(header, value)
            .send()
            .await
            .map_err(map_transport)?;
        let status = response.status().as_u16();
        let _ = read_limited_to(response, MAX_RECORD_ANSWER).await;
        match status {
            200..=299 | 404 => Ok(()),
            other => Err(RelayError::Server(ApiError::Http(other))),
        }
    }
}

/// A text note from folksonic on the relay connection.
#[derive(Deserialize)]
struct Note {
    r#type: String,
    #[serde(default)]
    to: Option<String>,
}

/// The relay transport: Studio's record on folksonic and the WebSocket that
/// serves phones through it. Dropping it stops the connection; call
/// [`RelayRemote::stop`] to also remove the record.
#[derive(Debug)]
pub struct RelayRemote {
    task: JoinHandle<()>,
    client: SubsonicClient,
    identity: Arc<Identity>,
    status: tokio::sync::watch::Receiver<RelayStatus>,
}

/// What the relay connection is doing, for the settings dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayStatus {
    Connecting,
    Connected,
    /// Waiting to reconnect after `reason`.
    Retrying {
        reason: String,
    },
}

impl Drop for RelayRemote {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RelayRemote {
    /// Publishes Studio's record and starts the relay connection. Fails at
    /// once when the server has no relay or refuses the record; connection
    /// failures later are retried with backoff.
    pub async fn start(
        hub: Arc<Hub>,
        client: SubsonicClient,
        http: HttpSettings,
    ) -> Result<Self, RelayError> {
        let identity = Arc::clone(hub.identity());
        client.publish_relay_record(&identity, hub.name()).await?;
        let (status_tx, status) = tokio::sync::watch::channel(RelayStatus::Connecting);
        let task = tokio::spawn(run(hub, client.clone(), http, status_tx));
        Ok(Self {
            task,
            client,
            identity,
            status,
        })
    }

    /// What the connection is doing now.
    pub fn status(&self) -> tokio::sync::watch::Receiver<RelayStatus> {
        self.status.clone()
    }

    /// Stops the connection and removes Studio's record from folksonic.
    pub async fn stop(self) -> Result<(), RelayError> {
        self.task.abort();
        self.client.withdraw_relay_record(&self.identity).await
    }
}

/// Keeps a relay connection open, reconnecting with backoff.
async fn run(
    hub: Arc<Hub>,
    client: SubsonicClient,
    http: HttpSettings,
    status: tokio::sync::watch::Sender<RelayStatus>,
) {
    let device = studio_device_id(hub.identity());
    let mut failures = 0usize;
    loop {
        status.send_replace(RelayStatus::Connecting);
        let reason = match connect(&client, &http, &device).await {
            Ok(socket) => {
                failures = 0;
                status.send_replace(RelayStatus::Connected);
                tracing::info!("relay connected");
                let ended = match socket {
                    Socket::Plain(ws) => pump(Arc::clone(&hub), *ws).await,
                    Socket::Tls(ws) => pump(Arc::clone(&hub), *ws).await,
                };
                match ended {
                    Ok(()) => "the server closed the relay".to_owned(),
                    Err(e) => e.to_string(),
                }
            }
            Err(e) => e.to_string(),
        };
        tracing::info!(%reason, "relay disconnected");
        let wait = BACKOFF[failures.min(BACKOFF.len() - 1)];
        failures += 1;
        status.send_replace(RelayStatus::Retrying { reason });
        tokio::time::sleep(wait).await;
    }
}

enum Socket {
    Plain(Box<WebSocketStream<tokio::net::TcpStream>>),
    Tls(Box<WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>>),
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_RELAY_MESSAGE))
        .max_frame_size(Some(MAX_RELAY_MESSAGE))
}

/// The rustls client configuration for the server: the pinned certificate
/// only when one is set, the platform's trust store otherwise; TLS 1.2+ and
/// HTTP/1.1 for the upgrade.
fn tls_client_config(http: &HttpSettings) -> Result<rustls::ClientConfig, RelayError> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| RelayError::Connection(e.to_string()))?;
    let mut config = match &http.pinned_certificate_pem {
        Some(pem) => {
            let mut roots = rustls::RootCertStore::empty();
            let ders =
                pinned_certificate_ders(pem).map_err(|e| RelayError::Connection(e.to_string()))?;
            for der in ders {
                roots
                    .add(der)
                    .map_err(|e| RelayError::Connection(e.to_string()))?;
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        None => {
            let verifier = rustls_platform_verifier::Verifier::new(provider)
                .map_err(|e| RelayError::Connection(e.to_string()))?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth()
        }
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Opens the relay WebSocket for `device`, with the credential in a header.
/// No proxy and no redirect: an answer other than 101 is an error.
async fn connect(
    client: &SubsonicClient,
    http: &HttpSettings,
    device: &str,
) -> Result<Socket, RelayError> {
    let mut url = client.relay_url("api/relay/connect")?;
    url.query_pairs_mut().append_pair("device", device);
    // The credential names the http(s) request; the socket URL only changes
    // the scheme.
    let (header, value) = credential(client.auth(), "GET", &url)?;
    let plain = url.scheme() == "http";
    let host = url
        .host_str()
        .ok_or(RelayError::Connection("no host".into()))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or(RelayError::Connection("no port".into()))?;
    let mut ws_url = url.clone();
    ws_url
        .set_scheme(if plain { "ws" } else { "wss" })
        .map_err(|()| RelayError::Connection("scheme".into()))?;
    let mut request = ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| RelayError::Connection(e.to_string()))?;
    let mut value =
        HeaderValue::from_str(&value).map_err(|_| RelayError::Connection("credential".into()))?;
    value.set_sensitive(true);
    request.headers_mut().insert(header, value);
    request.headers_mut().insert(
        "User-Agent",
        HeaderValue::from_static(concat!("DanceSonicStudio/", env!("CARGO_PKG_VERSION"))),
    );

    let host_for_tcp = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let tcp = tokio::time::timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host_for_tcp.as_str(), port)),
    )
    .await
    .map_err(|_| RelayError::Connection("timed out".into()))?
    .map_err(|e| RelayError::Connection(e.kind().to_string()))?;

    let handshake = |e: tungstenite::Error| match e {
        tungstenite::Error::Http(response) => match response.status().as_u16() {
            404 => RelayError::NotOffered,
            code => RelayError::Server(ApiError::Http(code)),
        },
        other => RelayError::Connection(other.to_string()),
    };
    if plain {
        let (ws, _) = tokio_tungstenite::client_async_with_config(request, tcp, Some(ws_config()))
            .await
            .map_err(handshake)?;
        return Ok(Socket::Plain(Box::new(ws)));
    }
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_client_config(http)?));
    let name = ServerName::try_from(host_for_tcp)
        .map_err(|_| RelayError::Connection("server name".into()))?;
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp))
        .await
        .map_err(|_| RelayError::Connection("timed out".into()))?
        .map_err(|e| RelayError::Connection(e.to_string()))?;
    let (ws, _) = tokio_tungstenite::client_async_with_config(request, tls, Some(ws_config()))
        .await
        .map_err(handshake)?;
    Ok(Socket::Tls(Box::new(ws)))
}

/// Moves frames between the WebSocket and [`serve`] until either side ends.
pub async fn pump<S>(hub: Arc<Hub>, ws: WebSocketStream<S>) -> Result<(), RelayError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut stream) = ws.split();
    let (to_serve, serve_in) = mpsc::channel(64);
    let (serve_out, mut from_serve) = mpsc::channel::<RelayOut>(64);
    let (stopped_tx, mut stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve(hub, serve_in, serve_out).await;
        let _ = stopped_tx.send(());
    });
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut pacer = Pacer::new(SEND_RATE);
    let result = loop {
        tokio::select! {
            message = stream.next() => {
                let message = match message {
                    None => break Ok(()),
                    Some(Err(e)) => break Err(RelayError::Connection(e.to_string())),
                    Some(Ok(m)) => m,
                };
                let input = match message {
                    Message::Binary(frame) => match decode_frame(&frame) {
                        Ok((from, payload)) => RelayIn::Frame { from, payload: payload.to_vec() },
                        Err(_) => {
                            tracing::debug!("malformed relay frame dropped");
                            continue;
                        }
                    },
                    Message::Text(text) => match serde_json::from_str::<Note>(text.as_str()) {
                        Ok(Note { r#type, to: Some(to) }) if r#type == "undelivered" && valid_device_id(&to) => {
                            RelayIn::Undelivered { to }
                        }
                        _ => continue,
                    },
                    Message::Close(_) => break Ok(()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
                };
                if to_serve.send(input).await.is_err() {
                    break Ok(());
                }
            }
            out = from_serve.recv() => {
                let Some(out) = out else { break Ok(()) };
                let Ok(frame) = encode_frame(&out.to, &out.payload) else {
                    tracing::warn!("a relay frame too large was dropped");
                    continue;
                };
                pacer.wait().await;
                if let Err(e) = sink.send(Message::Binary(frame.into())).await {
                    break Err(RelayError::Connection(e.to_string()));
                }
            }
            _ = ping.tick() => {
                if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await {
                    break Err(RelayError::Connection(e.to_string()));
                }
            }
            _ = &mut stopped => break Ok(()),
        }
    };
    server.abort();
    result
}

/// Spaces sends to at most `rate` a second, with a burst of one second.
struct Pacer {
    every: Duration,
    next: tokio::time::Instant,
    burst: Duration,
}

impl Pacer {
    fn new(rate: u32) -> Self {
        let every = Duration::from_secs(1) / rate.max(1);
        Self {
            every,
            next: tokio::time::Instant::now(),
            burst: Duration::from_secs(1),
        }
    }

    async fn wait(&mut self) {
        let now = tokio::time::Instant::now();
        // Unused time builds up to one second of burst, no more.
        let earliest = now.checked_sub(self.burst).unwrap_or(now);
        if self.next < earliest {
            self.next = earliest;
        }
        if self.next > now {
            tokio::time::sleep_until(self.next).await;
        }
        self.next += self.every;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes of Noise overhead on a transport message (the tag).
    const NOISE_TAG: usize = 16;

    #[test]
    fn frames_round_trip_and_bad_ones_are_refused() {
        let frame = encode_frame("phone-1", b"\x02hello").unwrap();
        assert_eq!(frame[0], 7);
        let (id, payload) = decode_frame(&frame).unwrap();
        assert_eq!((id.as_str(), payload), ("phone-1", &b"\x02hello"[..]));

        assert!(encode_frame("", b"x").is_err());
        assert!(encode_frame("bad id", b"x").is_err());
        assert!(encode_frame(&"a".repeat(65), b"x").is_err());
        assert!(
            encode_frame("p", &vec![0; MAX_RELAY_MESSAGE]).is_err(),
            "too large"
        );
        for bad in [
            &b""[..],
            &[5, b'a', b'b'][..],    // shorter than its length
            &[1, b' ', 2][..],       // invalid id
            &[2, 0xff, 0xfe, 1][..], // not UTF-8
            &[1, b'a'][..],          // no payload
        ] {
            assert!(decode_frame(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_largest_chunk_fits_a_relay_frame_with_the_longest_id() {
        let plain = chunks(&vec![7u8; CHUNK]);
        assert_eq!(plain.len(), 1);
        let sealed = plain[0].len() + NOISE_TAG;
        let frame = encode_frame(
            &"a".repeat(MAX_DEVICE_ID),
            &payload(Kind::Transport, &vec![0; sealed]),
        );
        assert!(frame.is_ok());
    }

    #[test]
    fn chunks_join_back_and_a_join_is_bounded() {
        let message: Vec<u8> = (0..(CHUNK * 2 + 5)).map(|i| i as u8).collect();
        let parts = chunks(&message);
        assert_eq!(parts.len(), 3);
        let mut joiner = Joiner::new(message.len());
        assert_eq!(joiner.push(&parts[0]).unwrap(), None);
        assert_eq!(joiner.push(&parts[1]).unwrap(), None);
        assert_eq!(joiner.push(&parts[2]).unwrap(), Some(message.clone()));
        assert_eq!(chunks(b""), [vec![FIN]]);

        let mut small = Joiner::new(CHUNK);
        assert!(small.push(&parts[0]).is_ok());
        assert!(small.push(&parts[1]).is_err(), "over the limit");
        assert!(Joiner::new(10).push(&[0x80, 1]).is_err(), "unknown flag");
        assert!(Joiner::new(10).push(&[]).is_err());
    }

    #[test]
    fn the_record_holds_only_public_keys_and_gives_the_fingerprint() {
        let identity = Identity::generate().unwrap();
        let text = studio_record(&identity);
        assert!(text.len() <= 1024, "folksonic's limit");
        let record = StudioRecord::parse(&text).unwrap();
        assert_eq!(&record.noise_public, identity.noise_public());
        assert_eq!(&record.ble_service, identity.ble_service());
        assert_eq!(record.tls_spki, identity.tls_spki());
        assert_eq!(&record.fingerprint(), identity.fingerprint());
        let secret = identity.noise_private();
        assert!(
            !text.contains(&crate::remote::b64u(secret.expose_secret())),
            "no private key"
        );

        let mut wrong_version = crate::remote::from_b64u(&text, 768).unwrap();
        wrong_version[0] = 2;
        assert!(StudioRecord::parse(&crate::remote::b64u(&wrong_version)).is_none());
        assert!(StudioRecord::parse("AQ").is_none(), "too short");
        assert!(StudioRecord::parse("not base64!").is_none());
    }

    #[test]
    fn the_device_id_is_stable_valid_and_tied_to_the_identity() {
        let identity = Identity::generate().unwrap();
        let id = studio_device_id(&identity);
        assert!(valid_device_id(&id), "{id}");
        assert_eq!(id, studio_device_id(&identity));
        assert_ne!(id, studio_device_id(&Identity::generate().unwrap()));
    }

    use crate::connection::{HttpPolicy, ServerUrl};
    use crate::subsonic::{ApiKey, Auth};

    fn http_settings() -> HttpSettings {
        HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        }
    }

    fn client_for(base: &str) -> SubsonicClient {
        let url = ServerUrl::parse(base, HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let http = crate::http::client(&http_settings()).unwrap();
        SubsonicClient::new(http, url, Auth::ApiKey(ApiKey::new("tsk_relay").unwrap()))
    }

    #[tokio::test]
    async fn the_record_is_published_and_withdrawn_with_the_key_in_a_header() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let identity = Identity::generate().unwrap();
        let device_path = format!("/api/relay/devices/{}", studio_device_id(&identity));
        Mock::given(method("PUT"))
            .and(path(device_path.as_str()))
            .and(header("X-Api-Key", "tsk_relay"))
            .and(body_partial_json(serde_json::json!({
                "kind": "studio",
                "name": "Studio on desk",
                "publicKey": studio_record(&identity),
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(device_path.as_str()))
            .and(header("X-Api-Key", "tsk_relay"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let client = client_for(&server.uri());
        client
            .publish_relay_record(&identity, "Studio on desk")
            .await
            .unwrap();
        // Already gone is fine.
        client.withdraw_relay_record(&identity).await.unwrap();
        for request in server.received_requests().await.unwrap() {
            assert!(
                !request.url.as_str().contains("tsk_relay"),
                "no key in the URL"
            );
        }
    }

    #[tokio::test]
    async fn a_server_without_the_relay_is_reported() {
        use wiremock::{MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let identity = Identity::generate().unwrap();
        assert!(matches!(
            client_for(&server.uri())
                .publish_relay_record(&identity, "Studio")
                .await,
            Err(RelayError::NotOffered)
        ));
    }

    /// The relay connection against a local WebSocket server standing in for
    /// folksonic: the credential travels in a header only, notes and bad
    /// frames are survived, and a phone's first handshake message is
    /// answered to that phone.
    #[tokio::test]
    // The handshake callback's error type is tungstenite's, and large.
    #[allow(clippy::result_large_err)]
    async fn the_websocket_carries_frames_with_the_key_in_a_header() {
        use std::sync::Mutex;
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        let rig = crate::remote::tests::rig();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // The request's URI and its X-Api-Key header.
        type Seen = Arc<Mutex<Option<(String, Option<String>)>>>;
        let seen: Seen = Arc::default();
        let record = studio_record(rig.hub.identity());
        let fp = StudioRecord::parse(&record).unwrap().fingerprint();

        let server_seen = Arc::clone(&seen);
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_hdr_async(tcp, |req: &Request, res: Response| {
                    let key = req
                        .headers()
                        .get("X-Api-Key")
                        .map(|v| v.to_str().unwrap().to_owned());
                    *server_seen.lock().unwrap() = Some((req.uri().to_string(), key));
                    Ok(res)
                })
                .await
                .unwrap();
            // A note, a malformed frame, then a phone's first handshake message.
            ws.send(Message::Text(
                r#"{"type":"undelivered","to":"phone-gone"}"#.into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Binary(vec![9u8, 1].into())).await.unwrap();
            let mut hs = noise_builder()
                .unwrap()
                .local_private_key(&noise_builder().unwrap().generate_keypair().unwrap().private)
                .unwrap()
                .prologue(&prologue(&fp))
                .unwrap()
                .build_initiator()
                .unwrap();
            let mut buf = vec![0u8; 1024];
            let n = hs.write_message(&[], &mut buf).unwrap();
            let frame = encode_frame("phone-x", &payload(Kind::Start, &buf[..n])).unwrap();
            ws.send(Message::Binary(frame.into())).await.unwrap();
            // Studio answers the phone with its handshake message.
            loop {
                match ws.next().await.unwrap().unwrap() {
                    Message::Binary(frame) => {
                        let (to, payload) = decode_frame(&frame).unwrap();
                        assert_eq!(to, "phone-x");
                        assert_eq!(payload[0], KIND_HANDSHAKE);
                        hs.read_message(&payload[1..], &mut buf).unwrap();
                        break;
                    }
                    _ => continue,
                }
            }
            ws.close(None).await.unwrap();
        });

        let client = client_for(&base);
        let device = studio_device_id(rig.hub.identity());
        let socket = connect(&client, &http_settings(), &device).await.unwrap();
        let Socket::Plain(ws) = socket else {
            panic!("plain ws to a private address with the opt-in");
        };
        tokio::time::timeout(Duration::from_secs(10), pump(Arc::clone(&rig.hub), *ws))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();

        let (uri, key) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(uri, format!("/api/relay/connect?device={device}"));
        assert_eq!(key.as_deref(), Some("tsk_relay"));
    }

    #[test]
    fn a_pinned_certificate_replaces_the_trust_store() {
        let identity = Identity::generate().unwrap();
        let cert = identity.certificate().unwrap();
        let pem = format!(
            "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &cert.certificate
            )
        );
        let pinned = HttpSettings {
            policy: HttpPolicy::HTTPS_ONLY,
            pinned_certificate_pem: Some(pem.into_bytes()),
        };
        assert!(tls_client_config(&pinned).is_ok());
        let bad = HttpSettings {
            policy: HttpPolicy::HTTPS_ONLY,
            pinned_certificate_pem: Some(b"not a certificate".to_vec()),
        };
        assert!(
            tls_client_config(&bad).is_err(),
            "a bad pin is never no pin"
        );
    }

    #[tokio::test]
    async fn the_pacer_keeps_below_the_rate() {
        let mut pacer = Pacer::new(100);
        let start = tokio::time::Instant::now();
        for _ in 0..30 {
            pacer.wait().await;
        }
        // 30 sends at 100 a second take at least 290 ms.
        assert!(
            start.elapsed() >= Duration::from_millis(285),
            "{:?}",
            start.elapsed()
        );
    }
}
