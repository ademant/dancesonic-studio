//! Tests of the remote control core against the test phone in `phone.rs`.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretSlice};
use serde_json::Value;
use tokio::sync::mpsc;

use super::ble::{self, Reassembler, frames};
use super::devices::{self, CounterError, DeviceId};
use super::hub::{self, Clock, Hub, HubEvent, Link};
use super::identity::{Identity, fingerprint, noise_builder};
use super::messages::{Command, Incoming, ParseError, PlayState, Transport};
use super::offer::{MAX_WRONG_PROOFS, Offer, OfferError, pair_transcript, proof};
use super::phone::{Phone, scan};
use super::{b64u, from_b64u};
use crate::db::Database;

const START_MS: i64 = 1_800_000_000_000;

#[derive(Debug)]
pub(crate) struct TestClock(AtomicI64);

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl TestClock {
    fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// What the app was asked, and how it answers.
#[derive(Default)]
pub(crate) struct App {
    pub(crate) accept: bool,
    pub(crate) commands: Vec<Command>,
    pub(crate) paired: Vec<String>,
}

pub(crate) struct Rig {
    _dir: tempfile::TempDir,
    pub(crate) db: Arc<Database>,
    pub(crate) hub: Arc<Hub>,
    pub(crate) clock: Arc<TestClock>,
    pub(crate) app: Arc<Mutex<App>>,
}

pub(crate) fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path().join("studio.db")).unwrap());
    let identity = Arc::new(Identity::generate().unwrap());
    let clock = Arc::new(TestClock(AtomicI64::new(START_MS)));
    let (events, mut received) = mpsc::channel(16);
    let hub = Hub::new(
        Arc::clone(&db),
        identity,
        "Studio\u{7}",
        clock.clone(),
        events,
    );
    let app = Arc::new(Mutex::new(App {
        accept: true,
        ..App::default()
    }));
    let seen = Arc::clone(&app);
    tokio::spawn(async move {
        while let Some(event) = received.recv().await {
            match event {
                HubEvent::PairRequest { decision, name, .. } => {
                    assert_eq!(name, "Pixel");
                    let yes = seen.lock().unwrap().accept;
                    let _ = decision.send(yes);
                }
                HubEvent::Paired { name, .. } => seen.lock().unwrap().paired.push(name),
                HubEvent::Command { command, reply, .. } => {
                    seen.lock().unwrap().commands.push(command);
                    let _ = reply.send(Ok(None));
                }
                HubEvent::Connected { .. } | HubEvent::Disconnected { .. } => {}
            }
        }
    });
    Rig {
        _dir: dir,
        db,
        hub,
        clock,
        app,
    }
}

/// A connection straight into the hub, without a transport.
struct Conn {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    binding: [u8; 32],
}

impl Conn {
    fn open(hub: &Arc<Hub>, transport: Transport, binding: u8, peer: Option<[u8; 32]>) -> Self {
        let (tx, incoming) = mpsc::channel(8);
        let (outgoing, rx) = mpsc::channel(32);
        let link = Link {
            transport,
            binding: [binding; 32],
            peer_noise: peer,
        };
        tokio::spawn(hub::run(Arc::clone(hub), link, incoming, outgoing));
        Self {
            tx,
            rx,
            binding: [binding; 32],
        }
    }

    async fn send(&self, message: Vec<u8>) {
        self.tx.send(message).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let bytes = tokio::time::timeout(Duration::from_secs(5), self.rx.recv())
            .await
            .expect("an answer")
            .expect("open");
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Whether the session ended (after draining what it sent).
    async fn closed(&mut self) -> bool {
        matches!(
            tokio::time::timeout(Duration::from_secs(5), async {
                while self.rx.recv().await.is_some() {}
            })
            .await,
            Ok(())
        )
    }
}

/// Pairs `phone` over a direct network-like connection and leaves it open.
async fn pair(rig: &Rig, phone: &mut Phone) -> Conn {
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    let mut conn = Conn::open(&rig.hub, Transport::Network, 7, None);
    conn.send(phone.pair_request(&scanned, &conn.binding, "Pixel"))
        .await;
    assert_eq!(conn.recv().await["type"], "pair_pending");
    let accepted = conn.recv().await;
    assert_eq!(accepted["type"], "pair_accepted", "{accepted}");
    phone.device = DeviceId::parse(accepted["device"].as_str().unwrap());
    conn
}

async fn hello(conn: &mut Conn, phone: &mut Phone) -> Value {
    let binding = conn.binding;
    conn.send(phone.hello(&binding)).await;
    conn.recv().await
}

#[test]
fn identity_survives_the_secret_store_and_binds_both_keys() {
    let identity = Identity::generate().unwrap();
    let again = Identity::from_secret(&identity.to_secret()).unwrap();
    assert_eq!(identity.fingerprint(), again.fingerprint());
    assert_eq!(identity.ble_service(), again.ble_service());
    assert_eq!(
        *identity.fingerprint(),
        fingerprint(identity.tls_spki(), identity.noise_public())
    );
    let uuid = identity.ble_service_text();
    assert_eq!(uuid.len(), 36);
    assert_eq!(&uuid[14..15], "4", "a version 4 UUID");
    // The certificate carries exactly the pinned key.
    let cert = identity.certificate().unwrap();
    assert!(
        cert.certificate
            .windows(identity.tls_spki().len())
            .any(|w| w == identity.tls_spki())
    );
    // Damaged or foreign secrets are refused, not half-read.
    let mut bytes = identity.to_secret().expose_secret().to_vec();
    bytes.pop();
    assert!(Identity::from_secret(&SecretSlice::from(bytes)).is_err());
    assert!(Identity::from_secret(&SecretSlice::from(vec![2u8; 200])).is_err());
    assert!(!format!("{identity:?}").contains("noise_private"));
}

#[tokio::test]
async fn the_identity_is_kept_in_the_vault_and_survives_sign_out() {
    use super::identity::{IDENTITY_ACCOUNT, load_or_create, reset};
    use crate::secrets::{SecretKind, SecretVault};
    #[derive(Debug)]
    struct NoService;
    impl crate::secrets::SecretBackend for NoService {
        async fn store(
            &self,
            _: &crate::secrets::SecretId,
            _: &SecretSlice<u8>,
        ) -> Result<(), crate::secrets::BackendError> {
            unreachable!()
        }
        async fn read(
            &self,
            _: &crate::secrets::SecretId,
        ) -> Result<Option<SecretSlice<u8>>, crate::secrets::BackendError> {
            unreachable!()
        }
        async fn delete(
            &self,
            _: &crate::secrets::SecretId,
        ) -> Result<(), crate::secrets::BackendError> {
            unreachable!()
        }
    }
    let vault = SecretVault::<NoService>::session_only("test");
    let first = load_or_create(&vault).await.unwrap();
    let again = load_or_create(&vault).await.unwrap();
    assert_eq!(first.fingerprint(), again.fingerprint(), "kept");
    // Signing out of a server deletes that server's secrets only.
    vault.delete_account("https://music.example").await.unwrap();
    assert_eq!(
        load_or_create(&vault).await.unwrap().fingerprint(),
        first.fingerprint()
    );
    let fresh = reset(&vault).await.unwrap();
    assert_ne!(
        fresh.fingerprint(),
        first.fingerprint(),
        "a reset makes new keys"
    );
    assert_eq!(
        load_or_create(&vault).await.unwrap().fingerprint(),
        fresh.fingerprint()
    );
    // A damaged entry is replaced rather than half-read.
    let id =
        crate::secrets::SecretId::new(SecretKind::RemoteIdentityKey, IDENTITY_ACCOUNT).unwrap();
    vault
        .store(&id, &SecretSlice::from(vec![1u8; 10]))
        .await
        .unwrap();
    let replaced = load_or_create(&vault).await.unwrap();
    assert_ne!(replaced.fingerprint(), fresh.fingerprint());
}

#[test]
fn an_offer_is_used_once_expires_and_counts_wrong_proofs() {
    let identity = Identity::generate().unwrap();
    let mut offer = Offer::with_secret(vec![9u8; 16].into(), 1000);
    let uri = offer.uri(&identity, "Ballroom & Co");
    let scanned = scan(uri.expose_secret());
    assert_eq!(scanned.fp, *identity.fingerprint());
    assert_eq!(scanned.exp, 1300);
    assert_eq!(scanned.ble, identity.ble_service_text());
    assert_eq!(scanned.name, "Ballroom & Co");
    assert!(!format!("{offer:?}").contains("secret"));

    let t = b"transcript";
    let good = proof(&scanned.secret, t);
    assert_eq!(offer.check(t, &[0; 32], 1001), Err(OfferError::BadProof));
    assert_eq!(offer.check(t, &good, 1300), Err(OfferError::Expired));
    assert_eq!(offer.check(t, &good, 1001), Ok(()));
    assert_eq!(
        offer.check(t, &good, 1002),
        Err(OfferError::Used),
        "single use"
    );

    let mut offer = Offer::with_secret(vec![9u8; 16].into(), 1000);
    for _ in 0..MAX_WRONG_PROOFS {
        assert_eq!(offer.check(t, &[1; 32], 1001), Err(OfferError::BadProof));
    }
    assert_eq!(offer.check(t, &good, 1001), Err(OfferError::Used), "void");
}

#[test]
fn messages_are_parsed_strictly() {
    let max = Transport::Network.max_message();
    let hello = br#"{"type":"hello","device":"00","counter":1,"sig":"x"}"#;
    assert!(Incoming::parse(hello, max).is_ok());
    let extra = br#"{"type":"hello","device":"00","counter":1,"sig":"x","admin":true}"#;
    assert_eq!(Incoming::parse(extra, max), Err(ParseError::BadMessage));
    assert_eq!(
        Incoming::parse(br#"{"type":"teleport"}"#, max),
        Err(ParseError::UnknownType)
    );
    assert_eq!(Incoming::parse(hello, 10), Err(ParseError::BadMessage));
    assert_eq!(Incoming::parse(b"[1,2]", max), Err(ParseError::BadMessage));
    let negative = br#"{"type":"command","counter":-1,"body":"{}","sig":"x"}"#;
    assert_eq!(Incoming::parse(negative, max), Err(ParseError::BadMessage));

    assert_eq!(Command::parse(r#"{"cmd":"play"}"#), Ok(Command::Play));
    assert_eq!(
        Command::parse(r#"{"cmd":"set_bpm","bpm":120}"#),
        Ok(Command::SetBpm { bpm: 120.0 })
    );
    for bad in [
        r#"{"cmd":"set_rate","rate":3.0}"#,
        r#"{"cmd":"set_bpm","bpm":900}"#,
        r#"{"cmd":"play","now":true}"#,
        r#"{"cmd":"library_songs","album":"a","artist":"b"}"#,
        r#"{"cmd":"library_songs"}"#,
        r#"{"cmd":"cover","id":""}"#,
        r#"{"cmd":"self_destruct"}"#,
    ] {
        assert_eq!(Command::parse(bad), Err(ParseError::BadMessage), "{bad}");
    }
    let many: Vec<String> = (0..1001).map(|i| format!("so-{i}")).collect();
    let body = serde_json::json!({"cmd": "enqueue", "songs": many}).to_string();
    assert_eq!(Command::parse(&body), Err(ParseError::BadMessage));
}

#[test]
fn counters_only_go_up_and_revoked_phones_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("studio.db")).unwrap();
    let id = DeviceId::random().unwrap();
    devices::add(&db, &id, "Pixel", &[4; 65], &[1; 32], 1).unwrap();
    let net = Transport::Network;
    devices::advance(&db, &id, 5, net, 2).unwrap();
    assert!(matches!(
        devices::advance(&db, &id, 5, Transport::Ble, 3),
        Err(CounterError::Replay)
    ));
    assert!(matches!(
        devices::advance(&db, &id, 4, net, 3),
        Err(CounterError::Replay)
    ));
    devices::advance(&db, &id, 6, Transport::Ble, 4).unwrap();
    let stored = devices::get(&db.reader().unwrap(), &id).unwrap().unwrap();
    assert_eq!(stored.last_counter, 6);
    assert_eq!(stored.last_transport.as_deref(), Some("ble"));
    let other = DeviceId::random().unwrap();
    assert!(matches!(
        devices::advance(&db, &other, 9, net, 5),
        Err(CounterError::NotPaired)
    ));
    devices::revoke(&db, &id, 6).unwrap();
    assert!(matches!(
        devices::advance(&db, &id, 7, net, 7),
        Err(CounterError::Revoked)
    ));
    // Pairing again with the same key replaces the revoked entry.
    let new = DeviceId::random().unwrap();
    devices::add(&db, &new, "Pixel", &[4; 65], &[1; 32], 8).unwrap();
    let all = devices::list(&db.reader().unwrap()).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, new);
    assert!(DeviceId::parse("../../etc").is_none());
}

#[tokio::test]
async fn a_phone_pairs_says_hello_and_controls_playback() {
    let rig = rig();
    let mut phone = Phone::new();
    let mut conn = pair(&rig, &mut phone).await;
    assert_eq!(rig.app.lock().unwrap().paired, ["Pixel"]);

    let welcome = hello(&mut conn, &mut phone).await;
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["name"], "Studio", "control characters removed");
    assert_eq!(welcome["transport"], "network");
    assert_eq!(conn.recv().await["type"], "state");

    let (n, play) = phone.command(r#"{"cmd":"play"}"#);
    conn.send(play).await;
    let result = conn.recv().await;
    assert_eq!(result["id"], n);
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(rig.app.lock().unwrap().commands, [Command::Play]);

    // State changes reach the phone.
    rig.hub.publish_state(PlayState {
        playing: true,
        song: Some("so-1".into()),
        rate: 1.0,
        ..PlayState::default()
    });
    let state = conn.recv().await;
    assert_eq!(state["type"], "state");
    assert_eq!(state["song"], "so-1");

    // get_state is answered by the hub itself.
    let (_, get) = phone.command(r#"{"cmd":"get_state"}"#);
    conn.send(get).await;
    assert_eq!(conn.recv().await["data"]["song"], "so-1");
}

#[tokio::test]
async fn replayed_forged_and_out_of_range_commands_are_refused() {
    let rig = rig();
    let mut phone = Phone::new();
    let mut conn = pair(&rig, &mut phone).await;
    hello(&mut conn, &mut phone).await;
    conn.recv().await; // state

    let (n, play) = phone.command(r#"{"cmd":"pause"}"#);
    conn.send(play.clone()).await;
    assert_eq!(conn.recv().await["ok"], true);
    // The same message again.
    conn.send(play).await;
    assert_eq!(conn.recv().await["code"], "replay");
    // A lower counter, correctly signed.
    conn.send(phone.command_at(n - 1, r#"{"cmd":"pause"}"#))
        .await;
    assert_eq!(conn.recv().await["code"], "replay");
    // Signed by another key.
    let mut thief = Phone::new();
    thief.device = phone.device.clone();
    conn.send(thief.command_at(n + 10, r#"{"cmd":"play"}"#))
        .await;
    assert_eq!(conn.recv().await["code"], "bad_signature");
    // A forged message does not move the counter.
    let (_, ok) = phone.command(r#"{"cmd":"stop"}"#);
    conn.send(ok).await;
    assert_eq!(conn.recv().await["ok"], true);
    // A valid signature over a body out of range.
    let (_, fast) = phone.command(r#"{"cmd":"set_rate","rate":9}"#);
    conn.send(fast).await;
    let result = conn.recv().await;
    assert_eq!(
        (result["ok"].clone(), result["error"].clone()),
        (false.into(), "bad_message".into())
    );
    assert_eq!(
        rig.app.lock().unwrap().commands,
        [Command::Pause, Command::Stop]
    );
}

#[tokio::test]
async fn the_counter_carries_over_restarts_and_transports() {
    let rig = rig();
    let mut phone = Phone::new();
    let mut conn = pair(&rig, &mut phone).await;
    phone.counter = 40;
    hello(&mut conn, &mut phone).await;
    drop(conn);
    // A new Bluetooth session must continue above 41.
    let mut ble = Conn::open(&rig.hub, Transport::Ble, 9, Some(phone.noise_public()));
    let binding = ble.binding;
    ble.send(phone.hello_at(41, &binding)).await;
    assert_eq!(ble.recv().await["code"], "replay");
    ble.send(phone.hello_at(42, &binding)).await;
    assert_eq!(ble.recv().await["transport"], "ble");
}

#[tokio::test]
async fn a_hello_is_bound_to_its_connection() {
    let rig = rig();
    let mut phone = Phone::new();
    let conn = pair(&rig, &mut phone).await;
    drop(conn);
    // Signed for binding 7, sent on a connection with binding 8.
    let mut other = Conn::open(&rig.hub, Transport::Network, 8, None);
    other.send(phone.hello_at(1, &[7; 32])).await;
    assert_eq!(other.recv().await["code"], "bad_signature");
}

#[tokio::test]
async fn revoking_ends_open_sessions_and_refuses_new_ones() {
    let rig = rig();
    let mut phone = Phone::new();
    let mut conn = pair(&rig, &mut phone).await;
    hello(&mut conn, &mut phone).await;
    conn.recv().await; // state
    rig.hub.revoke(phone.device.as_ref().unwrap()).unwrap();
    let bye = conn.recv().await;
    assert_eq!(
        (bye["type"].clone(), bye["reason"].clone()),
        ("bye".into(), "revoked".into())
    );
    assert!(conn.closed().await);

    let mut again = Conn::open(&rig.hub, Transport::Network, 3, None);
    let welcome = hello(&mut again, &mut phone).await;
    assert_eq!(welcome["type"], "bye");
    assert!(again.closed().await);
}

#[tokio::test]
async fn pairing_needs_the_shown_secret_and_the_users_yes() {
    let rig = rig();
    let phone = Phone::new();
    // No code shown.
    let mut conn = Conn::open(&rig.hub, Transport::Network, 1, None);
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    rig.hub.cancel_offer();
    conn.send(phone.pair_request(&scanned, &conn.binding, "Pixel"))
        .await;
    assert_eq!(conn.recv().await["code"], "expired");

    // A guessed secret, then the right one after the code is void.
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    let mut guessed = scan(uri.expose_secret());
    guessed.secret = vec![0; 16];
    for _ in 0..MAX_WRONG_PROOFS {
        rig.clock.advance(1000);
        conn.send(phone.pair_request(&guessed, &conn.binding, "Pixel"))
            .await;
        assert_eq!(conn.recv().await["code"], "bad_proof");
    }
    rig.clock.advance(1000);
    conn.send(phone.pair_request(&scanned, &conn.binding, "Pixel"))
        .await;
    assert_eq!(conn.recv().await["code"], "secret_used");

    // Too fast.
    conn.send(phone.pair_request(&scanned, &conn.binding, "Pixel"))
        .await;
    assert_eq!(conn.recv().await["code"], "rate_limited");

    // Expired.
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    rig.clock.advance(5 * 60 * 1000);
    conn.send(phone.pair_request(&scanned, &conn.binding, "Pixel"))
        .await;
    assert_eq!(conn.recv().await["code"], "expired");

    // A proof made for another connection.
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    rig.clock.advance(1000);
    conn.send(phone.pair_request(&scanned, &[2; 32], "Pixel"))
        .await;
    assert_eq!(conn.recv().await["code"], "bad_signature");

    // The user says no.
    rig.app.lock().unwrap().accept = false;
    rig.clock.advance(1000);
    conn.send(phone.pair_request(&scanned, &conn.binding, "Pixel"))
        .await;
    assert_eq!(conn.recv().await["type"], "pair_pending");
    assert_eq!(conn.recv().await["type"], "pair_rejected");
    assert!(conn.closed().await);
    assert!(devices::list(&rig.db.reader().unwrap()).unwrap().is_empty());
}

#[tokio::test]
async fn bluetooth_offers_no_library_and_checks_the_noise_key() {
    let rig = rig();
    let mut phone = Phone::new();
    drop(pair(&rig, &mut phone).await);
    // A paired phone on Bluetooth with another Noise key is refused.
    let mut wrong = Conn::open(&rig.hub, Transport::Ble, 4, Some([3; 32]));
    let w = hello(&mut wrong, &mut phone).await;
    assert_eq!(w["code"], "not_paired");

    let mut ble = Conn::open(&rig.hub, Transport::Ble, 5, Some(phone.noise_public()));
    let welcome = hello(&mut ble, &mut phone).await;
    assert!(
        !welcome["features"]
            .as_array()
            .unwrap()
            .contains(&"library".into())
    );
    ble.recv().await; // state
    let (_, artists) = phone.command(r#"{"cmd":"library_artists"}"#);
    ble.send(artists).await;
    assert_eq!(ble.recv().await["error"], "not_on_transport");
}

#[tokio::test]
async fn commands_are_rate_limited_per_phone() {
    let rig = rig();
    let mut phone = Phone::new();
    let mut conn = pair(&rig, &mut phone).await;
    hello(&mut conn, &mut phone).await;
    conn.recv().await; // state
    let mut limited = 0;
    for _ in 0..hub::MAX_COMMANDS_PER_SECOND + 5 {
        let (_, c) = phone.command(r#"{"cmd":"toggle"}"#);
        conn.send(c).await;
        if conn.recv().await["error"] == "rate_limited" {
            limited += 1;
        }
    }
    assert_eq!(limited, 5);
    rig.clock.advance(1000);
    let (_, c) = phone.command(r#"{"cmd":"toggle"}"#);
    conn.send(c).await;
    assert_eq!(conn.recv().await["ok"], true);
}

#[tokio::test]
async fn connections_are_limited() {
    let rig = rig();
    let slots: Vec<_> = (0..hub::MAX_CONNECTIONS)
        .map(|_| rig.hub.admit().unwrap())
        .collect();
    assert!(rig.hub.admit().is_none());
    drop(slots);
    assert!(rig.hub.admit().is_some());
}

#[test]
fn frames_split_and_join_messages() {
    let message: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    let parts = frames(&message, ble::MIN_MTU);
    assert!(parts.iter().all(|f| f.len() <= ble::MIN_MTU - 3));
    let mut joined = Reassembler::default();
    let mut out = None;
    for (i, f) in parts.iter().enumerate() {
        out = joined.push(f).unwrap();
        assert_eq!(out.is_some(), i == parts.len() - 1);
    }
    assert_eq!(out.unwrap(), message);
    assert_eq!(frames(&[], 100), [vec![1u8]]);
    assert!(
        Reassembler::default().push(&[0x80, 1]).is_err(),
        "unknown header bits"
    );
    assert!(Reassembler::default().push(&[]).is_err());
    let mut big = Reassembler::default();
    let chunk = vec![0u8; 1000];
    let mut failed = false;
    for _ in 0..20 {
        let mut f = vec![0];
        f.extend_from_slice(&chunk);
        if big.push(&f).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "a message larger than the limit is refused");
}

/// The phone side of the Bluetooth link, over frame channels.
struct BlePhone {
    to_studio: mpsc::Sender<Vec<u8>>,
    from_studio: mpsc::Receiver<Vec<u8>>,
    joined: Reassembler,
    noise: snow::TransportState,
    binding: [u8; 32],
    studio_static: [u8; 32],
}

const MTU: usize = 64;

async fn next(rx: &mut mpsc::Receiver<Vec<u8>>, joined: &mut Reassembler) -> Vec<u8> {
    loop {
        let f = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        if let Some(m) = joined.push(&f).unwrap() {
            return m;
        }
    }
}

async fn ble_connect(rig: &Rig, phone: &Phone) -> BlePhone {
    let (to_studio, frames_in) = mpsc::channel(64);
    let (frames_out, mut from_studio) = mpsc::channel(64);
    tokio::spawn(ble::serve_link(
        Arc::clone(&rig.hub),
        MTU,
        frames_in,
        frames_out,
    ));
    let service = *rig.hub.identity().ble_service();
    let mut hs = noise_builder()
        .unwrap()
        .local_private_key(&phone.noise.private)
        .unwrap()
        .prologue(&ble::prologue(&service))
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut buf = vec![0u8; 70_000];
    let mut joined = Reassembler::default();
    let n = hs.write_message(&[], &mut buf).unwrap();
    for f in frames(&buf[..n], MTU) {
        to_studio.send(f).await.unwrap();
    }
    let m2 = next(&mut from_studio, &mut joined).await;
    hs.read_message(&m2, &mut buf).unwrap();
    let n = hs.write_message(&[], &mut buf).unwrap();
    for f in frames(&buf[..n], MTU) {
        to_studio.send(f).await.unwrap();
    }
    let studio_static: [u8; 32] = hs.get_remote_static().unwrap().try_into().unwrap();
    let binding: [u8; 32] = hs.get_handshake_hash().try_into().unwrap();
    BlePhone {
        to_studio,
        from_studio,
        joined,
        noise: hs.into_transport_mode().unwrap(),
        binding,
        studio_static,
    }
}

impl BlePhone {
    async fn send(&mut self, message: &[u8]) {
        let mut buf = vec![0u8; 70_000];
        let n = self.noise.write_message(message, &mut buf).unwrap();
        for f in frames(&buf[..n], MTU) {
            self.to_studio.send(f).await.unwrap();
        }
    }

    async fn recv(&mut self) -> Value {
        let sealed = next(&mut self.from_studio, &mut self.joined).await;
        let mut buf = vec![0u8; 70_000];
        let n = self.noise.read_message(&sealed, &mut buf).unwrap();
        serde_json::from_slice(&buf[..n]).unwrap()
    }
}

#[tokio::test]
async fn a_phone_pairs_and_plays_over_bluetooth_with_noise() {
    let rig = rig();
    let mut phone = Phone::new();
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());

    let mut link = ble_connect(&rig, &phone).await;
    // The phone checks Studio's fingerprint before anything else.
    let identity = link.recv().await;
    assert_eq!(identity["type"], "identity");
    let spki = from_b64u(identity["tls_spki"].as_str().unwrap(), 4096).unwrap();
    assert_eq!(fingerprint(&spki, &link.studio_static), scanned.fp);

    let binding = link.binding;
    link.send(&phone.pair_request(&scanned, &binding, "Pixel"))
        .await;
    assert_eq!(link.recv().await["type"], "pair_pending");
    let accepted = link.recv().await;
    phone.device = DeviceId::parse(accepted["device"].as_str().unwrap());
    link.send(&phone.hello(&binding)).await;
    let welcome = link.recv().await;
    assert_eq!(welcome["transport"], "ble");
    assert_eq!(link.recv().await["type"], "state");

    // The network hint arrives inside the session.
    rig.hub.set_network_hint(Some(hub::NetworkHint {
        addresses: vec!["192.168.1.20".into()],
        port: 47800,
    }));
    let hint = link.recv().await;
    assert_eq!(hint["type"], "network_hint");
    assert_eq!(hint["port"], 47800);

    let (n, next_song) = phone.command(r#"{"cmd":"next"}"#);
    link.send(&next_song).await;
    let result = link.recv().await;
    assert_eq!(
        (result["id"].clone(), result["ok"].clone()),
        (n.into(), true.into())
    );
    assert_eq!(rig.app.lock().unwrap().commands, [Command::Next]);
}

#[tokio::test]
async fn a_pair_request_over_bluetooth_must_register_the_noise_key_in_use() {
    let rig = rig();
    let phone = Phone::new();
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    // The Noise session runs with one key, the request names another.
    let other = Phone::new();
    let mut link = ble_connect(&rig, &other).await;
    link.recv().await; // identity
    let binding = link.binding;
    link.send(&phone.pair_request(&scanned, &binding, "Pixel"))
        .await;
    assert_eq!(link.recv().await["code"], "bad_message");
}

#[test]
fn the_pair_transcript_is_what_the_document_says() {
    let t = pair_transcript(&[1; 32], &[2; 65], &[3; 32], &[4; 32]);
    let mut expected = b"dancesonic-pair-v1\0".to_vec();
    expected.extend([1; 32]);
    expected.extend([2; 65]);
    expected.extend([3; 32]);
    expected.extend([4; 32]);
    assert_eq!(t, expected);
    assert_eq!(b64u(&[0xfb, 0xff]), "-_8");
    assert!(from_b64u("AAAA", 2).is_none(), "longer than allowed");
    assert!(from_b64u("A+A=", 8).is_none(), "not base64url");
}
