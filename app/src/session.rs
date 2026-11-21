//! The server the window shows and plays from (#8), and the background work
//! for it. Everything here runs on the Tokio runtime; the window awaits the
//! results on the main thread.

use std::sync::Arc;

use studio_core::audio_cache::AudioCache;
use studio_core::catalog::{self, Album, Artist, ServerId, Song};
use studio_core::connect;
use studio_core::connection::{HttpPolicy, ServerUrl};
use studio_core::dancebrainz::{self, Dance, DanceBrainz, Mbid};
use studio_core::db::Database;
use studio_core::http::{self, HttpSettings};
use studio_core::library_feed::LibrarySource;
use studio_core::local_library::{self, LocalArtist, LocalFilter, LocalFolder, ScanReport};
use studio_core::offline::{self, PinKind};
use studio_core::plays::{self, Play};
use studio_core::queue::SongId;
use studio_core::server_extras::{ANALYSIS_EXTENSION, Waveform};
use studio_core::subsonic::SubsonicClient;
use studio_core::sync;
use studio_core::track_source::Tracks;

use crate::Services;

/// Most songs a search shows.
const SEARCH_LIMIT: u32 = 500;

/// A connected server: what the window needs to browse and play.
#[derive(Clone)]
pub(crate) struct Session {
    pub(crate) server: ServerId,
    pub(crate) client: SubsonicClient,
    pub(crate) db: Arc<Database>,
    /// The server offers folksonic's analysis, so waveforms can be asked for.
    pub(crate) has_analysis: bool,
    /// One sender of queued plays at a time, so a play is not sent twice
    /// at once.
    sending: Arc<tokio::sync::Mutex<()>>,
    /// Where offline copies are downloaded from (#17).
    offline: offline::Source,
    /// One offline sync at a time.
    keeping: Arc<tokio::sync::Mutex<()>>,
}

/// What the sidebar lists, read in one go.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    pub(crate) artists: Vec<Artist>,
    pub(crate) albums: Vec<Album>,
    pub(crate) dances: Vec<Dance>,
    pub(crate) playlists: Vec<(String, String)>,
    /// Artists with tracks only in local folders (#10).
    pub(crate) local_artists: Vec<LocalArtist>,
}

/// What the sidebar selected; the track list shows its songs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    /// A server artist; its local-only tracks under the same name come too.
    Artist(String),
    Album(String),
    /// An artist with tracks only in local folders.
    LocalArtist(String),
    LocalAlbum {
        artist: String,
        album: String,
    },
    Dance(i64),
    Playlist(String),
    Search(String),
    /// Songs by ID, in this order: server IDs and `local:<n>` (#20).
    Ids(Vec<String>),
}

/// The settings for a stored server: plain http only for an address that was
/// accepted with the opt-in, and [`ServerUrl::parse`] still allows that only
/// for a private address.
pub(crate) fn settings_for(base_url: &str) -> HttpSettings {
    let policy = if base_url.starts_with("http://") {
        HttpPolicy::ALLOW_PRIVATE_HTTP
    } else {
        HttpPolicy::HTTPS_ONLY
    };
    HttpSettings {
        policy,
        pinned_certificate_pem: None,
    }
}

/// Opens the server synced last, with the credentials in the keyring.
/// `Ok(None)` when no server was ever connected or its credentials are gone.
pub(crate) async fn open(services: Services) -> Result<Option<(Session, Tracks)>, String> {
    let db = Arc::clone(&services.db);
    let last = tokio::task::spawn_blocking(move || catalog::last_server(&db.reader()?))
        .await
        .map_err(|_| "The library task stopped.".to_owned())?
        .map_err(|e| e.to_string())?;
    let Some((server, base_url)) = last else {
        return Ok(None);
    };
    let settings = settings_for(&base_url);
    let url = ServerUrl::parse(&base_url, settings.policy).map_err(|e| e.to_string())?;
    let Some(auth) = connect::saved_auth(&services.vault, &url)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let http = http::client(&settings).map_err(|e| e.to_string())?;
    let streaming = http::streaming_client(&settings).map_err(|e| e.to_string())?;
    let client = SubsonicClient::new(http, url.clone(), auth.clone());
    let offline = offline::Source {
        http: streaming.clone(),
        server: url.clone(),
        auth: auth.clone(),
    };

    let cache_dir = gtk::glib::user_cache_dir()
        .join("dancesonic-studio")
        .join("audio");
    let cache_db = Arc::clone(&services.db);
    let cache_url = url.clone();
    let cache =
        tokio::task::spawn_blocking(move || AudioCache::open(cache_dir, cache_db, &cache_url))
            .await
            .map_err(|_| "The cache task stopped.".to_owned())?;
    let cache = match cache {
        Ok(cache) => Some(Arc::new(cache)),
        Err(error) => {
            // Playback works without the cache, only without reuse.
            tracing::warn!(%error, "the track cache could not be opened");
            None
        }
    };
    let tracks = Tracks::new(
        streaming,
        url,
        auth,
        cache,
        tokio::runtime::Handle::current(),
    )
    .map_err(|e| e.to_string())?;

    // Waveforms are optional: without an answer the seek bar is a slider.
    let has_analysis = match client.server_info().await {
        Ok(info) => info.extensions.iter().any(|e| e == ANALYSIS_EXTENSION),
        Err(error) => {
            tracing::debug!(%error, "server extensions unknown");
            false
        }
    };
    Ok(Some((
        Session {
            server,
            client,
            db: services.db,
            has_analysis,
            sending: Arc::default(),
            offline,
            keeping: Arc::default(),
        },
        tracks,
    )))
}

impl Session {
    /// Fetches what changed on the server, then the dances of new recordings.
    /// A failed DanceBrainz lookup only leaves the dances as they were.
    pub(crate) async fn sync(&self) -> Result<String, String> {
        let source = LibrarySource::new(self.client.clone());
        let report = sync::sync_server(&source, Arc::clone(&self.db), self.server)
            .await
            .map_err(|e| e.to_string())?;
        let db = Arc::clone(&self.db);
        let server = self.server;
        let mbids =
            tokio::task::spawn_blocking(move || catalog::recording_mbids(&db.reader()?, server))
                .await
                .map_err(|_| "The library task stopped.".to_owned())?
                .map_err(|e| e.to_string())?;
        let mbids: Vec<Mbid> = mbids.iter().filter_map(|m| Mbid::parse(m).ok()).collect();
        let dances = match dance_brainz() {
            Ok(client) => client
                .refresh(Arc::clone(&self.db), mbids)
                .await
                .map_err(|e| e.to_string()),
            Err(error) => Err(error),
        };
        Ok(match dances {
            Ok(_) => format!(
                "Library up to date ({} changes).",
                report.upserted + report.deleted
            ),
            Err(error) => {
                tracing::warn!(%error, "dance lookup failed");
                format!(
                    "Library up to date ({} changes). Dances could not be updated: {error}",
                    report.upserted + report.deleted
                )
            }
        })
    }

    /// Queues a play that counted (#31), then sends the queue.
    pub(crate) async fn record_play(&self, play: Play) {
        let db = Arc::clone(&self.db);
        let server = self.server;
        let queued = tokio::task::spawn_blocking(move || {
            db.write(|tx| plays::queue_play(tx, server, &play))
        })
        .await;
        match queued {
            Ok(Ok(0)) => {}
            Ok(Ok(dropped)) => {
                tracing::warn!(
                    dropped,
                    "the play queue is full; the oldest plays were dropped"
                );
            }
            Ok(Err(error)) => tracing::warn!(%error, "a play could not be queued"),
            Err(_) => tracing::warn!("the play queue task stopped"),
        }
        self.send_plays().await;
    }

    /// Sends the plays queued for this server. What fails stays queued for
    /// the next try.
    pub(crate) async fn send_plays(&self) {
        let _sending = self.sending.lock().await;
        match plays::send_queued(&self.client, Arc::clone(&self.db), self.server).await {
            Ok(0) => {}
            Ok(sent) => tracing::debug!(sent, "plays reported"),
            Err(error) => tracing::info!(%error, "plays stay queued"),
        }
    }

    /// Tells the server what is playing now. Not counted, so a failure only
    /// means the server does not show it.
    pub(crate) async fn now_playing(&self, song: SongId) {
        if let Err(error) = self
            .client
            .scrobble(&song, studio_core::sync::now_ms(), false)
            .await
        {
            tracing::debug!(%error, "now playing not reported");
        }
    }

    /// Whether an album or playlist is kept offline (#17).
    pub(crate) async fn pinned(&self, kind: PinKind, item: String) -> bool {
        let db = Arc::clone(&self.db);
        let base_url = self.client.server().key();
        tokio::task::spawn_blocking(move || {
            db.reader()
                .ok()
                .and_then(|conn| offline::is_pinned(&conn, &base_url, kind, &item).ok())
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }

    /// Marks an album or playlist "keep offline", or not, then brings the
    /// copies in line.
    pub(crate) async fn set_pinned(
        &self,
        kind: PinKind,
        item: String,
        pinned: bool,
    ) -> Result<offline::SyncReport, String> {
        let db = Arc::clone(&self.db);
        let base_url = self.client.server().key();
        tokio::task::spawn_blocking(move || {
            offline::set_pinned(&db, &base_url, kind, &item, pinned, sync::now_ms())
        })
        .await
        .map_err(|_| "The library task stopped.".to_owned())?
        .map_err(|e| e.to_string())?;
        self.sync_offline().await
    }

    /// Downloads what pinned albums and playlists hold and removes copies no
    /// longer wanted, within the size limit.
    pub(crate) async fn sync_offline(&self) -> Result<offline::SyncReport, String> {
        let _keeping = self.keeping.lock().await;
        let db = Arc::clone(&self.db);
        let (dir, limit) = tokio::task::spawn_blocking(move || -> Result<_, String> {
            let conn = db.reader().map_err(|e| e.to_string())?;
            let settings = offline::settings(&conn).map_err(|e| e.to_string())?;
            let dir = offline::store_dir(&offline_root(), &settings);
            // Leftovers of a stopped download.
            if let Err(error) = offline::clean_up(&db, &dir) {
                tracing::debug!(%error, "offline folder not cleaned");
            }
            Ok((dir, settings.limit_bytes))
        })
        .await
        .map_err(|_| "The library task stopped.".to_owned())??;
        offline::sync(&self.offline, Arc::clone(&self.db), self.server, dir, limit)
            .await
            .map_err(|e| e.to_string())
    }

    /// Everything the sidebar lists.
    pub(crate) async fn catalog(&self) -> Result<Catalog, String> {
        let db = Arc::clone(&self.db);
        let server = self.server;
        tokio::task::spawn_blocking(move || {
            let conn = db.reader()?;
            Ok::<_, studio_core::db::DbError>(Catalog {
                artists: catalog::artists(&conn, server)?,
                albums: catalog::albums(&conn, server)?,
                dances: dancebrainz::dances(&conn, server)?,
                playlists: catalog::playlists(&conn, server)?,
                local_artists: local_library::local_artists(&conn, Some(server))
                    .map_err(|e| studio_core::db::DbError::Invalid(e.to_string()))?,
            })
        })
        .await
        .map_err(|_| "The library task stopped.".to_owned())?
        .map_err(|e| e.to_string())
    }

    /// The songs of a sidebar entry or a search.
    pub(crate) async fn songs(&self, selection: Selection) -> Result<Vec<Song>, String> {
        let db = Arc::clone(&self.db);
        let server = self.server;
        let base_url = self.client.server().key();
        tokio::task::spawn_blocking(move || {
            let conn = db.reader().map_err(|e| e.to_string())?;
            let local = |filter: LocalFilter| {
                local_library::local_only_songs(&conn, Some(server), &filter, SEARCH_LIMIT)
                    .map_err(|e| e.to_string())
            };
            let mut songs = match &selection {
                Selection::Artist(id) => {
                    let mut songs =
                        catalog::songs_of_artist(&conn, server, id).map_err(|e| e.to_string())?;
                    if let Some(name) =
                        catalog::artist_name(&conn, server, id).map_err(|e| e.to_string())?
                    {
                        songs.extend(local(LocalFilter::Artist(name))?);
                    }
                    songs
                }
                Selection::Album(id) => {
                    catalog::songs_of_album(&conn, server, id).map_err(|e| e.to_string())?
                }
                Selection::LocalArtist(name) => local(LocalFilter::Artist(name.clone()))?,
                Selection::LocalAlbum { artist, album } => local(LocalFilter::Album {
                    artist: artist.clone(),
                    album: album.clone(),
                })?,
                Selection::Dance(tag) => {
                    dancebrainz::songs_of_dance(&conn, server, *tag).map_err(|e| e.to_string())?
                }
                Selection::Playlist(id) => {
                    catalog::playlist_songs(&conn, server, id).map_err(|e| e.to_string())?
                }
                Selection::Search(text) => {
                    let mut songs = catalog::search_songs(&conn, server, text, SEARCH_LIMIT)
                        .map_err(|e| e.to_string())?;
                    songs.extend(local(LocalFilter::Search(text.clone()))?);
                    songs
                }
                Selection::Ids(ids) => {
                    let mut songs = Vec::with_capacity(ids.len());
                    for id in ids {
                        match id
                            .strip_prefix(local_library::LOCAL_ID_PREFIX)
                            .and_then(|n| n.parse::<i64>().ok())
                        {
                            Some(n) => songs.extend(local(LocalFilter::Id(n))?),
                            None => songs.extend(
                                catalog::songs_by_id(&conn, server, std::slice::from_ref(id))
                                    .map_err(|e| e.to_string())?,
                            ),
                        }
                    }
                    songs
                }
            };
            // Every song with a local file plays from disk (#10), and so does
            // every song kept offline (#17).
            local_library::resolve_paths(&conn, &mut songs).map_err(|e| e.to_string())?;
            let dir = offline::current_dir(&conn, &offline_root()).map_err(|e| e.to_string())?;
            offline::resolve_paths(&conn, &dir, &base_url, &mut songs)
                .map_err(|e| e.to_string())?;
            Ok(songs)
        })
        .await
        .map_err(|_| "The library task stopped.".to_owned())?
    }

    /// The dance names of a recording, from the cache.
    pub(crate) async fn dances_of(&self, mbid: Option<String>) -> Vec<String> {
        let Some(mbid) = mbid.and_then(|m| Mbid::parse(&m).ok()) else {
            return Vec::new();
        };
        let db = Arc::clone(&self.db);
        let result =
            tokio::task::spawn_blocking(move || dancebrainz::dances_of(&db.reader()?, &mbid)).await;
        match result {
            Ok(Ok(tags)) => tags
                .into_iter()
                .filter(|t| t.kind == dancebrainz::DANCE_KIND)
                .map(|t| t.name)
                .collect(),
            Ok(Err(error)) => {
                tracing::warn!(%error, "dances could not be read");
                Vec::new()
            }
            Err(_) => Vec::new(),
        }
    }

    /// The waveform of a song, when the server has one.
    pub(crate) async fn waveform(&self, song: SongId) -> Option<Waveform> {
        if !self.has_analysis {
            return None;
        }
        match self.client.waveform(&song).await {
            Ok(wave) => wave,
            Err(error) => {
                tracing::debug!(%error, "no waveform");
                None
            }
        }
    }

    /// Cover image bytes, when there is a cover.
    pub(crate) async fn cover(&self, cover_id: String) -> Option<Vec<u8>> {
        match self.client.cover_art(&cover_id, 256).await {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                tracing::debug!(%error, "no cover");
                None
            }
        }
    }

    /// A `file://` URL of the cover, stored privately for the desktop's
    /// media controls (#7).
    pub(crate) async fn cover_file(&self, cover_id: String) -> Option<String> {
        let bytes = self.cover(cover_id.clone()).await?;
        let dir = gtk::glib::user_cache_dir()
            .join("dancesonic-studio")
            .join("covers");
        let server = self.client.server().key();
        let stored = tokio::task::spawn_blocking(move || {
            studio_core::server_extras::store_cover(&dir, &server, &cover_id, &bytes)
        })
        .await;
        match stored {
            Ok(Ok(path)) => gtk::glib::filename_to_uri(&path, None)
                .ok()
                .map(|uri| uri.to_string()),
            Ok(Err(error)) => {
                tracing::debug!(%error, "cover not stored");
                None
            }
            Err(_) => None,
        }
    }

    /// Adds a song to a playlist on the server, then syncs so the local
    /// playlist matches.
    pub(crate) async fn add_to_playlist(
        &self,
        playlist_id: String,
        song: SongId,
    ) -> Result<(), String> {
        self.client
            .add_to_playlist(&playlist_id, &song)
            .await
            .map_err(|e| e.to_string())?;
        let source = LibrarySource::new(self.client.clone());
        sync::sync_server(&source, Arc::clone(&self.db), self.server)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The default folder of offline copies (#17).
pub(crate) fn offline_root() -> std::path::PathBuf {
    gtk::glib::user_data_dir()
        .join("dancesonic-studio")
        .join("offline")
}

/// The offline settings, the folder in use and the space used by all copies.
pub(crate) async fn offline_status(
    db: Arc<Database>,
) -> Result<(offline::Settings, std::path::PathBuf, u64), String> {
    tokio::task::spawn_blocking(move || {
        let conn = db.reader().map_err(|e| e.to_string())?;
        let settings = offline::settings(&conn).map_err(|e| e.to_string())?;
        let dir = offline::store_dir(&offline_root(), &settings);
        let used = offline::used_bytes(&conn, None).map_err(|e| e.to_string())?;
        Ok((settings, dir, used))
    })
    .await
    .map_err(|_| "The library task stopped.".to_owned())?
}

/// Changes where offline copies live and how much space they may take.
pub(crate) async fn set_offline_settings(
    db: Arc<Database>,
    settings: offline::Settings,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        offline::set_settings(&db, &offline_root(), &settings).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The library task stopped.".to_owned())?
}

/// Checks every offline copy against its recorded SHA-256.
pub(crate) async fn verify_offline(db: Arc<Database>) -> Result<(usize, usize), String> {
    tokio::task::spawn_blocking(move || {
        let conn = db.reader().map_err(|e| e.to_string())?;
        let dir = offline::current_dir(&conn, &offline_root()).map_err(|e| e.to_string())?;
        offline::verify(&db, &dir, None).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The check stopped.".to_owned())?
}

/// Deletes the offline copies and pins of a server, for a sign-out.
pub(crate) async fn remove_offline(db: Arc<Database>, base_url: String) -> Result<usize, String> {
    tokio::task::spawn_blocking(move || {
        let conn = db.reader().map_err(|e| e.to_string())?;
        let dir = offline::current_dir(&conn, &offline_root()).map_err(|e| e.to_string())?;
        offline::remove_server(&db, &dir, &base_url).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The library task stopped.".to_owned())?
}

/// The local folders (#10).
pub(crate) async fn folders(db: Arc<Database>) -> Result<Vec<LocalFolder>, String> {
    tokio::task::spawn_blocking(move || {
        let conn = db.reader().map_err(|e| e.to_string())?;
        local_library::folders(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The library task stopped.".to_owned())?
}

/// Adds a folder and scans it.
pub(crate) async fn add_folder(
    db: Arc<Database>,
    path: std::path::PathBuf,
) -> Result<ScanReport, String> {
    tokio::task::spawn_blocking(move || {
        let now = sync::now_ms();
        let folder = local_library::add_folder(&db, &path, now).map_err(|e| e.to_string())?;
        local_library::scan_folder(&db, &folder, now).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The scan stopped.".to_owned())?
}

/// Forgets a folder; its files stay on disk.
pub(crate) async fn remove_folder(db: Arc<Database>, folder: i64) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        local_library::remove_folder(&db, folder).map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| "The library task stopped.".to_owned())?
}

/// Scans one folder again, or all of them for `None`.
pub(crate) async fn rescan(db: Arc<Database>, folder: Option<i64>) -> Result<ScanReport, String> {
    tokio::task::spawn_blocking(move || {
        let folders = local_library::folders(&db.reader().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let mut total = ScanReport::default();
        for f in folders
            .iter()
            .filter(|f| folder.is_none_or(|id| id == f.id))
        {
            let report =
                local_library::scan_folder(&db, f, sync::now_ms()).map_err(|e| e.to_string())?;
            total.added += report.added;
            total.updated += report.updated;
            total.moved += report.moved;
            total.removed += report.removed;
            total.unchanged += report.unchanged;
            total.unreadable += report.unreadable;
        }
        Ok(total)
    })
    .await
    .map_err(|_| "The scan stopped.".to_owned())?
}

/// The DanceBrainz client, read anonymously over https.
fn dance_brainz() -> Result<DanceBrainz, String> {
    let settings = HttpSettings::default();
    let http = http::client(&settings).map_err(|e| e.to_string())?;
    let base = ServerUrl::parse(dancebrainz::DEFAULT_SERVER, HttpPolicy::HTTPS_ONLY)
        .map_err(|e| e.to_string())?;
    Ok(DanceBrainz::new(http, base))
}
