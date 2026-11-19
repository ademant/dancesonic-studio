//! The audio engine: GStreamer `playbin3` driven by the core play queue (#6).
//!
//! Everything here runs on the GTK main thread, where GStreamer's bus messages
//! are delivered. Two things come from GStreamer's own threads: the
//! `about-to-finish` signal, and the bus messages it raises. The signal only
//! reads a URI the main thread prepared earlier. Shared state is behind
//! `Arc`/`Mutex` for that one hand-off, and everything else stays on the main
//! thread.
//!
//! Streams come from the loopback proxy in core, so the player never holds a
//! credential and GStreamer never sees a key.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;
use studio_core::queue::{PlayQueue, QueueError, SongId};
use studio_core::stream_proxy::StreamProxy;
use studio_core::tempo::{Rate, replay_volume};

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
    #[error("there is no stream proxy for this server yet")]
    NoProxy,
    #[error("the audio engine refused the change: {0}")]
    Engine(String),
}

/// State only the main thread reads or writes. The bus handler shares it.
struct Inner {
    queue: PlayQueue,
    tracks: Vec<Track>,
    proxy: Option<Rc<StreamProxy>>,
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
        let proxy = self.proxy.as_ref().ok_or(PlayerError::NoProxy)?;
        let song = self
            .queue
            .songs()
            .get(index)
            .ok_or(QueueError::NoSuchIndex)?;
        proxy
            .stream_url(song)
            .map_err(|_| PlayerError::Engine("the stream address".to_owned()))
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
            proxy: None,
            rate: Rate::NORMAL,
            user_paused: false,
            on_event: Rc::new(on_event),
        }));
        let prepared_next: Arc<Mutex<Option<String>>> = Arc::default();
        let switched = Arc::new(AtomicBool::new(false));

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
                        // Only the plain text: GStreamer can name the URI in
                        // its debug details, and the URI carries the proxy token.
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

    /// Gives the engine the proxy that turns song ids into playable URLs.
    pub(crate) fn set_proxy(&mut self, proxy: StreamProxy) {
        self.inner.borrow_mut().proxy = Some(Rc::new(proxy));
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
