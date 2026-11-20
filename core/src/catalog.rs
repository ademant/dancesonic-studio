//! The catalog: artists, albums, songs and playlists mirrored from a server.
//!
//! A sync hands a [`ChangeSet`] to [`apply_changes`], which writes it in one
//! transaction. Values the schema would reject (an empty id, a BPM outside
//! 20–300, …) are dropped or cleared here, so one bad record from a server
//! never fails a whole sync.
//!
//! Reads take a read-only connection from [`crate::db::Database::reader`].

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::db::{Database, DbError};

/// Longest accepted catalog id, in bytes. Matches the schema's CHECK.
const MAX_ID_LEN: usize = 512;
/// Tempo range the database accepts. folksonic validates the same range.
const BPM_RANGE: std::ops::RangeInclusive<f64> = 20.0..=300.0;

/// The local row id of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ServerId(i64);

impl ServerId {
    /// The row id, for queries in other modules.
    pub(crate) fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub sort_name: Option<String>,
    pub mbid: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Album {
    pub id: String,
    pub artist_id: Option<String>,
    pub artist_name: Option<String>,
    pub name: String,
    pub sort_name: Option<String>,
    pub year: Option<i32>,
    pub mbid: Option<String>,
    pub cover_art: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Song {
    pub id: String,
    pub album_id: Option<String>,
    pub artist_id: Option<String>,
    pub title: String,
    pub album_name: Option<String>,
    pub artist_name: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub duration_ms: Option<u64>,
    /// Recording MBID, the key for DanceBrainz.
    pub mbid: Option<String>,
    /// Tempo from folksonic, the only tempo source.
    pub bpm: Option<f64>,
    pub tempo_confidence: Option<f64>,
    pub bars_per_min: Option<f64>,
    pub bar_pulses: Option<u32>,
    pub track_gain: Option<f64>,
    pub track_peak: Option<f64>,
    pub size_bytes: Option<u64>,
    pub suffix: Option<String>,
    pub cover_art: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    /// Song ids in playlist order.
    pub song_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogKind {
    Artist,
    Album,
    Song,
    Playlist,
}

impl CatalogKind {
    fn table(self) -> &'static str {
        match self {
            CatalogKind::Artist => "artist",
            CatalogKind::Album => "album",
            CatalogKind::Song => "song",
            CatalogKind::Playlist => "playlist",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Deletion {
    pub kind: CatalogKind,
    pub id: String,
}

/// What changed on the server since the last sync.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChangeSet {
    /// The set is the whole catalog: everything not in it is removed.
    pub full: bool,
    /// The server's time for this answer, passed as `since` next time.
    pub server_time: i64,
    pub artists: Vec<Artist>,
    pub albums: Vec<Album>,
    pub songs: Vec<Song>,
    pub playlists: Vec<Playlist>,
    pub deleted: Vec<Deletion>,
}

/// What [`apply_changes`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub upserted: usize,
    pub deleted: usize,
    /// Records skipped because they had an invalid id.
    pub skipped: usize,
}

/// Returns the server with this base URL, creating it if needed.
pub fn ensure_server(db: &Database, base_url: &str, now_ms: i64) -> Result<ServerId, DbError> {
    db.write(|tx| {
        tx.execute(
            "INSERT INTO server (base_url, created_at) VALUES (?1, ?2)
             ON CONFLICT (base_url) DO NOTHING",
            params![base_url, now_ms],
        )?;
        let id = tx.query_row(
            "SELECT id FROM server WHERE base_url = ?1",
            [base_url],
            |r| r.get(0),
        )?;
        Ok(ServerId(id))
    })
}

/// The server with this base URL, if it exists. Unlike [`ensure_server`] it
/// never creates one.
pub fn find_server(conn: &Connection, base_url: &str) -> Result<Option<ServerId>, DbError> {
    Ok(conn
        .query_row(
            "SELECT id FROM server WHERE base_url = ?1",
            [base_url],
            |r| r.get(0),
        )
        .optional()?
        .map(ServerId))
}

/// The base URL of a server, `None` when it is unknown.
pub fn server_base_url(conn: &Connection, server: ServerId) -> Result<Option<String>, DbError> {
    Ok(conn
        .query_row(
            "SELECT base_url FROM server WHERE id = ?1",
            [server.0],
            |r| r.get(0),
        )
        .optional()?)
}

/// The `since` value for the next change feed request; `None` before the
/// first sync.
pub fn sync_since(conn: &Connection, server: ServerId) -> Result<Option<i64>, DbError> {
    Ok(conn
        .query_row(
            "SELECT sync_since FROM server WHERE id = ?1",
            [server.0],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// Deletes a server and its whole catalog. User data that is not part of
/// the catalog is not touched.
pub fn forget_server(db: &Database, server: ServerId) -> Result<(), DbError> {
    db.write(|tx| {
        tx.execute("DELETE FROM server WHERE id = ?1", [server.0])?;
        Ok(())
    })
}

/// Writes a change set in one transaction and records `server_time` as the
/// next `since`. For a full set, everything of the server that the set does
/// not contain is removed.
pub fn apply_changes(
    db: &Database,
    server: ServerId,
    changes: &ChangeSet,
    now_ms: i64,
) -> Result<ApplyReport, DbError> {
    db.write(|tx| {
        let previous: Option<i64> = tx
            .query_row(
                "SELECT synced_at FROM server WHERE id = ?1",
                [server.0],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| DbError::Invalid("unknown server".into()))?;
        // Strictly increasing, so a full sync can tell this run's rows from
        // older ones even when two runs fall in the same millisecond.
        let run = previous.map_or(now_ms, |p| now_ms.max(p + 1));
        let mut report = ApplyReport::default();

        for deletion in &changes.deleted {
            report.deleted += delete_one(tx, server, deletion)?;
        }
        for artist in &changes.artists {
            upsert(&mut report, &artist.id, || {
                upsert_artist(tx, server, artist, run)
            })?;
        }
        for album in &changes.albums {
            upsert(&mut report, &album.id, || {
                upsert_album(tx, server, album, run)
            })?;
        }
        for song in &changes.songs {
            upsert(&mut report, &song.id, || upsert_song(tx, server, song, run))?;
        }
        for playlist in &changes.playlists {
            upsert(&mut report, &playlist.id, || {
                upsert_playlist(tx, server, playlist, run)
            })?;
        }
        if changes.full {
            for kind in [
                CatalogKind::Song,
                CatalogKind::Album,
                CatalogKind::Artist,
                CatalogKind::Playlist,
            ] {
                // The table name comes from a closed enum, never from data.
                let sql = format!(
                    "DELETE FROM {} WHERE server_id = ?1 AND synced_at < ?2",
                    kind.table()
                );
                report.deleted += tx.execute(&sql, params![server.0, run])?;
            }
        }
        tx.execute(
            "UPDATE server SET sync_since = ?2, synced_at = ?3 WHERE id = ?1",
            params![server.0, changes.server_time, run],
        )?;
        Ok(report)
    })
}

fn upsert(
    report: &mut ApplyReport,
    id: &str,
    write: impl FnOnce() -> Result<(), DbError>,
) -> Result<(), DbError> {
    if valid_id(id) {
        write()?;
        report.upserted += 1;
    } else {
        tracing::warn!(
            len = id.len(),
            "skipping a catalog record with an invalid id"
        );
        report.skipped += 1;
    }
    Ok(())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LEN
}

fn delete_one(
    tx: &Transaction<'_>,
    server: ServerId,
    deletion: &Deletion,
) -> Result<usize, DbError> {
    let sql = format!(
        "DELETE FROM {} WHERE server_id = ?1 AND id = ?2",
        deletion.kind.table()
    );
    Ok(tx
        .prepare_cached(&sql)?
        .execute(params![server.0, deletion.id])?)
}

fn upsert_artist(
    tx: &Transaction<'_>,
    server: ServerId,
    a: &Artist,
    run: i64,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO artist (server_id, id, name, sort_name, mbid, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (server_id, id) DO UPDATE SET
           name = excluded.name, sort_name = excluded.sort_name,
           mbid = excluded.mbid, synced_at = excluded.synced_at",
    )?
    .execute(params![server.0, a.id, a.name, a.sort_name, a.mbid, run])?;
    Ok(())
}

fn upsert_album(
    tx: &Transaction<'_>,
    server: ServerId,
    a: &Album,
    run: i64,
) -> Result<(), DbError> {
    let year = a.year.filter(|y| (0..=9999).contains(y));
    tx.prepare_cached(
        "INSERT INTO album (server_id, id, artist_id, artist_name, name, sort_name, year,
                            mbid, cover_art, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (server_id, id) DO UPDATE SET
           artist_id = excluded.artist_id, artist_name = excluded.artist_name,
           name = excluded.name, sort_name = excluded.sort_name, year = excluded.year,
           mbid = excluded.mbid, cover_art = excluded.cover_art,
           synced_at = excluded.synced_at",
    )?
    .execute(params![
        server.0,
        a.id,
        a.artist_id,
        a.artist_name,
        a.name,
        a.sort_name,
        year,
        a.mbid,
        a.cover_art,
        run
    ])?;
    Ok(())
}

fn upsert_song(tx: &Transaction<'_>, server: ServerId, s: &Song, run: i64) -> Result<(), DbError> {
    let s = sanitize_song(s);
    tx.prepare_cached(
        "INSERT INTO song (server_id, id, album_id, artist_id, title, album_name, artist_name,
                           track, disc, year, duration_ms, mbid, bpm, tempo_confidence,
                           bars_per_min, bar_pulses, track_gain, track_peak, size_bytes,
                           suffix, cover_art, synced_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22)
         ON CONFLICT (server_id, id) DO UPDATE SET
           album_id = excluded.album_id, artist_id = excluded.artist_id,
           title = excluded.title, album_name = excluded.album_name,
           artist_name = excluded.artist_name, track = excluded.track, disc = excluded.disc,
           year = excluded.year, duration_ms = excluded.duration_ms, mbid = excluded.mbid,
           bpm = excluded.bpm, tempo_confidence = excluded.tempo_confidence,
           bars_per_min = excluded.bars_per_min, bar_pulses = excluded.bar_pulses,
           track_gain = excluded.track_gain, track_peak = excluded.track_peak,
           size_bytes = excluded.size_bytes, suffix = excluded.suffix,
           cover_art = excluded.cover_art, synced_at = excluded.synced_at",
    )?
    .execute(params![
        server.0,
        s.id,
        s.album_id,
        s.artist_id,
        s.title,
        s.album_name,
        s.artist_name,
        s.track,
        s.disc,
        s.year,
        s.duration_ms.and_then(|d| i64::try_from(d).ok()),
        s.mbid,
        s.bpm,
        s.tempo_confidence,
        s.bars_per_min,
        s.bar_pulses,
        s.track_gain,
        s.track_peak,
        s.size_bytes.and_then(|b| i64::try_from(b).ok()),
        s.suffix,
        s.cover_art,
        run
    ])?;
    Ok(())
}

/// Clears values the schema would reject, so they cannot fail the sync.
fn sanitize_song(s: &Song) -> Song {
    let finite = |v: Option<f64>| v.filter(|x| x.is_finite());
    Song {
        year: s.year.filter(|y| (0..=9999).contains(y)),
        bpm: finite(s.bpm).filter(|b| BPM_RANGE.contains(b)),
        tempo_confidence: finite(s.tempo_confidence).filter(|c| (0.0..=1.0).contains(c)),
        bars_per_min: finite(s.bars_per_min).filter(|b| *b > 0.0),
        bar_pulses: s.bar_pulses.filter(|p| *p > 0),
        track_gain: finite(s.track_gain),
        track_peak: finite(s.track_peak).filter(|p| *p >= 0.0),
        ..s.clone()
    }
}

fn upsert_playlist(
    tx: &Transaction<'_>,
    server: ServerId,
    p: &Playlist,
    run: i64,
) -> Result<(), DbError> {
    tx.prepare_cached(
        "INSERT INTO playlist (server_id, id, name, synced_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (server_id, id) DO UPDATE SET
           name = excluded.name, synced_at = excluded.synced_at",
    )?
    .execute(params![server.0, p.id, p.name, run])?;
    tx.prepare_cached("DELETE FROM playlist_entry WHERE server_id = ?1 AND playlist_id = ?2")?
        .execute(params![server.0, p.id])?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO playlist_entry (server_id, playlist_id, position, song_id)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (position, song_id) in p.song_ids.iter().enumerate() {
        let position =
            i64::try_from(position).map_err(|_| DbError::Invalid("playlist too long".into()))?;
        insert.execute(params![server.0, p.id, position, song_id])?;
    }
    Ok(())
}

// ---- Queries ----

/// Artists of a server, sorted by sort name, case-insensitive.
pub fn artists(conn: &Connection, server: ServerId) -> Result<Vec<Artist>, DbError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, name, sort_name, mbid FROM artist WHERE server_id = ?1
         ORDER BY coalesce(sort_name, name) COLLATE NOCASE, id",
    )?;
    let rows = stmt.query_map([server.0], |r| {
        Ok(Artist {
            id: r.get(0)?,
            name: r.get(1)?,
            sort_name: r.get(2)?,
            mbid: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Albums of an artist, oldest first.
pub fn albums_of_artist(
    conn: &Connection,
    server: ServerId,
    artist_id: &str,
) -> Result<Vec<Album>, DbError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, artist_id, artist_name, name, sort_name, year, mbid, cover_art
         FROM album WHERE server_id = ?1 AND artist_id = ?2
         ORDER BY year, coalesce(sort_name, name) COLLATE NOCASE, id",
    )?;
    let rows = stmt.query_map(params![server.0, artist_id], |r| {
        Ok(Album {
            id: r.get(0)?,
            artist_id: r.get(1)?,
            artist_name: r.get(2)?,
            name: r.get(3)?,
            sort_name: r.get(4)?,
            year: r.get(5)?,
            mbid: r.get(6)?,
            cover_art: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

const SONG_COLUMNS: &str = "s.id, s.album_id, s.artist_id, s.title, s.album_name, s.artist_name,
    s.track, s.disc, s.year, s.duration_ms, s.mbid, s.bpm, s.tempo_confidence, s.bars_per_min,
    s.bar_pulses, s.track_gain, s.track_peak, s.size_bytes, s.suffix, s.cover_art";

fn song_from_row(r: &Row<'_>) -> rusqlite::Result<Song> {
    Ok(Song {
        id: r.get(0)?,
        album_id: r.get(1)?,
        artist_id: r.get(2)?,
        title: r.get(3)?,
        album_name: r.get(4)?,
        artist_name: r.get(5)?,
        track: r.get(6)?,
        disc: r.get(7)?,
        year: r.get(8)?,
        duration_ms: r
            .get::<_, Option<i64>>(9)?
            .and_then(|d| u64::try_from(d).ok()),
        mbid: r.get(10)?,
        bpm: r.get(11)?,
        tempo_confidence: r.get(12)?,
        bars_per_min: r.get(13)?,
        bar_pulses: r.get(14)?,
        track_gain: r.get(15)?,
        track_peak: r.get(16)?,
        size_bytes: r
            .get::<_, Option<i64>>(17)?
            .and_then(|b| u64::try_from(b).ok()),
        suffix: r.get(18)?,
        cover_art: r.get(19)?,
    })
}

/// Songs of an album in disc and track order.
pub fn songs_of_album(
    conn: &Connection,
    server: ServerId,
    album_id: &str,
) -> Result<Vec<Song>, DbError> {
    let sql = format!(
        "SELECT {SONG_COLUMNS} FROM song s WHERE s.server_id = ?1 AND s.album_id = ?2
         ORDER BY s.disc, s.track, s.title COLLATE NOCASE, s.id"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![server.0, album_id], song_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Songs of a playlist in playlist order. Entries whose song is not in the
/// catalog are left out.
pub fn playlist_songs(
    conn: &Connection,
    server: ServerId,
    playlist_id: &str,
) -> Result<Vec<Song>, DbError> {
    let sql = format!(
        "SELECT {SONG_COLUMNS} FROM playlist_entry e
         JOIN song s ON s.server_id = e.server_id AND s.id = e.song_id
         WHERE e.server_id = ?1 AND e.playlist_id = ?2
         ORDER BY e.position"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![server.0, playlist_id], song_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Playlists of a server by name.
pub fn playlists(conn: &Connection, server: ServerId) -> Result<Vec<(String, String)>, DbError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, name FROM playlist WHERE server_id = ?1 ORDER BY name COLLATE NOCASE, id",
    )?;
    let rows = stmt.query_map([server.0], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Full-text search over title, artist and album. Every word of `text` must
/// match the start of a word; FTS syntax in `text` has no effect.
pub fn search_songs(
    conn: &Connection,
    server: ServerId,
    text: &str,
    limit: u32,
) -> Result<Vec<Song>, DbError> {
    let Some(query) = fts_query(text) else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT {SONG_COLUMNS} FROM song_fts f JOIN song s ON s.rowid = f.rowid
         WHERE song_fts MATCH ?1 AND s.server_id = ?2
         ORDER BY f.rank LIMIT ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![query, server.0, limit], song_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Turns user text into an FTS5 query: each word becomes a quoted prefix
/// term, so quotes, `OR`, `NEAR`, `*` or column filters typed by the user
/// are searched for literally instead of being interpreted.
fn fts_query(text: &str) -> Option<String> {
    const MAX_TERMS: usize = 16;
    let terms: Vec<String> = text
        .split_whitespace()
        .take(MAX_TERMS)
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::db::tests::temp_db;

    pub(crate) fn song(id: &str, album: &str, title: &str) -> Song {
        Song {
            id: id.into(),
            album_id: Some(album.into()),
            artist_id: Some("ar1".into()),
            title: title.into(),
            album_name: Some("Bal à Nantes".into()),
            artist_name: Some("Blowzabella".into()),
            ..Song::default()
        }
    }

    fn artist(id: &str, name: &str) -> Artist {
        Artist {
            id: id.into(),
            name: name.into(),
            sort_name: None,
            mbid: None,
        }
    }

    fn album(id: &str, artist_id: &str, name: &str, year: i32) -> Album {
        Album {
            id: id.into(),
            artist_id: Some(artist_id.into()),
            artist_name: None,
            name: name.into(),
            sort_name: None,
            year: Some(year),
            mbid: None,
            cover_art: None,
        }
    }

    fn full_set() -> ChangeSet {
        ChangeSet {
            full: true,
            server_time: 1000,
            artists: vec![artist("ar1", "Blowzabella"), artist("ar2", "alba")],
            albums: vec![
                album("al1", "ar1", "Pingha Frenzy", 1983),
                album("al2", "ar1", "Vanilla", 1990),
            ],
            songs: vec![
                song("s1", "al1", "An Dro"),
                song("s2", "al1", "Bourrée"),
                song("s3", "al2", "Mazurka"),
            ],
            playlists: vec![Playlist {
                id: "p1".into(),
                name: "Course 1".into(),
                song_ids: vec!["s3".into(), "s1".into()],
            }],
            deleted: vec![],
        }
    }

    fn setup() -> (tempfile::TempDir, Database, ServerId) {
        let (dir, db) = temp_db();
        let server = ensure_server(&db, "https://music.example", 1).unwrap();
        (dir, db, server)
    }

    #[test]
    fn ensure_server_is_idempotent() {
        let (_dir, db, server) = setup();
        assert_eq!(
            ensure_server(&db, "https://music.example", 2).unwrap(),
            server
        );
        let other = ensure_server(&db, "https://other.example", 2).unwrap();
        assert_ne!(other, server);
    }

    #[test]
    fn full_sync_then_queries() {
        let (_dir, db, server) = setup();
        let report = apply_changes(&db, server, &full_set(), 10).unwrap();
        assert_eq!(report.upserted, 2 + 2 + 3 + 1);
        let conn = db.reader().unwrap();

        let names: Vec<_> = artists(&conn, server)
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["alba", "Blowzabella"], "case-insensitive order");

        let albums: Vec<_> = albums_of_artist(&conn, server, "ar1")
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(albums, ["al1", "al2"]);

        let songs = songs_of_album(&conn, server, "al1").unwrap();
        assert_eq!(songs.len(), 2);

        let pl: Vec<_> = playlist_songs(&conn, server, "p1")
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(pl, ["s3", "s1"]);
        assert_eq!(
            playlists(&conn, server).unwrap(),
            [("p1".to_string(), "Course 1".to_string())]
        );
        assert_eq!(sync_since(&conn, server).unwrap(), Some(1000));
    }

    #[test]
    fn full_sync_removes_what_is_gone() {
        let (_dir, db, server) = setup();
        apply_changes(&db, server, &full_set(), 10).unwrap();
        let mut next = full_set();
        next.songs.retain(|s| s.id != "s2");
        next.artists.retain(|a| a.id != "ar2");
        next.playlists.clear();
        // Same millisecond as the first run: still must sweep.
        apply_changes(&db, server, &next, 10).unwrap();
        let conn = db.reader().unwrap();
        assert_eq!(songs_of_album(&conn, server, "al1").unwrap().len(), 1);
        assert_eq!(artists(&conn, server).unwrap().len(), 1);
        assert!(playlists(&conn, server).unwrap().is_empty());
        let entries: i64 = conn
            .query_row("SELECT count(*) FROM playlist_entry", [], |r| r.get(0))
            .unwrap();
        assert_eq!(entries, 0, "entries go with their playlist");
    }

    #[test]
    fn incremental_sync_updates_and_deletes_only_what_changed() {
        let (_dir, db, server) = setup();
        apply_changes(&db, server, &full_set(), 10).unwrap();
        let mut renamed = song("s1", "al1", "An Dro Pleiben");
        renamed.bpm = Some(132.0);
        let delta = ChangeSet {
            full: false,
            server_time: 2000,
            songs: vec![renamed],
            deleted: vec![Deletion {
                kind: CatalogKind::Song,
                id: "s2".into(),
            }],
            ..ChangeSet::default()
        };
        let report = apply_changes(&db, server, &delta, 20).unwrap();
        assert_eq!((report.upserted, report.deleted), (1, 1));
        let conn = db.reader().unwrap();
        let songs = songs_of_album(&conn, server, "al1").unwrap();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].title, "An Dro Pleiben");
        assert_eq!(songs[0].bpm, Some(132.0));
        assert_eq!(
            songs_of_album(&conn, server, "al2").unwrap().len(),
            1,
            "untouched"
        );
        assert_eq!(sync_since(&conn, server).unwrap(), Some(2000));
    }

    #[test]
    fn invalid_values_are_cleared_and_bad_ids_skipped() {
        let (_dir, db, server) = setup();
        let mut odd = song("s1", "al1", "Odd");
        odd.bpm = Some(1000.0);
        odd.tempo_confidence = Some(f64::NAN);
        odd.year = Some(-5);
        odd.track_peak = Some(-1.0);
        let changes = ChangeSet {
            server_time: 5,
            songs: vec![
                odd,
                song("", "al1", "No id"),
                song(&"x".repeat(MAX_ID_LEN + 1), "al1", "Long id"),
            ],
            ..ChangeSet::default()
        };
        let report = apply_changes(&db, server, &changes, 10).unwrap();
        assert_eq!((report.upserted, report.skipped), (1, 2));
        let s = &songs_of_album(&db.reader().unwrap(), server, "al1").unwrap()[0];
        assert_eq!(
            (s.bpm, s.tempo_confidence, s.year, s.track_peak),
            (None, None, None, None)
        );
    }

    #[test]
    fn search_finds_prefixes_and_ignores_fts_syntax() {
        let (_dir, db, server) = setup();
        apply_changes(&db, server, &full_set(), 10).unwrap();
        let conn = db.reader().unwrap();
        let ids = |q: &str| -> Vec<String> {
            let mut v: Vec<_> = search_songs(&conn, server, q, 50)
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect();
            v.sort();
            v
        };
        assert_eq!(ids("dro"), ["s1"]);
        assert_eq!(ids("bourree"), ["s2"], "diacritics removed");
        assert_eq!(ids("blowza maz"), ["s3"]);
        assert_eq!(ids("blowzabella").len(), 3);
        assert!(ids("").is_empty());
        // FTS operators and quotes are searched literally, never a syntax error.
        for hostile in [
            "\"",
            "an OR maz",
            "title:dro",
            "NEAR(a b)",
            "*",
            "dro\" OR \"x",
            "-x",
            "^a",
        ] {
            assert!(
                search_songs(&conn, server, hostile, 50).is_ok(),
                "{hostile}"
            );
        }
        assert!(
            ids("an OR maz").is_empty(),
            "OR is a word here, not an operator"
        );
    }

    #[test]
    fn search_follows_updates_and_deletes() {
        let (_dir, db, server) = setup();
        apply_changes(&db, server, &full_set(), 10).unwrap();
        let delta = ChangeSet {
            server_time: 2,
            songs: vec![song("s1", "al1", "Laridé")],
            deleted: vec![Deletion {
                kind: CatalogKind::Song,
                id: "s3".into(),
            }],
            ..ChangeSet::default()
        };
        apply_changes(&db, server, &delta, 20).unwrap();
        let conn = db.reader().unwrap();
        assert!(search_songs(&conn, server, "dro", 10).unwrap().is_empty());
        assert_eq!(search_songs(&conn, server, "laride", 10).unwrap().len(), 1);
        assert!(
            search_songs(&conn, server, "mazurka", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn forget_server_removes_its_catalog_only() {
        let (_dir, db, server) = setup();
        let other = ensure_server(&db, "https://other.example", 1).unwrap();
        apply_changes(&db, server, &full_set(), 10).unwrap();
        apply_changes(&db, other, &full_set(), 10).unwrap();
        forget_server(&db, server).unwrap();
        let conn = db.reader().unwrap();
        for table in ["artist", "album", "song", "playlist", "playlist_entry"] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE server_id = ?1"),
                    [other.0],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(n > 0, "{table} of the other server kept");
            let n: i64 = conn
                .query_row(
                    &format!("SELECT count(*) FROM {table} WHERE server_id = ?1"),
                    [server.0],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        assert!(search_songs(&conn, other, "dro", 10).unwrap().len() == 1);
        assert!(server_base_url(&conn, server).unwrap().is_none());
    }

    #[test]
    fn hot_queries_use_indexes() {
        let (_dir, db, server) = setup();
        let conn = db.reader().unwrap();
        let plan = |sql: &str| -> String {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
            rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
        };
        let _ = server;
        for sql in [
            "SELECT * FROM song WHERE server_id = 1 AND album_id = 'a'",
            "SELECT * FROM album WHERE server_id = 1 AND artist_id = 'a'",
            "SELECT * FROM song WHERE mbid = 'm'",
            "DELETE FROM song WHERE server_id = 1 AND synced_at < 5",
        ] {
            let p = plan(sql);
            assert!(
                p.contains("USING INDEX") || p.contains("USING COVERING INDEX"),
                "{sql}\n{p}"
            );
        }
    }
    #[test]
    fn find_server_never_creates_one() {
        let (_dir, db) = temp_db();
        assert_eq!(
            find_server(&db.reader().unwrap(), "https://music.example").unwrap(),
            None
        );
        let created = ensure_server(&db, "https://music.example", 1).unwrap();
        assert_eq!(
            find_server(&db.reader().unwrap(), "https://music.example").unwrap(),
            Some(created)
        );
        assert_eq!(
            find_server(&db.reader().unwrap(), "https://other.example").unwrap(),
            None
        );
    }
}
