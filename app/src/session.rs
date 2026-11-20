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
}

/// What the sidebar lists, read in one go.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    pub(crate) artists: Vec<Artist>,
    pub(crate) albums: Vec<Album>,
    pub(crate) dances: Vec<Dance>,
    pub(crate) playlists: Vec<(String, String)>,
}

/// What the sidebar selected; the track list shows its songs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Selection {
    Artist(String),
    Album(String),
    Dance(i64),
    Playlist(String),
    Search(String),
}

/// The settings for a stored server: plain http only for an address that was
/// accepted with the opt-in, and [`ServerUrl::parse`] still allows that only
/// for a private address.
fn settings_for(base_url: &str) -> HttpSettings {
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
        tokio::task::spawn_blocking(move || {
            let conn = db.reader()?;
            match &selection {
                Selection::Artist(id) => catalog::songs_of_artist(&conn, server, id),
                Selection::Album(id) => catalog::songs_of_album(&conn, server, id),
                Selection::Dance(tag) => dancebrainz::songs_of_dance(&conn, server, *tag),
                Selection::Playlist(id) => catalog::playlist_songs(&conn, server, id),
                Selection::Search(text) => catalog::search_songs(&conn, server, text, SEARCH_LIMIT),
            }
        })
        .await
        .map_err(|_| "The library task stopped.".to_owned())?
        .map_err(|e| e.to_string())
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

/// The DanceBrainz client, read anonymously over https.
fn dance_brainz() -> Result<DanceBrainz, String> {
    let settings = HttpSettings::default();
    let http = http::client(&settings).map_err(|e| e.to_string())?;
    let base = ServerUrl::parse(dancebrainz::DEFAULT_SERVER, HttpPolicy::HTTPS_ONLY)
        .map_err(|e| e.to_string())?;
    Ok(DanceBrainz::new(http, base))
}
