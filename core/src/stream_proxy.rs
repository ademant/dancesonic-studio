//! A loopback proxy that signs the player's stream requests (#6).
//!
//! The player fetches a track with plain HTTP requests and cannot attach a
//! fresh DPoP proof to each one. A proof is good for 60 seconds, and a stream
//! makes many range requests, so a fixed header would fail after a minute.
//! This proxy listens on `127.0.0.1` only. It accepts `GET` and `HEAD` for the
//! stream endpoint, signs each request with the device key (or sends the API
//! key), and relays the answer back as it arrives.
//!
//! The random path token is the only thing that lets another local process use
//! the proxy. It is compared in constant time, and the URL that carries it is
//! never logged. Requests are limited in size and time, and the proxy answers
//! one request per connection.
//!
//! With an [`AudioCache`] (#30), the proxy also keeps whole tracks on disk, so
//! seeks and rate changes do not wait for the network. Playback still starts
//! on the first bytes: the first request reads the download as it arrives.
//! A request for bytes the download will reach within [`AHEAD_WAIT`] waits for
//! it; one further ahead goes to the server, and its bytes are written into
//! the cache too. When the cache cannot be used, requests go to the server as
//! before.

#[cfg(test)]
mod cache_tests;
mod download;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinHandle, JoinSet};

use crate::audio_cache::AudioCache;
use crate::connection::ServerUrl;
use crate::http::STREAM_IDLE_TIMEOUT;
use crate::queue::SongId;
use crate::subsonic::{API_VERSION, Auth, CLIENT_NAME, credential};
use download::{Download, FALLBACK_CONTENT_TYPE, Phase, Tee};

/// Largest request head read, in bytes. A stream request is a few hundred.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Longest a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Random bytes in the path token.
const TOKEN_BYTES: usize = 24;

/// Longest `Range` header relayed, in bytes.
const MAX_RANGE_BYTES: usize = 128;

/// Attempts per stream request. A request that fails before any answer, such
/// as one on a connection the server has just closed, gets one more attempt
/// on a fresh connection. That is safe because the proxy only relays `GET` and
/// `HEAD`, which are idempotent.
const STREAM_ATTEMPTS: u32 = 2;

/// Pause before the second attempt.
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Most error sources written to the log for one failure.
const MAX_ERROR_SOURCES: usize = 8;

/// Longest a request waits for the download to reach the bytes it asks for,
/// when the download is expected to get there in that time.
pub const AHEAD_WAIT: Duration = Duration::from_secs(2);

/// Bytes ahead of the download that a request always waits for, whatever the
/// download's speed so far.
const SMALL_AHEAD: u64 = 256 * 1024;

/// Longest a request waits for the download's answer from the server (length
/// and type) before it goes to the server itself.
const HEADERS_WAIT: Duration = Duration::from_secs(10);

/// Bytes read from a cache file at a time.
const READ_CHUNK: usize = 64 * 1024;

/// Errors from starting the proxy. Carry no token and no address of the server.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("the stream proxy could not listen on this machine")]
    Listen(#[source] std::io::Error),
    #[error("the system random source failed")]
    Random,
    #[error("the address of the server is not usable")]
    Address,
    #[error("the song id cannot be used")]
    BadSong,
}

/// What the proxy needs to reach the server for every request.
struct Upstream {
    http: reqwest::Client,
    server: ServerUrl,
    auth: Auth,
    token: String,
    cache: Option<Arc<AudioCache>>,
    downloads: Mutex<Downloads>,
    runtime: tokio::runtime::Handle,
}

/// The proxy's downloads. A finished download leaves the map; a failed one
/// stays, so its song is streamed without the cache instead of retried on
/// every request.
#[derive(Default)]
struct Downloads {
    known: HashMap<SongId, (Arc<Download>, AbortHandle)>,
    /// Downloaded once no other download is running.
    next: Option<SongId>,
}

impl Downloads {
    fn busy(&self) -> bool {
        self.known
            .values()
            .any(|(d, _)| matches!(d.state().phase, Phase::Starting | Phase::Running))
    }
}

impl Upstream {
    fn downloads(&self) -> MutexGuard<'_, Downloads> {
        self.downloads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The download of `song`, started when there is none. `None` without a cache.
    fn download_for(self: &Arc<Self>, song: &SongId) -> Option<Arc<Download>> {
        let cache = self.cache.as_ref()?;
        let mut downloads = self.downloads();
        if let Some((download, _)) = downloads.known.get(song) {
            return Some(Arc::clone(download));
        }
        Some(self.start_download(&mut downloads, cache, song))
    }

    fn start_download(
        self: &Arc<Self>,
        downloads: &mut Downloads,
        cache: &Arc<AudioCache>,
        song: &SongId,
    ) -> Arc<Download> {
        let download = Download::new(song.clone(), cache.partial_path(song));
        let task = self.runtime.spawn(download::run(
            Arc::clone(&download),
            Arc::clone(self),
            Arc::clone(cache),
        ));
        downloads
            .known
            .insert(song.clone(), (Arc::clone(&download), task.abort_handle()));
        download
    }

    /// Downloads `song` once no other download is running.
    fn prefetch(self: &Arc<Self>, song: &SongId) {
        let Some(cache) = &self.cache else { return };
        if cache.complete(song).is_some() {
            return;
        }
        let mut downloads = self.downloads();
        if downloads.known.contains_key(song) {
            return;
        }
        if downloads.busy() {
            downloads.next = Some(song.clone());
        } else {
            self.start_download(&mut downloads, cache, song);
        }
    }

    /// Stops the downloads of songs not in `keep`.
    fn retain(&self, keep: &[SongId]) {
        let mut downloads = self.downloads();
        downloads.known.retain(|song, (_, task)| {
            let kept = keep.contains(song);
            if !kept {
                task.abort();
            }
            kept
        });
        if downloads
            .next
            .as_ref()
            .is_some_and(|next| !keep.contains(next))
        {
            downloads.next = None;
        }
    }

    /// Called by a download task when it ends: starts the queued one.
    fn download_finished(self: &Arc<Self>, song: &SongId) {
        let Some(cache) = &self.cache else { return };
        let mut downloads = self.downloads();
        let complete = downloads
            .known
            .get(song)
            .is_some_and(|(d, _)| d.state().phase == Phase::Complete);
        if complete {
            downloads.known.remove(song);
        }
        if downloads.busy() {
            return;
        }
        if let Some(next) = downloads.next.take()
            && cache.complete(&next).is_none()
            && !downloads.known.contains_key(&next)
        {
            self.start_download(&mut downloads, cache, &next);
        }
    }
}

impl std::fmt::Debug for StreamProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamProxy")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// A running proxy. Dropping it stops the listener, every open stream and
/// every download.
pub struct StreamProxy {
    port: u16,
    token: String,
    task: JoinHandle<()>,
    upstream: Arc<Upstream>,
}

impl Drop for StreamProxy {
    fn drop(&mut self) {
        self.task.abort();
        self.upstream.retain(&[]);
    }
}

impl StreamProxy {
    /// Starts the proxy on a free loopback port. `http` should come from
    /// [`crate::http::streaming_client`], so the https and pinning rules apply.
    pub async fn start(
        http: reqwest::Client,
        server: ServerUrl,
        auth: Auth,
    ) -> Result<Self, ProxyError> {
        Self::launch(http, server, auth, None).await
    }

    /// Starts the proxy with an audio cache for `server` (#30).
    pub async fn start_with_cache(
        http: reqwest::Client,
        server: ServerUrl,
        auth: Auth,
        cache: Arc<AudioCache>,
    ) -> Result<Self, ProxyError> {
        if cache.server_key() != server.key() {
            return Err(ProxyError::Address);
        }
        Self::launch(http, server, auth, Some(cache)).await
    }

    async fn launch(
        http: reqwest::Client,
        server: ServerUrl,
        auth: Auth,
        cache: Option<Arc<AudioCache>>,
    ) -> Result<Self, ProxyError> {
        let token = random_token()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(ProxyError::Listen)?;
        let port = listener.local_addr().map_err(ProxyError::Listen)?.port();
        let upstream = Arc::new(Upstream {
            http,
            server,
            auth,
            token: token.clone(),
            cache,
            downloads: Mutex::new(Downloads::default()),
            runtime: tokio::runtime::Handle::current(),
        });
        let task = tokio::spawn(accept_loop(listener, Arc::clone(&upstream)));
        Ok(Self {
            port,
            token,
            task,
            upstream,
        })
    }

    /// Downloads `song` into the cache once no other download is running: the
    /// next track in the queue. Does nothing without a cache or when the track
    /// is cached already. Does not block; callable from the GTK main thread.
    pub fn prefetch(&self, song: &SongId) {
        self.upstream.prefetch(song);
    }

    /// Stops the downloads of all songs but `songs`, for example when the
    /// queue changes. Their partial files are deleted. Does not block.
    pub fn retain(&self, songs: &[SongId]) {
        self.upstream.retain(songs);
    }

    /// The URL the player opens for one song. It carries the path token, so it
    /// is a local secret: give it to the player and to nothing else.
    pub fn stream_url(&self, song: &SongId) -> Result<String, ProxyError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("id", song.as_str())
            .finish();
        Ok(format!(
            "http://127.0.0.1:{}/{}/stream?{query}",
            self.port, self.token
        ))
    }

    /// The port the proxy listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

fn random_token() -> Result<String, ProxyError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| ProxyError::Random)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Accepts connections until the proxy is dropped. Each connection runs in a
/// task owned by the set, so dropping the listener aborts them all.
async fn accept_loop(listener: TcpListener, upstream: Arc<Upstream>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((socket, _)) => {
                    connections.spawn(serve(socket, Arc::clone(&upstream)));
                }
                Err(error) => {
                    tracing::warn!(%error, "stream proxy could not accept a connection");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn serve(mut socket: TcpStream, upstream: Arc<Upstream>) {
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut socket)).await {
        Ok(Ok(head)) => head,
        Ok(Err(HeadError::TooLarge)) => return respond(&mut socket, 431).await,
        Ok(Err(HeadError::Closed)) => return,
        Err(_) => return respond(&mut socket, 408).await,
    };
    let request = match parse(&head, &upstream.token) {
        Ok(request) => request,
        Err(status) => return respond(&mut socket, status).await,
    };
    let result = match &upstream.cache {
        Some(cache) => serve_with_cache(&mut socket, &upstream, cache, &request).await,
        None => relay(&mut socket, &upstream, &request, None).await,
    };
    if let Err(error) = result {
        // Never the URL: it carries the path token.
        tracing::debug!(error, "stream relay ended");
    }
}

enum HeadError {
    TooLarge,
    Closed,
}

/// Reads the request head, up to the blank line, within [`MAX_HEAD_BYTES`].
async fn read_head(socket: &mut TcpStream) -> Result<Vec<u8>, HeadError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.truncate(end);
            return Ok(buf);
        }
        if buf.len() >= MAX_HEAD_BYTES {
            return Err(HeadError::TooLarge);
        }
        let read = socket
            .read(&mut chunk)
            .await
            .map_err(|_| HeadError::Closed)?;
        if read == 0 {
            return Err(HeadError::Closed);
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// A request the proxy will relay.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    head_only: bool,
    song: SongId,
    range: Option<String>,
}

/// Checks a request head. Returns the HTTP status to answer with when the
/// request is refused.
fn parse(head: &[u8], token: &str) -> Result<Request, u16> {
    let text = std::str::from_utf8(head).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let mut line = lines.next().ok_or(400u16)?.split(' ');
    let (method, target, version) = (line.next(), line.next(), line.next());
    if line.next().is_some() {
        return Err(400);
    }
    let head_only = match method {
        Some("GET") => false,
        Some("HEAD") => true,
        _ => return Err(405),
    };
    if !matches!(version, Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err(400);
    }
    let target = target.ok_or(400u16)?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let (path_token, rest) = path
        .strip_prefix('/')
        .and_then(|p| p.split_once('/'))
        .ok_or(404u16)?;
    let token_ok: bool = path_token.as_bytes().ct_eq(token.as_bytes()).into();
    if !token_ok || rest != "stream" {
        return Err(404);
    }

    let mut ids = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(name, _)| name == "id")
        .map(|(_, value)| value.into_owned());
    let id = ids.next().ok_or(400u16)?;
    if ids.next().is_some() {
        return Err(400);
    }
    let song = SongId::parse(&id).map_err(|_| 400u16)?;

    // Only a byte range is relayed; anything else is dropped and the whole
    // track is sent instead.
    let range = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| is_byte_range(value));
    Ok(Request {
        head_only,
        song,
        range,
    })
}

fn is_byte_range(value: &str) -> bool {
    value.len() <= MAX_RANGE_BYTES
        && value.strip_prefix("bytes=").is_some_and(|spec| {
            !spec.is_empty()
                && spec
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b == b'-' || b == b',')
        })
}

/// Why a request to the server failed before any answer.
enum SendError {
    Signing,
    Unreachable,
}

/// Sends one signed request for `song` to the server. A request that fails
/// before any answer is sent once more, with a fresh credential.
async fn send_upstream(
    upstream: &Upstream,
    method: reqwest::Method,
    song: &SongId,
    range: Option<&str>,
) -> Result<reqwest::Response, SendError> {
    let mut url = upstream
        .server
        .endpoint("rest/stream.view")
        .map_err(|_| SendError::Unreachable)?;
    url.query_pairs_mut()
        .append_pair("id", song.as_str())
        .append_pair("c", CLIENT_NAME)
        .append_pair("v", API_VERSION);
    let mut attempt = 1;
    loop {
        // A fresh credential per attempt: a DPoP proof must not be sent twice.
        let (name, value) =
            credential(&upstream.auth, method.as_str(), &url).map_err(|_| SendError::Signing)?;
        let mut outgoing = upstream
            .http
            .request(method.clone(), url.clone())
            .header(name, value);
        if let Some(range) = range {
            outgoing = outgoing.header(RANGE, range);
        }
        match outgoing.send().await {
            Ok(answer) => return Ok(answer),
            Err(error) => {
                let retry = attempt < STREAM_ATTEMPTS && worth_retrying(&error);
                tracing::debug!(
                    attempt,
                    retry,
                    error = %error_chain(error),
                    "stream request failed"
                );
                if !retry {
                    return Err(SendError::Unreachable);
                }
                attempt += 1;
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// Sends the request to the server and copies its answer back. With `tee`,
/// a partial answer that starts where the tee does is also written into the
/// download's file.
async fn relay(
    socket: &mut TcpStream,
    upstream: &Upstream,
    request: &Request,
    tee: Option<(Arc<Download>, u64)>,
) -> Result<(), &'static str> {
    let method = if request.head_only {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };
    let mut answer =
        match send_upstream(upstream, method, &request.song, request.range.as_deref()).await {
            Ok(answer) => answer,
            Err(SendError::Signing) => {
                respond(socket, 500).await;
                return Err("signing");
            }
            Err(SendError::Unreachable) => {
                respond(socket, 502).await;
                return Err("unreachable");
            }
        };

    let status = answer.status();
    if !status.is_success() {
        respond(socket, status.as_u16()).await;
        return Ok(());
    }
    let mut head = status_line(status);
    for (header, label) in [
        (CONTENT_TYPE, "Content-Type"),
        (CONTENT_LENGTH, "Content-Length"),
        (CONTENT_RANGE, "Content-Range"),
    ] {
        let value = answer.headers().get(&header).and_then(|v| v.to_str().ok());
        if let Some(value) = value.filter(|value| relayable(&header, value)) {
            head.push_str(&format!("{label}: {value}\r\n"));
        }
    }
    head.push_str("Accept-Ranges: bytes\r\nConnection: close\r\n\r\n");
    socket
        .write_all(head.as_bytes())
        .await
        .map_err(|_| "client closed")?;
    if request.head_only {
        return Ok(());
    }
    let mut tee = match tee {
        Some((download, start))
            if status == StatusCode::PARTIAL_CONTENT
                && content_range_start(answer.headers()) == Some(start) =>
        {
            Tee::open(download, start).await
        }
        _ => None,
    };
    while let Some(chunk) = answer.chunk().await.map_err(|_| "stream interrupted")? {
        socket
            .write_all(&chunk)
            .await
            .map_err(|_| "client closed")?;
        let failed = match tee.as_mut() {
            Some(tee) => tee.write(&chunk).await.is_err(),
            None => false,
        };
        if failed {
            tee = None;
        }
    }
    // The answer is complete; a failed shutdown changes nothing for the player.
    let _ = socket.shutdown().await;
    Ok(())
}

/// Serves a request with the cache: from the complete file, from the download
/// as it arrives, or from the server for bytes far ahead of the download.
async fn serve_with_cache(
    socket: &mut TcpStream,
    upstream: &Arc<Upstream>,
    cache: &Arc<AudioCache>,
    request: &Request,
) -> Result<(), &'static str> {
    let range = match request.range.as_deref().map(ByteRange::parse) {
        None => None,
        Some(Some(range)) => Some(range),
        // Several ranges, or a form the cache does not serve.
        Some(None) => return relay(socket, upstream, request, None).await,
    };
    if let Some(track) = cache.complete(&request.song) {
        touch(cache, &request.song).await;
        return serve_file(socket, upstream, request, &track, range).await;
    }
    let Some(download) = upstream.download_for(&request.song) else {
        return relay(socket, upstream, request, None).await;
    };
    download
        .wait_for(HEADERS_WAIT, |s| {
            s.total.is_some() || s.phase == Phase::Failed
        })
        .await;
    let state = download.state();
    let total = match (state.phase, state.total) {
        (Phase::Running | Phase::Complete, Some(total)) => total,
        // No answer in time, or the track cannot be cached.
        _ => return relay(socket, upstream, request, None).await,
    };
    if state.phase == Phase::Complete
        && let Some(track) = cache.complete(&request.song)
    {
        return serve_file(socket, upstream, request, &track, range).await;
    }
    let Some((start, end)) = resolve(range, total) else {
        return refuse_range(socket, total).await;
    };
    let served = Served {
        start,
        end,
        total,
        partial: range.is_some(),
        content_type: state
            .content_type
            .clone()
            .unwrap_or_else(|| FALLBACK_CONTENT_TYPE.to_owned()),
    };
    if request.head_only {
        return write_head(socket, &served).await;
    }
    let on_disk = |s: &download::State| s.covered_until(start) > start;
    let ready = on_disk(&state)
        || (download.arrives_within(start, AHEAD_WAIT, SMALL_AHEAD)
            && download.wait_for(AHEAD_WAIT, on_disk).await);
    if ready {
        follow(socket, upstream, cache, &download, &served).await
    } else {
        relay(socket, upstream, request, Some((download, start))).await
    }
}

/// What a request gets: bytes `start..=end` of a track of `total` bytes.
struct Served {
    start: u64,
    end: u64,
    total: u64,
    partial: bool,
    content_type: String,
}

/// Serves a complete track from its file.
async fn serve_file(
    socket: &mut TcpStream,
    upstream: &Upstream,
    request: &Request,
    track: &crate::audio_cache::CachedTrack,
    range: Option<ByteRange>,
) -> Result<(), &'static str> {
    let Some((start, end)) = resolve(range, track.size) else {
        return refuse_range(socket, track.size).await;
    };
    // Opened before answering: a file evicted in the meantime falls back to the server.
    let Ok(mut file) = tokio::fs::File::open(&track.path).await else {
        return relay(socket, upstream, request, None).await;
    };
    let served = Served {
        start,
        end,
        total: track.size,
        partial: range.is_some(),
        content_type: track.content_type.clone(),
    };
    write_head(socket, &served).await?;
    if request.head_only {
        return Ok(());
    }
    file.seek(std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| "cache read failed")?;
    let mut body = file.take(end - start + 1);
    tokio::io::copy(&mut body, socket)
        .await
        .map_err(|_| "client closed")?;
    let _ = socket.shutdown().await;
    Ok(())
}

/// Serves bytes from the download's file as the download writes them. When
/// the download fails, the rest comes from the server.
async fn follow(
    socket: &mut TcpStream,
    upstream: &Upstream,
    cache: &AudioCache,
    download: &Download,
    served: &Served,
) -> Result<(), &'static str> {
    // The partial file, or the complete one if the download just finished.
    let file = match tokio::fs::File::open(&download.part).await {
        Ok(file) => Some(file),
        Err(_) => match cache.complete(&download.song) {
            Some(track) => tokio::fs::File::open(&track.path).await.ok(),
            None => None,
        },
    };
    write_head(socket, served).await?;
    let Some(mut file) = file else {
        return continue_upstream(socket, upstream, &download.song, served.start, served.end).await;
    };
    file.seek(std::io::SeekFrom::Start(served.start))
        .await
        .map_err(|_| "cache read failed")?;
    let mut buf = vec![0u8; READ_CHUNK];
    let mut pos = served.start;
    while pos <= served.end {
        let state = download.state();
        let available = state.covered_until(pos);
        if available > pos {
            let want = (available.min(served.end + 1) - pos).min(READ_CHUNK as u64);
            let want = usize::try_from(want).unwrap_or(READ_CHUNK);
            let read = file
                .read(&mut buf[..want])
                .await
                .map_err(|_| "cache read failed")?;
            if read == 0 {
                return continue_upstream(socket, upstream, &download.song, pos, served.end).await;
            }
            socket
                .write_all(&buf[..read])
                .await
                .map_err(|_| "client closed")?;
            pos += u64::try_from(read).unwrap_or(u64::MAX);
            continue;
        }
        if state.phase == Phase::Failed {
            return continue_upstream(socket, upstream, &download.song, pos, served.end).await;
        }
        let arrived = download
            .wait_for(STREAM_IDLE_TIMEOUT, |s| {
                s.covered_until(pos) > pos || s.phase == Phase::Failed
            })
            .await;
        if !arrived {
            return Err("download stalled");
        }
    }
    let _ = socket.shutdown().await;
    Ok(())
}

/// Sends bytes `pos..=end` from the server, after the head was already sent.
async fn continue_upstream(
    socket: &mut TcpStream,
    upstream: &Upstream,
    song: &SongId,
    pos: u64,
    end: u64,
) -> Result<(), &'static str> {
    let range = format!("bytes={pos}-{end}");
    let mut answer = send_upstream(upstream, reqwest::Method::GET, song, Some(&range))
        .await
        .map_err(|_| "unreachable")?;
    if answer.status() != StatusCode::PARTIAL_CONTENT
        || content_range_start(answer.headers()) != Some(pos)
    {
        return Err("the server cannot continue the track");
    }
    while let Some(chunk) = answer.chunk().await.map_err(|_| "stream interrupted")? {
        socket
            .write_all(&chunk)
            .await
            .map_err(|_| "client closed")?;
    }
    let _ = socket.shutdown().await;
    Ok(())
}

/// Records a play of a cached track, for the eviction order.
async fn touch(cache: &Arc<AudioCache>, song: &SongId) {
    let cache = Arc::clone(cache);
    let song = song.clone();
    match tokio::task::spawn_blocking(move || cache.touch(&song)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "could not record a play"),
        Err(_) => tracing::debug!("the play-recording task failed"),
    }
}

async fn write_head(socket: &mut TcpStream, served: &Served) -> Result<(), &'static str> {
    let status = if served.partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut head = status_line(status);
    head.push_str(&format!(
        "Content-Type: {}\r\nContent-Length: {}\r\n",
        served.content_type,
        served.end - served.start + 1
    ));
    if served.partial {
        head.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            served.start, served.end, served.total
        ));
    }
    head.push_str("Accept-Ranges: bytes\r\nConnection: close\r\n\r\n");
    socket
        .write_all(head.as_bytes())
        .await
        .map_err(|_| "client closed")
}

async fn refuse_range(socket: &mut TcpStream, total: u64) -> Result<(), &'static str> {
    let head = format!(
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    socket
        .write_all(head.as_bytes())
        .await
        .map_err(|_| "client closed")?;
    let _ = socket.shutdown().await;
    Ok(())
}

/// One byte range, as the player asks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ByteRange {
    /// `bytes=a-`
    From(u64),
    /// `bytes=a-b`
    Between(u64, u64),
    /// `bytes=-n`: the last `n` bytes.
    Last(u64),
}

impl ByteRange {
    /// A single range; `None` for several ranges or anything malformed.
    fn parse(value: &str) -> Option<Self> {
        let spec = value.strip_prefix("bytes=")?;
        if spec.contains(',') {
            return None;
        }
        let (first, last) = spec.split_once('-')?;
        match (first.is_empty(), last.is_empty()) {
            (false, true) => Some(Self::From(first.parse().ok()?)),
            (false, false) => {
                let (from, to) = (first.parse().ok()?, last.parse().ok()?);
                (from <= to).then_some(Self::Between(from, to))
            }
            (true, false) => Some(Self::Last(last.parse().ok()?)),
            (true, true) => None,
        }
    }
}

/// The first and last byte a request gets from a track of `total` bytes;
/// `None` when the range lies outside it.
fn resolve(range: Option<ByteRange>, total: u64) -> Option<(u64, u64)> {
    let last = total.checked_sub(1)?;
    match range {
        None => Some((0, last)),
        Some(ByteRange::From(from)) => (from <= last).then_some((from, last)),
        Some(ByteRange::Between(from, to)) => (from <= last).then_some((from, to.min(last))),
        Some(ByteRange::Last(count)) => (count > 0).then(|| (total - count.min(total), last)),
    }
}

/// The first byte of a `Content-Range: bytes a-b/t` header.
fn content_range_start(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let (first, _) = value.strip_prefix("bytes ")?.split_once('-')?;
    first.parse().ok()
}

/// Headers that may be copied to the player. The content type is limited to
/// audio, so a server cannot make the player treat the stream as anything else.
fn relayable(header: &reqwest::header::HeaderName, value: &str) -> bool {
    let plain = value.len() <= 256 && value.bytes().all(|b| (0x20..0x7f).contains(&b));
    if !plain {
        return false;
    }
    if *header == CONTENT_TYPE {
        return value.starts_with("audio/") || value == "application/octet-stream";
    }
    true
}

/// Whether a failed request may be sent again. Only failures before any
/// answer: the connection could not be made, or it closed while the request
/// was sent. A timeout is not retried; it would only double the wait.
fn worth_retrying(error: &reqwest::Error) -> bool {
    !error.is_timeout() && (error.is_connect() || error.is_request())
}

/// The error and its causes on one line, without the URL: the URL is the
/// server's stream endpoint and carries the song id. The causes are I/O and
/// protocol errors, which carry no URL.
fn error_chain(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    for _ in 0..MAX_ERROR_SOURCES {
        let Some(cause) = source else { break };
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn status_line(status: StatusCode) -> String {
    format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    )
}

/// Answers with an empty body and a status. Write errors are ignored: the
/// client has gone away and there is nothing left to tell it.
async fn respond(socket: &mut TcpStream, code: u16) {
    let reason = StatusCode::from_u16(code)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("");
    let head =
        format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    // The client may already be gone; there is no one left to tell.
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::HttpPolicy;
    use crate::http::{HttpSettings, streaming_client};
    use tokio::io::AsyncReadExt;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TOKEN: &str = "test-token-abc";

    fn head(line: &str, extra: &str) -> Vec<u8> {
        format!("{line}\r\nHost: 127.0.0.1\r\n{extra}\r\n").into_bytes()
    }

    #[test]
    fn a_stream_request_with_the_token_is_accepted() {
        let request = parse(
            &head("GET /test-token-abc/stream?id=so-7 HTTP/1.1", ""),
            TOKEN,
        )
        .unwrap();
        assert_eq!(request.song.as_str(), "so-7");
        assert!(!request.head_only);
        assert_eq!(request.range, None);
    }

    #[test]
    fn a_wrong_token_or_path_is_not_found() {
        assert_eq!(
            parse(&head("GET /wrong/stream?id=so-7 HTTP/1.1", ""), TOKEN).err(),
            Some(404)
        );
        assert_eq!(
            parse(
                &head("GET /test-token-abc/other?id=so-7 HTTP/1.1", ""),
                TOKEN
            )
            .err(),
            Some(404)
        );
        assert_eq!(
            parse(
                &head("GET /test-token-abc/../stream?id=so-7 HTTP/1.1", ""),
                TOKEN
            )
            .err(),
            Some(404)
        );
    }

    #[test]
    fn only_get_and_head_are_relayed() {
        assert_eq!(
            parse(
                &head("POST /test-token-abc/stream?id=so-7 HTTP/1.1", ""),
                TOKEN
            )
            .err(),
            Some(405)
        );
        let head_request = parse(
            &head("HEAD /test-token-abc/stream?id=so-7 HTTP/1.1", ""),
            TOKEN,
        )
        .unwrap();
        assert!(head_request.head_only);
    }

    #[test]
    fn a_missing_or_repeated_song_id_is_refused() {
        assert_eq!(
            parse(&head("GET /test-token-abc/stream HTTP/1.1", ""), TOKEN).err(),
            Some(400)
        );
        assert_eq!(
            parse(
                &head("GET /test-token-abc/stream?id=a&id=b HTTP/1.1", ""),
                TOKEN
            )
            .err(),
            Some(400)
        );
    }

    #[test]
    fn only_byte_ranges_are_relayed() {
        let ok = parse(
            &head(
                "GET /test-token-abc/stream?id=so-7 HTTP/1.1",
                "Range: bytes=0-1023\r\n",
            ),
            TOKEN,
        )
        .unwrap();
        assert_eq!(ok.range.as_deref(), Some("bytes=0-1023"));
        let odd = parse(
            &head(
                "GET /test-token-abc/stream?id=so-7 HTTP/1.1",
                "Range: items=0-1\r\n",
            ),
            TOKEN,
        )
        .unwrap();
        assert_eq!(odd.range, None);
    }

    #[test]
    fn the_token_comparison_is_exact() {
        assert!(
            parse(
                &head("GET /test-token-abc-x/stream?id=so-7 HTTP/1.1", ""),
                TOKEN
            )
            .is_err()
        );
    }

    #[test]
    fn only_audio_content_types_are_relayed() {
        assert!(relayable(&CONTENT_TYPE, "audio/flac"));
        assert!(!relayable(&CONTENT_TYPE, "text/html"));
        assert!(!relayable(&CONTENT_LENGTH, "12\r\nInjected: yes"));
    }

    fn plain_http() -> reqwest::Client {
        streaming_client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap()
    }

    async fn server_url(server: &MockServer) -> ServerUrl {
        ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap()
    }

    /// Sends one raw request to the proxy and returns everything it answers.
    async fn ask(port: u16, request: &str) -> String {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        socket.read_to_end(&mut answer).await.unwrap();
        String::from_utf8_lossy(&answer).into_owned()
    }

    async fn proxy_for(server: &MockServer, auth: Auth) -> StreamProxy {
        StreamProxy::start(plain_http(), server_url(server).await, auth)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_player_gets_the_track_signed_with_the_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/stream.view"))
            .and(query_param("id", "so-7"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "audio/flac")
                    .set_body_bytes(b"0123456789".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = proxy_for(&server, Auth::ApiKey(key)).await;
        let url = proxy.stream_url(&SongId::parse("so-7").unwrap()).unwrap();
        let rest = url.strip_prefix("http://127.0.0.1:").unwrap();
        let (port, target) = rest.split_once('/').unwrap();
        let answer = ask(
            port.parse().unwrap(),
            &format!(
                "GET /{} HTTP/1.1\r\nHost: x\r\n\r\n",
                target.trim_start_matches('/')
            ),
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.contains("Content-Type: audio/flac"));
        assert!(answer.ends_with("0123456789"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0].headers.get("x-api-key").unwrap(),
            "tsk_test_key_12345"
        );
        assert!(
            !requests[0].url.as_str().contains("tsk_test_key"),
            "the key never appears in a URL"
        );
    }

    #[tokio::test]
    async fn a_device_request_is_signed_per_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/stream.view"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"ok".to_vec()))
            .mount(&server)
            .await;
        let device = crate::dpop::DeviceKey::generate().unwrap();
        let proxy = proxy_for(&server, Auth::Device(device)).await;
        let url = proxy.stream_url(&SongId::parse("so-7").unwrap()).unwrap();
        let target = url.trim_start_matches(&format!("http://127.0.0.1:{}", proxy.port()));
        for _ in 0..2 {
            ask(
                proxy.port(),
                &format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n"),
            )
            .await;
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let first = requests[0].headers.get("dpop").unwrap().to_str().unwrap();
        let second = requests[1].headers.get("dpop").unwrap().to_str().unwrap();
        assert_ne!(first, second, "every request gets its own proof");
        assert!(requests[0].headers.get("x-api-key").is_none());
    }

    #[tokio::test]
    async fn a_wrong_token_never_reaches_the_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = proxy_for(&server, Auth::ApiKey(key)).await;
        let answer = ask(
            proxy.port(),
            "GET /guessed/stream?id=so-7 HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
    }

    #[tokio::test]
    async fn an_oversized_request_head_is_refused() {
        let server = MockServer::start().await;
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = proxy_for(&server, Auth::ApiKey(key)).await;
        let padding = "x".repeat(MAX_HEAD_BYTES);
        let answer = ask(
            proxy.port(),
            &format!("GET /t/stream?id=a HTTP/1.1\r\nX-Pad: {padding}\r\n\r\n"),
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 431"), "{answer}");
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_bad_gateway() {
        // A port nothing listens on. A dropped mock server's port can be
        // reused by another test's server, which would answer 404 instead.
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let base = ServerUrl::parse(
            &format!("http://127.0.0.1:{port}"),
            HttpPolicy::ALLOW_PRIVATE_HTTP,
        )
        .unwrap();
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = StreamProxy::start(plain_http(), base, Auth::ApiKey(key))
            .await
            .unwrap();
        let answer = ask(
            proxy.port(),
            &format!(
                "GET /{}/stream?id=so-7 HTTP/1.1\r\nHost: x\r\n\r\n",
                proxy.token
            ),
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    }

    /// A minimal upstream server. The first `drop_first` connections are
    /// closed after the request head is read, without an answer, like a
    /// connection the server closed just as the request went out. Later
    /// connections answer every request and stay open (keep-alive).
    /// Returns the server address, the number of connections accepted and the
    /// request heads received.
    async fn raw_upstream(
        drop_first: usize,
    ) -> (
        ServerUrl,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(AtomicUsize::new(0));
        let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (count, seen) = (Arc::clone(&connections), Arc::clone(&heads));
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let number = count.fetch_add(1, Ordering::SeqCst) + 1;
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    while let Ok(head) = read_head(&mut socket).await {
                        seen.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&head).into_owned());
                        if number <= drop_first {
                            return;
                        }
                        let answer = "HTTP/1.1 200 OK\r\nContent-Type: audio/ogg\r\n\
                                      Content-Length: 2\r\n\r\nok";
                        if socket.write_all(answer.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        let url = ServerUrl::parse(
            &format!("http://127.0.0.1:{port}"),
            HttpPolicy::ALLOW_PRIVATE_HTTP,
        )
        .unwrap();
        (url, connections, heads)
    }

    async fn ask_for_song(proxy: &StreamProxy) -> String {
        ask(
            proxy.port(),
            &format!(
                "GET /{}/stream?id=so-7 HTTP/1.1\r\nHost: x\r\n\r\n",
                proxy.token
            ),
        )
        .await
    }

    /// Regression: a stream request sent on a connection the server closed
    /// failed with "error sending request", and the player stopped with 502.
    #[tokio::test]
    async fn a_request_dropped_before_any_answer_is_sent_again() {
        let (server, connections, heads) = raw_upstream(1).await;
        let device = crate::dpop::DeviceKey::generate().unwrap();
        let proxy = StreamProxy::start(plain_http(), server, Auth::Device(device))
            .await
            .unwrap();
        let answer = ask_for_song(&proxy).await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.ends_with("ok"), "{answer}");
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 2);

        let heads = heads.lock().unwrap();
        let proof = |head: &str| {
            head.lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("dpop"))
                        .map(|(_, value)| value.trim().to_owned())
                })
                .unwrap()
        };
        assert_ne!(
            proof(&heads[0]),
            proof(&heads[1]),
            "the second attempt carries a fresh proof"
        );
    }

    #[tokio::test]
    async fn a_server_that_keeps_dropping_gets_two_attempts_then_a_bad_gateway() {
        let (server, connections, _) = raw_upstream(usize::MAX).await;
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = StreamProxy::start(plain_http(), server, Auth::ApiKey(key))
            .await
            .unwrap();
        let answer = ask_for_song(&proxy).await;
        assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
        assert_eq!(
            connections.load(std::sync::atomic::Ordering::SeqCst),
            STREAM_ATTEMPTS as usize,
            "the retry is bounded"
        );
    }

    /// Requests in quick succession (one seek) share a connection, so a seek
    /// does not open a new one per request. A connection idle longer than
    /// [`crate::http::STREAM_POOL_IDLE_TIMEOUT`] is not reused, so a later
    /// request does not land on one the server may have closed.
    #[tokio::test]
    async fn an_idle_stream_connection_is_reused_only_briefly() {
        use std::sync::atomic::Ordering;
        let (server, connections, _) = raw_upstream(0).await;
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = StreamProxy::start(plain_http(), server, Auth::ApiKey(key))
            .await
            .unwrap();
        for _ in 0..2 {
            let answer = ask_for_song(&proxy).await;
            assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        }
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "back-to-back requests share one connection"
        );

        tokio::time::sleep(crate::http::STREAM_POOL_IDLE_TIMEOUT + Duration::from_millis(500))
            .await;
        let answer = ask_for_song(&proxy).await;
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            2,
            "a connection idle past the timeout is not reused"
        );
    }

    #[tokio::test]
    async fn dropping_the_proxy_stops_it() {
        let server = MockServer::start().await;
        let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
        let proxy = proxy_for(&server, Auth::ApiKey(key)).await;
        let port = proxy.port();
        drop(proxy);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }
}
