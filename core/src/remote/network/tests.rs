//! The network transport against a phone that pins Studio's key as the
//! document says, over real TLS on the loopback interface.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls13_signature};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::remote::devices::DeviceId;
use crate::remote::phone::{Phone, scan};
use crate::remote::tests::{Rig, rig};
use crate::remote::{from_b64u, identity};

/// Accepts any certificate, but checks that the server holds its key; the
/// pin is checked afterwards, as the phone does.
#[derive(Debug)]
struct AnyKey(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyKey {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.3 only".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

async fn connect(port: u16) -> TlsStream<TcpStream> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyKey(provider)))
        .with_no_client_auth();
    let tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from("studio.local").unwrap(), tcp)
        .await
        .unwrap()
}

fn certificate(tls: &TlsStream<TcpStream>) -> Vec<u8> {
    tls.get_ref().1.peer_certificates().unwrap()[0].to_vec()
}

/// `GET path`: the status line and the body.
async fn get(port: u16, head: &str) -> (String, Vec<u8>, Vec<u8>) {
    let mut tls = connect(port).await;
    let cert = certificate(&tls);
    tls.write_all(head.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    // The server closes after answering; a reset just ends the read.
    let _ = tls.read_to_end(&mut answer).await;
    let text = String::from_utf8_lossy(&answer).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.lines().next().unwrap_or("").to_owned();
    (status, body.as_bytes().to_vec(), cert)
}

/// What a phone does before trusting a found Studio: `/v1/info`, then the
/// pin check over the certificate and the Noise key.
async fn pinned(port: u16, fingerprint: &[u8; 32]) -> bool {
    let (status, body, cert) = get(port, "GET /v1/info HTTP/1.1\r\nHost: studio\r\n\r\n").await;
    assert!(status.contains("200"), "{status}");
    let info: Value = serde_json::from_slice(&body).unwrap();
    let noise = from_b64u(info["noise_key"].as_str().unwrap(), 32).unwrap();
    pin_matches(&cert, &noise, fingerprint)
}

struct WsPhone {
    ws: WebSocketStream<TlsStream<TcpStream>>,
    binding: [u8; 32],
}

impl WsPhone {
    async fn open(port: u16) -> Self {
        let tls = connect(port).await;
        let binding = tls
            .get_ref()
            .1
            .export_keying_material([0u8; 32], EXPORTER_LABEL, None)
            .unwrap();
        let (ws, _) = tokio_tungstenite::client_async("wss://studio.local/v1/session", tls)
            .await
            .unwrap();
        Self { ws, binding }
    }

    async fn send(&mut self, message: Vec<u8>) {
        self.ws
            .send(Message::text(String::from_utf8(message).unwrap()))
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("an answer")
                .expect("open")
                .unwrap();
            if let Message::Text(text) = message {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    async fn closed(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.ws.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .unwrap_or(false)
    }
}

async fn server(rig: &Rig) -> NetworkServer {
    NetworkServer::start(Arc::clone(&rig.hub), 0).await.unwrap()
}

async fn pair(rig: &Rig, port: u16, phone: &mut Phone) -> WsPhone {
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    assert!(
        pinned(port, &scanned.fp).await,
        "the phone pins the right Studio"
    );
    let mut ws = WsPhone::open(port).await;
    let binding = ws.binding;
    ws.send(phone.pair_request(&scanned, &binding, "Pixel"))
        .await;
    assert_eq!(ws.recv().await["type"], "pair_pending");
    let accepted = ws.recv().await;
    phone.device = DeviceId::parse(accepted["device"].as_str().unwrap());
    ws
}

#[test]
fn only_local_network_peers_are_local() {
    for local in [
        "192.168.1.2",
        "10.0.0.1",
        "172.16.5.4",
        "169.254.1.1",
        "127.0.0.1",
        "::1",
        "fe80::1",
        "fd00::1",
        "::ffff:192.168.0.9",
    ] {
        assert!(is_local_peer(local.parse().unwrap()), "{local}");
    }
    for public in [
        "8.8.8.8",
        "172.32.0.1",
        "2001:db8::1",
        "::ffff:8.8.8.8",
        "100.64.0.1",
    ] {
        assert!(!is_local_peer(public.parse().unwrap()), "{public}");
    }
}

#[test]
fn request_heads_are_parsed_strictly() {
    let head = b"GET /v1/info HTTP/1.1\r\nHost: x\r\nUpgrade: WebSocket\r\n";
    let request = parse_head(head).unwrap();
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("GET", "/v1/info")
    );
    assert!(request.has_token("upgrade", "websocket"));
    assert!(parse_head(b"GET /v1/info HTTP/1.0\r\n").is_none());
    assert!(parse_head(b"GET /v1/info HTTP/1.1 extra\r\n").is_none());
    assert!(parse_head(b"GET /v1/info HTTP/1.1\r\nno colon\r\n").is_none());
}

#[test]
fn the_certificate_carries_the_pinned_key() {
    let identity = identity::Identity::generate().unwrap();
    let cert = identity.certificate().unwrap().certificate;
    assert_eq!(certificate_spki(&cert), Some(identity.tls_spki()));
    assert!(pin_matches(
        &cert,
        identity.noise_public(),
        identity.fingerprint()
    ));
    assert!(
        !pin_matches(&cert, &[0; 32], identity.fingerprint()),
        "Noise key bound too"
    );
    assert!(certificate_spki(&cert[..cert.len() / 2]).is_none());
    assert!(certificate_spki(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]).is_none());
}

#[tokio::test]
async fn a_phone_pairs_and_controls_studio_over_the_network() {
    let rig = rig();
    let server = server(&rig).await;
    let mut phone = Phone::new();
    let mut ws = pair(&rig, server.port(), &mut phone).await;
    let binding = ws.binding;
    ws.send(phone.hello(&binding)).await;
    let welcome = ws.recv().await;
    assert_eq!(welcome["transport"], "network");
    assert!(
        welcome["features"]
            .as_array()
            .unwrap()
            .contains(&"library".into())
    );
    assert_eq!(ws.recv().await["type"], "state");

    let (n, play) = phone.command(r#"{"cmd":"library_artists"}"#);
    ws.send(play.clone()).await;
    let result = ws.recv().await;
    assert_eq!(
        (result["id"].clone(), result["ok"].clone()),
        (n.into(), true.into())
    );
    ws.send(play).await;
    assert_eq!(ws.recv().await["code"], "replay");

    rig.hub.revoke(phone.device.as_ref().unwrap()).unwrap();
    assert_eq!(ws.recv().await["reason"], "revoked");
    assert!(ws.closed().await);
}

#[tokio::test]
async fn unpaired_phones_get_nothing() {
    let rig = rig();
    let server = server(&rig).await;
    let mut stranger = Phone::new();
    stranger.device = Some(DeviceId::random().unwrap());
    let mut ws = WsPhone::open(server.port()).await;
    let (_, play) = stranger.command(r#"{"cmd":"play"}"#);
    ws.send(play).await;
    assert_eq!(ws.recv().await["code"], "not_paired");
    let binding = ws.binding;
    ws.send(stranger.hello(&binding)).await;
    assert_eq!(ws.recv().await["code"], "not_paired");
    assert!(ws.closed().await);
    assert!(rig.app.lock().unwrap().commands.is_empty());
}

#[tokio::test]
async fn http_requests_outside_the_protocol_are_refused() {
    let rig = rig();
    let server = server(&rig).await;
    let port = server.port();
    let (status, _, _) = get(port, "GET /rest/ping.view HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(status.contains("404"), "{status}");
    let (status, _, _) = get(port, "GET /v1/session HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(status.contains("400"), "not an upgrade: {status}");
    let long = format!("GET /v1/info HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(10_000));
    let (status, _, _) = get(port, &long).await;
    assert!(status.contains("400"), "{status}");
}

#[tokio::test]
async fn an_oversized_message_ends_the_session() {
    let rig = rig();
    let server = server(&rig).await;
    let mut ws = WsPhone::open(server.port()).await;
    let big = format!(r#"{{"type":"hello","pad":"{}"}}"#, "a".repeat(70_000));
    // The server may close while the frame is still being sent.
    let _ = ws.ws.send(Message::text(big)).await;
    assert!(ws.closed().await);
}

#[tokio::test]
async fn the_phone_picks_its_studio_and_refuses_an_impostor() {
    let ours = rig();
    let other = rig();
    let our_server = server(&ours).await;
    let other_server = server(&other).await;
    let ours_id = ours.hub.identity();
    let other_id = other.hub.identity();
    let local = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    // Three announcements: another Studio, an impostor that copies our tag
    // but runs with its own keys, and ours.
    let found = vec![
        Found {
            tag: other_id.mdns_tag(),
            addresses: local.clone(),
            port: other_server.port(),
        },
        Found {
            tag: ours_id.mdns_tag(),
            addresses: local.clone(),
            port: other_server.port(),
        },
        Found {
            tag: ours_id.mdns_tag(),
            addresses: local,
            port: our_server.port(),
        },
    ];
    let candidates = matching(&found, ours_id.fingerprint());
    assert_eq!(candidates.len(), 2, "the tag selects ours and the impostor");
    let mut trusted = Vec::new();
    for candidate in candidates {
        if pinned(candidate.port, ours_id.fingerprint()).await {
            trusted.push(candidate.port);
        }
    }
    assert_eq!(
        trusted,
        [our_server.port()],
        "only the pinned key is trusted"
    );
}

#[derive(Default)]
struct Seen {
    announced: Vec<Vec<IpAddr>>,
}

struct FakeAnnouncer(Arc<Mutex<Seen>>);

impl Announcer for FakeAnnouncer {
    fn announce(&mut self, addresses: &[IpAddr], _port: u16) {
        self.0.lock().unwrap().announced.push(addresses.to_vec());
    }
    fn withdraw(&mut self) {}
}

struct FakeAddresses(Arc<Mutex<Vec<IpAddr>>>);

impl AddressSource for FakeAddresses {
    fn addresses(&self) -> Vec<IpAddr> {
        self.0.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn studio_announces_again_when_the_network_changes() {
    let rig = rig();
    let addresses = Arc::new(Mutex::new(vec!["192.168.1.20".parse::<IpAddr>().unwrap()]));
    let seen = Arc::new(Mutex::new(Seen::default()));
    let watch = tokio::spawn(watch_addresses(
        Arc::clone(&rig.hub),
        47800,
        Box::new(FakeAddresses(Arc::clone(&addresses))),
        Box::new(FakeAnnouncer(Arc::clone(&seen))),
        Duration::from_millis(20),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(seen.lock().unwrap().announced.len(), 1, "unchanged: once");
    // The laptop moves to another Wi-Fi.
    *addresses.lock().unwrap() = vec![
        "10.0.0.7".parse().unwrap(),
        IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 7)),
    ];
    tokio::time::sleep(Duration::from_millis(100)).await;
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.announced.len(), 2);
        assert_eq!(seen.announced[1].len(), 2);
    }
    // A phone on Bluetooth would now be told the new address.
    let mut phone = Phone::new();
    let (uri, _) = rig.hub.new_offer().unwrap();
    let scanned = scan(uri.expose_secret());
    let (tx, incoming) = tokio::sync::mpsc::channel(8);
    let (outgoing, mut rx) = tokio::sync::mpsc::channel(32);
    let link = Link {
        transport: Transport::Ble,
        binding: [1; 32],
        peer_noise: Some(phone.noise_public()),
    };
    tokio::spawn(hub::run(Arc::clone(&rig.hub), link, incoming, outgoing));
    tx.send(phone.pair_request(&scanned, &[1; 32], "Pixel"))
        .await
        .unwrap();
    let mut next = async || -> Value { serde_json::from_slice(&rx.recv().await.unwrap()).unwrap() };
    next().await; // pending
    phone.device = DeviceId::parse(next().await["device"].as_str().unwrap());
    tx.send(phone.hello(&[1; 32])).await.unwrap();
    next().await; // welcome
    next().await; // state
    let hint = next().await;
    assert_eq!(hint["type"], "network_hint");
    assert_eq!(hint["addresses"][0], "10.0.0.7");
    watch.abort();
}
