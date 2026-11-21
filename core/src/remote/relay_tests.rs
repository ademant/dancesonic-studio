//! Remote control through folksonic's relay (#38), tested against a phone
//! written from `docs/remote-protocol.md` and a fake relay made of channels.

use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::sync::mpsc;

use super::devices::{self, DeviceId};
use super::hub::Hub;
use super::identity::{fingerprint, noise_builder};
use super::messages::{Command, PlayState, Transport};
use super::offer::comparison_code;
use super::phone::Phone;
use super::relay::{
    self, Joiner, Kind, RelayIn, RelayOut, StudioRecord, chunks, payload, studio_record,
};
use super::tests::{Conn, Rig, ble_connect, hello, rig};
use super::{b64u, from_b64u};

const PHONE_ID: &str = "phone-pixel";

/// A fake folksonic relay: frames go straight to [`relay::serve`].
struct FakeRelay {
    to_studio: mpsc::Sender<RelayIn>,
    from_studio: mpsc::Receiver<RelayOut>,
}

fn fake_relay(hub: &Arc<Hub>) -> FakeRelay {
    let (to_studio, incoming) = mpsc::channel(64);
    let (outgoing, from_studio) = mpsc::channel(64);
    tokio::spawn(relay::serve(Arc::clone(hub), incoming, outgoing));
    FakeRelay {
        to_studio,
        from_studio,
    }
}

impl FakeRelay {
    async fn deliver(&self, from: &str, payload: Vec<u8>) {
        self.to_studio
            .send(RelayIn::Frame {
                from: from.to_owned(),
                payload,
            })
            .await
            .unwrap();
    }

    /// The next frame Studio sends, with the phone it is for.
    async fn next(&mut self) -> RelayOut {
        tokio::time::timeout(Duration::from_secs(5), self.from_studio.recv())
            .await
            .expect("a frame")
            .expect("open")
    }

    /// Whether Studio sends nothing more for a while.
    async fn quiet(&mut self) -> bool {
        tokio::time::timeout(Duration::from_millis(300), self.from_studio.recv())
            .await
            .is_err()
    }
}

/// The phone's end of a Noise session over the relay.
struct RelayPhone {
    id: String,
    noise: snow::TransportState,
    joiner: Joiner,
    binding: [u8; 32],
    studio_static: [u8; 32],
}

/// Runs the Noise XX handshake as the phone, for the Studio `record` says.
async fn relay_connect(relay: &mut FakeRelay, phone: &Phone, id: &str, record: &str) -> RelayPhone {
    let record = StudioRecord::parse(record).unwrap();
    let mut hs = noise_builder()
        .unwrap()
        .local_private_key(&phone.noise.private)
        .unwrap()
        .prologue(&relay::prologue(&record.fingerprint()))
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut buf = vec![0u8; 70_000];
    let n = hs.write_message(&[], &mut buf).unwrap();
    relay.deliver(id, payload(Kind::Start, &buf[..n])).await;
    let m2 = relay.next().await;
    assert_eq!(m2.to, id);
    assert_eq!(m2.payload[0], 3, "a handshake message");
    hs.read_message(&m2.payload[1..], &mut buf).unwrap();
    let n = hs.write_message(&[], &mut buf).unwrap();
    relay.deliver(id, payload(Kind::Handshake, &buf[..n])).await;
    let studio_static: [u8; 32] = hs.get_remote_static().unwrap().try_into().unwrap();
    let binding: [u8; 32] = hs.get_handshake_hash().try_into().unwrap();
    RelayPhone {
        id: id.to_owned(),
        noise: hs.into_transport_mode().unwrap(),
        joiner: Joiner::new(1024 * 1024),
        binding,
        studio_static,
    }
}

impl RelayPhone {
    /// The sealed payloads of one message, without delivering them.
    fn seal(&mut self, message: &[u8]) -> Vec<Vec<u8>> {
        let mut buf = vec![0u8; 70_000];
        chunks(message)
            .iter()
            .map(|plain| {
                let n = self.noise.write_message(plain, &mut buf).unwrap();
                payload(Kind::Transport, &buf[..n])
            })
            .collect()
    }

    async fn send(&mut self, relay: &FakeRelay, message: &[u8]) {
        for p in self.seal(message) {
            relay.deliver(&self.id, p).await;
        }
    }

    async fn recv(&mut self, relay: &mut FakeRelay) -> Value {
        let mut buf = vec![0u8; 70_000];
        loop {
            let out = relay.next().await;
            assert_eq!(out.to, self.id);
            assert_eq!(out.payload[0], 2, "a transport message");
            let n = self
                .noise
                .read_message(&out.payload[1..], &mut buf)
                .unwrap();
            if let Some(message) = self.joiner.push(&buf[..n]).unwrap() {
                return serde_json::from_slice(&message).unwrap();
            }
        }
    }
}

/// Connects, checks Studio's identity against its record, and pairs by code.
async fn relay_paired(rig: &Rig, relay: &mut FakeRelay, phone: &mut Phone) -> RelayPhone {
    let record = studio_record(rig.hub.identity());
    let fp = StudioRecord::parse(&record).unwrap().fingerprint();
    let mut link = relay_connect(relay, phone, PHONE_ID, &record).await;
    assert_eq!(link.recv(relay).await["type"], "identity");
    let binding = link.binding;
    link.send(relay, &phone.pair_code_request(&fp, &binding, "Pixel"))
        .await;
    assert_eq!(link.recv(relay).await["type"], "pair_pending");
    let accepted = link.recv(relay).await;
    assert_eq!(accepted["type"], "pair_accepted", "{accepted}");
    phone.device = DeviceId::parse(accepted["device"].as_str().unwrap());
    link.send(relay, &phone.hello(&binding)).await;
    assert_eq!(link.recv(relay).await["transport"], "relay");
    assert_eq!(link.recv(relay).await["type"], "state");
    link
}

#[tokio::test]
async fn a_phone_pairs_through_folksonic_by_comparing_codes_and_plays() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let mut phone = Phone::new();
    let record = studio_record(rig.hub.identity());
    let fp = StudioRecord::parse(&record).unwrap().fingerprint();

    let mut link = relay_connect(&mut relay, &phone, PHONE_ID, &record).await;
    // The phone checks Studio's identity against the record.
    let identity = link.recv(&mut relay).await;
    let spki = from_b64u(identity["tls_spki"].as_str().unwrap(), 4096).unwrap();
    assert_eq!(fingerprint(&spki, &link.studio_static), fp);

    let binding = link.binding;
    link.send(&relay, &phone.pair_code_request(&fp, &binding, "Pixel"))
        .await;
    assert_eq!(link.recv(&mut relay).await["type"], "pair_pending");
    let accepted = link.recv(&mut relay).await;
    assert_eq!(accepted["type"], "pair_accepted");
    // Studio showed the same code the phone computes from its handshake.
    let ours = comparison_code(&binding);
    assert_eq!(ours.len(), 6);
    assert!(ours.bytes().all(|b| b.is_ascii_digit()));
    assert_eq!(rig.app.lock().unwrap().comparisons, [Some(ours)]);

    phone.device = DeviceId::parse(accepted["device"].as_str().unwrap());
    link.send(&relay, &phone.hello(&binding)).await;
    let welcome = link.recv(&mut relay).await;
    assert_eq!(welcome["transport"], "relay");
    assert!(
        welcome["features"]
            .as_array()
            .unwrap()
            .contains(&"library".into()),
        "the full feature set"
    );
    assert_eq!(link.recv(&mut relay).await["type"], "state");
    let (n, next) = phone.command(r#"{"cmd":"next"}"#);
    link.send(&relay, &next).await;
    let result = link.recv(&mut relay).await;
    assert_eq!(
        (result["id"].clone(), result["ok"].clone()),
        (n.into(), true.into())
    );
    assert_eq!(rig.app.lock().unwrap().commands, [Command::Next]);
    let stored = devices::list(&rig.db.reader().unwrap()).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].last_transport.as_deref(), Some("relay"));
}

#[tokio::test]
async fn a_rejected_phone_is_not_stored() {
    let rig = rig();
    rig.app.lock().unwrap().accept = false;
    let mut relay = fake_relay(&rig.hub);
    let phone = Phone::new();
    let record = studio_record(rig.hub.identity());
    let fp = StudioRecord::parse(&record).unwrap().fingerprint();
    let mut link = relay_connect(&mut relay, &phone, PHONE_ID, &record).await;
    link.recv(&mut relay).await; // identity
    let binding = link.binding;
    link.send(&relay, &phone.pair_code_request(&fp, &binding, "Pixel"))
        .await;
    assert_eq!(link.recv(&mut relay).await["type"], "pair_pending");
    assert_eq!(link.recv(&mut relay).await["type"], "pair_rejected");
    assert!(devices::list(&rig.db.reader().unwrap()).unwrap().is_empty());
}

/// folksonic, or anyone in its place, answers the phone with its own key and
/// talks to Studio as a phone. Both handshakes succeed, but the codes on the
/// two screens differ, so the user rejects the phone.
#[tokio::test]
async fn a_man_in_the_middle_shows_a_different_code() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let phone = Phone::new();

    // The middle publishes a record with its own keys to the phone...
    let middle_studio = super::identity::Identity::generate().unwrap();
    let fake_record = studio_record(&middle_studio);
    let fake_fp = StudioRecord::parse(&fake_record).unwrap().fingerprint();
    let mut hs = noise_builder()
        .unwrap()
        .local_private_key(&phone.noise.private)
        .unwrap()
        .prologue(&relay::prologue(&fake_fp))
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut middle = noise_builder()
        .unwrap()
        .local_private_key(middle_studio.noise_private().expose_secret())
        .unwrap()
        .prologue(&relay::prologue(&fake_fp))
        .unwrap()
        .build_responder()
        .unwrap();
    let (mut a, mut b) = (vec![0u8; 4096], vec![0u8; 4096]);
    let n = hs.write_message(&[], &mut a).unwrap();
    middle.read_message(&a[..n], &mut b).unwrap();
    let n = middle.write_message(&[], &mut a).unwrap();
    hs.read_message(&a[..n], &mut b).unwrap();
    let n = hs.write_message(&[], &mut a).unwrap();
    middle.read_message(&a[..n], &mut b).unwrap();
    let phone_code = comparison_code(&hs.get_handshake_hash().try_into().unwrap());

    // ...and pairs with the real Studio as a phone of its own.
    let middle_phone = Phone::new();
    let record = studio_record(rig.hub.identity());
    let fp = StudioRecord::parse(&record).unwrap().fingerprint();
    let mut link = relay_connect(&mut relay, &middle_phone, "phone-middle", &record).await;
    link.recv(&mut relay).await; // identity
    let binding = link.binding;
    link.send(
        &relay,
        &middle_phone.pair_code_request(&fp, &binding, "Pixel"),
    )
    .await;
    assert_eq!(link.recv(&mut relay).await["type"], "pair_pending");
    link.recv(&mut relay).await; // the rig accepts; a real user would not

    let shown = rig.app.lock().unwrap().comparisons.clone();
    assert_eq!(shown.len(), 1);
    assert_ne!(shown[0].as_deref(), Some(phone_code.as_str()));
}

#[tokio::test]
async fn the_phone_refuses_a_studio_that_is_not_the_record() {
    let ours = rig();
    let other = rig();
    let mut relay = fake_relay(&other.hub);
    let phone = Phone::new();
    let record = studio_record(ours.hub.identity());
    // The relay delivers to another Studio: its Noise key is not the one in
    // the record, and the prologue (the record's fingerprint) differs, so
    // the handshake already fails on Studio's side.
    let record_key = StudioRecord::parse(&record).unwrap().noise_public;
    let mut hs = noise_builder()
        .unwrap()
        .local_private_key(&phone.noise.private)
        .unwrap()
        .prologue(&relay::prologue(
            &StudioRecord::parse(&record).unwrap().fingerprint(),
        ))
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let n = hs.write_message(&[], &mut buf).unwrap();
    relay
        .deliver(PHONE_ID, payload(Kind::Start, &buf[..n]))
        .await;
    let m2 = relay.next().await;
    // Different prologues: the phone cannot read Studio's answer.
    assert!(hs.read_message(&m2.payload[1..], &mut buf).is_err());
    assert_ne!(&record_key, other.hub.identity().noise_public());
}

#[tokio::test]
async fn code_pairing_is_only_on_the_relay() {
    let rig = rig();
    let phone = Phone::new();
    let fp = *rig.hub.identity().fingerprint();
    for transport in [Transport::Network, Transport::Ble] {
        let peer = (transport == Transport::Ble).then(|| phone.noise_public());
        let mut conn = Conn::open(&rig.hub, transport, 3, peer);
        conn.send(phone.pair_code_request(&fp, &[3; 32], "Pixel"))
            .await;
        assert_eq!(conn.recv().await["code"], "not_on_transport");
    }
    assert!(rig.app.lock().unwrap().comparisons.is_empty());
}

#[tokio::test]
async fn a_code_request_must_be_signed_for_this_session_and_key() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let phone = Phone::new();
    let other = Phone::new();
    let record = studio_record(rig.hub.identity());
    let fp = StudioRecord::parse(&record).unwrap().fingerprint();
    let mut link = relay_connect(&mut relay, &phone, PHONE_ID, &record).await;
    link.recv(&mut relay).await; // identity
    let binding = link.binding;
    // Signed for another session.
    link.send(&relay, &phone.pair_code_request(&fp, &[9; 32], "Pixel"))
        .await;
    assert_eq!(link.recv(&mut relay).await["code"], "bad_signature");
    // Registers a Noise key the session does not use.
    rig.clock.advance(1100);
    link.send(&relay, &other.pair_code_request(&fp, &binding, "Pixel"))
        .await;
    assert_eq!(link.recv(&mut relay).await["code"], "bad_message");
    assert!(rig.app.lock().unwrap().comparisons.is_empty());
}

#[tokio::test]
async fn only_one_code_pairing_waits_at_a_time() {
    let rig = rig();
    rig.app.lock().unwrap().hold = true;
    let mut relay = fake_relay(&rig.hub);
    let record = studio_record(rig.hub.identity());
    let fp = StudioRecord::parse(&record).unwrap().fingerprint();

    let first = Phone::new();
    let mut a = relay_connect(&mut relay, &first, "phone-a", &record).await;
    a.recv(&mut relay).await;
    let ba = a.binding;
    a.send(&relay, &first.pair_code_request(&fp, &ba, "Pixel"))
        .await;
    assert_eq!(a.recv(&mut relay).await["type"], "pair_pending");

    let second = Phone::new();
    let mut b = relay_connect(&mut relay, &second, "phone-b", &record).await;
    b.recv(&mut relay).await;
    let bb = b.binding;
    b.send(&relay, &second.pair_code_request(&fp, &bb, "Pixel"))
        .await;
    assert_eq!(b.recv(&mut relay).await["code"], "rate_limited");

    // The first is decided; then another may ask.
    let held = rig.app.lock().unwrap().held.pop().unwrap();
    held.send(false).unwrap();
    assert_eq!(a.recv(&mut relay).await["type"], "pair_rejected");
    rig.clock.advance(1100);
    b.send(&relay, &second.pair_code_request(&fp, &bb, "Pixel"))
        .await;
    assert_eq!(b.recv(&mut relay).await["type"], "pair_pending");
}

#[tokio::test]
async fn a_replayed_frame_ends_the_session_and_runs_nothing_twice() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let mut phone = Phone::new();
    let mut link = relay_paired(&rig, &mut relay, &mut phone).await;
    let (_, play) = phone.command(r#"{"cmd":"play"}"#);
    let sealed = link.seal(&play);
    for p in &sealed {
        relay.deliver(PHONE_ID, p.clone()).await;
    }
    assert_eq!(link.recv(&mut relay).await["ok"], true);
    // The relay sends the same frame again.
    for p in sealed {
        relay.deliver(PHONE_ID, p).await;
    }
    assert!(relay.quiet().await, "no answer, the session is gone");
    assert_eq!(rig.app.lock().unwrap().commands, [Command::Play]);
}

#[tokio::test]
async fn a_reordered_or_changed_frame_ends_the_session() {
    for tamper in [false, true] {
        let rig = rig();
        let mut relay = fake_relay(&rig.hub);
        let mut phone = Phone::new();
        let mut link = relay_paired(&rig, &mut relay, &mut phone).await;
        let (_, play) = phone.command(r#"{"cmd":"play"}"#);
        let (_, pause) = phone.command(r#"{"cmd":"pause"}"#);
        let mut first = link.seal(&play);
        let second = link.seal(&pause);
        if tamper {
            let last = first[0].len() - 1;
            first[0][last] ^= 1;
            relay.deliver(PHONE_ID, first.remove(0)).await;
        } else {
            // The second message arrives first.
            relay.deliver(PHONE_ID, second[0].clone()).await;
            relay.deliver(PHONE_ID, first[0].clone()).await;
        }
        assert!(relay.quiet().await, "tamper = {tamper}");
        assert!(
            rig.app.lock().unwrap().commands.is_empty(),
            "tamper = {tamper}"
        );
    }
}

#[tokio::test]
async fn an_undelivered_note_ends_the_phones_session() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let mut phone = Phone::new();
    let mut link = relay_paired(&rig, &mut relay, &mut phone).await;
    relay
        .to_studio
        .send(RelayIn::Undelivered {
            to: PHONE_ID.to_owned(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, play) = phone.command(r#"{"cmd":"play"}"#);
    link.send(&relay, &play).await;
    assert!(relay.quiet().await);
    assert!(rig.app.lock().unwrap().commands.is_empty());
}

#[tokio::test]
async fn large_answers_come_in_chunks_below_the_relay_limit() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let mut phone = Phone::new();
    let mut link = relay_paired(&rig, &mut relay, &mut phone).await;
    let queue: Vec<String> = (0..1000).map(|i| format!("so-{i:0>40}")).collect();
    rig.hub.publish_state(PlayState {
        queue: queue.clone(),
        ..PlayState::default()
    });
    // Check the frames as they arrive, then the joined message.
    let mut buf = vec![0u8; 70_000];
    let mut frames = 0;
    let state = loop {
        let out = relay.next().await;
        assert!(1 + out.to.len() + out.payload.len() <= relay::MAX_RELAY_MESSAGE);
        frames += 1;
        let n = link
            .noise
            .read_message(&out.payload[1..], &mut buf)
            .unwrap();
        if let Some(m) = link.joiner.push(&buf[..n]).unwrap() {
            break serde_json::from_slice::<Value>(&m).unwrap();
        }
    };
    assert!(frames > 1, "split into chunks");
    assert_eq!(state["queue"].as_array().unwrap().len(), queue.len());
}

#[tokio::test]
async fn the_phone_switches_network_relay_bluetooth_network_without_pairing_again() {
    let rig = rig();
    let mut relay = fake_relay(&rig.hub);
    let mut phone = Phone::new();

    let mut link = relay_paired(&rig, &mut relay, &mut phone).await;
    let (_, play) = phone.command(r#"{"cmd":"play"}"#);
    link.send(&relay, &play).await;
    assert_eq!(link.recv(&mut relay).await["ok"], true);
    let replay_on_network = play.clone();

    // On the local network: same keys, the counter goes on.
    let mut net = Conn::open(&rig.hub, Transport::Network, 5, None);
    assert_eq!(hello(&mut net, &mut phone).await["transport"], "network");
    net.recv().await; // state
    net.send(replay_on_network).await;
    assert_eq!(
        net.recv().await["code"],
        "replay",
        "no replay across transports"
    );
    let (_, next) = phone.command(r#"{"cmd":"next"}"#);
    net.send(next).await;
    assert_eq!(net.recv().await["ok"], true);
    drop(net);

    // Bluetooth: the Noise key from the relay pairing works here too.
    let mut ble = ble_connect(&rig, &phone).await;
    ble.recv().await; // identity
    let binding = ble.binding;
    ble.send(&phone.hello(&binding)).await;
    assert_eq!(ble.recv().await["transport"], "ble");
    ble.recv().await; // state
    let (_, pause) = phone.command(r#"{"cmd":"pause"}"#);
    ble.send(&pause).await;
    assert_eq!(ble.recv().await["ok"], true);
    drop(ble);

    // Back to the relay with a fresh handshake.
    let record = studio_record(rig.hub.identity());
    let mut link = relay_connect(&mut relay, &phone, PHONE_ID, &record).await;
    link.recv(&mut relay).await; // identity
    let binding = link.binding;
    link.send(&relay, &phone.hello(&binding)).await;
    assert_eq!(link.recv(&mut relay).await["transport"], "relay");
    link.recv(&mut relay).await; // state
    let (_, stop) = phone.command(r#"{"cmd":"stop"}"#);
    link.send(&relay, &stop).await;
    assert_eq!(link.recv(&mut relay).await["ok"], true);

    assert_eq!(
        rig.app.lock().unwrap().commands,
        [Command::Play, Command::Next, Command::Pause, Command::Stop]
    );
    assert_eq!(devices::list(&rig.db.reader().unwrap()).unwrap().len(), 1);
}

#[test]
fn the_comparison_code_is_what_the_document_says() {
    use sha2::{Digest, Sha256};
    let h = [7u8; 32];
    let hash = Sha256::new()
        .chain_update(b"dancesonic-compare-v1\0")
        .chain_update(h)
        .finalize();
    let n = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]) % 1_000_000;
    assert_eq!(comparison_code(&h), format!("{n:06}"));
    assert_ne!(comparison_code(&h), comparison_code(&[8; 32]));
    let t = super::offer::pair_code_transcript(&[1; 32], &[2; 65], &[3; 32], &[4; 32]);
    assert!(t.starts_with(b"dancesonic-pair-code-v1\0"));
    assert_ne!(
        t,
        super::offer::pair_transcript(&[1; 32], &[2; 65], &[3; 32], &[4; 32])
    );
    assert_eq!(b64u(&[1]), "AQ");
}
