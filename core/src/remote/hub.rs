//! What every remote control connection does, whatever its transport.
//!
//! A transport (the network server or the Bluetooth GATT server) accepts a
//! connection, establishes its channel binding, and hands the decrypted
//! message bytes to [`run`]. The hub checks pairing proofs, signatures,
//! counters and limits, and passes accepted commands and pairing requests to
//! the app as [`HubEvent`]s. The app answers through the `oneshot` senders.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use secrecy::SecretString;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::devices::{self, CounterError, Device, DeviceId};
use super::identity::Identity;
use super::messages::{Command, Incoming, MAX_COUNTER, Outgoing, PlayState, Transport};
use super::offer::{self, Offer, OfferError};
use crate::db::Database;

/// Most connections at once, over both transports.
pub const MAX_CONNECTIONS: usize = 4;
/// Most commands per phone per second.
pub const MAX_COMMANDS_PER_SECOND: u32 = 20;
/// How long the user has to accept a phone.
pub const DECISION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the app has to carry out a command.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

const HELLO_LABEL: &[u8] = b"dancesonic-hello-v1\0";
const COMMAND_LABEL: &[u8] = b"dancesonic-cmd-v1\0";
const MAX_NAME_CHARS: usize = 64;

/// The time, so tests can move it.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// The system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::sync::now_ms()
    }
}

/// What the app answers to a command: optional data, or a wire error code.
pub type Reply = Result<Option<serde_json::Value>, &'static str>;

/// What the hub asks of, or tells, the app.
#[derive(Debug)]
pub enum HubEvent {
    /// A phone with a valid proof asks to pair; the user decides.
    PairRequest {
        name: String,
        transport: Transport,
        /// The comparison code to show next to the question, when the phone
        /// pairs through folksonic (#38); `None` for a QR pairing.
        comparison: Option<String>,
        decision: oneshot::Sender<bool>,
    },
    /// The user accepted a phone and it was stored.
    Paired { device: DeviceId, name: String },
    /// An authenticated command for the player or the library.
    Command {
        device: DeviceId,
        command: Command,
        reply: oneshot::Sender<Reply>,
    },
    /// A paired phone opened a session.
    Connected {
        device: DeviceId,
        name: String,
        transport: Transport,
    },
    /// A session ended.
    Disconnected {
        device: DeviceId,
        transport: Transport,
    },
}

/// Studio's addresses and port, for the Bluetooth network hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkHint {
    pub addresses: Vec<String>,
    pub port: u16,
}

/// The shared state of all connections.
pub struct Hub {
    db: Arc<Database>,
    identity: Arc<Identity>,
    name: String,
    clock: Arc<dyn Clock>,
    offer: Mutex<Option<Offer>>,
    events: mpsc::Sender<HubEvent>,
    state: watch::Sender<PlayState>,
    hint: watch::Sender<Option<NetworkHint>>,
    revoked: broadcast::Sender<DeviceId>,
    connections: AtomicUsize,
    rates: Mutex<HashMap<DeviceId, (i64, u32)>>,
    /// A pairing by comparison code is waiting for the user. One at a time,
    /// so another device of the account cannot pile up dialogs.
    code_pairing: AtomicBool,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("identity", &self.identity)
            .field("connections", &self.connections)
            .finish_non_exhaustive()
    }
}

impl Hub {
    /// A hub for `identity`, named `name` towards phones. Events go to
    /// `events`.
    pub fn new(
        db: Arc<Database>,
        identity: Arc<Identity>,
        name: &str,
        clock: Arc<dyn Clock>,
        events: mpsc::Sender<HubEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            db,
            identity,
            name: clean_name(name).unwrap_or_else(|| "DanceSonic Studio".to_owned()),
            clock,
            offer: Mutex::new(None),
            events,
            state: watch::channel(PlayState::default()).0,
            hint: watch::channel(None).0,
            revoked: broadcast::channel(16).0,
            connections: AtomicUsize::new(0),
            rates: Mutex::new(HashMap::new()),
            code_pairing: AtomicBool::new(false),
        })
    }

    /// Studio's identity.
    pub fn identity(&self) -> &Arc<Identity> {
        &self.identity
    }

    /// Studio's name towards phones.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Shows a new pairing code, replacing any earlier one, and returns the
    /// QR text and when it expires (Unix s). The text holds the secret.
    pub fn new_offer(&self) -> Result<(SecretString, i64), getrandom::Error> {
        let offer = Offer::new(self.clock.now_ms() / 1000)?;
        let uri = offer.uri(&self.identity, &self.name);
        let expires = offer.expires_at();
        *self.offer.lock().unwrap_or_else(PoisonError::into_inner) = Some(offer);
        Ok((uri, expires))
    }

    /// Withdraws the pairing code.
    pub fn cancel_offer(&self) {
        *self.offer.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Publishes a new playback state to every session.
    pub fn publish_state(&self, state: PlayState) {
        self.state.send_replace(state);
    }

    /// Sets the addresses the Bluetooth sessions announce.
    pub fn set_network_hint(&self, hint: Option<NetworkHint>) {
        self.hint.send_replace(hint);
    }

    /// Revokes a phone and ends its open sessions.
    pub fn revoke(&self, id: &DeviceId) -> Result<bool, crate::db::DbError> {
        let done = devices::revoke(&self.db, id, self.clock.now_ms())?;
        // No receiver just means no session is open.
        let _ = self.revoked.send(id.clone());
        Ok(done)
    }

    /// Forgets a phone and ends its open sessions.
    pub fn remove(&self, id: &DeviceId) -> Result<bool, crate::db::DbError> {
        let done = devices::remove(&self.db, id)?;
        // No receiver just means no session is open.
        let _ = self.revoked.send(id.clone());
        Ok(done)
    }

    /// Takes a connection slot, or `None` when all are in use.
    pub fn admit(self: &Arc<Self>) -> Option<Slot> {
        let taken = self
            .connections
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_CONNECTIONS).then_some(n + 1)
            })
            .is_ok();
        taken.then(|| Slot(Arc::clone(self)))
    }

    fn within_rate(&self, id: &DeviceId) -> bool {
        let second = self.clock.now_ms() / 1000;
        let mut rates = self.rates.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = rates.entry(id.clone()).or_insert((second, 0));
        if entry.0 != second {
            *entry = (second, 0);
        }
        entry.1 += 1;
        entry.1 <= MAX_COMMANDS_PER_SECOND
    }

    fn reader(&self) -> Option<rusqlite::Connection> {
        self.db.reader().ok()
    }
}

/// A connection slot; freed on drop.
#[derive(Debug)]
pub struct Slot(Arc<Hub>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Marks a pairing by comparison code as waiting; cleared on drop.
struct CodePairing(Arc<Hub>);

impl Drop for CodePairing {
    fn drop(&mut self) {
        self.0.code_pairing.store(false, Ordering::Release);
    }
}

/// What the transport established about a connection.
#[derive(Debug, Clone)]
pub struct Link {
    pub transport: Transport,
    /// The channel binding: the TLS exporter or the Noise handshake hash.
    pub binding: [u8; 32],
    /// The phone's Noise static key (Bluetooth and the relay).
    pub peer_noise: Option<[u8; 32]>,
}

enum Phase {
    Fresh,
    Ready(Device),
}

/// Whether the session goes on after a message.
enum Flow {
    Continue,
    Close,
}

/// Runs one session: reads messages from `incoming`, writes answers and
/// events to `outgoing`, and returns when either side closes or the session
/// must end. The transport closes the connection when this returns.
pub async fn run(
    hub: Arc<Hub>,
    link: Link,
    mut incoming: mpsc::Receiver<Vec<u8>>,
    outgoing: mpsc::Sender<Vec<u8>>,
) {
    let mut session = Session {
        hub: Arc::clone(&hub),
        link,
        out: outgoing,
        phase: Phase::Fresh,
        last_pair_ms: None,
    };
    let mut state = hub.state.subscribe();
    let mut hint = hub.hint.subscribe();
    let mut revoked = hub.revoked.subscribe();
    loop {
        let ready = matches!(session.phase, Phase::Ready(_));
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break };
                if let Flow::Close = session.handle(&message).await {
                    break;
                }
            }
            changed = state.changed(), if ready => {
                if changed.is_err() { break }
                let current = state.borrow_and_update().clone();
                if !session.send(Outgoing::State(current)).await { break }
            }
            changed = hint.changed(), if ready && session.link.transport == Transport::Ble => {
                if changed.is_err() { break }
                let current = hint.borrow_and_update().clone();
                if let Some(h) = current
                    && !session.send(Outgoing::NetworkHint { addresses: h.addresses, port: h.port }).await
                {
                    break;
                }
            }
            gone = revoked.recv(), if ready => {
                match gone {
                    Ok(id) if session.is_device(&id) => {
                        session.send(Outgoing::Bye { reason: "revoked".into() }).await;
                        break;
                    }
                    Ok(_) => {}
                    // Missed notices: check this phone in the database.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if !session.still_paired() {
                            session.send(Outgoing::Bye { reason: "revoked".into() }).await;
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    if let Phase::Ready(device) = &session.phase {
        // The app may have gone away on shutdown; nothing to tell then.
        let _ = hub
            .events
            .send(HubEvent::Disconnected {
                device: device.id.clone(),
                transport: session.link.transport,
            })
            .await;
    }
}

struct Session {
    hub: Arc<Hub>,
    link: Link,
    out: mpsc::Sender<Vec<u8>>,
    phase: Phase,
    last_pair_ms: Option<i64>,
}

impl Session {
    async fn send(&self, message: Outgoing) -> bool {
        self.out.send(message.to_json()).await.is_ok()
    }

    /// Sends an error; the session goes on unless the peer is gone.
    async fn refuse(&self, code: &str) -> Flow {
        if self.send(Outgoing::error(code)).await {
            Flow::Continue
        } else {
            Flow::Close
        }
    }

    fn is_device(&self, id: &DeviceId) -> bool {
        matches!(&self.phase, Phase::Ready(d) if &d.id == id)
    }

    fn still_paired(&self) -> bool {
        let Phase::Ready(device) = &self.phase else {
            return false;
        };
        self.hub
            .reader()
            .and_then(|conn| devices::get(&conn, &device.id).ok().flatten())
            .is_some_and(|d| !d.is_revoked())
    }

    async fn handle(&mut self, bytes: &[u8]) -> Flow {
        let message = match Incoming::parse(bytes, self.link.transport.max_message()) {
            Ok(m) => m,
            Err(e) => return self.refuse(e.code()).await,
        };
        match message {
            Incoming::PairRequest {
                v,
                name,
                device_key,
                noise_key,
                proof,
                sig,
            } => {
                if matches!(self.phase, Phase::Ready(_)) {
                    return self.refuse("bad_message").await;
                }
                self.pair(v, &name, &device_key, &noise_key, &proof, &sig)
                    .await
            }
            Incoming::PairCodeRequest {
                v,
                name,
                device_key,
                noise_key,
                sig,
            } => {
                if matches!(self.phase, Phase::Ready(_)) {
                    return self.refuse("bad_message").await;
                }
                self.pair_code(v, &name, &device_key, &noise_key, &sig)
                    .await
            }
            Incoming::Hello {
                device,
                counter,
                sig,
            } => {
                if matches!(self.phase, Phase::Ready(_)) {
                    return self.refuse("bad_message").await;
                }
                self.hello(&device, counter, &sig).await
            }
            Incoming::Command { counter, body, sig } => match &self.phase {
                Phase::Fresh => self.refuse("not_paired").await,
                Phase::Ready(device) => {
                    let device = device.clone();
                    self.command(&device, counter, &body, &sig).await
                }
            },
        }
    }

    async fn pair(
        &mut self,
        v: u8,
        name: &str,
        device_key: &str,
        noise_key: &str,
        proof: &str,
        sig: &str,
    ) -> Flow {
        let now = self.hub.clock.now_ms();
        if self.last_pair_ms.is_some_and(|last| now - last < 1000) {
            return self.refuse("rate_limited").await;
        }
        self.last_pair_ms = Some(now);
        let parsed = parse_pairing(v, name, device_key, noise_key, sig).and_then(|p| {
            let proof = super::from_b64u(proof, 32).filter(|p| p.len() == 32)?;
            Some((p, proof))
        });
        let Some((
            Pairing {
                name,
                device_key,
                noise_key,
                sig,
            },
            proof,
        )) = parsed
        else {
            return self.refuse("bad_message").await;
        };
        // On Bluetooth the phone registers the key it is talking with.
        if self.link.peer_noise.is_some_and(|peer| peer != noise_key) {
            return self.refuse("bad_message").await;
        }
        let transcript = offer::pair_transcript(
            self.hub.identity.fingerprint(),
            &device_key,
            &noise_key,
            &self.link.binding,
        );
        if !verify(&device_key, &transcript, &sig) {
            return self.refuse("bad_signature").await;
        }
        let checked = {
            let mut offer = self
                .hub
                .offer
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match offer.as_mut() {
                Some(o) => o.check(&transcript, &proof, now / 1000),
                None => Err(OfferError::NoOffer),
            }
        };
        if let Err(e) = checked {
            tracing::info!(reason = %e, "pairing request refused");
            return self.refuse(e.code()).await;
        }
        self.decide_and_store(name, &device_key, &noise_key, None, now)
            .await
    }

    /// Pairing through folksonic (#38): only on the relay, where the Noise
    /// session already proves the phone holds `noise_key`. No QR secret: the
    /// user allows the phone in Studio after comparing the code on both
    /// screens.
    async fn pair_code(
        &mut self,
        v: u8,
        name: &str,
        device_key: &str,
        noise_key: &str,
        sig: &str,
    ) -> Flow {
        if self.link.transport != Transport::Relay {
            return self.refuse("not_on_transport").await;
        }
        let now = self.hub.clock.now_ms();
        if self.last_pair_ms.is_some_and(|last| now - last < 1000) {
            return self.refuse("rate_limited").await;
        }
        self.last_pair_ms = Some(now);
        let Some(Pairing {
            name,
            device_key,
            noise_key,
            sig,
        }) = parse_pairing(v, name, device_key, noise_key, sig)
        else {
            return self.refuse("bad_message").await;
        };
        // The relay link always knows the peer's Noise key; it must be the
        // one the phone registers.
        if self.link.peer_noise != Some(noise_key) {
            return self.refuse("bad_message").await;
        }
        let transcript = offer::pair_code_transcript(
            self.hub.identity.fingerprint(),
            &device_key,
            &noise_key,
            &self.link.binding,
        );
        if !verify(&device_key, &transcript, &sig) {
            return self.refuse("bad_signature").await;
        }
        if self
            .hub
            .code_pairing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return self.refuse("rate_limited").await;
        }
        // Released however this ends, also when the session is dropped while
        // the user still decides.
        let _waiting = CodePairing(Arc::clone(&self.hub));
        let comparison = offer::comparison_code(&self.link.binding);
        self.decide_and_store(name, &device_key, &noise_key, Some(comparison), now)
            .await
    }

    /// Asks the user about a phone whose request checked out, and stores it
    /// when they accept.
    async fn decide_and_store(
        &mut self,
        name: String,
        device_key: &[u8],
        noise_key: &[u8; 32],
        comparison: Option<String>,
        now: i64,
    ) -> Flow {
        if !self.send(Outgoing::PairPending).await {
            return Flow::Close;
        }
        let (decision, decided) = oneshot::channel();
        let asked = self
            .hub
            .events
            .send(HubEvent::PairRequest {
                name: name.clone(),
                transport: self.link.transport,
                comparison,
                decision,
            })
            .await
            .is_ok();
        let accepted = asked
            && matches!(
                tokio::time::timeout(DECISION_TIMEOUT, decided).await,
                Ok(Ok(true))
            );
        if !accepted {
            tracing::info!("pairing rejected");
            self.send(Outgoing::PairRejected {
                reason: "rejected".into(),
            })
            .await;
            return Flow::Close;
        }
        let stored = DeviceId::random().ok().and_then(|id| {
            devices::add(&self.hub.db, &id, &name, device_key, noise_key, now)
                .ok()
                .map(|()| id)
        });
        let Some(id) = stored else {
            tracing::warn!("cannot store a paired phone");
            return self.refuse("failed").await;
        };
        tracing::info!("phone paired");
        let _ = self
            .hub
            .events
            .send(HubEvent::Paired {
                device: id.clone(),
                name,
            })
            .await;
        if self
            .send(Outgoing::PairAccepted {
                device: id.as_str().to_owned(),
            })
            .await
        {
            Flow::Continue
        } else {
            Flow::Close
        }
    }

    async fn hello(&mut self, device: &str, counter: u64, sig: &str) -> Flow {
        let (Some(id), Some(sig)) = (DeviceId::parse(device), super::from_b64u(sig, 80)) else {
            return self.refuse("bad_message").await;
        };
        let found = self
            .hub
            .reader()
            .map(|conn| devices::get(&conn, &id).ok().flatten());
        let Some(found) = found else {
            return self.refuse("failed").await;
        };
        let Some(device) = found else {
            self.refuse("not_paired").await;
            return Flow::Close;
        };
        if device.is_revoked() {
            self.send(Outgoing::Bye {
                reason: "revoked".into(),
            })
            .await;
            return Flow::Close;
        }
        // On Bluetooth the phone must use the key registered at pairing.
        if self
            .link
            .peer_noise
            .is_some_and(|peer| peer != device.noise_key)
        {
            self.refuse("not_paired").await;
            return Flow::Close;
        }
        if counter > MAX_COUNTER {
            return self.refuse("bad_message").await;
        }
        let signed = hello_bytes(&id, counter, &self.link.binding);
        if !verify(&device.device_key, &signed, &sig) {
            return self.refuse("bad_signature").await;
        }
        match devices::advance(
            &self.hub.db,
            &id,
            counter,
            self.link.transport,
            self.hub.clock.now_ms(),
        ) {
            Ok(()) => {}
            Err(CounterError::Revoked) => {
                self.send(Outgoing::Bye {
                    reason: "revoked".into(),
                })
                .await;
                return Flow::Close;
            }
            Err(e) => return self.refuse(e.code()).await,
        }
        let welcome = Outgoing::Welcome {
            protocol: super::PROTOCOL_VERSION,
            name: self.hub.name.clone(),
            transport: self.link.transport,
            features: self
                .link
                .transport
                .features()
                .iter()
                .map(|f| (*f).to_owned())
                .collect(),
        };
        if !self.send(welcome).await {
            return Flow::Close;
        }
        let _ = self
            .hub
            .events
            .send(HubEvent::Connected {
                device: id.clone(),
                name: device.name.clone(),
                transport: self.link.transport,
            })
            .await;
        self.phase = Phase::Ready(device);
        let state = self.hub.state.borrow().clone();
        if !self.send(Outgoing::State(state)).await {
            return Flow::Close;
        }
        if self.link.transport == Transport::Ble {
            let hint = self.hub.hint.borrow().clone();
            if let Some(h) = hint
                && !self
                    .send(Outgoing::NetworkHint {
                        addresses: h.addresses,
                        port: h.port,
                    })
                    .await
            {
                return Flow::Close;
            }
        }
        Flow::Continue
    }

    async fn command(&mut self, device: &Device, counter: u64, body: &str, sig: &str) -> Flow {
        let Some(sig) = super::from_b64u(sig, 80) else {
            return self.refuse("bad_message").await;
        };
        if counter > MAX_COUNTER {
            return self.refuse("bad_message").await;
        }
        let signed = command_bytes(&device.id, counter, body);
        if !verify(&device.device_key, &signed, &sig) {
            return self.refuse("bad_signature").await;
        }
        match devices::advance(
            &self.hub.db,
            &device.id,
            counter,
            self.link.transport,
            self.hub.clock.now_ms(),
        ) {
            Ok(()) => {}
            Err(CounterError::Revoked | CounterError::NotPaired) => {
                self.send(Outgoing::Bye {
                    reason: "revoked".into(),
                })
                .await;
                return Flow::Close;
            }
            Err(e) => return self.refuse(e.code()).await,
        }
        let reply = if !self.hub.within_rate(&device.id) {
            Err("rate_limited")
        } else {
            match Command::parse(body) {
                Err(e) => Err(e.code()),
                Ok(c) if !self.link.transport.features().contains(&c.feature()) => {
                    Err("not_on_transport")
                }
                Ok(Command::GetState) => {
                    let state = self.hub.state.borrow().clone();
                    Ok(serde_json::to_value(state).ok())
                }
                Ok(command) => self.ask_app(device, command).await,
            }
        };
        let result = match reply {
            Ok(data) => Outgoing::Result {
                id: counter,
                ok: true,
                error: None,
                data,
            },
            Err(code) => Outgoing::Result {
                id: counter,
                ok: false,
                error: Some(code.to_owned()),
                data: None,
            },
        };
        if self.send(result).await {
            Flow::Continue
        } else {
            Flow::Close
        }
    }

    async fn ask_app(&self, device: &Device, command: Command) -> Reply {
        let (reply, answer) = oneshot::channel();
        self.hub
            .events
            .send(HubEvent::Command {
                device: device.id.clone(),
                command,
                reply,
            })
            .await
            .map_err(|_| "failed")?;
        match tokio::time::timeout(COMMAND_TIMEOUT, answer).await {
            Ok(Ok(reply)) => reply,
            _ => Err("failed"),
        }
    }
}

/// The bytes a `hello` signature covers.
pub fn hello_bytes(id: &DeviceId, counter: u64, binding: &[u8; 32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HELLO_LABEL.len() + 32 + 8 + 32);
    out.extend_from_slice(HELLO_LABEL);
    out.extend_from_slice(id.as_str().as_bytes());
    out.extend_from_slice(&counter.to_be_bytes());
    out.extend_from_slice(binding);
    out
}

/// The bytes a command signature covers.
pub fn command_bytes(id: &DeviceId, counter: u64, body: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(COMMAND_LABEL.len() + 32 + 8 + body.len());
    out.extend_from_slice(COMMAND_LABEL);
    out.extend_from_slice(id.as_str().as_bytes());
    out.extend_from_slice(&counter.to_be_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

/// Checks a DER ECDSA P-256 signature.
fn verify(device_key: &[u8], message: &[u8], der: &[u8]) -> bool {
    let (Ok(key), Ok(sig)) = (
        VerifyingKey::from_sec1_bytes(device_key),
        Signature::from_der(der),
    ) else {
        return false;
    };
    // Android's Keystore may produce high-S signatures; both forms are valid.
    let sig = sig.normalize_s();
    key.verify(message, &sig).is_ok()
}

/// The fields every pairing request carries: name, device key, Noise key and
/// signature, decoded with their sizes checked. `None` for anything malformed
/// or another protocol version.
fn parse_pairing(
    v: u8,
    name: &str,
    device_key: &str,
    noise_key: &str,
    sig: &str,
) -> Option<Pairing> {
    let name = clean_name(name)?;
    let device_key = super::from_b64u(device_key, 65).filter(|k| k.len() == 65)?;
    let noise_key: [u8; 32] = super::from_b64u(noise_key, 32)?.try_into().ok()?;
    let sig = super::from_b64u(sig, 80)?;
    (v == super::PROTOCOL_VERSION).then_some(Pairing {
        name,
        device_key,
        noise_key,
        sig,
    })
}

/// The decoded fields of a pairing request.
struct Pairing {
    name: String,
    device_key: Vec<u8>,
    noise_key: [u8; 32],
    sig: Vec<u8>,
}

/// A display name without control characters, 1 to 64 characters.
fn clean_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_owned();
    (!cleaned.is_empty()).then_some(cleaned)
}
