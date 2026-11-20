//! Catalog changes from folksonic's change feed (`getLibraryChanges`).
//!
//! With `include=items` the feed carries each changed artist, album and song
//! with its data, so a full sync takes a few requests instead of one per
//! album. Playlists come as ids and are fetched one by one. Starred items are
//! not part of the catalog and are ignored.
//!
//! Everything the server sends is untrusted: pages are bounded, cursors must
//! not repeat, and values out of range (such as a BPM of 999) are dropped
//! rather than stored.

use std::collections::HashSet;

use serde::Deserialize;

use crate::catalog::{Album, Artist, CatalogKind, ChangeSet, Deletion, Playlist, Song};
use crate::subsonic::{Answer, ApiError, SubsonicClient, truncate_to_bytes};
use crate::sync::{CatalogSource, SourceError};

/// The vendor extension this source needs. Servers without it are not
/// supported by the feed.
pub const FEED_EXTENSION: &str = "tinysonicLibraryChanges";

/// Items per page, the largest the server accepts.
pub const PAGE_SIZE: &str = "500";

/// Pages read in one sync before giving up. A real library needs a few; this
/// only stops a server that never ends its cursor chain.
const MAX_PAGES: usize = 10_000;

/// Longest cursor accepted, in bytes.
const MAX_CURSOR_BYTES: usize = 512;

/// Longest text field kept, in bytes (titles, names, paths).
const MAX_TEXT_BYTES: usize = 1024;

/// BPM range the database accepts; folksonic validates the same range.
const BPM_RANGE: std::ops::RangeInclusive<f64> = 20.0..=300.0;

/// Subsonic error code for "data not found".
const ERROR_NOT_FOUND: u32 = 70;

/// The catalog source for a folksonic (or other tinysonic-feed) server.
#[derive(Debug)]
pub struct LibrarySource {
    client: SubsonicClient,
}

impl LibrarySource {
    /// A source that reads the change feed through `client`.
    pub fn new(client: SubsonicClient) -> Self {
        Self { client }
    }
}

#[derive(Deserialize)]
struct ChangesPayload {
    #[serde(default, rename = "libraryChanges")]
    library_changes: Option<LibraryChanges>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LibraryChanges {
    #[serde(default)]
    full: bool,
    server_time: i64,
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default)]
    artists: Vec<Entry<ArtistItem>>,
    #[serde(default)]
    albums: Vec<Entry<AlbumItem>>,
    #[serde(default)]
    songs: Vec<Entry<SongItem>>,
    #[serde(default)]
    playlists: Vec<IdRef>,
    #[serde(default)]
    deleted: Vec<DeletedDto>,
}

/// A changed entry. `item` is missing when the object was deleted after the
/// snapshot; the next sync reports the deletion.
#[derive(Deserialize)]
struct Entry<T> {
    item: Option<T>,
}

#[derive(Deserialize)]
struct IdRef {
    id: String,
}

#[derive(Deserialize)]
struct DeletedDto {
    #[serde(rename = "type")]
    kind: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtistItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    music_brainz_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlbumItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    artist_id: Option<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    music_brainz_id: Option<String>,
    #[serde(default)]
    cover_art: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SongItem {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album_id: Option<String>,
    #[serde(default)]
    artist_id: Option<String>,
    #[serde(default)]
    track: Option<u32>,
    #[serde(default)]
    disc_number: Option<u32>,
    #[serde(default)]
    year: Option<i64>,
    /// Seconds.
    #[serde(default)]
    duration: Option<u64>,
    #[serde(default)]
    music_brainz_id: Option<String>,
    #[serde(default)]
    bpm: Option<f64>,
    #[serde(default)]
    tinysonic_tempo: Option<TempoDto>,
    #[serde(default)]
    replay_gain: Option<ReplayGainDto>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    suffix: Option<String>,
    #[serde(default)]
    cover_art: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TempoDto {
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    bars_per_min: Option<f64>,
    #[serde(default)]
    bar_pulses: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayGainDto {
    #[serde(default)]
    track_gain: Option<f64>,
    #[serde(default)]
    track_peak: Option<f64>,
}

#[derive(Deserialize)]
struct PlaylistPayload {
    #[serde(default)]
    playlist: Option<PlaylistDto>,
}

#[derive(Deserialize)]
struct PlaylistDto {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    entry: Vec<IdRef>,
}

impl CatalogSource for LibrarySource {
    async fn changes(&self, since: Option<i64>) -> Result<ChangeSet, SourceError> {
        let mut out = ChangeSet::default();
        let mut playlist_ids: Vec<String> = Vec::new();
        let mut seen_cursors: HashSet<String> = HashSet::new();
        let mut cursor: Option<String> = None;

        for page_index in 0..MAX_PAGES {
            let since_text = match (page_index, since) {
                (0, Some(since)) => Some(since.to_string()),
                _ => None,
            };
            let mut query: Vec<(&str, &str)> = vec![("include", "items"), ("size", PAGE_SIZE)];
            if let Some(text) = &since_text {
                query.push(("since", text));
            }
            if let Some(text) = &cursor {
                query.push(("cursor", text));
            }

            let answer: Answer<ChangesPayload> = self
                .client
                .get("getLibraryChanges", &query)
                .await
                .map_err(to_source_error)?;
            let page = answer
                .payload
                .library_changes
                .ok_or_else(|| SourceError::Invalid("no change feed in the answer".into()))?;

            if page_index == 0 {
                out.full = page.full;
                out.server_time = page.server_time;
            }
            let next_cursor = page.next_cursor.clone();
            add_page(&mut out, &mut playlist_ids, page);

            match next_cursor {
                None => break,
                Some(next) => {
                    if next.len() > MAX_CURSOR_BYTES || !seen_cursors.insert(next.clone()) {
                        return Err(SourceError::Invalid(
                            "the change feed cursor repeats".into(),
                        ));
                    }
                    cursor = Some(next);
                }
            }
            if page_index + 1 == MAX_PAGES {
                return Err(SourceError::Invalid("the change feed is too long".into()));
            }
        }

        for id in dedup(playlist_ids) {
            if let Some(playlist) = self.fetch_playlist(&id).await? {
                out.playlists.push(playlist);
            }
        }
        Ok(out)
    }
}

impl LibrarySource {
    /// Fetches one playlist. `None` when it was removed meanwhile; its removal
    /// reaches the next sync as a deletion.
    async fn fetch_playlist(&self, id: &str) -> Result<Option<Playlist>, SourceError> {
        match self
            .client
            .get::<PlaylistPayload>("getPlaylist", &[("id", id)])
            .await
        {
            Ok(answer) => Ok(answer.payload.playlist.map(|p| Playlist {
                id: p.id,
                name: bounded(p.name.unwrap_or_default()),
                song_ids: p.entry.into_iter().map(|e| e.id).collect(),
            })),
            Err(ApiError::Refused {
                code: Some(ERROR_NOT_FOUND),
                ..
            }) => Ok(None),
            Err(error) => Err(to_source_error(error)),
        }
    }
}

/// Adds one page's entries to the change set, dropping entries without items.
fn add_page(out: &mut ChangeSet, playlist_ids: &mut Vec<String>, page: LibraryChanges) {
    out.artists.extend(
        page.artists
            .into_iter()
            .filter_map(|e| e.item)
            .map(artist_from),
    );
    out.albums.extend(
        page.albums
            .into_iter()
            .filter_map(|e| e.item)
            .map(album_from),
    );
    out.songs
        .extend(page.songs.into_iter().filter_map(|e| e.item).map(song_from));
    playlist_ids.extend(page.playlists.into_iter().map(|p| p.id));
    for entry in page.deleted {
        let kind = match entry.kind.as_str() {
            "artist" => CatalogKind::Artist,
            "album" => CatalogKind::Album,
            "song" => CatalogKind::Song,
            "playlist" => CatalogKind::Playlist,
            // Starred items are not part of the catalog.
            _ => continue,
        };
        out.deleted.push(Deletion { kind, id: entry.id });
    }
}

fn artist_from(item: ArtistItem) -> Artist {
    Artist {
        id: item.id,
        name: bounded(item.name.unwrap_or_default()),
        sort_name: None,
        mbid: item.music_brainz_id,
    }
}

fn album_from(item: AlbumItem) -> Album {
    Album {
        id: item.id,
        artist_id: item.artist_id,
        artist_name: item.artist.map(bounded),
        name: bounded(item.name.or(item.title).unwrap_or_default()),
        sort_name: None,
        year: year_value(item.year),
        mbid: item.music_brainz_id,
        cover_art: item.cover_art,
    }
}

fn song_from(item: SongItem) -> Song {
    let tempo = item.tinysonic_tempo;
    let gain = item.replay_gain;
    Song {
        id: item.id,
        album_id: item.album_id,
        artist_id: item.artist_id,
        title: bounded(item.title.unwrap_or_default()),
        album_name: item.album.map(bounded),
        artist_name: item.artist.map(bounded),
        track: item.track,
        disc: item.disc_number,
        year: year_value(item.year),
        duration_ms: item.duration.and_then(|s| s.checked_mul(1000)),
        mbid: item.music_brainz_id,
        bpm: item.bpm.filter(|bpm| BPM_RANGE.contains(bpm)),
        tempo_confidence: tempo
            .as_ref()
            .and_then(|t| t.confidence)
            .filter(|c| (0.0..=1.0).contains(c)),
        bars_per_min: tempo
            .as_ref()
            .and_then(|t| t.bars_per_min)
            .filter(|b| b.is_finite() && *b > 0.0),
        bar_pulses: tempo
            .as_ref()
            .and_then(|t| t.bar_pulses)
            .filter(|p| *p <= 64),
        track_gain: gain
            .as_ref()
            .and_then(|g| g.track_gain)
            .filter(|g| g.is_finite()),
        track_peak: gain
            .as_ref()
            .and_then(|g| g.track_peak)
            .filter(|p| p.is_finite()),
        size_bytes: item.size,
        suffix: item.suffix.map(bounded),
        cover_art: item.cover_art,
        on_server: true,
        local_id: None,
        local_path: None,
    }
}

/// A year in a plausible range; anything else is unknown.
fn year_value(year: Option<i64>) -> Option<i32> {
    year.and_then(|y| i32::try_from(y).ok())
        .filter(|y| (1000..=9999).contains(y))
}

/// Text from the server, cut to the length the catalog keeps.
fn bounded(text: String) -> String {
    truncate_to_bytes(&text, MAX_TEXT_BYTES)
}

fn dedup(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn to_source_error(error: ApiError) -> SourceError {
    match error {
        ApiError::Unreachable | ApiError::Timeout => SourceError::Unreachable(error.to_string()),
        ApiError::Http(_) | ApiError::Refused { .. } => SourceError::Refused(error.to_string()),
        ApiError::Invalid(_) | ApiError::TooLarge | ApiError::Missing(_) | ApiError::Signing => {
            SourceError::Invalid(error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{HttpPolicy, ServerUrl};
    use crate::http::{HttpSettings, client};
    use crate::subsonic::{ApiKey, Auth};
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn source(server: &MockServer) -> LibrarySource {
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let http = client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap();
        let key = ApiKey::new("tsk_test_key").unwrap();
        LibrarySource::new(SubsonicClient::new(http, url, Auth::ApiKey(key)))
    }

    fn ok(payload: serde_json::Value) -> ResponseTemplate {
        let mut body = json!({"status": "ok", "version": "1.16.1"});
        body.as_object_mut()
            .unwrap()
            .extend(payload.as_object().unwrap().clone());
        ResponseTemplate::new(200).set_body_json(json!({ "subsonic-response": body }))
    }

    fn first_page() -> serde_json::Value {
        json!({"libraryChanges": {
            "full": true,
            "serverTime": 1000,
            "nextCursor": "page-2",
            "artists": [{"id": "ar-1", "updated": 5, "item": {"id": "ar-1", "name": "Artist", "musicBrainzId": "mb-ar"}}],
            "albums": [{"id": "al-1", "updated": 5, "item": {
                "id": "al-1", "name": "Album", "artist": "Artist", "artistId": "ar-1",
                "year": 2001, "musicBrainzId": "mb-al", "coverArt": "al-1"}}],
            "songs": [
                {"id": "so-1", "updated": 5, "item": {
                    "id": "so-1", "title": "Song", "albumId": "al-1", "artistId": "ar-1",
                    "duration": 200, "bpm": 128, "track": 3, "discNumber": 1,
                    "tinysonicTempo": {"confidence": 0.9, "barsPerMin": 32.0, "barPulses": 4},
                    "replayGain": {"trackGain": -3.2, "trackPeak": 0.98},
                    "size": 1000, "suffix": "mp3"}},
                {"id": "so-bad", "updated": 5, "item": {
                    "id": "so-bad", "title": "Out of range", "bpm": 999,
                    "tinysonicTempo": {"confidence": 7.0, "barPulses": 9000}}},
                {"id": "so-gone", "updated": 5}
            ],
            "playlists": [{"id": "pl-1", "updated": 7}],
            "starred": [{"id": "so-1", "type": "song", "updated": 7}],
            "deleted": [
                {"type": "song", "id": "so-old", "deleted": 9},
                {"type": "starred", "id": "so-1", "deleted": 9}
            ]
        }})
    }

    fn second_page() -> serde_json::Value {
        json!({"libraryChanges": {
            "serverTime": 1000,
            "artists": [], "albums": [],
            "songs": [{"id": "so-2", "updated": 6, "item": {"id": "so-2", "title": "Second", "albumId": "al-1"}}],
            "playlists": [], "starred": [], "deleted": []
        }})
    }

    async fn mount_feed(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/rest/getLibraryChanges.view"))
            .and(query_param("include", "items"))
            .and(query_param("since", "500"))
            .and(query_param_is_missing("cursor"))
            .and(header("X-Api-Key", "tsk_test_key"))
            .respond_with(ok(first_page()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/getLibraryChanges.view"))
            .and(query_param("cursor", "page-2"))
            .and(query_param_is_missing("since"))
            .respond_with(ok(second_page()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/getPlaylist.view"))
            .and(query_param("id", "pl-1"))
            .respond_with(ok(json!({"playlist": {
                "id": "pl-1", "name": "Set", "entry": [{"id": "so-1"}, {"id": "so-2"}]}})))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn pages_are_followed_and_items_mapped() {
        let server = MockServer::start().await;
        mount_feed(&server).await;
        let changes = source(&server).await.changes(Some(500)).await.unwrap();

        assert!(changes.full);
        assert_eq!(changes.server_time, 1000);
        assert_eq!(changes.artists.len(), 1);
        assert_eq!(changes.artists[0].mbid.as_deref(), Some("mb-ar"));
        assert_eq!(changes.albums[0].year, Some(2001));
        assert_eq!(changes.albums[0].artist_name.as_deref(), Some("Artist"));

        let song = changes.songs.iter().find(|s| s.id == "so-1").unwrap();
        assert_eq!(song.duration_ms, Some(200_000));
        assert_eq!(song.bpm, Some(128.0));
        assert_eq!(song.bars_per_min, Some(32.0));
        assert_eq!(song.bar_pulses, Some(4));
        assert_eq!(song.track_gain, Some(-3.2));
        assert_eq!(song.track, Some(3));

        // An out-of-range BPM and tempo are dropped; the song itself is kept.
        let bad = changes.songs.iter().find(|s| s.id == "so-bad").unwrap();
        assert_eq!(bad.bpm, None);
        assert_eq!(bad.tempo_confidence, None);
        assert_eq!(bad.bar_pulses, None);

        // Entries without an item are skipped; the next sync reports them.
        assert!(changes.songs.iter().all(|s| s.id != "so-gone"));
        assert!(changes.songs.iter().any(|s| s.id == "so-2"));
        assert_eq!(changes.songs.len(), 3);

        // Starred entries are not catalog changes.
        assert_eq!(changes.deleted.len(), 1);
        assert_eq!(changes.deleted[0].kind, CatalogKind::Song);
        assert_eq!(changes.deleted[0].id, "so-old");

        assert_eq!(changes.playlists.len(), 1);
        assert_eq!(changes.playlists[0].name, "Set");
        assert_eq!(changes.playlists[0].song_ids, vec!["so-1", "so-2"]);
    }

    #[tokio::test]
    async fn the_key_is_sent_in_a_header_and_never_in_the_url() {
        let server = MockServer::start().await;
        mount_feed(&server).await;
        source(&server).await.changes(Some(500)).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        for request in requests {
            let url = request.url.to_string();
            assert!(!url.contains("tsk_test_key"), "{url}");
            assert!(!url.contains("apiKey"), "{url}");
        }
    }

    #[tokio::test]
    async fn a_failed_status_is_a_refusal_with_the_server_message() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getLibraryChanges.view"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "subsonic-response": {"status": "failed",
                    "error": {"code": 50, "message": "Not allowed"}}})))
            .mount(&server)
            .await;
        match source(&server).await.changes(None).await {
            Err(SourceError::Refused(text)) => assert!(text.contains("Not allowed"), "{text}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_repeating_cursor_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getLibraryChanges.view"))
            .respond_with(ok(json!({"libraryChanges": {
                "serverTime": 1, "nextCursor": "same"}})))
            .mount(&server)
            .await;
        assert!(matches!(
            source(&server).await.changes(None).await,
            Err(SourceError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn an_answer_without_a_feed_is_invalid() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getLibraryChanges.view"))
            .respond_with(ok(json!({})))
            .mount(&server)
            .await;
        assert!(matches!(
            source(&server).await.changes(None).await,
            Err(SourceError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn an_unreachable_server_is_reported_as_unreachable() {
        // A port that was free a moment ago and has nothing listening on it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = ServerUrl::parse(
            &format!("http://127.0.0.1:{port}"),
            HttpPolicy::ALLOW_PRIVATE_HTTP,
        )
        .unwrap();
        let http = client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap();
        let source = LibrarySource::new(SubsonicClient::new(
            http,
            url,
            Auth::ApiKey(ApiKey::new("tsk_test_key").unwrap()),
        ));
        let result = source.changes(None).await;
        assert!(
            matches!(result, Err(SourceError::Unreachable(_))),
            "{result:?}"
        );
    }

    #[test]
    fn years_and_text_are_bounded() {
        assert_eq!(year_value(Some(2001)), Some(2001));
        assert_eq!(year_value(Some(0)), None);
        assert_eq!(year_value(Some(i64::MAX)), None);
        assert_eq!(bounded("x".repeat(5000)).len(), MAX_TEXT_BYTES);
    }
}
