//! MPRIS (#7): the desktop's media controls, media keys, and KDE Connect or
//! GSConnect on a phone drive the player through D-Bus.
//!
//! The D-Bus server runs on the Tokio runtime. It answers property reads
//! from a shared snapshot that the window keeps up to date, and hands method
//! calls to the window as [`Command`]s through a channel, so the player is
//! only ever touched on the main thread.

use std::sync::{Arc, Mutex, PoisonError};

use mpris_server::zbus::{Result as ZResult, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, PlayerInterface, Property, RootInterface,
    Server, Signal, Time, TrackId, Volume,
};
use studio_core::tempo::{MAX_RATE, MIN_RATE};

/// The bus name is `org.mpris.MediaPlayer2.<this>`.
const BUS_NAME_SUFFIX: &str = "DanceSonicStudio";

/// Prefix of the track ids: one per queue position.
const TRACK_PATH: &str = "/org/dancesonic/Studio/track/";

/// What a remote control asked for.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Command {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    /// Move by this many milliseconds; negative is backwards.
    SeekBy(i64),
    /// Go to this position (ms) in the track at this queue position.
    SetPosition {
        index: usize,
        position_ms: u64,
    },
    SetRate(f64),
    SetVolume(f64),
    Raise,
    Quit,
}

/// The now-playing facts the window reports.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NowPlaying {
    pub(crate) index: usize,
    pub(crate) title: String,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) length_ms: Option<u64>,
    /// A `file://` URL of the cover.
    pub(crate) art_url: Option<String>,
}

#[derive(Debug)]
struct State {
    status: PlaybackStatus,
    track: Option<NowPlaying>,
    position_ms: u64,
    rate: f64,
    volume: f64,
    can_next: bool,
    can_previous: bool,
}

/// The window's handle on the MPRIS server. Dropping it ends the server and
/// releases the bus name.
pub(crate) struct Mpris {
    state: Arc<Mutex<State>>,
    changes: async_channel::Sender<Change>,
}

enum Change {
    Properties(Vec<Property>),
    Seeked(u64),
}

impl Mpris {
    /// Starts the server on `runtime`. Commands arrive on the returned
    /// receiver. A desktop without a session bus only logs a warning.
    pub(crate) fn start(
        runtime: &tokio::runtime::Runtime,
    ) -> (Self, async_channel::Receiver<Command>) {
        let state = Arc::new(Mutex::new(State {
            status: PlaybackStatus::Stopped,
            track: None,
            position_ms: 0,
            rate: 1.0,
            volume: 1.0,
            can_next: false,
            can_previous: false,
        }));
        let (commands, received) = async_channel::unbounded();
        let (changes, pending) = async_channel::unbounded::<Change>();
        let imp = Imp {
            state: Arc::clone(&state),
            commands,
        };
        runtime.spawn(async move {
            let server = match Server::new(BUS_NAME_SUFFIX, imp).await {
                Ok(server) => server,
                Err(error) => {
                    tracing::warn!(%error, "media controls (MPRIS) are not available");
                    return;
                }
            };
            tracing::info!("media controls (MPRIS) ready");
            // Ends when the window drops its handle.
            while let Ok(change) = pending.recv().await {
                let sent = match change {
                    Change::Properties(properties) => server.properties_changed(properties).await,
                    Change::Seeked(ms) => {
                        server
                            .emit(Signal::Seeked {
                                position: Time::from_millis(i64::try_from(ms).unwrap_or(i64::MAX)),
                            })
                            .await
                    }
                };
                if let Err(error) = sent {
                    tracing::debug!(%error, "MPRIS update not sent");
                }
            }
        });
        (Self { state, changes }, received)
    }

    fn change(&self, update: impl FnOnce(&mut State) -> Vec<Property>) {
        let properties = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            update(&mut state)
        };
        if !properties.is_empty() {
            // Ignored: closed only when the server could not start.
            let _ = self.changes.try_send(Change::Properties(properties));
        }
    }

    /// A new track, or none.
    pub(crate) fn set_track(&self, track: Option<NowPlaying>, can_previous: bool, can_next: bool) {
        self.change(|state| {
            state.track = track;
            state.position_ms = 0;
            state.can_next = can_next;
            state.can_previous = can_previous;
            vec![
                Property::Metadata(metadata(state.track.as_ref())),
                Property::CanGoNext(can_next),
                Property::CanGoPrevious(can_previous),
                Property::CanSeek(state.track.is_some()),
                Property::CanPause(state.track.is_some()),
            ]
        });
    }

    pub(crate) fn set_playing(&self, playing: Option<bool>) {
        self.change(|state| {
            let status = match playing {
                Some(true) => PlaybackStatus::Playing,
                Some(false) => PlaybackStatus::Paused,
                None => PlaybackStatus::Stopped,
            };
            if state.status == status {
                return Vec::new();
            }
            state.status = status;
            vec![Property::PlaybackStatus(status)]
        });
    }

    pub(crate) fn set_rate(&self, rate: f64) {
        self.change(|state| {
            if (state.rate - rate).abs() < f64::EPSILON {
                return Vec::new();
            }
            state.rate = rate;
            vec![Property::Rate(rate)]
        });
    }

    pub(crate) fn set_volume(&self, volume: f64) {
        self.change(|state| {
            state.volume = volume;
            vec![Property::Volume(volume)]
        });
    }

    /// The position as playback moves on; read on demand, no signal.
    pub(crate) fn set_position(&self, position_ms: u64) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .position_ms = position_ms;
    }

    /// The position jumped (a seek): tells the controls.
    pub(crate) fn seeked(&self, position_ms: u64) {
        self.set_position(position_ms);
        // Ignored as in `change`.
        let _ = self.changes.try_send(Change::Seeked(position_ms));
    }
}

/// The D-Bus side: reads the snapshot, forwards calls.
struct Imp {
    state: Arc<Mutex<State>>,
    commands: async_channel::Sender<Command>,
}

impl Imp {
    fn read<T>(&self, get: impl FnOnce(&State) -> T) -> T {
        get(&self.state.lock().unwrap_or_else(PoisonError::into_inner))
    }

    async fn send(&self, command: Command) -> fdo::Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| fdo::Error::Failed("the player has closed".to_owned()))
    }
}

fn track_id(index: usize) -> TrackId {
    TrackId::try_from(format!("{TRACK_PATH}{index}")).unwrap_or(TrackId::NO_TRACK)
}

/// The queue position named by a track id from a client, if it is ours.
fn index_of(track: &TrackId) -> Option<usize> {
    track.as_str().strip_prefix(TRACK_PATH)?.parse().ok()
}

fn metadata(track: Option<&NowPlaying>) -> Metadata {
    let Some(track) = track else {
        return Metadata::builder().trackid(TrackId::NO_TRACK).build();
    };
    let mut builder = Metadata::builder()
        .trackid(track_id(track.index))
        .title(track.title.clone());
    if let Some(artist) = &track.artist {
        builder = builder.artist([artist.clone()]);
    }
    if let Some(album) = &track.album {
        builder = builder.album(album.clone());
    }
    if let Some(length) = track.length_ms {
        builder = builder.length(Time::from_millis(i64::try_from(length).unwrap_or(i64::MAX)));
    }
    if let Some(art) = &track.art_url {
        builder = builder.art_url(art.clone());
    }
    builder.build()
}

fn not_supported() -> fdo::Error {
    fdo::Error::NotSupported("not supported by DanceSonic Studio".to_owned())
}

impl RootInterface for Imp {
    async fn raise(&self) -> fdo::Result<()> {
        self.send(Command::Raise).await
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.send(Command::Quit).await
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> ZResult<()> {
        Err(not_supported().into())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(studio_core::APP_NAME.to_owned())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok(studio_core::APP_ID.to_owned())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl PlayerInterface for Imp {
    async fn next(&self) -> fdo::Result<()> {
        self.send(Command::Next).await
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.send(Command::Previous).await
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(Command::Pause).await
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(Command::PlayPause).await
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.send(Command::Stop).await
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send(Command::Play).await
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.send(Command::SeekBy(offset.as_millis())).await
    }

    async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
        // The spec: ignore a stale track id or a position outside the track.
        let Some(index) = index_of(&track_id) else {
            return Ok(());
        };
        let current = self.read(|s| s.track.as_ref().map(|t| (t.index, t.length_ms)));
        let Some((playing, length)) = current else {
            return Ok(());
        };
        let Ok(position_ms) = u64::try_from(position.as_millis()) else {
            return Ok(());
        };
        if index != playing || length.is_some_and(|l| position_ms > l) {
            return Ok(());
        }
        self.send(Command::SetPosition { index, position_ms }).await
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(not_supported())
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(self.read(|s| s.status))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(LoopStatus::None)
    }

    async fn set_loop_status(&self, _loop_status: LoopStatus) -> ZResult<()> {
        Err(not_supported().into())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(self.read(|s| s.rate))
    }

    async fn set_rate(&self, rate: PlaybackRate) -> ZResult<()> {
        // The spec: a rate of 0 is a pause; others are clamped to the range.
        if rate == 0.0 {
            return Ok(self.send(Command::Pause).await?);
        }
        if !rate.is_finite() {
            return Ok(());
        }
        Ok(self
            .send(Command::SetRate(rate.clamp(MIN_RATE, MAX_RATE)))
            .await?)
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_shuffle(&self, _shuffle: bool) -> ZResult<()> {
        Err(not_supported().into())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(self.read(|s| metadata(s.track.as_ref())))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.read(|s| s.volume))
    }

    async fn set_volume(&self, volume: Volume) -> ZResult<()> {
        if !volume.is_finite() {
            return Ok(());
        }
        Ok(self
            .send(Command::SetVolume(volume.clamp(0.0, 1.0)))
            .await?)
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(self.read(|s| Time::from_millis(i64::try_from(s.position_ms).unwrap_or(i64::MAX))))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(MIN_RATE)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(MAX_RATE)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.read(|s| s.can_next))
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.read(|s| s.can_previous))
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.read(|s| s.track.is_some()))
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.read(|s| s.track.is_some()))
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imp() -> (Imp, async_channel::Receiver<Command>) {
        let (commands, received) = async_channel::unbounded();
        let state = Arc::new(Mutex::new(State {
            status: PlaybackStatus::Playing,
            track: Some(NowPlaying {
                index: 3,
                title: "An dro".into(),
                length_ms: Some(200_000),
                ..NowPlaying::default()
            }),
            position_ms: 0,
            rate: 1.0,
            volume: 1.0,
            can_next: true,
            can_previous: true,
        }));
        (Imp { state, commands }, received)
    }

    #[test]
    fn track_ids_name_the_queue_position() {
        assert_eq!(index_of(&track_id(3)), Some(3));
        assert_eq!(index_of(&TrackId::NO_TRACK), None);
        let foreign = TrackId::try_from("/org/other/track/3").unwrap();
        assert_eq!(index_of(&foreign), None);
    }

    #[tokio::test]
    async fn set_position_ignores_stale_tracks_and_positions_past_the_end() {
        let (imp, received) = imp();
        imp.set_position(track_id(2), Time::from_millis(1000))
            .await
            .unwrap();
        imp.set_position(track_id(3), Time::from_millis(300_000))
            .await
            .unwrap();
        imp.set_position(track_id(3), Time::from_millis(-5))
            .await
            .unwrap();
        assert!(received.is_empty());
        imp.set_position(track_id(3), Time::from_millis(1000))
            .await
            .unwrap();
        assert_eq!(
            received.try_recv().unwrap(),
            Command::SetPosition {
                index: 3,
                position_ms: 1000
            }
        );
    }

    #[tokio::test]
    async fn rates_are_clamped_and_zero_pauses() {
        let (imp, received) = imp();
        imp.set_rate(4.0).await.unwrap();
        imp.set_rate(0.1).await.unwrap();
        imp.set_rate(0.0).await.unwrap();
        imp.set_rate(f64::NAN).await.unwrap();
        let got: Vec<Command> = std::iter::from_fn(|| received.try_recv().ok()).collect();
        assert_eq!(
            got,
            [
                Command::SetRate(MAX_RATE),
                Command::SetRate(MIN_RATE),
                Command::Pause
            ]
        );
    }

    #[tokio::test]
    async fn volume_is_clamped_and_metadata_names_the_track() {
        let (imp, received) = imp();
        imp.set_volume(2.0).await.unwrap();
        assert_eq!(received.try_recv().unwrap(), Command::SetVolume(1.0));
        let meta = imp.metadata().await.unwrap();
        assert_eq!(meta.title(), Some("An dro"));
        assert_eq!(meta.trackid(), Some(track_id(3)));
        assert_eq!(meta.length(), Some(Time::from_millis(200_000)));
    }
}
