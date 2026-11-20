//! The audio cache (#30): complete tracks on disk, so seeks and rate changes do
//! not depend on the network.
//!
//! Files live in one directory (mode `0700`, files `0600`), named by a SHA-256
//! digest of the server and the song id, so a name never comes from the
//! server. The index of complete tracks is the `audio_cache` table. Only
//! complete tracks survive a restart: partial files are deleted when the cache
//! opens. The cache holds one server's tracks; opening it for another server
//! clears the rest, because song ids belong to one server.
//!
//! The track source fills the cache (see [`crate::track_source`]). All methods
//! here block on the file system and the database: call them off the GTK main
//! thread, and from `spawn_blocking` in async code.

use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::catalog::{self, ServerId};
use crate::connection::ServerUrl;
use crate::db::{Database, DbError};
use crate::queue::SongId;
use crate::sync::now_ms;

/// Most bytes the cache keeps on disk. The least recently played tracks go first.
pub const CACHE_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Largest track that is cached. Bigger ones are streamed without a cache.
pub const MAX_TRACK_BYTES: u64 = 200 * 1024 * 1024;

/// Extension of a complete track.
const COMPLETE_EXT: &str = "audio";

/// Extension of a track being downloaded.
const PARTIAL_EXT: &str = "part";

/// How often a played track's time is written, at most: seeks play the same
/// track many times in a row.
const TOUCH_INTERVAL_MS: i64 = 60_000;

/// Errors from the cache. Carry no URL and no secret.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("audio cache file error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("the audio cache directory is a symbolic link")]
    Symlink,
    #[error("the downloaded track has {found} bytes, but {expected} were expected")]
    Incomplete { found: u64, expected: u64 },
    #[error("the downloaded track has {found} bytes, the catalogue says {expected}")]
    SizeMismatch { found: u64, expected: u64 },
}

impl From<rusqlite::Error> for CacheError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::from(error))
    }
}

/// A complete track on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedTrack {
    pub path: PathBuf,
    pub size: u64,
    pub content_type: String,
}

#[derive(Clone, Debug)]
struct Entry {
    file_name: String,
    size: u64,
    content_type: String,
    played_at: i64,
}

/// The audio cache of one server.
#[derive(Debug)]
pub struct AudioCache {
    dir: PathBuf,
    db: Arc<Database>,
    server: ServerId,
    server_key: String,
    limit: u64,
    /// Complete tracks by song id; the table is the persistent copy.
    index: Mutex<HashMap<String, Entry>>,
}

impl AudioCache {
    /// Opens the cache in `dir` for `server`, creating the directory with mode
    /// `0700`. Deletes partial files, other servers' tracks, files without an
    /// index row and rows without a file.
    pub fn open(
        dir: impl Into<PathBuf>,
        db: Arc<Database>,
        server: &ServerUrl,
    ) -> Result<Self, CacheError> {
        let dir = dir.into();
        prepare_dir(&dir)?;
        let server_key = server.key();
        let server_id = catalog::ensure_server(&db, &server_key, now_ms())?;

        let rows = db.write(|tx| {
            tx.execute(
                "DELETE FROM audio_cache WHERE server_id <> ?1",
                [server_id.get()],
            )?;
            let mut select = tx.prepare(
                "SELECT song_id, file_name, size_bytes, content_type, played_at
                 FROM audio_cache WHERE server_id = ?1",
            )?;
            let rows = select
                .query_map([server_id.get()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Entry {
                            file_name: row.get(1)?,
                            size: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                            content_type: row.get(3)?,
                            played_at: row.get(4)?,
                        },
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;

        // Keep only rows whose file is there with the recorded size.
        let mut index = HashMap::new();
        let mut stale = Vec::new();
        for (song, entry) in rows {
            let intact = fs::symlink_metadata(dir.join(&entry.file_name))
                .is_ok_and(|meta| meta.is_file() && meta.len() == entry.size);
            if intact {
                index.insert(song, entry);
            } else {
                stale.push(song);
            }
        }
        if !stale.is_empty() {
            db.write(|tx| {
                let mut delete = tx.prepare_cached(
                    "DELETE FROM audio_cache WHERE server_id = ?1 AND song_id = ?2",
                )?;
                for song in &stale {
                    delete.execute(rusqlite::params![server_id.get(), song])?;
                }
                Ok(())
            })?;
        }

        // Delete what the index does not know: partial files and leftovers.
        let known: Vec<&str> = index.values().map(|e| e.file_name.as_str()).collect();
        for item in fs::read_dir(&dir)? {
            let item = item?;
            let name = item.file_name();
            let Some(name) = name.to_str() else { continue };
            if is_cache_file_name(name) && !known.contains(&name) {
                remove_if_present(&item.path())?;
            }
        }

        Ok(Self {
            dir,
            db,
            server: server_id,
            server_key,
            limit: CACHE_LIMIT_BYTES,
            index: Mutex::new(index),
        })
    }

    /// The server this cache belongs to, as [`ServerUrl::key`] gives it.
    pub fn server_key(&self) -> &str {
        &self.server_key
    }

    /// The same cache with another size limit.
    pub fn with_limit(mut self, bytes: u64) -> Self {
        self.limit = bytes;
        self
    }

    /// The complete track for `song`, if it is on disk.
    pub fn complete(&self, song: &SongId) -> Option<CachedTrack> {
        let index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
        index.get(song.as_str()).map(|entry| CachedTrack {
            path: self.dir.join(&entry.file_name),
            size: entry.size,
            content_type: entry.content_type.clone(),
        })
    }

    /// Records that `song` was played, for the eviction order.
    pub fn touch(&self, song: &SongId) -> Result<(), CacheError> {
        let now = now_ms();
        {
            let mut index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = index.get_mut(song.as_str()) else {
                return Ok(());
            };
            if now - entry.played_at < TOUCH_INTERVAL_MS {
                return Ok(());
            }
            entry.played_at = now;
        }
        self.db.write(|tx| {
            tx.execute(
                "UPDATE audio_cache SET played_at = ?3 WHERE server_id = ?1 AND song_id = ?2",
                rusqlite::params![self.server.get(), song.as_str(), now],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// The file a download of `song` writes to.
    pub(crate) fn partial_path(&self, song: &SongId) -> PathBuf {
        self.dir
            .join(format!("{}.{PARTIAL_EXT}", self.digest(song)))
    }

    /// Creates (or empties) the partial file of `song`, mode `0600`. A symbolic
    /// link in its place is refused.
    pub(crate) fn create_partial(&self, song: &SongId) -> Result<File, CacheError> {
        let path = self.partial_path(song);
        remove_if_present(&path)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(file)
    }

    /// Makes the finished download of `song` a complete track: checks that it
    /// has `expected` bytes and that the catalogue agrees, renames it into
    /// place, records it and evicts old tracks above the limit. On an error
    /// the partial file is left for the caller to delete.
    pub(crate) fn commit(
        &self,
        song: &SongId,
        content_type: &str,
        expected: u64,
    ) -> Result<CachedTrack, CacheError> {
        let partial = self.partial_path(song);
        let found = fs::symlink_metadata(&partial)?.len();
        if found != expected {
            return Err(CacheError::Incomplete { found, expected });
        }
        let catalogue: Option<Option<i64>> = self
            .db
            .reader()?
            .query_row(
                "SELECT size_bytes FROM song WHERE server_id = ?1 AND id = ?2",
                rusqlite::params![self.server.get(), song.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(listed) = catalogue.flatten().and_then(|s| u64::try_from(s).ok())
            && listed != found
        {
            return Err(CacheError::SizeMismatch {
                found,
                expected: listed,
            });
        }

        File::open(&partial)?.sync_all()?;
        let file_name = format!("{}.{COMPLETE_EXT}", self.digest(song));
        let path = self.dir.join(&file_name);
        fs::rename(&partial, &path)?;

        let now = now_ms();
        let size = i64::try_from(found).unwrap_or(i64::MAX);
        self.db.write(|tx| {
            tx.execute(
                "INSERT INTO audio_cache
                     (server_id, song_id, file_name, size_bytes, content_type, cached_at, played_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT (server_id, song_id) DO UPDATE SET
                     file_name = excluded.file_name, size_bytes = excluded.size_bytes,
                     content_type = excluded.content_type, cached_at = excluded.cached_at,
                     played_at = excluded.played_at",
                rusqlite::params![
                    self.server.get(),
                    song.as_str(),
                    file_name,
                    size,
                    content_type,
                    now
                ],
            )?;
            Ok(())
        })?;
        self.index
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                song.as_str().to_owned(),
                Entry {
                    file_name,
                    size: found,
                    content_type: content_type.to_owned(),
                    played_at: now,
                },
            );
        self.evict(song)?;
        Ok(CachedTrack {
            path,
            size: found,
            content_type: content_type.to_owned(),
        })
    }

    /// Bytes of all complete tracks.
    pub fn used_bytes(&self) -> u64 {
        self.index
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|e| e.size)
            .sum()
    }

    /// Deletes every track of this server.
    pub fn clear(&self) -> Result<(), CacheError> {
        let entries: Vec<Entry> = self
            .index
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
            .map(|(_, e)| e)
            .collect();
        for entry in &entries {
            remove_if_present(&self.dir.join(&entry.file_name))?;
        }
        self.db.write(|tx| {
            tx.execute(
                "DELETE FROM audio_cache WHERE server_id = ?1",
                [self.server.get()],
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Deletes the least recently played tracks until the cache is within its
    /// limit. `keep` (the track just added) is never deleted.
    fn evict(&self, keep: &SongId) -> Result<(), CacheError> {
        loop {
            let victim = {
                let index = self.index.lock().unwrap_or_else(PoisonError::into_inner);
                let used: u64 = index.values().map(|e| e.size).sum();
                if used <= self.limit {
                    return Ok(());
                }
                index
                    .iter()
                    .filter(|(song, _)| song.as_str() != keep.as_str())
                    .min_by_key(|(_, entry)| entry.played_at)
                    .map(|(song, entry)| (song.clone(), entry.file_name.clone()))
            };
            let Some((song, file_name)) = victim else {
                return Ok(());
            };
            remove_if_present(&self.dir.join(&file_name))?;
            self.db.write(|tx| {
                tx.execute(
                    "DELETE FROM audio_cache WHERE server_id = ?1 AND song_id = ?2",
                    rusqlite::params![self.server.get(), song],
                )?;
                Ok(())
            })?;
            self.index
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&song);
        }
    }

    /// Hex SHA-256 of the server and the song id.
    fn digest(&self, song: &SongId) -> String {
        let mut hash = Sha256::new();
        hash.update(self.server_key.as_bytes());
        hash.update(b"\n");
        hash.update(song.as_str().as_bytes());
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// Whether `name` is one of the cache's own files: a 64-digit hex digest with
/// the complete or partial extension. Other files in the directory are left
/// alone.
fn is_cache_file_name(name: &str) -> bool {
    let Some((stem, ext)) = name.split_once('.') else {
        return false;
    };
    stem.len() == 64
        && stem
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && (ext == COMPLETE_EXT || ext == PARTIAL_EXT)
}

fn remove_if_present(path: &Path) -> Result<(), CacheError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Creates the directory with mode `0700`, narrows a wider one, and refuses a
/// symbolic link.
fn prepare_dir(dir: &Path) -> Result<(), CacheError> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let meta = fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(CacheError::Symlink);
    }
    if meta.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::HttpPolicy;
    use std::io::Write;

    fn server(url: &str) -> ServerUrl {
        ServerUrl::parse(url, HttpPolicy::HTTPS_ONLY).unwrap()
    }

    fn song(id: &str) -> SongId {
        SongId::parse(id).unwrap()
    }

    fn setup() -> (tempfile::TempDir, Arc<Database>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(dir.path().join("library.db")).unwrap());
        (dir, db)
    }

    /// Writes `bytes` as the finished download of `id` and commits it.
    fn add(cache: &AudioCache, id: &str, bytes: &[u8]) -> CachedTrack {
        let mut file = cache.create_partial(&song(id)).unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        cache
            .commit(&song(id), "audio/ogg", bytes.len() as u64)
            .unwrap()
    }

    #[test]
    fn a_complete_track_survives_a_restart() {
        let (dir, db) = setup();
        let audio = dir.path().join("audio");
        let cache =
            AudioCache::open(&audio, Arc::clone(&db), &server("https://a.example")).unwrap();
        let track = add(&cache, "so-1", b"opus data");
        drop(cache);

        let cache = AudioCache::open(&audio, db, &server("https://a.example")).unwrap();
        let again = cache.complete(&song("so-1")).unwrap();
        assert_eq!(again, track);
        assert_eq!(fs::read(&again.path).unwrap(), b"opus data");
    }

    #[test]
    fn partial_files_are_deleted_when_the_cache_opens() {
        let (dir, db) = setup();
        let audio = dir.path().join("audio");
        let cache =
            AudioCache::open(&audio, Arc::clone(&db), &server("https://a.example")).unwrap();
        let mut partial = cache.create_partial(&song("so-1")).unwrap();
        partial.write_all(b"half").unwrap();
        let path = cache.partial_path(&song("so-1"));
        drop((partial, cache));
        assert!(path.exists());

        let other = audio.join("notes.txt");
        fs::write(&other, b"not ours").unwrap();
        let cache = AudioCache::open(&audio, db, &server("https://a.example")).unwrap();
        assert!(!path.exists(), "the partial file is gone");
        assert!(cache.complete(&song("so-1")).is_none());
        assert!(
            other.exists(),
            "files that are not the cache's are left alone"
        );
    }

    #[test]
    fn a_track_of_the_wrong_length_is_not_committed() {
        let (dir, db) = setup();
        let cache =
            AudioCache::open(dir.path().join("audio"), db, &server("https://a.example")).unwrap();
        let mut file = cache.create_partial(&song("so-1")).unwrap();
        file.write_all(b"short").unwrap();
        assert!(matches!(
            cache.commit(&song("so-1"), "audio/ogg", 100),
            Err(CacheError::Incomplete {
                found: 5,
                expected: 100
            })
        ));
        assert!(cache.complete(&song("so-1")).is_none());
    }

    #[test]
    fn a_size_that_disagrees_with_the_catalogue_is_refused() {
        let (dir, db) = setup();
        let url = server("https://a.example");
        let id = catalog::ensure_server(&db, &url.key(), 1).unwrap();
        db.write(|tx| {
            tx.execute(
                "INSERT INTO song (server_id, id, title, size_bytes, synced_at)
                 VALUES (?1, 'so-1', 'Tango', 999, 1)",
                [id.get()],
            )?;
            Ok(())
        })
        .unwrap();
        let cache = AudioCache::open(dir.path().join("audio"), db, &url).unwrap();
        let mut file = cache.create_partial(&song("so-1")).unwrap();
        file.write_all(b"twelve bytes").unwrap();
        assert!(matches!(
            cache.commit(&song("so-1"), "audio/ogg", 12),
            Err(CacheError::SizeMismatch {
                found: 12,
                expected: 999
            })
        ));
        assert!(cache.complete(&song("so-1")).is_none());
    }

    #[test]
    fn the_least_recently_played_track_is_evicted_first() {
        let (dir, db) = setup();
        let cache = AudioCache::open(dir.path().join("audio"), db, &server("https://a.example"))
            .unwrap()
            .with_limit(20);
        let first = add(&cache, "so-1", &[1; 8]);
        add(&cache, "so-2", &[2; 8]);
        // so-2 was played longest ago: make it older than so-1.
        cache
            .index
            .lock()
            .unwrap()
            .get_mut("so-2")
            .unwrap()
            .played_at = 0;
        add(&cache, "so-3", &[3; 8]);

        assert!(cache.complete(&song("so-1")).is_some());
        assert!(
            cache.complete(&song("so-2")).is_none(),
            "the oldest is evicted"
        );
        assert!(
            cache.complete(&song("so-3")).is_some(),
            "the new track stays"
        );
        assert!(cache.used_bytes() <= 20);
        assert!(first.path.exists());
    }

    #[test]
    fn opening_the_cache_for_another_server_clears_the_old_one() {
        let (dir, db) = setup();
        let audio = dir.path().join("audio");
        let cache =
            AudioCache::open(&audio, Arc::clone(&db), &server("https://a.example")).unwrap();
        let track = add(&cache, "so-1", b"from a");
        drop(cache);

        let cache =
            AudioCache::open(&audio, Arc::clone(&db), &server("https://b.example")).unwrap();
        assert!(cache.complete(&song("so-1")).is_none());
        assert!(!track.path.exists(), "the old server's file is deleted");
        let rows: i64 = db
            .reader()
            .unwrap()
            .query_row("SELECT count(*) FROM audio_cache", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn files_are_named_by_digest_and_private() {
        let (dir, db) = setup();
        let audio = dir.path().join("audio");
        let cache = AudioCache::open(&audio, db, &server("https://a.example")).unwrap();
        let track = add(&cache, "../../etc/passwd", b"x");
        assert_eq!(track.path.parent().unwrap(), audio);
        let name = track.path.file_name().unwrap().to_str().unwrap();
        assert!(is_cache_file_name(name), "{name}");
        assert!(name.ends_with(".audio"));
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&track.path), 0o600);
        assert_eq!(mode(&audio), 0o700);
    }

    #[test]
    fn a_symlinked_cache_directory_is_refused() {
        let (dir, db) = setup();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("audio");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            AudioCache::open(&link, db, &server("https://a.example")),
            Err(CacheError::Symlink)
        ));
    }

    #[test]
    fn clearing_deletes_every_track() {
        let (dir, db) = setup();
        let cache =
            AudioCache::open(dir.path().join("audio"), db, &server("https://a.example")).unwrap();
        let track = add(&cache, "so-1", b"data");
        cache.clear().unwrap();
        assert!(cache.complete(&song("so-1")).is_none());
        assert!(!track.path.exists());
        assert_eq!(cache.used_bytes(), 0);
    }
}
