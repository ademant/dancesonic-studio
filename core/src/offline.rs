//! Server tracks kept offline (#17).
//!
//! The user marks albums and playlists "keep offline". [`sync`] downloads
//! their songs in the original format (`download`) into the offline folder,
//! up to a size limit, and removes copies that are no longer wanted. Copies
//! play from disk without the server.
//!
//! Files are named by a SHA-256 of server and song id and live in a folder of
//! their own (`0700`, files `0600`), so nothing the server sends becomes part
//! of a path, and only files with that name shape are ever deleted. Each copy
//! is checked against its recorded size on every sync and against its
//! recorded SHA-256 by [`verify`].

use std::collections::HashSet;
use std::fs::{self, DirBuilder};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::catalog::{ServerId, Song};
use crate::connection::ServerUrl;
use crate::db::{Database, DbError};
use crate::queue::SongId;
use crate::subsonic::{API_VERSION, Auth, CLIENT_NAME, credential};

/// Largest original file kept offline.
pub const MAX_OFFLINE_TRACK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The name of Studio's own folder inside the folder the user picked, so a
/// cleanup never touches the user's other files.
pub const FOLDER_NAME: &str = "dancesonic-studio-offline";

/// Default limit for all copies together: 10 GiB.
pub const DEFAULT_LIMIT_BYTES: u64 = 10 * 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum OfflineError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("the offline folder: {0}")]
    Io(#[from] std::io::Error),
    #[error("the server could not be reached")]
    Unreachable,
    #[error("the server refused the download (HTTP {0})")]
    Http(u16),
    #[error("the server did not send audio")]
    NotAudio,
    #[error("the file is larger than allowed or than announced")]
    TooLarge,
    #[error("the download stopped early")]
    Incomplete,
    #[error("the device key could not sign the request")]
    Signing,
    #[error("the database task stopped")]
    Task,
}

impl From<rusqlite::Error> for OfflineError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::Sqlite(error))
    }
}

/// What can be pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinKind {
    Album,
    Playlist,
}

impl PinKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Playlist => "playlist",
        }
    }
}

/// Where copies live and how much space they may take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The folder the user picked; `None` for the default.
    pub folder: Option<PathBuf>,
    pub limit_bytes: u64,
}

/// What a sync did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub downloaded: usize,
    pub removed: usize,
    pub kept: usize,
    /// Wanted songs left out because the limit was reached.
    pub over_limit: usize,
    pub failed: usize,
    pub used_bytes: u64,
}

/// The server to download from: a streaming HTTP client (no total timeout),
/// its address and the credential.
#[derive(Clone, Debug)]
pub struct Source {
    pub http: reqwest::Client,
    pub server: ServerUrl,
    pub auth: Auth,
}

pub fn settings(conn: &Connection) -> Result<Settings, OfflineError> {
    let (folder, limit): (Option<String>, i64) = conn.query_row(
        "SELECT folder, limit_bytes FROM offline_settings WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Settings {
        folder: folder.map(PathBuf::from),
        limit_bytes: u64::try_from(limit).unwrap_or(DEFAULT_LIMIT_BYTES),
    })
}

/// Changes the settings. A new folder starts empty: the copies in the old one
/// are deleted and downloaded again by the next sync.
pub fn set_settings(
    db: &Database,
    default_root: &Path,
    settings: &Settings,
) -> Result<(), OfflineError> {
    let old = self::settings(&db.reader()?)?;
    let folder = match &settings.folder {
        Some(path) => Some(
            fs::canonicalize(path)?
                .to_str()
                .ok_or_else(|| std::io::Error::other("the folder name is not UTF-8"))?
                .to_owned(),
        ),
        None => None,
    };
    let limit = i64::try_from(settings.limit_bytes.max(1)).unwrap_or(i64::MAX);
    let moved = old.folder.as_deref().map(Path::to_owned) != folder.as_deref().map(PathBuf::from);
    if moved {
        let old_dir = store_dir(default_root, &old);
        let names = db.write(|tx| {
            let names: Vec<String> = tx
                .prepare("SELECT file_name FROM offline_track")?
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            tx.execute("DELETE FROM offline_track", [])?;
            Ok(names)
        })?;
        for name in names {
            remove_copy(&old_dir, &name);
        }
    }
    db.write(|tx| {
        tx.execute(
            "UPDATE offline_settings SET folder = ?1, limit_bytes = ?2 WHERE id = 1",
            params![folder, limit],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// The folder that holds the copies: Studio's own folder inside the one the
/// user picked, or `default_root` itself.
pub fn store_dir(default_root: &Path, settings: &Settings) -> PathBuf {
    match &settings.folder {
        Some(folder) => folder.join(FOLDER_NAME),
        None => default_root.to_owned(),
    }
}

/// The folder that holds the copies now.
pub fn current_dir(conn: &Connection, default_root: &Path) -> Result<PathBuf, OfflineError> {
    Ok(store_dir(default_root, &settings(conn)?))
}

/// Marks or unmarks an album or playlist "keep offline".
pub fn set_pinned(
    db: &Database,
    base_url: &str,
    kind: PinKind,
    item_id: &str,
    pinned: bool,
    now_ms: i64,
) -> Result<(), OfflineError> {
    db.write(|tx| {
        if pinned {
            tx.execute(
                "INSERT INTO offline_pin (base_url, kind, item_id, pinned_at)
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                params![base_url, kind.as_str(), item_id, now_ms],
            )?;
        } else {
            tx.execute(
                "DELETE FROM offline_pin WHERE base_url = ?1 AND kind = ?2 AND item_id = ?3",
                params![base_url, kind.as_str(), item_id],
            )?;
        }
        Ok(())
    })?;
    Ok(())
}

pub fn is_pinned(
    conn: &Connection,
    base_url: &str,
    kind: PinKind,
    item_id: &str,
) -> Result<bool, OfflineError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM offline_pin WHERE base_url = ?1 AND kind = ?2 AND item_id = ?3",
            params![base_url, kind.as_str(), item_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The songs pinned albums and playlists hold, oldest pin first and in
/// album or playlist order, each once, with its size from the catalog.
fn wanted(
    conn: &Connection,
    server: ServerId,
    base_url: &str,
) -> Result<Vec<(String, Option<u64>)>, OfflineError> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, size_bytes FROM (
             SELECT s.id, s.size_bytes, p.pinned_at, p.kind, p.item_id,
                    coalesce(s.disc, 0) * 100000 + coalesce(s.track, 0) AS place, s.title
             FROM offline_pin p
             JOIN song s ON s.server_id = ?1 AND p.kind = 'album' AND s.album_id = p.item_id
             WHERE p.base_url = ?2
             UNION ALL
             SELECT s.id, s.size_bytes, p.pinned_at, p.kind, p.item_id, e.position, ''
             FROM offline_pin p
             JOIN playlist_entry e ON e.server_id = ?1 AND p.kind = 'playlist'
                  AND e.playlist_id = p.item_id
             JOIN song s ON s.server_id = ?1 AND s.id = e.song_id
             WHERE p.base_url = ?2)
         ORDER BY pinned_at, kind, item_id, place, title, id",
    )?;
    let rows = stmt.query_map(params![server.get(), base_url], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1)?
                .and_then(|b| u64::try_from(b).ok()),
        ))
    })?;
    let mut seen = HashSet::new();
    let mut songs = Vec::new();
    for row in rows {
        let (id, size) = row?;
        if seen.insert(id.clone()) {
            songs.push((id, size));
        }
    }
    Ok(songs)
}

/// A stored copy.
struct Copy {
    song_id: String,
    file_name: String,
    size: u64,
}

fn copies(conn: &Connection, base_url: &str) -> Result<Vec<Copy>, OfflineError> {
    let mut stmt = conn.prepare_cached(
        "SELECT song_id, file_name, size_bytes FROM offline_track WHERE base_url = ?1",
    )?;
    let rows = stmt.query_map([base_url], |r| {
        Ok(Copy {
            song_id: r.get(0)?,
            file_name: r.get(1)?,
            size: r.get::<_, i64>(2)?.try_into().unwrap_or(0),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The file name of a copy: never anything the server sent.
fn file_name(base_url: &str, song_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(base_url.as_bytes());
    hash.update([0]);
    hash.update(song_id.as_bytes());
    format!("{}.audio", hex(&hash.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `name` has the shape of a copy's file name.
fn is_copy_name(name: &str) -> bool {
    name.len() == 70
        && name.ends_with(".audio")
        && name[..64]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Creates the folder private, refusing a link in its place.
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(std::io::Error::other(
            "the offline folder is not a plain folder",
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Deletes a copy's file. Only names of copies are accepted.
fn remove_copy(dir: &Path, name: &str) {
    if is_copy_name(name)
        && let Err(error) = fs::remove_file(dir.join(name))
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "an offline copy could not be deleted");
    }
}

/// Brings the copies of a server in line with its pins: removes what is no
/// longer wanted or broken, then downloads what is missing until the limit.
pub async fn sync(
    source: &Source,
    db: Arc<Database>,
    server: ServerId,
    dir: PathBuf,
    limit_bytes: u64,
) -> Result<SyncReport, OfflineError> {
    let base_url = source.server.key();
    let (wanted, stored) = {
        let db = Arc::clone(&db);
        let base_url = base_url.clone();
        tokio::task::spawn_blocking(move || -> Result<_, OfflineError> {
            let conn = db.reader()?;
            Ok((wanted(&conn, server, &base_url)?, copies(&conn, &base_url)?))
        })
        .await
        .map_err(|_| OfflineError::Task)??
    };
    {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || prepare_dir(&dir))
            .await
            .map_err(|_| OfflineError::Task)??;
    }
    let wanted_ids: HashSet<&str> = wanted.iter().map(|(id, _)| id.as_str()).collect();
    let mut report = SyncReport::default();
    let mut have: HashSet<String> = HashSet::new();
    let mut drop_rows = Vec::new();
    for copy in stored {
        let path = dir.join(&copy.file_name);
        let intact = fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() == copy.size);
        if wanted_ids.contains(copy.song_id.as_str()) && intact {
            report.used_bytes += copy.size;
            have.insert(copy.song_id);
        } else {
            remove_copy(&dir, &copy.file_name);
            drop_rows.push(copy.song_id);
            report.removed += 1;
        }
    }
    if !drop_rows.is_empty() {
        let db = Arc::clone(&db);
        let base_url = base_url.clone();
        tokio::task::spawn_blocking(move || {
            db.write(|tx| {
                let mut delete = tx.prepare_cached(
                    "DELETE FROM offline_track WHERE base_url = ?1 AND song_id = ?2",
                )?;
                for id in &drop_rows {
                    delete.execute(params![base_url, id])?;
                }
                Ok(())
            })
        })
        .await
        .map_err(|_| OfflineError::Task)??;
    }
    report.kept = have.len();

    for (id, size) in &wanted {
        if have.contains(id) {
            continue;
        }
        let expected = size.unwrap_or(0);
        if report.used_bytes + expected > limit_bytes {
            report.over_limit += 1;
            continue;
        }
        let Ok(song) = SongId::parse(id) else {
            continue;
        };
        match download(source, &song, *size, &dir, report.used_bytes, limit_bytes).await {
            Ok((name, written, digest)) => {
                let db = Arc::clone(&db);
                let base_url = base_url.clone();
                let id = id.clone();
                let stored_at = crate::sync::now_ms();
                tokio::task::spawn_blocking(move || {
                    db.write(|tx| {
                        tx.execute(
                            "INSERT INTO offline_track
                                 (base_url, song_id, file_name, size_bytes, sha256, stored_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                             ON CONFLICT (base_url, song_id) DO UPDATE SET
                                 file_name = excluded.file_name, size_bytes = excluded.size_bytes,
                                 sha256 = excluded.sha256, stored_at = excluded.stored_at",
                            params![
                                base_url,
                                id,
                                name,
                                i64::try_from(written).unwrap_or(i64::MAX),
                                digest,
                                stored_at
                            ],
                        )?;
                        Ok(())
                    })
                })
                .await
                .map_err(|_| OfflineError::Task)??;
                report.used_bytes += written;
                report.downloaded += 1;
            }
            Err(OfflineError::TooLarge) if report.used_bytes > 0 => {
                report.over_limit += 1;
            }
            Err(OfflineError::Unreachable) => {
                // No point in trying the rest now; the next sync goes on.
                report.failed += 1;
                break;
            }
            Err(error) => {
                tracing::warn!(%error, "a track could not be kept offline");
                report.failed += 1;
            }
        }
    }
    tracing::info!(
        downloaded = report.downloaded,
        removed = report.removed,
        over_limit = report.over_limit,
        failed = report.failed,
        "offline copies updated"
    );
    Ok(report)
}

/// Downloads the original file of `song` into `dir`: into a private temp
/// file, hashed while written, then renamed. Returns the file name, its size
/// and its SHA-256.
async fn download(
    source: &Source,
    song: &SongId,
    expected: Option<u64>,
    dir: &Path,
    used: u64,
    limit: u64,
) -> Result<(String, u64, String), OfflineError> {
    let mut url = source
        .server
        .endpoint("rest/download.view")
        .map_err(|_| OfflineError::Unreachable)?;
    url.query_pairs_mut()
        .append_pair("id", song.as_str())
        .append_pair("c", CLIENT_NAME)
        .append_pair("v", API_VERSION);
    let (name, value) = credential(&source.auth, "GET", &url).map_err(|_| OfflineError::Signing)?;
    let mut answer = source
        .http
        .get(url)
        .header(name, value)
        .send()
        .await
        .map_err(|error| {
            tracing::debug!(error = %error.without_url(), "download failed");
            OfflineError::Unreachable
        })?;
    if !answer.status().is_success() {
        return Err(OfflineError::Http(answer.status().as_u16()));
    }
    // A Subsonic error comes as JSON or XML with status 200.
    let kind = answer
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !(kind.starts_with("audio/") || kind.starts_with("application/octet-stream")) {
        return Err(OfflineError::NotAudio);
    }
    let room = limit.saturating_sub(used).min(MAX_OFFLINE_TRACK_BYTES);
    let announced = answer.content_length();
    let cap = announced.or(expected).unwrap_or(room);
    if cap > room {
        return Err(OfflineError::TooLarge);
    }

    let file_name = file_name(&source.server.key(), song.as_str());
    let temp = dir.join(format!("{file_name}.{}.part", std::process::id()));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)
        .await?;
    let mut hash = Sha256::new();
    let mut written: u64 = 0;
    let result: Result<(), OfflineError> = async {
        loop {
            let chunk = answer.chunk().await.map_err(|_| OfflineError::Incomplete)?;
            let Some(chunk) = chunk else { break };
            written += chunk.len() as u64;
            if written > room || announced.is_some_and(|a| written > a) {
                return Err(OfflineError::TooLarge);
            }
            hash.update(&chunk);
            file.write_all(&chunk).await?;
        }
        if written == 0 || announced.is_some_and(|a| written != a) {
            return Err(OfflineError::Incomplete);
        }
        file.sync_all().await?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(error) = result {
        // Ignored: the temp file is removed again by the next cleanup.
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    tokio::fs::rename(&temp, dir.join(&file_name)).await?;
    Ok((file_name, written, hex(&hash.finalize())))
}

/// Fills in `local_path` of songs with an intact copy, unless they already
/// play from a local folder.
pub fn resolve_paths(
    conn: &Connection,
    dir: &Path,
    base_url: &str,
    songs: &mut [Song],
) -> Result<(), OfflineError> {
    let mut stmt = conn.prepare_cached(
        "SELECT file_name, size_bytes FROM offline_track WHERE base_url = ?1 AND song_id = ?2",
    )?;
    for song in songs
        .iter_mut()
        .filter(|s| s.on_server && s.local_path.is_none())
    {
        let row: Option<(String, i64)> = stmt
            .query_row(params![base_url, song.id], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((name, size)) = row else { continue };
        if !is_copy_name(&name) {
            continue;
        }
        let path = dir.join(&name);
        let intact = fs::symlink_metadata(&path)
            .is_ok_and(|m| m.is_file() && i64::try_from(m.len()).ok() == Some(size));
        if intact {
            song.local_path = Some(path);
            song.kept_offline = true;
        }
    }
    Ok(())
}

/// Re-reads every copy (of one server, or all for `None`) and drops the
/// ones whose content no longer matches; the next sync downloads them again.
/// Returns (checked, broken).
pub fn verify(
    db: &Database,
    dir: &Path,
    base_url: Option<&str>,
) -> Result<(usize, usize), OfflineError> {
    let rows: Vec<(String, String)> = {
        let conn = db.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT file_name, sha256 FROM offline_track WHERE ?1 IS NULL OR base_url = ?1",
        )?;
        let rows = stmt.query_map([base_url], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut broken = Vec::new();
    for (name, digest) in &rows {
        if !is_copy_name(name) || sha256_of(&dir.join(name)).ok().as_ref() != Some(digest) {
            broken.push(name.clone());
        }
    }
    db.write(|tx| {
        let mut delete = tx.prepare_cached("DELETE FROM offline_track WHERE file_name = ?1")?;
        for name in &broken {
            delete.execute([name])?;
        }
        Ok(())
    })?;
    for name in &broken {
        remove_copy(dir, name);
    }
    if !broken.is_empty() {
        tracing::warn!(broken = broken.len(), "offline copies failed their check");
    }
    Ok((rows.len(), broken.len()))
}

fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hex(&hash.finalize()))
}

/// Deletes a server's copies and pins, for a sign-out where the user chose
/// to remove them.
pub fn remove_server(db: &Database, dir: &Path, base_url: &str) -> Result<usize, OfflineError> {
    let names = db.write(|tx| {
        let names: Vec<String> = tx
            .prepare("SELECT file_name FROM offline_track WHERE base_url = ?1")?
            .query_map([base_url], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        tx.execute("DELETE FROM offline_track WHERE base_url = ?1", [base_url])?;
        tx.execute("DELETE FROM offline_pin WHERE base_url = ?1", [base_url])?;
        Ok(names)
    })?;
    for name in &names {
        remove_copy(dir, name);
    }
    Ok(names.len())
}

/// Removes files in the offline folder that no row knows: left from a
/// stopped download or a crash. Only names Studio gives are touched.
pub fn clean_up(db: &Database, dir: &Path) -> Result<usize, OfflineError> {
    let known: HashSet<String> = {
        let conn = db.reader()?;
        let mut stmt = conn.prepare_cached("SELECT file_name FROM offline_track")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let stray = if let Some(stem) = name.strip_suffix(".part") {
            stem.rsplit_once('.').is_some_and(|(copy, pid)| {
                is_copy_name(copy) && pid.bytes().all(|b| b.is_ascii_digit())
            })
        } else {
            is_copy_name(&name) && !known.contains(&name)
        };
        if stray && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Bytes used by the copies of one server, or of all for `None`.
pub fn used_bytes(conn: &Connection, base_url: Option<&str>) -> Result<u64, OfflineError> {
    let used: i64 = conn.query_row(
        "SELECT coalesce(sum(size_bytes), 0) FROM offline_track
         WHERE ?1 IS NULL OR base_url = ?1",
        [base_url],
        |r| r.get(0),
    )?;
    Ok(u64::try_from(used).unwrap_or(0))
}

#[cfg(test)]
mod tests;
