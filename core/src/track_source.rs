//! Tracks as files the player reads in-process (#33).
//!
//! The player asks a [`TrackSource`] for bytes at an offset, as it would read
//! a file. Nothing leaves the process: no listener, no URL with a credential.
//! Every request to the server is signed when it is sent (a fresh DPoP proof
//! or the API key), so a long track never outlives its credential.
//!
//! With an [`AudioCache`] (#30) every track is downloaded once, from the
//! start, into the cache, and the player reads that download as it arrives:
//!
//! - bytes on disk are read at once;
//! - bytes the download will reach within [`AHEAD_WAIT`] are waited for;
//! - bytes further ahead are fetched with a ranged request that writes into
//!   the same file, so they are on disk too;
//! - a complete track is the cached file.
//!
//! When the cache cannot be used, or a download fails, the source reads from
//! the server directly with ranged requests.
//!
//! [`TrackSource`] blocks. Call it from a player's streaming thread, never
//! from the GTK main thread and never from a Tokio worker.

mod download;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, RANGE};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use crate::audio_cache::AudioCache;
use crate::connection::ServerUrl;
use crate::http::STREAM_IDLE_TIMEOUT;
use crate::queue::SongId;
use crate::subsonic::{API_VERSION, Auth, CLIENT_NAME, credential};
use download::{Download, Phase};

/// Longest a read waits for the download to reach the bytes it asks for,
/// when the download is expected to get there in that time.
pub const AHEAD_WAIT: Duration = Duration::from_secs(2);

/// Bytes ahead of the download that a read always waits for, whatever the
/// download's speed so far.
const SMALL_AHEAD: u64 = 256 * 1024;

/// Longest a read waits for the server's answer to the download (length and
/// type) before it reads from the server directly.
const HEADERS_WAIT: Duration = Duration::from_secs(10);

/// Attempts per request. A request that fails before any answer, such as one
/// on a connection the server has just closed, gets one more attempt on a
/// fresh connection. Only `GET`s are sent, which are idempotent.
const ATTEMPTS: u32 = 2;

/// Pause before the second attempt.
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// How often a blocked read looks at its cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Most error sources written to the log for one failure.
const MAX_ERROR_SOURCES: usize = 8;

/// Content type when the server sends none that may be kept.
const FALLBACK_CONTENT_TYPE: &str = "application/octet-stream";

/// Errors from reading a track. Carry no URL and no credential.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SourceError {
    #[error("the read was cancelled")]
    Cancelled,
    #[error("the server cannot be reached")]
    Unreachable,
    #[error("the server refused the track (status {0})")]
    Refused(u16),
    #[error("the server sent no usable track length")]
    NoLength,
    #[error("no audio arrived for {} s", STREAM_IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("the request could not be signed")]
    Signing,
    #[error("the cache belongs to another server")]
    WrongCache,
    #[error("reading the cached track failed")]
    File,
}

/// What reads need to reach the server, shared by every source and download.
struct Upstream {
    http: reqwest::Client,
    server: ServerUrl,
    auth: Auth,
    cache: Option<Arc<AudioCache>>,
    downloads: Mutex<Downloads>,
    runtime: Handle,
}

/// The downloads. A finished download leaves the map; a failed one stays, so
/// its song is read from the server instead of being downloaded again on
/// every read.
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

    fn retain(&self, keep: &[SongId]) {
        let mut downloads = self.downloads();
        downloads.known.retain(|song, (download, task)| {
            let kept = keep.contains(song);
            if !kept {
                task.abort();
                download.stop_range();
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

/// The tracks of one server. Dropping it stops every download and deletes
/// partial files; sources opened from it keep working without the cache.
pub struct Tracks {
    upstream: Arc<Upstream>,
}

impl std::fmt::Debug for Tracks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracks").finish_non_exhaustive()
    }
}

impl Drop for Tracks {
    fn drop(&mut self) {
        self.upstream.retain(&[]);
    }
}

impl Tracks {
    /// Tracks of `server`, downloads run on `runtime`. `http` should come
    /// from [`crate::http::streaming_client`], so the https and pinning rules
    /// apply. `cache` must belong to the same server.
    pub fn new(
        http: reqwest::Client,
        server: ServerUrl,
        auth: Auth,
        cache: Option<Arc<AudioCache>>,
        runtime: Handle,
    ) -> Result<Self, SourceError> {
        if cache
            .as_ref()
            .is_some_and(|c| c.server_key() != server.key())
        {
            return Err(SourceError::WrongCache);
        }
        Ok(Self {
            upstream: Arc::new(Upstream {
                http,
                server,
                auth,
                cache,
                downloads: Mutex::new(Downloads::default()),
                runtime,
            }),
        })
    }

    /// A source for one song. Does not block; nothing is fetched until the
    /// first read.
    pub fn open(&self, song: &SongId) -> TrackSource {
        TrackSource {
            upstream: Arc::clone(&self.upstream),
            song: song.clone(),
            len: None,
            direct: None,
            touched: false,
        }
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
}

/// An open answer from the server, read from `pos` on.
struct Direct {
    pos: u64,
    answer: reqwest::Response,
    /// Bytes received but not yet read.
    rest: bytes::Bytes,
}

/// One song, read like a file. Blocking; see the module documentation.
pub struct TrackSource {
    upstream: Arc<Upstream>,
    song: SongId,
    len: Option<u64>,
    direct: Option<Direct>,
    touched: bool,
}

impl std::fmt::Debug for TrackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackSource")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl TrackSource {
    /// The song this source reads.
    pub fn song(&self) -> &SongId {
        &self.song
    }

    /// The length of the track in bytes. Blocks until the server answers.
    pub fn len(&mut self, cancel: &dyn Fn() -> bool) -> Result<u64, SourceError> {
        if let Some(len) = self.len {
            return Ok(len);
        }
        let len = self.find_len(cancel)?;
        self.len = Some(len);
        Ok(len)
    }

    fn find_len(&mut self, cancel: &dyn Fn() -> bool) -> Result<u64, SourceError> {
        if let Some(cache) = &self.upstream.cache
            && let Some(track) = cache.complete(&self.song)
        {
            return Ok(track.size);
        }
        if let Some(download) = self.upstream.download_for(&self.song) {
            self.wait(cancel, HEADERS_WAIT, &download, |s| {
                s.total.is_some() || s.phase == Phase::Failed
            })?;
            if let Some(total) = download.state().total {
                return Ok(total);
            }
        }
        // No cache, or the download failed: ask the server.
        self.open_direct(0, cancel)?;
        self.len.ok_or(SourceError::NoLength)
    }

    /// Reads bytes at `pos` into `buf`. Returns 0 at the end of the track.
    /// Blocks until at least one byte is there, the read is cancelled, or no
    /// byte arrived for [`STREAM_IDLE_TIMEOUT`].
    pub fn read_at(
        &mut self,
        pos: u64,
        buf: &mut [u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<usize, SourceError> {
        let len = self.len(cancel)?;
        if pos >= len || buf.is_empty() {
            return Ok(0);
        }
        let want = usize::try_from((len - pos).min(buf.len() as u64)).unwrap_or(buf.len());
        let buf = &mut buf[..want];
        let started = Instant::now();
        loop {
            if cancel() {
                return Err(SourceError::Cancelled);
            }
            if started.elapsed() > STREAM_IDLE_TIMEOUT {
                return Err(SourceError::Stalled);
            }
            if let Some(cache) = self.upstream.cache.clone()
                && let Some(track) = cache.complete(&self.song)
            {
                self.touch(&cache);
                let file = File::open(&track.path).map_err(|_| SourceError::File)?;
                return file.read_at(buf, pos).map_err(|_| SourceError::File);
            }
            let Some(download) = self.upstream.download_for(&self.song) else {
                return self.read_direct(pos, buf, cancel);
            };
            let state = download.state();
            match state.phase {
                Phase::Failed => return self.read_direct(pos, buf, cancel),
                // In the cache now; read it there on the next turn.
                Phase::Complete => continue,
                Phase::Starting | Phase::Running => {}
            }
            let covered = state.covered_until(pos);
            if covered > pos {
                let take = usize::try_from(covered - pos).unwrap_or(want).min(want);
                // The partial file may just have been renamed into the cache
                // or deleted; then look again.
                if let Ok(file) = File::open(&download.part)
                    && let Ok(read) = file.read_at(&mut buf[..take], pos)
                    && read > 0
                {
                    return Ok(read);
                }
                continue;
            }
            if !download.arrives_within(pos, AHEAD_WAIT, SMALL_AHEAD) {
                download.fetch_range(&self.upstream, pos);
            }
            self.wait(cancel, CANCEL_POLL * 4, &download, |s| {
                s.covered_until(pos) > pos || matches!(s.phase, Phase::Failed | Phase::Complete)
            })?;
        }
    }

    /// Waits up to `limit` for `ready`, looking at `cancel` in between.
    fn wait(
        &self,
        cancel: &dyn Fn() -> bool,
        limit: Duration,
        download: &Download,
        ready: impl Fn(&download::State) -> bool,
    ) -> Result<(), SourceError> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if cancel() {
                return Err(SourceError::Cancelled);
            }
            let slice = CANCEL_POLL.min(deadline.saturating_duration_since(Instant::now()));
            if self
                .upstream
                .runtime
                .block_on(download.wait_for(slice, &ready))
            {
                return Ok(());
            }
        }
        Ok(())
    }

    fn touch(&mut self, cache: &AudioCache) {
        if self.touched {
            return;
        }
        self.touched = true;
        if let Err(error) = cache.touch(&self.song) {
            tracing::debug!(%error, "could not record a play");
        }
    }

    /// Reads from the server directly, reusing the open answer when it is at `pos`.
    fn read_direct(
        &mut self,
        pos: u64,
        buf: &mut [u8],
        cancel: &dyn Fn() -> bool,
    ) -> Result<usize, SourceError> {
        if self.direct.as_ref().is_none_or(|d| d.pos != pos) {
            self.open_direct(pos, cancel)?;
        }
        let Some(direct) = self.direct.as_mut() else {
            return Err(SourceError::Unreachable);
        };
        if direct.rest.is_empty() {
            let chunk = block_cancellable(
                &self.upstream.runtime,
                cancel,
                STREAM_IDLE_TIMEOUT,
                direct.answer.chunk(),
            )?;
            match chunk {
                Ok(Some(chunk)) => direct.rest = chunk,
                Ok(None) => return Ok(0),
                Err(_) => {
                    self.direct = None;
                    return Err(SourceError::Unreachable);
                }
            }
        }
        let n = direct.rest.len().min(buf.len());
        buf[..n].copy_from_slice(&direct.rest[..n]);
        direct.rest = direct.rest.slice(n..);
        direct.pos += n as u64;
        Ok(n)
    }

    fn open_direct(&mut self, pos: u64, cancel: &dyn Fn() -> bool) -> Result<(), SourceError> {
        self.direct = None;
        let range = format!("bytes={pos}-");
        let answer = block_cancellable(
            &self.upstream.runtime,
            cancel,
            STREAM_IDLE_TIMEOUT,
            send(&self.upstream, &self.song, Some(&range)),
        )??;
        let status = answer.status();
        let total = match status {
            StatusCode::PARTIAL_CONTENT => {
                let (start, total) =
                    content_range(answer.headers()).ok_or(SourceError::NoLength)?;
                if start != pos {
                    return Err(SourceError::NoLength);
                }
                total
            }
            // A server without ranges sends the whole track: usable from 0 only.
            StatusCode::OK if pos == 0 => answer.content_length().ok_or(SourceError::NoLength)?,
            _ if !status.is_success() => return Err(SourceError::Refused(status.as_u16())),
            _ => return Err(SourceError::NoLength),
        };
        self.len.get_or_insert(total);
        self.direct = Some(Direct {
            pos,
            answer,
            rest: bytes::Bytes::new(),
        });
        Ok(())
    }
}

/// Runs `future` on `runtime`, giving up when `cancel` says so or after `limit`.
fn block_cancellable<F: Future>(
    runtime: &Handle,
    cancel: &dyn Fn() -> bool,
    limit: Duration,
    future: F,
) -> Result<F::Output, SourceError> {
    let deadline = Instant::now() + limit;
    runtime.block_on(async {
        let mut future = pin!(future);
        loop {
            if cancel() {
                return Err(SourceError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(SourceError::Stalled);
            }
            if let Ok(output) = tokio::time::timeout(CANCEL_POLL, &mut future).await {
                return Ok(output);
            }
        }
    })
}

/// Sends one signed `GET` for `song`. A request that fails before any answer
/// is sent once more, with a fresh credential.
async fn send(
    upstream: &Upstream,
    song: &SongId,
    range: Option<&str>,
) -> Result<reqwest::Response, SourceError> {
    let mut url = upstream
        .server
        .endpoint("rest/stream.view")
        .map_err(|_| SourceError::Unreachable)?;
    url.query_pairs_mut()
        .append_pair("id", song.as_str())
        .append_pair("c", CLIENT_NAME)
        .append_pair("v", API_VERSION);
    let mut attempt = 1;
    loop {
        // A fresh credential per attempt: a DPoP proof must not be sent twice.
        let (name, value) =
            credential(&upstream.auth, "GET", &url).map_err(|_| SourceError::Signing)?;
        let mut outgoing = upstream.http.get(url.clone()).header(name, value);
        if let Some(range) = range {
            outgoing = outgoing.header(RANGE, range);
        }
        match outgoing.send().await {
            Ok(answer) => return Ok(answer),
            Err(error) => {
                let retry = attempt < ATTEMPTS && worth_retrying(&error);
                tracing::debug!(attempt, retry, error = %error_chain(error), "track request failed");
                if !retry {
                    return Err(SourceError::Unreachable);
                }
                attempt += 1;
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

/// Whether a failed request may be sent again. Only failures before any
/// answer: the connection could not be made, or it closed while the request
/// was sent. A timeout is not retried; it would only double the wait.
fn worth_retrying(error: &reqwest::Error) -> bool {
    !error.is_timeout() && (error.is_connect() || error.is_request())
}

/// The error and its causes on one line, without the URL: the URL is the
/// server's stream endpoint and carries the song id.
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

/// First byte and total length of a `Content-Range: bytes a-b/t` header.
fn content_range(headers: &reqwest::header::HeaderMap) -> Option<(u64, u64)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let (span, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (first, _) = span.split_once('-')?;
    Some((first.parse().ok()?, total.parse().ok()?))
}

/// The content type to keep with a cached track: audio only, plain text,
/// bounded, so a server cannot store anything else.
fn audio_content_type(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| {
            v.len() <= 256
                && v.bytes().all(|b| (0x20..0x7f).contains(&b))
                && (v.starts_with("audio/") || *v == FALLBACK_CONTENT_TYPE)
        })
        .unwrap_or(FALLBACK_CONTENT_TYPE)
        .to_owned()
}
