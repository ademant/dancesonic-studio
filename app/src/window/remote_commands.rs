//! The window's side of remote control (#20): carrying out a phone's
//! commands and telling the phones what plays.

use serde_json::{Value, json};
use studio_core::remote::hub::Reply;
use studio_core::remote::messages::{Command as RemoteCommand, MAX_SONGS, PlayState, Transport};
use tokio::sync::oneshot;

use super::*;
use crate::remote::{self, Remote};
use crate::remote_dialog;

/// How often the state goes out while nothing but the position changes.
const STATE_EVERY_MS: i64 = 5000;
/// The largest cover a phone gets.
const MAX_COVER_BYTES: usize = 256 * 1024;

impl Window {
    /// Starts remote control as the switches say.
    pub(super) fn start_remote(self: &Rc<Self>, services: &Services) {
        let weak = Rc::downgrade(self);
        let on_command: remote::OnCommand = Rc::new(move |command, reply| {
            if let Some(me) = weak.upgrade() {
                me.on_remote_command(command, reply);
            }
        });
        let remote = Remote::new(
            Arc::clone(&self.runtime),
            Arc::clone(&services.db),
            Arc::clone(&services.vault),
            self.window.clone(),
            on_command,
        );
        let weak = Rc::downgrade(self);
        remote.listen(Rc::new(move || {
            if let Some(me) = weak.upgrade() {
                me.update_remote_label();
            }
        }));
        remote.start();
        *self.remote.borrow_mut() = Some(remote);
    }

    /// The remote control dialog.
    pub(super) fn show_remote(self: &Rc<Self>) {
        match self.remote.borrow().as_ref() {
            Some(remote) => remote_dialog::present(&self.window, remote),
            None => self
                .banner
                .show("Studio is still starting. Try again in a moment."),
        }
    }

    /// Shows which phone controls Studio, and over what.
    fn update_remote_label(&self) {
        let sessions = self
            .remote
            .borrow()
            .as_ref()
            .map(|r| r.sessions())
            .unwrap_or_default();
        let label = &self.bar.remote;
        match sessions.last() {
            Some(last) => {
                let short = match last.transport {
                    Transport::Ble => "BT",
                    Transport::Network => "Wi-Fi",
                    Transport::Relay => "Relay",
                };
                let over = crate::remote::transport_phrase(last.transport);
                label.set_text(short);
                label.set_tooltip_text(Some(&format!("Controlled by {} {over}", last.name)));
                label.set_visible(true);
            }
            None => label.set_visible(false),
        }
    }

    /// Sends the playback state to the phones when it changed, and every few
    /// seconds while playing.
    pub(super) fn publish_remote(&self) {
        let Some(remote) = self.remote.borrow().clone() else {
            return;
        };
        let position = self.player.borrow().as_ref().and_then(Player::position_ms);
        let current = self.current_song();
        let rate = self.rate.get().factor();
        let state = PlayState {
            playing: self.playing.get(),
            song: current.as_ref().map(|s| s.id.clone()),
            index: self.now_playing.get(),
            position_ms: position.map_or(0, |(p, _)| p),
            length_ms: position
                .and_then(|(_, l)| l)
                .or_else(|| current.as_ref().and_then(|s| s.duration_ms)),
            rate,
            bpm: self.current_bpm().map(|b| b * rate),
            queue: self
                .queue
                .borrow()
                .iter()
                .take(MAX_SONGS)
                .map(|s| s.id.clone())
                .collect(),
            dance: None,
        };
        let now = studio_core::sync::now_ms();
        let mut last = self.published.borrow_mut();
        let changed = last.0.as_ref().is_none_or(|old| {
            PlayState {
                position_ms: 0,
                ..old.clone()
            } != PlayState {
                position_ms: 0,
                ..state.clone()
            }
        });
        if changed || (state.playing && now - last.1 >= STATE_EVERY_MS) {
            *last = (Some(state.clone()), now);
            drop(last);
            remote.publish(state);
        }
    }

    /// Carries out an authenticated command from a phone.
    pub(super) fn on_remote_command(
        self: &Rc<Self>,
        command: RemoteCommand,
        reply: oneshot::Sender<Reply>,
    ) {
        tracing::debug!(feature = command.feature(), "remote command");
        let done: Option<Reply> = match command {
            RemoteCommand::Play => {
                if !self.playing.get() {
                    self.toggle_play();
                }
                Some(Ok(None))
            }
            RemoteCommand::Pause => {
                if self.playing.get() {
                    self.toggle_play();
                }
                Some(Ok(None))
            }
            RemoteCommand::Toggle => {
                self.toggle_play();
                Some(Ok(None))
            }
            RemoteCommand::Stop => {
                self.on_command(Command::Stop);
                Some(Ok(None))
            }
            RemoteCommand::Next => {
                self.on_command(Command::Next);
                Some(Ok(None))
            }
            RemoteCommand::Previous => {
                self.on_command(Command::Previous);
                Some(Ok(None))
            }
            RemoteCommand::Seek { position_ms } => Some(if self.now_playing.get().is_some() {
                self.seek(position_ms);
                Ok(None)
            } else {
                Err("failed")
            }),
            RemoteCommand::SetRate { rate } => {
                self.bar.tempo_mode.set_active(true);
                self.set_rate(rate);
                Some(Ok(None))
            }
            RemoteCommand::SetBpm { bpm } => Some(match self.current_bpm() {
                Some(original) => {
                    self.bar.bpm_mode.set_active(true);
                    self.set_rate(bpm / original);
                    Ok(None)
                }
                None => Err("failed"),
            }),
            RemoteCommand::PlaySongs { songs, start } => {
                return self.remote_play(Selection::Ids(songs), Some(start), reply);
            }
            RemoteCommand::Enqueue { songs } => {
                return self.remote_enqueue(songs, reply);
            }
            RemoteCommand::PlayDance { tag_id } => {
                return self.remote_play(Selection::Dance(tag_id), Some(0), reply);
            }
            RemoteCommand::NextOfDance { tag_id } => {
                return self.remote_play(Selection::Dance(tag_id), None, reply);
            }
            RemoteCommand::PlayPlaylist { playlist, start } => {
                return self.remote_play(Selection::Playlist(playlist), Some(start), reply);
            }
            RemoteCommand::GetState => Some(Ok(None)),
            RemoteCommand::LibraryArtists
            | RemoteCommand::LibraryPlaylists
            | RemoteCommand::LibraryDances
            | RemoteCommand::LibraryAlbums { .. } => {
                return self.remote_catalog(command, reply);
            }
            RemoteCommand::LibrarySongs {
                album,
                artist,
                playlist,
                dance,
                query,
            } => {
                let selection = match (album, artist, playlist, dance, query) {
                    (Some(a), ..) => Selection::Album(a),
                    (_, Some(a), ..) => Selection::Artist(a),
                    (_, _, Some(p), ..) => Selection::Playlist(p),
                    (_, _, _, Some(d), _) => Selection::Dance(d),
                    (.., Some(q)) => Selection::Search(q),
                    _ => Selection::Ids(Vec::new()),
                };
                return self.remote_songs(selection, reply);
            }
            RemoteCommand::Cover { id } => {
                return self.remote_cover(id, reply);
            }
            _ => Some(Err("failed")),
        };
        if let Some(done) = done {
            // The phone may have gone; nobody waits then.
            let _ = reply.send(done);
        }
        self.publish_remote();
    }

    fn session_or_fail(
        &self,
        reply: oneshot::Sender<Reply>,
    ) -> Option<(Session, oneshot::Sender<Reply>)> {
        match self.session.borrow().clone() {
            Some(session) => Some((session, reply)),
            None => {
                let _ = reply.send(Err("failed"));
                None
            }
        }
    }

    /// Plays the songs of `selection` from `start`, or with `None` plays one
    /// of them next, now, without dropping the queue.
    fn remote_play(
        self: &Rc<Self>,
        selection: Selection,
        start: Option<usize>,
        reply: oneshot::Sender<Reply>,
    ) {
        let Some((session, reply)) = self.session_or_fail(reply) else {
            return;
        };
        self.spawn(
            async move { session.songs(selection).await },
            move |me, songs| {
                let answer = match songs {
                    Ok(songs) if !songs.is_empty() => match start {
                        Some(start) => me.load_songs(songs, start),
                        None => me.play_next_of(songs),
                    },
                    Ok(_) => Err("unknown_song"),
                    Err(_) => Err("failed"),
                };
                let _ = reply.send(answer.map(|()| None));
                me.publish_remote();
            },
        );
    }

    fn remote_enqueue(self: &Rc<Self>, ids: Vec<String>, reply: oneshot::Sender<Reply>) {
        let Some((session, reply)) = self.session_or_fail(reply) else {
            return;
        };
        self.spawn(
            async move { session.songs(Selection::Ids(ids)).await },
            move |me, songs| {
                let answer = match songs {
                    Ok(songs) if !songs.is_empty() => {
                        if me.now_playing.get().is_none() {
                            // Nothing queued: the songs start playing.
                            me.load_songs(songs, 0)
                        } else {
                            let tracks: Vec<Track> = songs.iter().filter_map(track_of).collect();
                            me.queue.borrow_mut().extend(songs);
                            me.with_player(|p| p.append(tracks));
                            Ok(())
                        }
                    }
                    Ok(_) => Err("unknown_song"),
                    Err(_) => Err("failed"),
                };
                let _ = reply.send(answer.map(|()| None));
                me.publish_remote();
            },
        );
    }

    /// Replaces the queue with `songs` and plays from `start`.
    fn load_songs(self: &Rc<Self>, songs: Vec<Song>, start: usize) -> Result<(), &'static str> {
        self.load_songs_at(songs, start, 0)
    }

    /// Replaces the queue with `songs` and plays from `start`, `position_ms`
    /// into that song.
    pub(super) fn load_songs_at(
        self: &Rc<Self>,
        songs: Vec<Song>,
        start: usize,
        position_ms: u64,
    ) -> Result<(), &'static str> {
        let (songs, tracks): (Vec<Song>, Vec<Track>) = songs
            .into_iter()
            .filter_map(|s| track_of(&s).map(|t| (s, t)))
            .unzip();
        if songs.is_empty() {
            return Err("unknown_song");
        }
        let start = start.min(songs.len() - 1);
        *self.queue.borrow_mut() = songs;
        self.with_player(|p| p.load_at(tracks, start, position_ms));
        Ok(())
    }

    /// Plays one of `songs` now, after the current one, keeping the rest of
    /// the queue: the first that is not already queued, or the first.
    fn play_next_of(self: &Rc<Self>, songs: Vec<Song>) -> Result<(), &'static str> {
        let queued: std::collections::HashSet<String> =
            self.queue.borrow().iter().map(|s| s.id.clone()).collect();
        let pick = songs
            .iter()
            .find(|s| !queued.contains(&s.id))
            .or_else(|| songs.first())
            .cloned()
            .ok_or("unknown_song")?;
        let mut queue = self.queue.borrow().clone();
        let at = self
            .now_playing
            .get()
            .map_or(0, |i| (i + 1).min(queue.len()));
        queue.insert(at, pick);
        self.load_songs(queue, at)
    }

    fn remote_catalog(self: &Rc<Self>, command: RemoteCommand, reply: oneshot::Sender<Reply>) {
        let Some((session, reply)) = self.session_or_fail(reply) else {
            return;
        };
        self.spawn(async move { session.catalog().await }, move |_, catalog| {
            let answer = match catalog {
                Ok(catalog) => Ok(Some(match command {
                    RemoteCommand::LibraryArtists => Value::from_iter(
                        catalog
                            .artists
                            .iter()
                            .map(|a| json!({"id": a.id, "name": a.name})),
                    ),
                    RemoteCommand::LibraryPlaylists => Value::from_iter(
                        catalog
                            .playlists
                            .iter()
                            .map(|(id, name)| json!({"id": id, "name": name})),
                    ),
                    RemoteCommand::LibraryDances => Value::from_iter(
                        catalog
                            .dances
                            .iter()
                            .map(|d| json!({"tag_id": d.tag_id, "name": d.name})),
                    ),
                    RemoteCommand::LibraryAlbums { artist } => Value::from_iter(
                        catalog
                            .albums
                            .iter()
                            .filter(|a| a.artist_id.as_deref() == Some(artist.as_str()))
                            .map(|a| json!({"id": a.id, "name": a.name, "year": a.year})),
                    ),
                    _ => Value::Null,
                })),
                Err(_) => Err("failed"),
            };
            let _ = reply.send(answer);
        });
    }

    fn remote_songs(self: &Rc<Self>, selection: Selection, reply: oneshot::Sender<Reply>) {
        let Some((session, reply)) = self.session_or_fail(reply) else {
            return;
        };
        self.spawn(
            async move { session.songs(selection).await },
            move |_, songs| {
                let answer = songs
                    .map(|songs| {
                        Some(Value::from_iter(songs.iter().take(MAX_SONGS).map(|s| {
                            json!({
                                "id": s.id,
                                "title": s.title,
                                "artist": s.artist_name,
                                "album": s.album_name,
                                "duration_ms": s.duration_ms,
                                "bpm": s.bpm,
                            })
                        })))
                    })
                    .map_err(|_| "failed");
                let _ = reply.send(answer);
            },
        );
    }

    fn remote_cover(self: &Rc<Self>, id: String, reply: oneshot::Sender<Reply>) {
        let Some((session, reply)) = self.session_or_fail(reply) else {
            return;
        };
        self.spawn(async move { session.cover(id).await }, move |_, bytes| {
            let answer = match bytes {
                Some(bytes) if bytes.len() <= MAX_COVER_BYTES => Ok(Some(json!({
                    "image": studio_core::remote::b64u(&bytes),
                    "content_type": image_type(&bytes),
                }))),
                _ => Err("failed"),
            };
            let _ = reply.send(answer);
        });
    }
}

/// The player's track for a song, when its ID is valid.
fn track_of(song: &Song) -> Option<Track> {
    let id = SongId::parse(&song.id).ok()?;
    Some(Track {
        song: id,
        gain_db: song.track_gain,
        peak: song.track_peak,
        local: song.local_path.clone(),
    })
}

/// The media type of an image, from its first bytes.
fn image_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        "image/jpeg"
    } else if bytes.len() > 12 && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}
