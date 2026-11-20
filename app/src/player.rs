//! The audio engine: GStreamer `playbin3` driven by the core play queue (#6).
//!
//! Everything here runs on the GTK main thread, where GStreamer's bus messages
//! are delivered. Two things come from GStreamer's own threads: the
//! `about-to-finish` signal, and the bus messages it raises. The signal only
//! reads a URI the main thread prepared earlier. Shared state is behind
//! `Arc`/`Mutex` for that one hand-off, and everything else stays on the main
//! thread.
//!
//! Tracks come from core's track sources through `appsrc` (#33): GStreamer
//! reads bytes in-process, never a URL, so it never sees a credential and
//! never seeks over HTTP.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use studio_core::queue::{PlayQueue, QueueError, SongId};
use studio_core::tempo::{Rate, replay_volume};
use studio_core::track_source::Tracks;

use crate::track_feed::Feeds;

/// What the engine tells the window. Delivered on the main thread.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlayerEvent {
    /// A new song started; the queue position is in the event.
    TrackChanged(usize),
    /// The last song finished.
    Finished,
    /// Playback failed. The text is for the user and carries no URL.
    Failed(String),
}

/// A song in the queue with its ReplayGain figures, from the library.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Track {
    pub(crate) song: SongId,
    /// Track gain in dB, when the library has one.
    pub(crate) gain_db: Option<f64>,
    /// Track peak as a linear value, when the library has one.
    pub(crate) peak: Option<f64>,
}

/// Errors from starting the engine or changing what plays.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlayerError {
    #[error("the audio engine could not start: {0}")]
    Start(String),
    #[error("{0}")]
    Queue(#[from] QueueError),
    #[error("there is no server to play from yet")]
    NoServer,
    #[error("the audio engine refused the change: {0}")]
    Engine(String),
}

/// State only the main thread reads or writes. The bus handler shares it.
struct Inner {
    queue: PlayQueue,
    tracks: Vec<Track>,
    /// The server's tracks; `None` until connected.
    source: Option<Rc<Tracks>>,
    feeds: Feeds,
    rate: Rate,
    /// The user paused. Buffering must not resume a pause the user asked for.
    user_paused: bool,
    on_event: Rc<dyn Fn(PlayerEvent)>,
}

impl Inner {
    fn emit(&self, event: PlayerEvent) {
        (self.on_event)(event);
    }

    fn uri_for(&self, index: usize) -> Result<String, PlayerError> {
        let tracks = self.source.as_ref().ok_or(PlayerError::NoServer)?;
        let song = self
            .queue
            .songs()
            .get(index)
            .ok_or(QueueError::NoSuchIndex)?;
        Ok(self.feeds.uri(tracks, song))
    }

    /// Keeps the cache downloads of the current and next song only, and
    /// fetches the next song once the current one is complete (#30).
    fn cache_ahead(&self) {
        let Some(tracks) = &self.source else { return };
        let Some(current) = self.queue.current_index() else {
            return;
        };
        let songs = self.queue.songs();
        let keep: Vec<SongId> = songs.iter().skip(current).take(2).cloned().collect();
        tracks.retain(&keep);
        if let Some(next) = keep.get(1) {
            tracks.prefetch(next);
        }
    }

    /// The URI of the song after the current one, or nothing at the end.
    fn next_uri(&self) -> Option<String> {
        let next = self.queue.current_index()? + 1;
        if next >= self.queue.len() {
            return None;
        }
        self.uri_for(next).ok()
    }
}

pub(crate) struct Player {
    playbin: gst::Element,
    inner: Rc<RefCell<Inner>>,
    /// The URI queued for gapless playback. The main thread writes it; the
    /// streaming thread takes it.
    prepared_next: Arc<Mutex<Option<String>>>,
    /// Set by the streaming thread when it has switched to the prepared song.
    switched: Arc<AtomicBool>,
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Player {
    /// Builds the pipeline. Must run on the main thread.
    pub(crate) fn new(on_event: impl Fn(PlayerEvent) + 'static) -> Result<Self, PlayerError> {
        gst::init().map_err(|err| PlayerError::Start(err.to_string()))?;
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|err| PlayerError::Start(err.to_string()))?;
        // scaletempo changes speed without changing pitch (#6).
        let scaletempo = gst::ElementFactory::make("scaletempo")
            .build()
            .map_err(|err| PlayerError::Start(format!("scaletempo: {err}")))?;
        playbin.set_property("audio-filter", &scaletempo);
        // Apply a new uri at once, so the switch in about-to-finish is not
        // deferred to end of stream (GStreamer 1.22 and later).
        playbin.set_property("instant-uri", true);

        let inner = Rc::new(RefCell::new(Inner {
            queue: PlayQueue::new(),
            tracks: Vec::new(),
            source: None,
            feeds: Feeds::default(),
            rate: Rate::NORMAL,
            user_paused: false,
            on_event: Rc::new(on_event),
        }));
        let prepared_next: Arc<Mutex<Option<String>>> = Arc::default();
        let switched = Arc::new(AtomicBool::new(false));

        // Streaming thread: playbin made the appsrc for a track; feed it.
        {
            let feeds = inner.borrow().feeds.clone();
            playbin.connect("source-setup", false, move |values| {
                if let Some(source) = values.get(1).and_then(|v| v.get::<gst::Element>().ok()) {
                    feeds.attach(&source);
                }
                None
            });
        }

        // Streaming thread: switch to the prepared URI without a gap. Weak, so
        // the pipeline does not keep itself alive through this handler.
        {
            let weak = playbin.downgrade();
            let prepared = Arc::clone(&prepared_next);
            let switched = Arc::clone(&switched);
            playbin.connect("about-to-finish", false, move |_| {
                let next = prepared.lock().ok().and_then(|mut slot| slot.take());
                if let (Some(next), Some(playbin)) = (next, weak.upgrade()) {
                    playbin.set_property("uri", &next);
                    switched.store(true, Ordering::SeqCst);
                }
                None
            });
        }

        let bus = playbin
            .bus()
            .ok_or_else(|| PlayerError::Start("no message bus".to_owned()))?;
        let watch = {
            let weak = playbin.downgrade();
            let inner = Rc::clone(&inner);
            let prepared = Arc::clone(&prepared_next);
            let switched = Arc::clone(&switched);
            bus.add_watch_local(move |_, message| {
                match message.view() {
                    gst::MessageView::StreamStart(_) => {
                        let switched_now = switched.swap(false, Ordering::SeqCst);
                        if let (true, Some(playbin)) = (switched_now, weak.upgrade()) {
                            on_gapless_switch(&playbin, &inner, &prepared);
                        }
                    }
                    gst::MessageView::Buffering(buffering) => {
                        // Network sources: hold playback until the buffer is full.
                        if !inner.borrow().user_paused
                            && let Some(playbin) = weak.upgrade()
                        {
                            let state = if buffering.percent() < 100 {
                                gst::State::Paused
                            } else {
                                gst::State::Playing
                            };
                            let _ = playbin.set_state(state);
                        }
                    }
                    gst::MessageView::Eos(_) => {
                        let emit = inner.borrow().on_event.clone();
                        emit(PlayerEvent::Finished);
                    }
                    gst::MessageView::Error(err) => {
                        // Only the plain text, not GStreamer's debug details.
                        let emit = inner.borrow().on_event.clone();
                        emit(PlayerEvent::Failed(err.error().to_string()));
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            })
            .map_err(|err| PlayerError::Start(err.to_string()))?
        };

        Ok(Self {
            playbin,
            inner,
            prepared_next,
            switched,
            _bus_watch: watch,
        })
    }

    /// Gives the engine the server's tracks.
    pub(crate) fn set_tracks(&mut self, tracks: Tracks) {
        self.inner.borrow_mut().source = Some(Rc::new(tracks));
    }

    /// Replaces the queue and starts playing at `start`.
    pub(crate) fn load(&mut self, tracks: Vec<Track>, start: usize) -> Result<(), PlayerError> {
        {
            let mut inner = self.inner.borrow_mut();
            let songs = tracks.iter().map(|t| t.song.clone()).collect();
            inner.queue.replace(songs, start)?;
            inner.tracks = tracks;
        }
        self.play_current()
    }

    /// The queue position of the song that plays.
    pub(crate) fn current_index(&self) -> Option<usize> {
        self.inner.borrow().queue.current_index()
    }

    /// The number of songs in the queue.
    pub(crate) fn queue_len(&self) -> usize {
        self.inner.borrow().queue.len()
    }

    /// Starts the current song from the beginning and prepares the next one.
    fn play_current(&mut self) -> Result<(), PlayerError> {
        let (uri, index, rate) = {
            let inner = self.inner.borrow();
            let index = inner.queue.current_index().ok_or(QueueError::NoSuchIndex)?;
            (inner.uri_for(index)?, index, inner.rate)
        };
        // A new track plays unless the user pauses it again.
        self.inner.borrow_mut().user_paused = false;
        self.playbin
            .set_state(gst::State::Null)
            .map_err(|err| PlayerError::Engine(err.to_string()))?;
        self.playbin.set_property("uri", &uri);
        apply_replay_gain(&self.playbin, &self.inner.borrow(), index);
        set_prepared(&self.prepared_next, self.inner.borrow().next_uri());
        self.inner.borrow().cache_ahead();
        self.playbin
            .set_state(gst::State::Playing)
            .map_err(|err| PlayerError::Engine(err.to_string()))?;
        if rate != Rate::NORMAL {
            seek_with_rate(&self.playbin, 0, rate)?;
        }
        let emit = self.inner.borrow().on_event.clone();
        emit(PlayerEvent::TrackChanged(index));
        Ok(())
    }

    /// Moves to the next song now, or stops after the last one.
    pub(crate) fn next(&mut self) -> Result<(), PlayerError> {
        let advanced = self.inner.borrow_mut().queue.advance().is_some();
        if !advanced {
            self.stop();
            let emit = self.inner.borrow().on_event.clone();
            emit(PlayerEvent::Finished);
            return Ok(());
        }
        self.play_current()
    }

    /// Moves to the previous song, or restarts the current one when it is the
    /// first.
    pub(crate) fn previous(&mut self) -> Result<(), PlayerError> {
        let moved = self.inner.borrow_mut().queue.back().is_some();
        if !moved {
            return self.seek_to(0);
        }
        self.play_current()
    }

    pub(crate) fn pause(&self) -> Result<(), PlayerError> {
        self.inner.borrow_mut().user_paused = true;
        self.playbin
            .set_state(gst::State::Paused)
            .map_err(|err| PlayerError::Engine(err.to_string()))?;
        Ok(())
    }

    pub(crate) fn resume(&self) -> Result<(), PlayerError> {
        self.inner.borrow_mut().user_paused = false;
        self.playbin
            .set_state(gst::State::Playing)
            .map_err(|err| PlayerError::Engine(err.to_string()))?;
        Ok(())
    }

    /// Stops playback. The queue is kept.
    pub(crate) fn stop(&self) {
        set_prepared(&self.prepared_next, None);
        // Stopping can only fail if the pipeline is already gone, which
        // leaves nothing to play anyway.
        let _ = self.playbin.set_state(gst::State::Null);
    }

    /// Seeks within the current song, in milliseconds.
    pub(crate) fn seek_to(&self, position_ms: u64) -> Result<(), PlayerError> {
        let rate = self.inner.borrow().rate;
        seek_with_rate(&self.playbin, position_ms, rate)
    }

    /// Position and length of the song, in milliseconds.
    pub(crate) fn position_ms(&self) -> Option<(u64, Option<u64>)> {
        let position = self.playbin.query_position::<gst::ClockTime>()?;
        let duration = self
            .playbin
            .query_duration::<gst::ClockTime>()
            .map(|d| d.mseconds());
        Some((position.mseconds(), duration))
    }

    /// Changes the speed from here on. The pitch stays the same (`scaletempo`).
    pub(crate) fn set_rate(&mut self, rate: Rate) -> Result<(), PlayerError> {
        self.inner.borrow_mut().rate = rate;
        let position = self.position_ms().map_or(0, |(p, _)| p);
        seek_with_rate(&self.playbin, position, rate)
    }

    /// The speed the engine plays at.
    pub(crate) fn rate(&self) -> Rate {
        self.inner.borrow().rate
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

fn set_prepared(slot: &Arc<Mutex<Option<String>>>, uri: Option<String>) {
    if let Ok(mut slot) = slot.lock() {
        *slot = uri;
    }
}

/// Main thread, after the streaming thread switched to the prepared song:
/// moves the queue on, applies that song's gain and prepares the one after.
fn on_gapless_switch(
    playbin: &gst::Element,
    inner: &Rc<RefCell<Inner>>,
    prepared: &Arc<Mutex<Option<String>>>,
) {
    let (index, emit) = {
        let mut state = inner.borrow_mut();
        if state.queue.advance().is_none() {
            return;
        }
        let index = state.queue.current_index().unwrap_or(0);
        apply_replay_gain(playbin, &state, index);
        set_prepared(prepared, state.next_uri());
        state.cache_ahead();
        (index, state.on_event.clone())
    };
    emit(PlayerEvent::TrackChanged(index));
}

fn apply_replay_gain(playbin: &gst::Element, inner: &Inner, index: usize) {
    let Some(track) = inner.tracks.get(index) else {
        return;
    };
    playbin.set_property("volume", replay_volume(track.gain_db, track.peak));
}

/// Seeks to `position_ms` at `rate`. The event goes to the audio sink, not to
/// playbin, so one seek is performed (GStreamer's playback-speed guidance).
/// Falls back to playbin before the sink exists.
fn seek_with_rate(playbin: &gst::Element, position_ms: u64, rate: Rate) -> Result<(), PlayerError> {
    let target = playbin
        .property::<Option<gst::Element>>("audio-sink")
        .unwrap_or_else(|| playbin.clone());
    let event = gst::event::Seek::new(
        rate.factor(),
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
        gst::SeekType::Set,
        gst::ClockTime::from_mseconds(position_ms),
        gst::SeekType::None,
        gst::ClockTime::NONE,
    );
    if target.send_event(event) {
        Ok(())
    } else {
        Err(PlayerError::Engine("the seek was refused".to_owned()))
    }
}

/// End-to-end check of the track feed (#33): a real GStreamer pipeline plays a
/// local Opus file that a small range server sends, through the track source
/// and `appsrc`. Opt-in: needs the GStreamer plugins and `TRACK`, the path of
/// an Ogg/Opus file of at least three minutes.
#[cfg(test)]
mod feed_check {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};
    use studio_core::audio_cache::AudioCache;
    use studio_core::connection::{HttpPolicy, ServerUrl};
    use studio_core::db::Database;
    use studio_core::http::{HttpSettings, streaming_client};
    use studio_core::subsonic::{ApiKey, Auth};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn pump(millis: u64) {
        let context = gst::glib::MainContext::default();
        let _owner = context.acquire().expect("main context");
        let end = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < end {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn position(player: &Player) -> Option<u64> {
        for _ in 0..50 {
            if let Some((p, _)) = player.position_ms() {
                return Some(p);
            }
            pump(20);
        }
        None
    }

    fn wait_async_done(counter: &AtomicU64, seen: u64) -> bool {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if counter.load(Ordering::SeqCst) > seen {
                return true;
            }
            pump(5);
        }
        false
    }

    /// Serves `body` for any song, honouring `Range: bytes=a-`. Counts requests.
    fn range_server(
        rt: &tokio::runtime::Runtime,
        body: Arc<Vec<u8>>,
    ) -> (ServerUrl, Arc<AtomicU64>) {
        let listener = rt
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", 0)))
            .expect("bind");
        let port = listener.local_addr().expect("address").port();
        let requests = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&requests);
        rt.spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = Arc::clone(&body);
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    let head = String::from_utf8_lossy(&head).to_lowercase();
                    let start = head
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| r.split('-').next())
                        .and_then(|s| s.parse::<usize>().ok());
                    let len = body.len();
                    let answer = match start {
                        Some(first) => format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/ogg\r\n\
                             Content-Length: {}\r\nContent-Range: bytes {first}-{}/{len}\r\n\
                             Connection: close\r\n\r\n",
                            len - first,
                            len - 1
                        ),
                        None => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/ogg\r\n\
                             Content-Length: {len}\r\nConnection: close\r\n\r\n"
                        ),
                    };
                    if socket.write_all(answer.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = socket.write_all(&body[start.unwrap_or(0)..]).await;
                });
            }
        });
        let url = ServerUrl::parse(
            &format!("http://127.0.0.1:{port}"),
            HttpPolicy::ALLOW_PRIVATE_HTTP,
        )
        .expect("url");
        (url, requests)
    }

    #[test]
    #[ignore = "local: needs the GStreamer plugins and TRACK (an Ogg/Opus file)"]
    fn a_track_plays_seeks_and_switches_through_the_feed() {
        let path = std::env::var("TRACK").expect("TRACK");
        let body = Arc::new(std::fs::read(&path).expect("read TRACK"));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let (server, requests) = range_server(&rt, body);
        let dir = std::env::temp_dir().join(format!("feed-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Arc::new(Database::open(dir.join("library.db")).expect("db"));
        let cache = Arc::new(AudioCache::open(dir.join("audio"), db, &server).expect("cache"));
        let http = streaming_client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .expect("client");
        let auth = Auth::ApiKey(ApiKey::new("tsk_test_key_12345".to_owned()).expect("key"));
        let tracks = Tracks::new(
            http,
            server,
            auth,
            Some(Arc::clone(&cache)),
            rt.handle().clone(),
        )
        .expect("tracks");

        let started = Instant::now();
        let mut player = Player::new(move |event| {
            println!("EVENT at {:>6} ms {event:?}", started.elapsed().as_millis());
        })
        .expect("player");
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", true)
            .build()
            .expect("fakesink");
        player.playbin.set_property("audio-sink", &sink);
        let async_done = Arc::new(AtomicU64::new(0));
        {
            let async_done = Arc::clone(&async_done);
            player
                .playbin
                .bus()
                .expect("bus")
                .set_sync_handler(move |_, message| {
                    if let gst::MessageView::AsyncDone(_) = message.view() {
                        async_done.fetch_add(1, Ordering::SeqCst);
                    }
                    gst::BusSyncReply::Pass
                });
        }
        player.set_tracks(tracks);
        let songs = ["so-1", "so-2"]
            .iter()
            .map(|id| Track {
                song: SongId::parse(id).expect("id"),
                gain_db: None,
                peak: None,
            })
            .collect();
        player.load(songs, 0).expect("load");
        pump(2000);
        let mut failed = Vec::new();
        let first = position(&player);
        println!("PLAY after 2 s: {first:?}");
        if !first.is_some_and(|p| p > 1000) {
            failed.push("playback starts".to_owned());
        }

        let total = player.position_ms().and_then(|(_, d)| d).expect("duration");
        let seeks = |player: &Player, label: &str, failed: &mut Vec<String>| {
            for target in [5_000, 60_000, 120_000, total / 2, total - 15_000, 31_337] {
                let seen = async_done.load(Ordering::SeqCst);
                player.seek_to(target).expect("seek");
                let done = wait_async_done(&async_done, seen);
                let landed = position(player);
                pump(1000);
                let later = position(player);
                println!(
                    "SEEK {label} {target:>7}: done {done}, landed {landed:?}, 1 s later {later:?}"
                );
                let ok = done
                    && landed.is_some_and(|l| l.abs_diff(target) < 300)
                    && matches!((landed, later), (Some(a), Some(b)) if b > a && (b - a).abs_diff(1000) < 300);
                if !ok {
                    failed.push(format!("seek {label} {target}"));
                }
            }
        };
        seeks(&player, "while downloading", &mut failed);

        for (i, percent) in [50.0, 75.0, 110.0, 125.0, 150.0, 100.0]
            .iter()
            .cycle()
            .take(18)
            .enumerate()
        {
            let rate = Rate::from_percent(*percent).expect("rate");
            let seen = async_done.load(Ordering::SeqCst);
            player.set_rate(rate).expect("rate");
            let done = wait_async_done(&async_done, seen);
            let a = position(&player);
            pump(1000);
            let b = position(&player);
            let speed = match (a, b) {
                (Some(a), Some(b)) => (b as f64 - a as f64) / 1000.0,
                _ => 0.0,
            };
            let ok = done && (speed - percent / 100.0).abs() <= 0.1 * percent / 100.0;
            println!("RATE {i:>2} {percent:>5} %: done {done}, speed {speed:.3}");
            if !ok {
                failed.push(format!("rate {i} {percent}"));
            }
        }
        player.set_rate(Rate::NORMAL).expect("rate");

        for _ in 0..200 {
            if cache
                .complete(&SongId::parse("so-1").expect("id"))
                .is_some()
            {
                break;
            }
            pump(50);
        }
        let cached = cache
            .complete(&SongId::parse("so-1").expect("id"))
            .is_some();
        println!(
            "CACHE so-1 complete: {cached}; requests so far {}",
            requests.load(Ordering::SeqCst)
        );
        if !cached {
            failed.push("track cached".to_owned());
        }
        let before = requests.load(Ordering::SeqCst);
        seeks(&player, "from the cache", &mut failed);
        let after = requests.load(Ordering::SeqCst);
        println!("CACHE requests during cached seeks: {}", after - before);

        let seen_index = player.current_index();
        let _ = player.seek_to(total - 4_000);
        let waiting = Instant::now();
        while waiting.elapsed() < Duration::from_secs(15) && player.current_index() == seen_index {
            pump(50);
        }
        let switched = player.current_index() == Some(1);
        pump(2000);
        let plays = position(&player).is_some_and(|p| p > 1000);
        println!(
            "GAPLESS switched {switched} after {} ms, plays {plays}",
            waiting.elapsed().as_millis()
        );
        if !(switched && plays) {
            failed.push("gapless".to_owned());
        }
        player.stop();
        drop(player);
        let _ = std::fs::remove_dir_all(&dir);
        println!("SUMMARY failed {failed:?}");
        assert!(failed.is_empty(), "{failed:?}");
    }
}
