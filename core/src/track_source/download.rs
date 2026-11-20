//! Whole-track downloads into the audio cache, and which bytes are on disk.
//!
//! One download per song writes the track from the start to its partial file.
//! A read far ahead of it starts a ranged request that writes into the same
//! file (see [`Tee`]); every written range is recorded, so a read can tell
//! which bytes are on disk.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::AbortHandle;

use super::{SMALL_AHEAD, Upstream, audio_content_type, content_range, send};
use crate::audio_cache::{AudioCache, MAX_TRACK_BYTES};
use crate::queue::SongId;

/// Where a download is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Waiting for the server's answer.
    Starting,
    /// Writing the track; its length and type are known.
    Running,
    /// The track is complete and in the cache.
    Complete,
    /// The download stopped. Requests go to the server instead.
    Failed,
}

/// The state of one download, shared with the requests that read from it.
#[derive(Debug)]
pub(super) struct Download {
    pub(super) song: SongId,
    pub(super) part: PathBuf,
    started: Instant,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    /// The ranged request for bytes far ahead, if one runs.
    range: Mutex<Option<RangeFetch>>,
}

/// A ranged request writing into the partial file.
#[derive(Debug)]
struct RangeFetch {
    start: u64,
    /// How far it has written.
    pos: Arc<std::sync::atomic::AtomicU64>,
    task: AbortHandle,
}

#[derive(Clone, Debug)]
pub(super) struct State {
    pub(super) phase: Phase,
    pub(super) total: Option<u64>,
    pub(super) content_type: Option<String>,
    /// How far the download from the start has written.
    pub(super) frontier: u64,
    /// Written byte ranges, half-open, sorted and merged.
    spans: Vec<(u64, u64)>,
}

impl State {
    /// The end of the bytes on disk that start at `pos`; `pos` when the byte at
    /// `pos` is not there.
    pub(super) fn covered_until(&self, pos: u64) -> u64 {
        self.spans
            .iter()
            .find(|(start, end)| *start <= pos && pos < *end)
            .map_or(pos, |(_, end)| *end)
    }

    fn add_span(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.spans.push((start, end));
        self.spans.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.spans.len());
        for (start, end) in self.spans.drain(..) {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        self.spans = merged;
    }
}

impl Download {
    pub(super) fn new(song: SongId, part: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            song,
            part,
            started: Instant::now(),
            state: Mutex::new(State {
                phase: Phase::Starting,
                total: None,
                content_type: None,
                frontier: 0,
                spans: Vec::new(),
            }),
            changed: watch::Sender::new(0),
            range: Mutex::new(None),
        })
    }

    /// Fetches the track from `pos` on into the partial file, unless a ranged
    /// request already covers `pos` or will soon. Replaces a request that is
    /// elsewhere: the player has moved on.
    pub(super) fn fetch_range(self: &Arc<Self>, upstream: &Arc<Upstream>, pos: u64) {
        use std::sync::atomic::Ordering;
        let mut range = self.range.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(fetch) = range.as_ref() {
            let at = fetch.pos.load(Ordering::SeqCst);
            if !fetch.task.is_finished() && fetch.start <= pos && pos <= at + SMALL_AHEAD {
                return;
            }
            fetch.task.abort();
        }
        let progress = Arc::new(std::sync::atomic::AtomicU64::new(pos));
        let task = upstream.runtime.spawn(fetch_range(
            Arc::clone(self),
            Arc::clone(upstream),
            pos,
            Arc::clone(&progress),
        ));
        *range = Some(RangeFetch {
            start: pos,
            pos: progress,
            task: task.abort_handle(),
        });
    }

    /// Stops the ranged request, if any.
    pub(super) fn stop_range(&self) {
        if let Some(fetch) = self
            .range
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            fetch.task.abort();
        }
    }

    pub(super) fn state(&self) -> State {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner));
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Records bytes written at `start..end`, by the download or a [`Tee`].
    pub(super) fn add_span(&self, start: u64, end: u64) {
        self.update(|state| state.add_span(start, end));
    }

    /// Waits until `ready` holds, for at most `limit`. Returns whether it held.
    pub(super) async fn wait_for(&self, limit: Duration, ready: impl Fn(&State) -> bool) -> bool {
        let mut changes = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if ready(&self.state()) {
                return true;
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                // Timed out, or the download is gone: one last look.
                _ => return ready(&self.state()),
            }
        }
    }

    /// Whether the download will probably reach `pos` within `limit`, judged
    /// by its speed so far. A request far ahead goes to the server at once
    /// instead of waiting in vain.
    pub(super) fn arrives_within(&self, pos: u64, limit: Duration, small: u64) -> bool {
        let state = self.state();
        if state.phase != Phase::Running && state.phase != Phase::Starting {
            return false;
        }
        let ahead = pos.saturating_sub(state.frontier);
        if ahead <= small {
            return true;
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return false;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a speed estimate; track sizes are far below 2^52"
        )]
        let speed = state.frontier as f64 / elapsed;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a speed estimate; track sizes are far below 2^52"
        )]
        let needed = ahead as f64;
        speed * limit.as_secs_f64() >= needed
    }
}

/// Marks the download failed and deletes its partial file unless it finished.
/// Runs when the download task ends in any way, including being aborted.
struct Finish {
    download: Arc<Download>,
    done: bool,
}

impl Drop for Finish {
    fn drop(&mut self) {
        self.download.stop_range();
        if self.done {
            return;
        }
        // Gone already, or never created: nothing to delete.
        if let Err(error) = std::fs::remove_file(&self.download.part)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(%error, "could not delete a partial track");
        }
        self.download.update(|state| state.phase = Phase::Failed);
    }
}

/// Downloads the whole track of `download` into the cache.
pub(super) async fn run(download: Arc<Download>, upstream: Arc<Upstream>, cache: Arc<AudioCache>) {
    let mut finish = Finish {
        download: Arc::clone(&download),
        done: false,
    };
    match fetch(&download, &upstream, &cache).await {
        Ok(()) => {
            finish.done = true;
            download.update(|state| state.phase = Phase::Complete);
        }
        Err(reason) => tracing::debug!(reason, "track download stopped"),
    }
    drop(finish);
    upstream.download_finished(&download.song);
}

async fn fetch(
    download: &Download,
    upstream: &Upstream,
    cache: &Arc<AudioCache>,
) -> Result<(), &'static str> {
    let mut answer = send(upstream, &download.song, None)
        .await
        .map_err(|_| "server unreachable")?;
    if answer.status() != StatusCode::OK {
        return Err("server refused the track");
    }
    let total = answer.content_length().ok_or("no track length")?;
    if total == 0 || total > MAX_TRACK_BYTES {
        return Err("track too large to cache");
    }
    let content_type = audio_content_type(answer.headers());

    let file = {
        let cache = Arc::clone(cache);
        let song = download.song.clone();
        tokio::task::spawn_blocking(move || cache.create_partial(&song))
            .await
            .map_err(|_| "cache task failed")?
            .map_err(|error| {
                tracing::debug!(%error, "cannot create a partial track");
                "cannot write the cache"
            })?
    };
    let mut file = tokio::fs::File::from_std(file);
    download.update(|state| {
        state.phase = Phase::Running;
        state.total = Some(total);
        state.content_type = Some(content_type.clone());
    });

    let mut written = 0u64;
    while let Some(chunk) = answer.chunk().await.map_err(|_| "download interrupted")? {
        let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        if written.saturating_add(len) > total {
            return Err("the server sent more than it announced");
        }
        // Flushed per chunk: readers open the file separately and must see
        // every byte the span says is there.
        file.write_all(&chunk)
            .await
            .map_err(|_| "cache write failed")?;
        file.flush().await.map_err(|_| "cache write failed")?;
        let start = written;
        written += len;
        download.update(|state| {
            state.add_span(start, written);
            state.frontier = written;
        });
    }
    if written != total {
        return Err("the download ended early");
    }
    file.sync_all().await.map_err(|_| "cache write failed")?;
    drop(file);

    let cache = Arc::clone(cache);
    let song = download.song.clone();
    tokio::task::spawn_blocking(move || cache.commit(&song, &content_type, total))
        .await
        .map_err(|_| "cache task failed")?
        .map_err(|error| {
            tracing::debug!(%error, "a downloaded track was not kept");
            "track not kept"
        })?;
    Ok(())
}

/// Fetches the track from `start` on into the partial file, until the bytes
/// ahead are on disk already or the download ends.
async fn fetch_range(
    download: Arc<Download>,
    upstream: Arc<Upstream>,
    start: u64,
    progress: Arc<std::sync::atomic::AtomicU64>,
) {
    let range = format!("bytes={start}-");
    let Ok(mut answer) = send(&upstream, &download.song, Some(&range)).await else {
        return;
    };
    if answer.status() != StatusCode::PARTIAL_CONTENT
        || content_range(answer.headers()).map(|(first, _)| first) != Some(start)
    {
        tracing::debug!("the server cannot send a part of the track");
        return;
    }
    let Some(mut tee) = Tee::open(Arc::clone(&download), start).await else {
        return;
    };
    while let Ok(Some(chunk)) = answer.chunk().await {
        if tee.write(&chunk).await.is_err() {
            return;
        }
        progress.store(tee.pos, std::sync::atomic::Ordering::SeqCst);
        let state = download.state();
        // Caught up with bytes already on disk, or the download is over.
        if state.phase != Phase::Running || state.covered_until(tee.pos) > tee.pos {
            return;
        }
    }
}

/// Writes the bytes of a ranged request into the partial file.
pub(super) struct Tee {
    download: Arc<Download>,
    file: tokio::fs::File,
    pos: u64,
}

impl Tee {
    /// Opens the partial file of `download` at `start`. `None` when there is no
    /// partial file any more (the download finished or failed).
    pub(super) async fn open(download: Arc<Download>, start: u64) -> Option<Self> {
        if download.state().phase != Phase::Running {
            return None;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&download.part)
            .await
            .ok()?;
        file.seek(std::io::SeekFrom::Start(start)).await.ok()?;
        Some(Self {
            download,
            file,
            pos: start,
        })
    }

    /// Writes `chunk`. On an error the tee stops; the request itself goes on.
    pub(super) async fn write(&mut self, chunk: &[u8]) -> Result<(), ()> {
        self.file.write_all(chunk).await.map_err(|_| ())?;
        self.file.flush().await.map_err(|_| ())?;
        let start = self.pos;
        self.pos += u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        self.download.add_span(start, self.pos);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State {
            phase: Phase::Running,
            total: Some(100),
            content_type: None,
            frontier: 0,
            spans: Vec::new(),
        }
    }

    #[test]
    fn spans_merge_and_report_what_is_on_disk() {
        let mut state = state();
        state.add_span(0, 10);
        state.add_span(50, 60);
        state.add_span(10, 20);
        assert_eq!(state.spans, vec![(0, 20), (50, 60)]);
        assert_eq!(state.covered_until(5), 20);
        assert_eq!(state.covered_until(20), 20, "byte 20 is not there");
        assert_eq!(state.covered_until(55), 60);
        state.add_span(15, 55);
        assert_eq!(state.spans, vec![(0, 60)]);
        state.add_span(70, 70);
        assert_eq!(state.spans, vec![(0, 60)], "an empty range adds nothing");
    }
}
