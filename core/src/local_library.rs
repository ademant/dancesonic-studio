//! Local music folders (#10): the folders the user picked, the audio files in
//! them and their tags.
//!
//! A scan walks a folder, reads the tags of new or changed files with `lofty`
//! and records each file by its path relative to the folder. A file that moved
//! or was renamed is found again by its fingerprint, so it keeps its row. A
//! file that is gone loses its row. Studio never changes or deletes a file on
//! disk.
//!
//! Symbolic links are not followed, so a scan never leaves the folder, and a
//! path read back from the database is checked to be inside its folder before
//! it is played.
//!
//! All calls here block; run them off the GTK main thread.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, UNIX_EPOCH};

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::catalog::{ServerId, Song};
use crate::db::{Database, DbError};

/// File name extensions read as audio. Anything else is skipped.
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "ape", "flac", "m4a", "mp3", "mp4", "mpc", "oga", "ogg", "opus", "spx",
    "wav", "wma", "wv",
];

/// Bytes read from each end of a file for its fingerprint.
const FINGERPRINT_SPAN: u64 = 64 * 1024;

/// Deepest folder level a scan descends to.
const MAX_DEPTH: usize = 32;

/// Most files one folder may hold; a scan stops counting beyond it.
const MAX_FILES: usize = 200_000;

/// Rows written per transaction during a scan.
const WRITE_BATCH: usize = 200;

/// Prefix of the song ids given to tracks that exist only locally. Server ids
/// never reach the queue with it, because a local-only song is never sent to
/// the server.
pub const LOCAL_ID_PREFIX: &str = "local:";

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is not a folder that can be added")]
    NotAFolder(PathBuf),
    #[error("the tags could not be read: {0}")]
    Tags(String),
    #[error("folder watching failed: {0}")]
    Watch(String),
}

impl From<rusqlite::Error> for LocalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::Sqlite(error))
    }
}

/// A folder the user added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalFolder {
    pub id: i64,
    pub path: PathBuf,
}

/// The tags Studio reads from a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LocalTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<i32>,
    pub duration_ms: Option<u64>,
    pub mbid: Option<String>,
    pub release_mbid: Option<String>,
    pub artist_mbid: Option<String>,
    pub bpm: Option<f64>,
}

/// What a scan changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub moved: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// Audio files whose tags could not be read; they are listed with their
    /// file name as title.
    pub unreadable: usize,
}

/// An artist with tracks that only exist locally, and their albums.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalArtist {
    pub name: String,
    pub albums: Vec<String>,
}

/// Which local-only tracks to list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalFilter {
    Artist(String),
    Album {
        artist: String,
        album: String,
    },
    Search(String),
    /// One track by its local ID (the number in `local:<n>`).
    Id(i64),
}

/// Adds a folder. The path is made absolute and resolved, so the same folder
/// is not added twice under two names.
pub fn add_folder(db: &Database, path: &Path, now_ms: i64) -> Result<LocalFolder, LocalError> {
    let resolved = fs::canonicalize(path).map_err(|source| LocalError::Io {
        path: path.to_owned(),
        source,
    })?;
    if !resolved.is_dir() {
        return Err(LocalError::NotAFolder(path.to_owned()));
    }
    let text = resolved
        .to_str()
        .ok_or_else(|| LocalError::NotAFolder(path.to_owned()))?
        .to_owned();
    let id = db.write(|tx| {
        tx.execute(
            "INSERT INTO local_folder (path, added_at) VALUES (?1, ?2)
             ON CONFLICT (path) DO NOTHING",
            params![text, now_ms],
        )?;
        Ok(tx.query_row(
            "SELECT id FROM local_folder WHERE path = ?1",
            [&text],
            |r| r.get(0),
        )?)
    })?;
    Ok(LocalFolder { id, path: resolved })
}

/// Forgets a folder and its tracks. The files stay on disk.
pub fn remove_folder(db: &Database, folder: i64) -> Result<(), LocalError> {
    db.write(|tx| {
        tx.execute("DELETE FROM local_folder WHERE id = ?1", [folder])?;
        Ok(())
    })?;
    Ok(())
}

/// The folders, by path.
pub fn folders(conn: &Connection) -> Result<Vec<LocalFolder>, LocalError> {
    let mut stmt = conn.prepare_cached("SELECT id, path FROM local_folder ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        Ok(LocalFolder {
            id: r.get(0)?,
            path: PathBuf::from(r.get::<_, String>(1)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One audio file found on disk.
struct Found {
    rel_path: String,
    size: u64,
    modified_ms: i64,
}

/// A row already known for a folder.
struct Known {
    id: i64,
    size: u64,
    modified_ms: i64,
    fingerprint: String,
}

/// A file to write, with what was read from it.
struct Scanned {
    rel_path: String,
    size: u64,
    modified_ms: i64,
    fingerprint: String,
    tags: LocalTags,
}

/// Scans a folder: adds new files, re-reads changed ones, follows moved ones
/// and drops rows of files that are gone.
pub fn scan_folder(
    db: &Database,
    folder: &LocalFolder,
    now_ms: i64,
) -> Result<ScanReport, LocalError> {
    let mut found = Vec::new();
    walk(&folder.path, &folder.path, 0, &mut found);

    let mut known: HashMap<String, Known> = {
        let conn = db.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT id, rel_path, size_bytes, modified_ms, fingerprint
             FROM local_track WHERE folder_id = ?1",
        )?;
        let rows = stmt.query_map([folder.id], |r| {
            Ok((
                r.get::<_, String>(1)?,
                Known {
                    id: r.get(0)?,
                    size: r.get::<_, i64>(2)?.try_into().unwrap_or(0),
                    modified_ms: r.get(3)?,
                    fingerprint: r.get(4)?,
                },
            ))
        })?;
        rows.collect::<Result<_, _>>()?
    };

    let mut report = ScanReport::default();
    let mut changed = Vec::new();
    let mut new = Vec::new();
    for file in found {
        match known.remove(&file.rel_path) {
            Some(row) if row.size == file.size && row.modified_ms == file.modified_ms => {
                report.unchanged += 1;
            }
            Some(row) => changed.push((row.id, file)),
            None => new.push(file),
        }
    }
    // What is left in `known` is gone from its old path: moved or removed.
    let mut gone: HashMap<(String, u64), Vec<i64>> = HashMap::new();
    for row in known.into_values() {
        gone.entry((row.fingerprint, row.size))
            .or_default()
            .push(row.id);
    }

    let mut updates: Vec<(i64, Scanned)> = Vec::new();
    for (id, file) in changed {
        if let Some(scanned) = read_file(folder, file, &mut report) {
            updates.push((id, scanned));
            report.updated += 1;
        }
    }
    let mut moves: Vec<(i64, String, i64)> = Vec::new();
    let mut inserts: Vec<Scanned> = Vec::new();
    for file in new {
        let path = folder.path.join(&file.rel_path);
        let Ok(print) = fingerprint(&path, file.size) else {
            continue;
        };
        if let Some(id) = gone.get_mut(&(print.clone(), file.size)).and_then(Vec::pop) {
            moves.push((id, file.rel_path, file.modified_ms));
            report.moved += 1;
            continue;
        }
        if let Some(scanned) = read_file(folder, file, &mut report) {
            inserts.push(scanned);
            report.added += 1;
        }
    }
    let removed: Vec<i64> = gone.into_values().flatten().collect();
    report.removed = removed.len();

    // Moves first, so a file moved onto the path of a removed one is free.
    db.write(|tx| {
        let mut delete = tx.prepare_cached("DELETE FROM local_track WHERE id = ?1")?;
        for id in &removed {
            delete.execute([id])?;
        }
        let mut relocate = tx.prepare_cached(
            "UPDATE local_track SET rel_path = ?2, modified_ms = ?3, scanned_at = ?4 WHERE id = ?1",
        )?;
        for (id, rel_path, modified_ms) in &moves {
            relocate.execute(params![id, rel_path, modified_ms, now_ms])?;
        }
        Ok(())
    })?;
    for batch in updates.chunks(WRITE_BATCH) {
        db.write(|tx| {
            for (id, scanned) in batch {
                write_track(tx, folder.id, Some(*id), scanned, now_ms)?;
            }
            Ok(())
        })?;
    }
    for batch in inserts.chunks(WRITE_BATCH) {
        db.write(|tx| {
            for scanned in batch {
                write_track(tx, folder.id, None, scanned, now_ms)?;
            }
            Ok(())
        })?;
    }
    db.write(|tx| {
        tx.execute(
            "UPDATE local_folder SET scanned_at = ?2 WHERE id = ?1",
            params![folder.id, now_ms],
        )?;
        Ok(())
    })?;
    tracing::info!(
        added = report.added,
        updated = report.updated,
        moved = report.moved,
        removed = report.removed,
        "local folder scanned"
    );
    Ok(report)
}

/// Reads tags and fingerprint of a found file. A file that cannot be opened
/// is skipped; one whose tags cannot be read is kept with its file name.
fn read_file(folder: &LocalFolder, file: Found, report: &mut ScanReport) -> Option<Scanned> {
    let path = folder.path.join(&file.rel_path);
    let fingerprint = match fingerprint(&path, file.size) {
        Ok(print) => print,
        Err(error) => {
            tracing::debug!(%error, "a local file could not be read");
            return None;
        }
    };
    let tags = match read_tags(&path) {
        Ok(tags) => tags,
        Err(error) => {
            tracing::debug!(%error, "tags not readable");
            report.unreadable += 1;
            LocalTags::default()
        }
    };
    Some(Scanned {
        rel_path: file.rel_path,
        size: file.size,
        modified_ms: file.modified_ms,
        fingerprint,
        tags,
    })
}

fn write_track(
    tx: &rusqlite::Transaction<'_>,
    folder: i64,
    id: Option<i64>,
    scanned: &Scanned,
    now_ms: i64,
) -> Result<(), DbError> {
    let t = &scanned.tags;
    let size = i64::try_from(scanned.size).unwrap_or(i64::MAX);
    let duration = t.duration_ms.and_then(|d| i64::try_from(d).ok());
    match id {
        Some(id) => {
            tx.prepare_cached(
                "UPDATE local_track SET size_bytes = ?2, modified_ms = ?3, fingerprint = ?4,
                     title = ?5, artist = ?6, album = ?7, album_artist = ?8, track = ?9,
                     disc = ?10, year = ?11, duration_ms = ?12, mbid = ?13,
                     release_mbid = ?14, artist_mbid = ?15, bpm = ?16, scanned_at = ?17
                 WHERE id = ?1",
            )?
            .execute(params![
                id,
                size,
                scanned.modified_ms,
                scanned.fingerprint,
                t.title,
                t.artist,
                t.album,
                t.album_artist,
                t.track,
                t.disc,
                t.year,
                duration,
                t.mbid,
                t.release_mbid,
                t.artist_mbid,
                t.bpm,
                now_ms
            ])?;
        }
        None => {
            tx.prepare_cached(
                "INSERT INTO local_track (folder_id, rel_path, size_bytes, modified_ms,
                     fingerprint, title, artist, album, album_artist, track, disc, year,
                     duration_ms, mbid, release_mbid, artist_mbid, bpm, scanned_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18)
                 ON CONFLICT (folder_id, rel_path) DO NOTHING",
            )?
            .execute(params![
                folder,
                scanned.rel_path,
                size,
                scanned.modified_ms,
                scanned.fingerprint,
                t.title,
                t.artist,
                t.album,
                t.album_artist,
                t.track,
                t.disc,
                t.year,
                duration,
                t.mbid,
                t.release_mbid,
                t.artist_mbid,
                t.bpm,
                now_ms
            ])?;
        }
    }
    Ok(())
}

/// Collects the audio files under `dir`. Symbolic links, hidden entries and
/// names that are not UTF-8 are skipped.
fn walk(root: &Path, dir: &Path, depth: usize, found: &mut Vec<Found>) {
    if depth > MAX_DEPTH || found.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        tracing::debug!("a local folder could not be listed");
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        // `file_type` does not follow links, so a link is never entered.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            walk(root, &path, depth + 1, found);
        } else if kind.is_file() && is_audio(name) {
            let Ok(meta) = entry.metadata() else { continue };
            let Some(rel_path) = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .map(str::to_owned)
            else {
                continue;
            };
            found.push(Found {
                rel_path,
                size: meta.len(),
                modified_ms: modified_ms(&meta),
            });
            if found.len() >= MAX_FILES {
                tracing::warn!(limit = MAX_FILES, "a local folder holds too many files");
                return;
            }
        }
    }
}

fn is_audio(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(e))
        })
}

fn modified_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// SHA-256 over the size and the first and last 64 KiB: the same for a moved
/// file, cheap for a large library.
pub fn fingerprint(path: &Path, size: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    hash.update(size.to_le_bytes());
    let mut buffer = vec![0u8; usize::try_from(FINGERPRINT_SPAN).unwrap_or(65_536)];
    let head = read_up_to(&mut file, &mut buffer)?;
    hash.update(&buffer[..head]);
    if size > FINGERPRINT_SPAN * 2 {
        file.seek(SeekFrom::Start(size - FINGERPRINT_SPAN))?;
        let tail = read_up_to(&mut file, &mut buffer)?;
        hash.update(&buffer[..tail]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = file.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

/// Reads the tags Studio uses. Cover art is not read. Values are untrusted:
/// texts are trimmed and bounded, MBIDs must look like UUIDs, and a BPM must
/// be within 20–300.
pub fn read_tags(path: &Path) -> Result<LocalTags, LocalError> {
    let file = Probe::open(path)
        .map_err(|e| LocalError::Tags(e.to_string()))?
        .options(ParseOptions::new().read_cover_art(false))
        .read()
        .map_err(|e| LocalError::Tags(e.to_string()))?;
    let duration = file.properties().duration();
    let mut tags = LocalTags {
        duration_ms: (duration > Duration::ZERO)
            .then(|| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        ..LocalTags::default()
    };
    let Some(tag) = file.primary_tag().or_else(|| file.first_tag()) else {
        return Ok(tags);
    };
    tags.title = text(tag.title().as_deref());
    tags.artist = text(tag.artist().as_deref());
    tags.album = text(tag.album().as_deref());
    tags.album_artist = text(tag.get_string(ItemKey::AlbumArtist));
    tags.track = tag.track().filter(|n| *n > 0);
    tags.disc = tag.disk().filter(|n| *n > 0);
    tags.year = tag
        .date()
        .map(|d| i32::from(d.year))
        .filter(|y| (1..=9999).contains(y));
    tags.mbid = uuid(tag, ItemKey::MusicBrainzRecordingId);
    tags.release_mbid = uuid(tag, ItemKey::MusicBrainzReleaseId);
    tags.artist_mbid = uuid(tag, ItemKey::MusicBrainzArtistId);
    tags.bpm = [ItemKey::Bpm, ItemKey::IntegerBpm]
        .into_iter()
        .filter_map(|key| tag.get_string(key))
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .find(|bpm| (20.0..=300.0).contains(bpm));
    Ok(tags)
}

/// A tag text, trimmed and at most 1024 characters; `None` when empty.
fn text(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.chars().take(1024).collect())
}

/// An MBID from a tag, in lower case, if it is a UUID.
fn uuid(tag: &Tag, key: ItemKey) -> Option<String> {
    let value = tag.get_string(key)?.trim().to_ascii_lowercase();
    let shape = value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    shape.then_some(value)
}

/// The file of a local track, if it is still inside its folder. `rel_path`
/// comes from the database, so it is checked again: no absolute path, no
/// `..`, and no link that leads out of the folder.
pub fn resolve(root: &Path, rel_path: &str) -> Option<PathBuf> {
    let relative = Path::new(rel_path);
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let root = fs::canonicalize(root).ok()?;
    let path = fs::canonicalize(root.join(relative)).ok()?;
    (path.starts_with(&root) && path.is_file()).then_some(path)
}

/// Fills in `local_path` for songs that have a local file, checking each
/// path. Songs whose file is gone keep `None`.
pub fn resolve_paths(conn: &Connection, songs: &mut [Song]) -> Result<(), LocalError> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.path, t.rel_path FROM local_track t
         JOIN local_folder f ON f.id = t.folder_id WHERE t.id = ?1",
    )?;
    for song in songs.iter_mut() {
        let Some(id) = song.local_id else { continue };
        let row: Option<(String, String)> = stmt
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        song.local_path = row.and_then(|(root, rel)| resolve(Path::new(&root), &rel));
    }
    Ok(())
}

/// The condition for a local track that no song of `server` matches.
const LOCAL_ONLY: &str = "(t.mbid IS NULL OR NOT EXISTS (
     SELECT 1 FROM song s WHERE s.server_id = ?1 AND s.mbid = t.mbid))";

/// The name a local track is listed under.
const ARTIST_OF: &str = "coalesce(t.album_artist, t.artist, 'Unknown artist')";
const ALBUM_OF: &str = "coalesce(t.album, 'Unknown album')";

/// Artists of local tracks that no server song matches, with their albums.
/// `server` is the connected server, if any.
pub fn local_artists(
    conn: &Connection,
    server: Option<ServerId>,
) -> Result<Vec<LocalArtist>, LocalError> {
    let sql = format!(
        "SELECT DISTINCT {ARTIST_OF}, {ALBUM_OF} FROM local_track t WHERE {LOCAL_ONLY}
         ORDER BY 1 COLLATE NOCASE, 2 COLLATE NOCASE"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map([server_key(server)], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut artists: Vec<LocalArtist> = Vec::new();
    for row in rows {
        let (artist, album) = row?;
        match artists.last_mut() {
            Some(last) if last.name.eq_ignore_ascii_case(&artist) => last.albums.push(album),
            _ => artists.push(LocalArtist {
                name: artist,
                albums: vec![album],
            }),
        }
    }
    Ok(artists)
}

/// Local tracks that no server song matches, as songs of the library.
pub fn local_only_songs(
    conn: &Connection,
    server: Option<ServerId>,
    filter: &LocalFilter,
    limit: u32,
) -> Result<Vec<Song>, LocalError> {
    let columns = "t.id, t.title, t.artist, t.album, t.track, t.disc, t.year, t.duration_ms,
                   t.mbid, t.bpm, t.size_bytes, t.rel_path";
    let order = "ORDER BY coalesce(t.album, '') COLLATE NOCASE, t.disc, t.track,
                 coalesce(t.title, t.rel_path) COLLATE NOCASE, t.id LIMIT ?4";
    let server = server_key(server);
    let (sql, a, b) = match filter {
        LocalFilter::Artist(name) => (
            format!(
                "SELECT {columns} FROM local_track t WHERE {LOCAL_ONLY}
                 AND {ARTIST_OF} = ?2 COLLATE NOCASE AND ?3 = ?3 {order}"
            ),
            name.clone(),
            String::new(),
        ),
        LocalFilter::Album { artist, album } => (
            format!(
                "SELECT {columns} FROM local_track t WHERE {LOCAL_ONLY}
                 AND {ARTIST_OF} = ?2 COLLATE NOCASE AND {ALBUM_OF} = ?3 COLLATE NOCASE {order}"
            ),
            artist.clone(),
            album.clone(),
        ),
        LocalFilter::Search(text) => {
            let pattern = format!("%{}%", like_escape(text.trim()));
            (
                format!(
                    "SELECT {columns} FROM local_track t WHERE {LOCAL_ONLY}
                     AND (t.title LIKE ?2 ESCAPE '\\' OR t.artist LIKE ?2 ESCAPE '\\'
                          OR t.album LIKE ?2 ESCAPE '\\' OR t.rel_path LIKE ?2 ESCAPE '\\')
                     AND ?3 = ?3 {order}"
                ),
                pattern,
                String::new(),
            )
        }
        LocalFilter::Id(id) => (
            format!(
                "SELECT {columns} FROM local_track t WHERE {LOCAL_ONLY}
                 AND t.id = CAST(?2 AS INTEGER) AND ?3 = ?3 {order}"
            ),
            id.to_string(),
            String::new(),
        ),
    };
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![server, a, b, limit], |r| {
        let id: i64 = r.get(0)?;
        let rel_path: String = r.get(11)?;
        let title = r
            .get::<_, Option<String>>(1)?
            .unwrap_or_else(|| file_title(&rel_path));
        Ok(Song {
            id: format!("{LOCAL_ID_PREFIX}{id}"),
            title,
            artist_name: r.get(2)?,
            album_name: r.get(3)?,
            track: r.get(4)?,
            disc: r.get(5)?,
            year: r.get(6)?,
            duration_ms: r
                .get::<_, Option<i64>>(7)?
                .and_then(|d| u64::try_from(d).ok()),
            mbid: r.get(8)?,
            bpm: r.get(9)?,
            size_bytes: r
                .get::<_, Option<i64>>(10)?
                .and_then(|b| u64::try_from(b).ok()),
            suffix: Path::new(&rel_path)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase),
            local_id: Some(id),
            on_server: false,
            ..Song::default()
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A server id that matches nothing when no server is connected.
fn server_key(server: Option<ServerId>) -> i64 {
    server.map_or(-1, ServerId::get)
}

fn like_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The file name without its extension, for a track without a title tag.
fn file_title(rel_path: &str) -> String {
    Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path)
        .to_owned()
}

/// Watches folders and reports which one changed, at most once per quiet
/// period, so a copy of many files gives one rescan. Dropping it stops it.
pub struct FolderWatcher {
    _watcher: notify::RecommendedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for FolderWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderWatcher")
    }
}

impl FolderWatcher {
    /// Starts watching `folders` recursively. `on_change` gets a folder id
    /// after its files changed and then were quiet for `quiet`.
    pub fn start(
        folders: &[LocalFolder],
        quiet: Duration,
        on_change: impl Fn(i64) + Send + 'static,
    ) -> Result<Self, LocalError> {
        use notify::Watcher as _;
        let (sender, receiver) = mpsc::channel::<PathBuf>();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if let Ok(event) = event
                    && !matches!(event.kind, notify::EventKind::Access(_))
                {
                    for path in event.paths {
                        // The receiver is gone only when the watcher stops.
                        let _ = sender.send(path);
                    }
                }
            })
            .map_err(|e| LocalError::Watch(e.to_string()))?;
        for folder in folders {
            watcher
                .watch(&folder.path, notify::RecursiveMode::Recursive)
                .map_err(|e| LocalError::Watch(e.to_string()))?;
        }
        let roots: Vec<LocalFolder> = folders.to_vec();
        let thread = std::thread::Builder::new()
            .name("folder-watch".to_owned())
            .spawn(move || debounce(&receiver, &roots, quiet, &on_change))
            .map_err(|e| LocalError::Watch(e.to_string()))?;
        Ok(Self {
            _watcher: watcher,
            _thread: thread,
        })
    }
}

/// Collects changed paths until nothing changed for `quiet`, then reports
/// each folder they belong to once. Ends when the watcher is dropped.
fn debounce(
    receiver: &mpsc::Receiver<PathBuf>,
    roots: &[LocalFolder],
    quiet: Duration,
    on_change: &dyn Fn(i64),
) {
    let mut pending: Vec<i64> = Vec::new();
    loop {
        let next = if pending.is_empty() {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(quiet)
        };
        match next {
            Ok(path) => {
                if let Some(folder) = roots.iter().find(|f| path.starts_with(&f.path))
                    && !pending.contains(&folder.id)
                {
                    pending.push(folder.id);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                for folder in pending.drain(..) {
                    on_change(folder);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests;
