//! The local-network transport (#20): HTTPS with Studio's pinned
//! self-signed certificate, a WebSocket session, mDNS announcement and the
//! network hint for Bluetooth sessions.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use secrecy::ExposeSecret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use super::hub::{self, Hub, Link, NetworkHint, Slot};
use super::identity::Identity;
use super::messages::Transport;

/// The port Studio tries first; another one is used when it is taken.
pub const DEFAULT_PORT: u16 = 47800;
/// The mDNS service type.
pub const SERVICE_TYPE: &str = "_dancesonic._tcp.local.";
/// The TLS exporter label of the channel binding.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-dancesonic-studio-v1";
/// The largest HTTP request head.
const MAX_HEAD: usize = 8 * 1024;
/// How long the TLS handshake and the request head may take.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the addresses are checked for a change.
pub const ADDRESS_CHECK: Duration = Duration::from_secs(5);

/// Why the network transport could not start.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("cannot create the TLS certificate: {0}")]
    Certificate(String),
    #[error("cannot listen for phones: {0}")]
    Listen(#[from] std::io::Error),
}

/// Whether a peer address is on the local network: private, link-local,
/// unique-local or loopback.
pub fn is_local_peer(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || first & 0xffc0 == 0xfe80 || first & 0xfe00 == 0xfc00
        }
    }
}

/// The running HTTPS server. Dropping it stops it and ends its sessions.
#[derive(Debug)]
pub struct NetworkServer {
    port: u16,
    task: JoinHandle<()>,
}

impl Drop for NetworkServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl NetworkServer {
    /// Listens on `port` (or any free port when it is taken) on all
    /// interfaces and accepts only peers on the local network.
    pub async fn start(hub: Arc<Hub>, port: u16) -> Result<Self, NetworkError> {
        let acceptor = TlsAcceptor::from(Arc::new(tls_config(hub.identity())?));
        let listener = bind(port).await?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(accept_loop(hub, acceptor, listener));
        tracing::info!(port, "remote control listening");
        Ok(Self { port, task })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

fn tls_config(identity: &Identity) -> Result<ServerConfig, NetworkError> {
    let cert = identity
        .certificate()
        .map_err(|e| NetworkError::Certificate(e.to_string()))?;
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key.expose_secret().to_vec()));
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| NetworkError::Certificate(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert.certificate)], key)
        .map_err(|e| NetworkError::Certificate(e.to_string()))
}

async fn bind(port: u16) -> std::io::Result<TcpListener> {
    // Both address families where IPv6 is available; IPv4 alone otherwise.
    let any6 = SocketAddr::from((Ipv6Addr::UNSPECIFIED, port));
    let any4 = SocketAddr::from(([0, 0, 0, 0], port));
    for addr in [any6, any4] {
        if let Ok(listener) = TcpListener::bind(addr).await {
            return Ok(listener);
        }
    }
    // The port is taken: any free one. Phones find it by mDNS or the hint.
    match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))).await {
        Ok(listener) => Ok(listener),
        Err(_) => TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], 0))).await,
    }
}

async fn accept_loop(hub: Arc<Hub>, acceptor: TlsAcceptor, listener: TcpListener) {
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                tracing::debug!(%error, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !is_local_peer(peer.ip()) {
            tracing::debug!("refused a connection from outside the local network");
            continue;
        }
        let slot = hub.admit();
        connections.spawn(serve(Arc::clone(&hub), acceptor.clone(), tcp, slot));
    }
}

/// A parsed request head.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn has_token(&self, name: &str, token: &str) -> bool {
        self.header(name)
            .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    }
}

fn parse_head(head: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, path, version) = (first.next()?, first.next()?, first.next()?);
    if version != "HTTP/1.1" || first.next().is_some() {
        return None;
    }
    let mut headers = Vec::new();
    for line in lines.take_while(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
        if headers.len() > 64 {
            return None;
        }
    }
    Some(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
    })
}

/// Reads up to the end of the request head. Bytes after it are refused: a
/// client must wait for the answer.
async fn read_head(tls: &mut TlsStream<TcpStream>) -> Option<Request> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = tls.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..n]);
        if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
            return (end + 4 == head.len())
                .then(|| parse_head(&head[..end + 2]))
                .flatten();
        }
        if head.len() > MAX_HEAD {
            return None;
        }
    }
}

async fn respond(tls: &mut TlsStream<TcpStream>, status: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    // The phone may already have gone; nothing to do then.
    let _ = tls.write_all(head.as_bytes()).await;
    let _ = tls.write_all(body).await;
    let _ = tls.shutdown().await;
}

async fn serve(hub: Arc<Hub>, acceptor: TlsAcceptor, tcp: TcpStream, slot: Option<Slot>) {
    let Ok(Ok(mut tls)) = tokio::time::timeout(HEAD_TIMEOUT, acceptor.accept(tcp)).await else {
        return;
    };
    let Ok(Some(request)) = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut tls)).await else {
        respond(&mut tls, "400 Bad Request", b"{}").await;
        return;
    };
    if slot.is_none() {
        respond(&mut tls, "429 Too Many Requests", b"{}").await;
        return;
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/info") => {
            let identity = hub.identity();
            let body = serde_json::json!({
                "protocol": super::PROTOCOL_VERSION,
                "name": hub.name(),
                "noise_key": super::b64u(identity.noise_public()),
                "fp_tag": identity.mdns_tag(),
            });
            respond(&mut tls, "200 OK", body.to_string().as_bytes()).await;
        }
        ("GET", "/v1/session") => {
            let key = request.header("Sec-WebSocket-Key");
            let upgrade = request.has_token("Upgrade", "websocket")
                && request.has_token("Connection", "upgrade")
                && request.header("Sec-WebSocket-Version") == Some("13");
            let Some(key) = key.filter(|_| upgrade) else {
                respond(&mut tls, "400 Bad Request", b"{}").await;
                return;
            };
            let accept = derive_accept_key(key.as_bytes());
            let head = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            if tls.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            let Ok(binding) =
                tls.get_ref()
                    .1
                    .export_keying_material([0u8; 32], EXPORTER_LABEL, None)
            else {
                return;
            };
            let max = Transport::Network.max_message();
            let config = WebSocketConfig::default()
                .max_message_size(Some(max))
                .max_frame_size(Some(max));
            let ws = WebSocketStream::from_raw_socket(tls, Role::Server, Some(config)).await;
            session(hub, ws, binding).await;
        }
        _ => respond(&mut tls, "404 Not Found", b"{}").await,
    }
}

async fn session(hub: Arc<Hub>, ws: WebSocketStream<TlsStream<TcpStream>>, binding: [u8; 32]) {
    let (mut sink, mut stream) = ws.split();
    let (to_hub, hub_in) = mpsc::channel(8);
    let (hub_out, mut from_hub) = mpsc::channel::<Vec<u8>>(32);
    let link = Link {
        transport: Transport::Network,
        binding,
        peer_noise: None,
    };
    let run = hub::run(hub, link, hub_in, hub_out);
    tokio::pin!(run);
    let mut to_hub = Some(to_hub);
    loop {
        tokio::select! {
            () = &mut run => break,
            message = stream.next(), if to_hub.is_some() => match message {
                Some(Ok(Message::Text(text))) => {
                    if let Some(tx) = &to_hub
                        && tx.send(text.as_bytes().to_vec()).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                // Binary frames are not part of the protocol; closing, errors
                // and oversized messages end the session.
                _ => to_hub = None,
            },
            out = from_hub.recv() => {
                let Some(out) = out else { break };
                let Ok(text) = String::from_utf8(out) else { continue };
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
        }
    }
    while let Ok(out) = from_hub.try_recv() {
        if let Ok(text) = String::from_utf8(out) {
            let _ = sink.send(Message::text(text)).await;
        }
    }
    // The phone may already have gone.
    let _ = sink.close().await;
}

/// Where the current addresses come from, so tests can change them.
pub trait AddressSource: Send {
    /// The local-network addresses of this computer.
    fn addresses(&self) -> Vec<IpAddr>;
}

/// The addresses of the network interfaces that are up.
#[derive(Debug, Default)]
pub struct SystemAddresses;

impl AddressSource for SystemAddresses {
    fn addresses(&self) -> Vec<IpAddr> {
        let mut out: Vec<IpAddr> = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_loopback() && i.is_oper_up())
            .map(|i| i.ip())
            .filter(|ip| is_local_peer(*ip))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Announces Studio on the network, so tests can watch it.
pub trait Announcer: Send {
    /// Announces (again) with these addresses.
    fn announce(&mut self, addresses: &[IpAddr], port: u16);
    /// Withdraws the announcement.
    fn withdraw(&mut self);
}

/// mDNS announcement of `_dancesonic._tcp`. Withdrawn on drop.
pub struct MdnsAnnouncer {
    daemon: mdns_sd::ServiceDaemon,
    instance: String,
    host: String,
    tag: String,
    fullname: Option<String>,
}

impl std::fmt::Debug for MdnsAnnouncer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdnsAnnouncer")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl MdnsAnnouncer {
    /// An announcer for `identity`, shown as `name`.
    pub fn new(identity: &Identity, name: &str) -> Result<Self, String> {
        let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| e.to_string())?;
        let tag = identity.mdns_tag();
        // The host name is per identity, not the computer's name.
        let host_tag: String = tag
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(8)
            .collect::<String>()
            .to_ascii_lowercase();
        let instance: String = name.chars().take(48).collect();
        Ok(Self {
            daemon,
            instance,
            host: format!("dancesonic-{host_tag}.local."),
            tag,
            fullname: None,
        })
    }
}

impl Announcer for MdnsAnnouncer {
    fn announce(&mut self, addresses: &[IpAddr], port: u16) {
        self.withdraw();
        if addresses.is_empty() {
            return;
        }
        let properties = [("v", "1"), ("fp", self.tag.as_str())];
        let info = mdns_sd::ServiceInfo::new(
            SERVICE_TYPE,
            &self.instance,
            &self.host,
            addresses,
            port,
            &properties[..],
        );
        match info {
            Ok(info) => {
                let fullname = info.get_fullname().to_owned();
                match self.daemon.register(info) {
                    Ok(()) => self.fullname = Some(fullname),
                    Err(error) => tracing::warn!(%error, "mDNS announcement failed"),
                }
            }
            Err(error) => tracing::warn!(%error, "mDNS announcement failed"),
        }
    }

    fn withdraw(&mut self) {
        if let Some(fullname) = self.fullname.take() {
            // Fails only when the daemon is gone, which ends it anyway.
            let _ = self.daemon.unregister(&fullname);
        }
    }
}

impl Drop for MdnsAnnouncer {
    fn drop(&mut self) {
        self.withdraw();
        // The daemon thread ends on its own when it cannot be told.
        let _ = self.daemon.shutdown();
    }
}

/// Keeps the announcement and the Bluetooth network hint in line with the
/// current addresses, checking every `every`. Runs until aborted.
pub async fn watch_addresses(
    hub: Arc<Hub>,
    port: u16,
    source: Box<dyn AddressSource>,
    mut announcer: Box<dyn Announcer>,
    every: Duration,
) {
    let mut last: Option<Vec<IpAddr>> = None;
    loop {
        let now = source.addresses();
        if last.as_ref() != Some(&now) {
            tracing::info!(
                count = now.len(),
                "network addresses changed, announcing again"
            );
            announcer.announce(&now, port);
            hub.set_network_hint((!now.is_empty()).then(|| NetworkHint {
                addresses: now.iter().map(ToString::to_string).collect(),
                port,
            }));
            last = Some(now);
        }
        tokio::time::sleep(every).await;
    }
}

/// The network transport while "Allow remote control" is on: the server,
/// the announcement and the address watch. Dropping it stops all three.
#[derive(Debug)]
pub struct NetworkRemote {
    server: NetworkServer,
    watch: JoinHandle<()>,
    hub: Arc<Hub>,
}

impl Drop for NetworkRemote {
    fn drop(&mut self) {
        // Aborting drops the announcer, which withdraws the announcement.
        self.watch.abort();
        self.hub.set_network_hint(None);
    }
}

impl NetworkRemote {
    /// Starts serving and announcing.
    pub async fn start(
        hub: Arc<Hub>,
        port: u16,
        source: Box<dyn AddressSource>,
        announcer: Box<dyn Announcer>,
    ) -> Result<Self, NetworkError> {
        let server = NetworkServer::start(Arc::clone(&hub), port).await?;
        let watch = tokio::spawn(watch_addresses(
            Arc::clone(&hub),
            server.port(),
            source,
            announcer,
            ADDRESS_CHECK,
        ));
        Ok(Self { server, watch, hub })
    }

    /// The port phones connect to.
    pub fn port(&self) -> u16 {
        self.server.port()
    }
}

/// One `_dancesonic._tcp` instance a phone found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The `fp` TXT value.
    pub tag: String,
    pub addresses: Vec<IpAddr>,
    pub port: u16,
}

/// The instances whose tag matches a paired Studio's fingerprint. The tag
/// only selects; the phone still checks the pinned key on connecting.
pub fn matching<'a>(found: &'a [Found], fingerprint: &[u8; 32]) -> Vec<&'a Found> {
    let tag = super::identity::mdns_tag_of(fingerprint);
    found.iter().filter(|f| f.tag == tag).collect()
}

/// Whether a server certificate and the `noise_key` from `/v1/info` belong
/// to the pinned fingerprint: the phone's check before sending anything.
pub fn pin_matches(certificate: &[u8], noise_key: &[u8], fingerprint: &[u8; 32]) -> bool {
    certificate_spki(certificate).is_some_and(|spki| {
        let fp = super::identity::fingerprint(spki, noise_key);
        subtle::ConstantTimeEq::ct_eq(&fp[..], &fingerprint[..]).into()
    })
}

/// One DER element: (tag, content, whole element, rest).
type Element<'a> = (u8, &'a [u8], &'a [u8], &'a [u8]);

fn der(input: &[u8]) -> Option<Element<'_>> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |a, b| (a << 8) | usize::from(*b));
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    let header = input.len() - rest.len();
    Some((tag, &rest[..len], &input[..header + len], &rest[len..]))
}

/// The DER `SubjectPublicKeyInfo` of an X.509 certificate.
pub fn certificate_spki(certificate: &[u8]) -> Option<&[u8]> {
    let (0x30, cert, _, _) = der(certificate)? else {
        return None;
    };
    let (0x30, tbs, _, _) = der(cert)? else {
        return None;
    };
    let mut rest = tbs;
    // The optional explicit version.
    if rest.first() == Some(&0xa0) {
        rest = der(rest)?.3;
    }
    // serialNumber, signature, issuer, validity, subject.
    for _ in 0..5 {
        rest = der(rest)?.3;
    }
    match der(rest)? {
        (0x30, _, whole, _) => Some(whole),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
