//! The tag editor for local files (#15): the only part of Studio that changes
//! music files.
//!
//! An edit is planned first ([`preview`]): each file's tags are read from disk
//! and compared with the new values, so the user sees every change before
//! anything is written. [`write`] then changes only the files of the preview,
//! and only those that did not change on disk since. Before it writes, it
//! keeps the old values of the edited fields, so [`undo`] can write them back
//! to the files of the last write that are still as Studio left them.
//!
//! A file is never changed in place: Studio copies it next to itself under a
//! hidden name, writes the tags into the copy, reads the copy back to check
//! that it holds the new values, and only then renames it over the original.
//! A failure at any point leaves the original untouched.
//!
//! Tags are edited through each format's own tag type, split into the fields
//! Studio knows and the rest, and merged back, so frames and comments Studio
//! does not know (pictures, lyrics, private frames) are kept as they are.
//!
//! Only files in the user's local folders are edited. folksonic keeps tempo in
//! its database, so the BPM of a track the server also has is not changed
//! here; it goes through analysis.
//!
//! All calls here block; run them off the GTK main thread.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek};
use std::path::{Path, PathBuf};

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::id3::v2::Id3v2Version;
use lofty::probe::Probe;
use lofty::tag::items::Timestamp;
use lofty::tag::{Accessor, ItemKey, MergeTag, SplitTag, Tag};
use rusqlite::{Connection, OptionalExtension, params};

use crate::dancebrainz::Mbid;
use crate::db::{Database, DbError};
use crate::local_library::{self, LocalError, LocalTags};

/// Most files one edit may change.
pub const MAX_FILES: usize = 5_000;

/// Longest text value, in characters, the same bound the scan reads.
const MAX_TEXT: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Local(#[from] LocalError),
    #[error("{field}: {reason}")]
    Value { field: &'static str, reason: String },
    #[error("an edit may change at most {MAX_FILES} files")]
    TooMany,
}

impl From<rusqlite::Error> for TagError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::Sqlite(error))
    }
}

/// Why one file was not changed. Carries no path, only the reason.
#[derive(Debug, thiserror::Error)]
enum FileError {
    #[error("Studio cannot write tags in this file format")]
    Format,
    #[error("the file could not be read: {0}")]
    Read(String),
    #[error("the tags could not be written: {0}")]
    Write(String),
    #[error("this file format cannot hold {0}")]
    NotHeld(&'static str),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// A tag field the editor changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Track,
    Disc,
    Year,
    RecordingMbid,
    ReleaseMbid,
    ArtistMbid,
    Bpm,
}

impl Field {
    /// Every field, in the order the editor shows them.
    pub const ALL: [Self; 11] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::AlbumArtist,
        Self::Track,
        Self::Disc,
        Self::Year,
        Self::Bpm,
        Self::RecordingMbid,
        Self::ReleaseMbid,
        Self::ArtistMbid,
    ];

    /// The field's name in the editor.
    pub fn label(self) -> &'static str {
        match self {
            Self::Title => "Title",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::AlbumArtist => "Album artist",
            Self::Track => "Track number",
            Self::Disc => "Disc number",
            Self::Year => "Year",
            Self::RecordingMbid => "Recording MBID",
            Self::ReleaseMbid => "Release MBID",
            Self::ArtistMbid => "Artist MBID",
            Self::Bpm => "BPM",
        }
    }

    /// The field's name in the undo record.
    fn key(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Artist => "artist",
            Self::Album => "album",
            Self::AlbumArtist => "album_artist",
            Self::Track => "track",
            Self::Disc => "disc",
            Self::Year => "year",
            Self::RecordingMbid => "mbid",
            Self::ReleaseMbid => "release_mbid",
            Self::ArtistMbid => "artist_mbid",
            Self::Bpm => "bpm",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.key() == key)
    }

    /// A field that belongs to one recording, so setting it for many files
    /// at once makes no sense: the editor offers it for one file only.
    pub fn per_track(self) -> bool {
        matches!(
            self,
            Self::Title | Self::Track | Self::RecordingMbid | Self::Bpm
        )
    }

    /// Parses what the user typed. Empty text clears the field (`None`);
    /// anything else must be a valid value, returned in the form the scan
    /// would read it back.
    pub fn parse(self, text: &str) -> Result<Option<String>, TagError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let bad = |reason: &str| TagError::Value {
            field: self.label(),
            reason: reason.to_owned(),
        };
        let value = match self {
            Self::Title | Self::Artist | Self::Album | Self::AlbumArtist => {
                if text.chars().count() > MAX_TEXT {
                    return Err(bad("the text is longer than 1024 characters"));
                }
                if text.chars().any(char::is_control) {
                    return Err(bad("the text holds a line break or a control character"));
                }
                text.to_owned()
            }
            Self::Track | Self::Disc => text
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=9999).contains(n))
                .ok_or_else(|| bad("a number from 1 to 9999 is needed"))?
                .to_string(),
            Self::Year => text
                .parse::<i32>()
                .ok()
                .filter(|y| (1..=9999).contains(y))
                .ok_or_else(|| bad("a year from 1 to 9999 is needed"))?
                .to_string(),
            Self::RecordingMbid | Self::ReleaseMbid | Self::ArtistMbid => Mbid::parse(text)
                .map_err(|_| bad("a MusicBrainz ID looks like 0f9d8d5e-1c4b-4a8e-9b0c-…"))?
                .to_string(),
            // Written as a whole number: ID3v2 holds no other (`TBPM`).
            Self::Bpm => bpm_text(
                text.parse::<f64>()
                    .ok()
                    .map(f64::round)
                    .filter(|b| (20.0..=300.0).contains(b))
                    .ok_or_else(|| bad("a tempo from 20 to 300 BPM is needed"))?,
            ),
        };
        Ok(Some(value))
    }

    /// The field's value in tags read from a file, in the same form as
    /// [`Field::parse`] gives.
    pub fn of(self, tags: &LocalTags) -> Option<String> {
        match self {
            Self::Title => tags.title.clone(),
            Self::Artist => tags.artist.clone(),
            Self::Album => tags.album.clone(),
            Self::AlbumArtist => tags.album_artist.clone(),
            Self::Track => tags.track.map(|n| n.to_string()),
            Self::Disc => tags.disc.map(|n| n.to_string()),
            Self::Year => tags.year.map(|y| y.to_string()),
            Self::RecordingMbid => tags.mbid.clone(),
            Self::ReleaseMbid => tags.release_mbid.clone(),
            Self::ArtistMbid => tags.artist_mbid.clone(),
            Self::Bpm => tags.bpm.map(bpm_text),
        }
    }
}

/// A BPM as text: whole numbers without decimals, others with up to two.
fn bpm_text(bpm: f64) -> String {
    let text = format!("{bpm:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The new values for one file: a field set to a value, or cleared (`None`).
/// Built only from parsed values, so it always holds valid ones.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edit(BTreeMap<Field, Option<String>>);

impl Edit {
    /// No changes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `field` from what the user typed; empty text clears it.
    pub fn set(&mut self, field: Field, text: &str) -> Result<(), TagError> {
        self.0.insert(field, field.parse(text)?);
        Ok(())
    }

    /// Whether the edit changes nothing.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The fields and their new values.
    pub fn fields(&self) -> impl Iterator<Item = (Field, Option<&str>)> {
        self.0.iter().map(|(f, v)| (*f, v.as_deref()))
    }
}

/// One field of one file, before and after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldChange {
    pub field: Field,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// One file the write would change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub track_id: i64,
    /// The path inside its folder, for the user.
    pub name: String,
    pub changes: Vec<FieldChange>,
    /// The server has this recording. When it scans this folder, it sees the
    /// change there too.
    pub on_server: bool,
    path: PathBuf,
    size: u64,
    modified_ms: i64,
}

/// A file left out, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: String,
}

/// What a write would do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    pub files: Vec<FileChange>,
    /// Files that already hold the new values.
    pub unchanged: usize,
    pub skipped: Vec<Skipped>,
}

/// What a write or an undo did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteReport {
    pub written: usize,
    pub failed: Vec<Skipped>,
}

/// A local track's file and what Studio knows about it.
struct TrackFile {
    name: String,
    path: Option<PathBuf>,
    on_server: bool,
}

fn track_file(conn: &Connection, track_id: i64) -> Result<Option<TrackFile>, TagError> {
    Ok(conn
        .prepare_cached(
            "SELECT f.path, t.rel_path,
                 t.mbid IS NOT NULL AND EXISTS (SELECT 1 FROM song s WHERE s.mbid = t.mbid)
             FROM local_track t JOIN local_folder f ON f.id = t.folder_id WHERE t.id = ?1",
        )?
        .query_row([track_id], |r| {
            let root: String = r.get(0)?;
            let rel: String = r.get(1)?;
            Ok(TrackFile {
                path: local_library::resolve(Path::new(&root), &rel),
                name: rel,
                on_server: r.get(2)?,
            })
        })
        .optional()?)
}

/// Whether a recording MBID is a song on a server.
fn is_server_recording(conn: &Connection, mbid: &str) -> Result<bool, TagError> {
    Ok(conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM song WHERE mbid = ?1)")?
        .query_row([mbid], |r| r.get(0))?)
}

/// The tags of a local track's file as they are on disk now, for the editor
/// to start from. `None` when the track or its file is gone.
pub fn current_tags(db: &Database, track_id: i64) -> Result<Option<LocalTags>, TagError> {
    let conn = db.reader()?;
    let Some(TrackFile {
        path: Some(path), ..
    }) = track_file(&conn, track_id)?
    else {
        return Ok(None);
    };
    Ok(Some(local_library::read_tags(&path)?))
}

/// Plans an edit: reads each file's tags from disk and lists what would
/// change. Nothing is written.
pub fn preview(db: &Database, edits: &[(i64, Edit)]) -> Result<Preview, TagError> {
    if edits.len() > MAX_FILES {
        return Err(TagError::TooMany);
    }
    let conn = db.reader()?;
    let mut preview = Preview::default();
    for (track_id, edit) in edits {
        if edit.is_empty() {
            continue;
        }
        let Some(file) = track_file(&conn, *track_id)? else {
            continue;
        };
        let skip = |reason: &str| Skipped {
            name: file.name.clone(),
            reason: reason.to_owned(),
        };
        let Some(path) = file.path.clone() else {
            preview
                .skipped
                .push(skip("the file is missing or not inside its folder"));
            continue;
        };
        let (size, modified_ms) = match local_library::size_and_time(&path) {
            Ok(state) => state,
            Err(error) => {
                preview.skipped.push(skip(&error.to_string()));
                continue;
            }
        };
        let tags = match local_library::read_tags(&path) {
            Ok(tags) => tags,
            Err(error) => {
                preview.skipped.push(skip(&error.to_string()));
                continue;
            }
        };
        let changes: Vec<FieldChange> = edit
            .fields()
            .map(|(field, after)| FieldChange {
                field,
                before: field.of(&tags),
                after: after.map(str::to_owned),
            })
            .filter(|c| c.before != c.after)
            .collect();
        if changes.is_empty() {
            preview.unchanged += 1;
            continue;
        }
        let new_mbid = changes
            .iter()
            .find(|c| c.field == Field::RecordingMbid)
            .and_then(|c| c.after.as_deref());
        let on_server = file.on_server
            || match new_mbid {
                Some(mbid) => is_server_recording(&conn, mbid)?,
                None => false,
            };
        if on_server && changes.iter().any(|c| c.field == Field::Bpm) {
            preview.skipped.push(skip(
                "the server has this recording, and its tempo comes from folksonic; \
                 change it through analysis",
            ));
            continue;
        }
        preview.files.push(FileChange {
            track_id: *track_id,
            name: file.name,
            changes,
            on_server,
            path,
            size,
            modified_ms,
        });
    }
    Ok(preview)
}

/// Writes a preview: each file that did not change since the preview gets
/// its new values, and the old ones are kept for [`undo`]. Files that fail
/// are listed and left untouched; the others are still written.
pub fn write(db: &Database, preview: &Preview, now_ms: i64) -> Result<WriteReport, TagError> {
    // A new write replaces the undo of the last one.
    db.write(|tx| {
        tx.execute("DELETE FROM tag_undo", [])?;
        Ok(())
    })?;
    let mut report = WriteReport::default();
    for file in &preview.files {
        let fail = |reason: String| Skipped {
            name: file.name.clone(),
            reason,
        };
        let before = (file.size, file.modified_ms);
        match local_library::size_and_time(&file.path) {
            Ok(now) if now == before => {}
            Ok(_) => {
                report
                    .failed
                    .push(fail("the file changed since the preview".to_owned()));
                continue;
            }
            Err(error) => {
                report.failed.push(fail(error.to_string()));
                continue;
            }
        }
        let values: Vec<(Field, Option<String>)> = file
            .changes
            .iter()
            .map(|c| (c.field, c.after.clone()))
            .collect();
        let state = match write_file(&file.path, &values) {
            Ok(state) => state,
            Err(error) => {
                tracing::debug!(%error, "tags not written");
                report.failed.push(fail(error.to_string()));
                continue;
            }
        };
        let old: serde_json::Map<String, serde_json::Value> = file
            .changes
            .iter()
            .map(|c| {
                (
                    c.field.key().to_owned(),
                    c.before.clone().map_or(serde_json::Value::Null, Into::into),
                )
            })
            .collect();
        let old = serde_json::Value::Object(old).to_string();
        db.write(|tx| {
            local_library::record_written(tx, file.track_id, &state, before, now_ms)?;
            tx.prepare_cached(
                "INSERT OR REPLACE INTO tag_undo (track_id, size_bytes, modified_ms, before,
                     written_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                file.track_id,
                i64::try_from(state.size).unwrap_or(i64::MAX),
                state.modified_ms,
                old,
                now_ms
            ])?;
            Ok(())
        })?;
        report.written += 1;
    }
    tracing::info!(
        written = report.written,
        failed = report.failed.len(),
        "tags written"
    );
    Ok(report)
}

/// How many files the last write changed that [`undo`] can restore.
pub fn undo_count(conn: &Connection) -> Result<usize, TagError> {
    let n: i64 = conn.query_row("SELECT count(*) FROM tag_undo", [], |r| r.get(0))?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// Writes back the old values of the last write, to each of its files that
/// is still exactly as Studio left it. A file changed since is left alone
/// and listed. Afterwards there is nothing left to undo.
pub fn undo(db: &Database, now_ms: i64) -> Result<WriteReport, TagError> {
    let rows: Vec<(i64, u64, i64, String)> = {
        let conn = db.reader()?;
        let mut stmt = conn.prepare_cached(
            "SELECT track_id, size_bytes, modified_ms, before FROM tag_undo ORDER BY track_id",
        )?;
        stmt.query_map([], |r| {
            Ok((
                r.get(0)?,
                u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                r.get(2)?,
                r.get(3)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };
    let conn = db.reader()?;
    let mut report = WriteReport::default();
    for (track_id, size, modified_ms, before) in rows {
        let Some(file) = track_file(&conn, track_id)? else {
            continue;
        };
        let fail = |reason: &str| Skipped {
            name: file.name.clone(),
            reason: reason.to_owned(),
        };
        let Some(path) = file.path.clone() else {
            report
                .failed
                .push(fail("the file is missing or not inside its folder"));
            continue;
        };
        if local_library::size_and_time(&path).ok() != Some((size, modified_ms)) {
            report
                .failed
                .push(fail("the file changed since Studio wrote it"));
            continue;
        }
        let Some(values) = old_values(&before) else {
            report.failed.push(fail("the undo record does not read"));
            continue;
        };
        let state = match write_file(&path, &values) {
            Ok(state) => state,
            Err(error) => {
                report.failed.push(fail(&error.to_string()));
                continue;
            }
        };
        db.write(|tx| {
            local_library::record_written(tx, track_id, &state, (size, modified_ms), now_ms)
        })?;
        report.written += 1;
    }
    db.write(|tx| {
        tx.execute("DELETE FROM tag_undo", [])?;
        Ok(())
    })?;
    tracing::info!(
        restored = report.written,
        failed = report.failed.len(),
        "tag write undone"
    );
    Ok(report)
}

/// Reads an undo record back. It was written by Studio, but every value
/// goes through [`Field::parse`] again before it reaches a file.
fn old_values(text: &str) -> Option<Vec<(Field, Option<String>)>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value
        .as_object()?
        .iter()
        .map(|(key, value)| {
            let field = Field::from_key(key)?;
            let text = match value {
                serde_json::Value::Null => None,
                serde_json::Value::String(text) => Some(text.as_str()),
                _ => return None,
            };
            let parsed = match text {
                Some(text) => Some(field.parse(text).ok()??),
                None => None,
            };
            Some((field, parsed))
        })
        .collect()
}

// ---- Writing one file ----

/// Writes `values` into the file at `path` through a copy that replaces it,
/// and returns the file as it is afterwards.
fn write_file(
    path: &Path,
    values: &[(Field, Option<String>)],
) -> Result<local_library::FileState, FileError> {
    let file_type = Probe::open(path)
        .map_err(|e| FileError::Read(e.to_string()))?
        .guess_file_type()?
        .file_type()
        .ok_or(FileError::Format)?;
    let dir = path.parent().ok_or(FileError::Format)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(FileError::Format)?;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).map_err(|e| FileError::Write(e.to_string()))?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    // Hidden, so a scan of the folder never lists it, and with the same
    // extension, so it is read as the same format.
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or(FileError::Format)?;
    let temp = dir.join(format!(".{name}.{suffix}.studio-tags.{extension}"));

    let result = write_copy(path, &temp, file_type, values);
    match result {
        Ok(()) => {}
        Err(error) => {
            // Nothing was renamed: the original is as it was.
            if let Err(remove) = fs::remove_file(&temp)
                && remove.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(error = %remove, "a temporary tag file could not be removed");
            }
            return Err(error);
        }
    }
    if let Err(error) = fs::rename(&temp, path) {
        if let Err(remove) = fs::remove_file(&temp) {
            tracing::warn!(error = %remove, "a temporary tag file could not be removed");
        }
        return Err(error.into());
    }
    if let Ok(dir) = File::open(dir) {
        // The rename is done; syncing the folder only makes it durable sooner.
        if let Err(error) = dir.sync_all() {
            tracing::debug!(%error, "folder not synced");
        }
    }
    local_library::file_state(path).map_err(|e| FileError::Read(e.to_string()))
}

/// Copies `path` to `temp`, writes the tags into the copy and checks them.
fn write_copy(
    path: &Path,
    temp: &Path,
    file_type: FileType,
    values: &[(Field, Option<String>)],
) -> Result<(), FileError> {
    {
        let mut source = File::open(path)?;
        let mut copy = OpenOptions::new().write(true).create_new(true).open(temp)?;
        io::copy(&mut source, &mut copy)?;
        copy.set_permissions(source.metadata()?.permissions())?;
        copy.sync_all()?;
    }
    // Another tag of the file, for a file without its main tag yet: the
    // new main tag starts from it, so the values the scan read are kept.
    let seed = {
        let tagged = Probe::open(temp)
            .map_err(|e| FileError::Read(e.to_string()))?
            .set_file_type(file_type)
            .read()
            .map_err(|e| FileError::Read(e.to_string()))?;
        match tagged.primary_tag() {
            Some(_) => None,
            None => tagged.first_tag().cloned().map(|mut tag| {
                tag.re_map(file_type.primary_tag_type());
                tag
            }),
        }
    };
    let mut file = OpenOptions::new().read(true).write(true).open(temp)?;
    edit_native(&mut file, file_type, seed, values)?;
    file.sync_all()?;
    drop(file);

    // Read the copy back as the scan will: every value must be there.
    let tags = local_library::read_tags(temp).map_err(|e| FileError::Read(e.to_string()))?;
    for (field, value) in values {
        if field.of(&tags) != *value {
            return Err(FileError::NotHeld(field.label()));
        }
    }
    Ok(())
}

/// Edits the main tag of `file` in its own tag type and saves it.
fn edit_native(
    file: &mut File,
    file_type: FileType,
    seed: Option<Tag>,
    values: &[(Field, Option<String>)],
) -> Result<(), FileError> {
    let read = |e: lofty::error::FileParseError| FileError::Read(e.to_string());
    let save = |e: lofty::error::FileEncodingError| FileError::Write(e.to_string());
    let options = ParseOptions::new();
    // Formats whose main tag may be missing: take it out (or start one from
    // `seed`), edit it, put it back.
    macro_rules! optional_tag {
        ($file_ty:ty, $get:ident, $set:ident, $tag_ty:ty) => {{
            let mut audio = <$file_ty>::read_from(file, options).map_err(read)?;
            let write_options = write_options_for(audio.$get().map(|t| &*t));
            let tag: $tag_ty = match audio.$get().map(std::mem::take) {
                Some(tag) => tag,
                None => seed.map(<$tag_ty>::from).unwrap_or_default(),
            };
            audio.$set(edit_tag(tag, values));
            file.rewind()?;
            audio.save_to(file, write_options).map_err(save)
        }};
    }
    // Ogg formats always have their comment header.
    macro_rules! ogg_tag {
        ($file_ty:ty) => {{
            let mut audio = <$file_ty>::read_from(file, options).map_err(read)?;
            let tag = std::mem::take(audio.vorbis_comments_mut());
            audio.set_vorbis_comments(edit_tag(tag, values));
            file.rewind()?;
            audio.save_to(file, WriteOptions::default()).map_err(save)
        }};
    }
    use lofty::ape::{ApeFile, ApeTag};
    use lofty::flac::FlacFile;
    use lofty::id3::v2::Id3v2Tag;
    use lofty::iff::aiff::AiffFile;
    use lofty::iff::wav::WavFile;
    use lofty::mp4::{Ilst, Mp4File};
    use lofty::mpeg::MpegFile;
    use lofty::musepack::MpcFile;
    use lofty::ogg::tag::VorbisComments;
    use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
    use lofty::wavpack::WavPackFile;
    match file_type {
        FileType::Mpeg => optional_tag!(MpegFile, id3v2_mut, set_id3v2, Id3v2Tag),
        FileType::Wav => optional_tag!(WavFile, id3v2_mut, set_id3v2, Id3v2Tag),
        FileType::Aiff => optional_tag!(AiffFile, id3v2_mut, set_id3v2, Id3v2Tag),
        FileType::Flac => {
            optional_tag!(
                FlacFile,
                vorbis_comments_mut,
                set_vorbis_comments,
                VorbisComments
            )
        }
        FileType::Mp4 => optional_tag!(Mp4File, ilst_mut, set_ilst, Ilst),
        FileType::Ape => optional_tag!(ApeFile, ape_mut, set_ape, ApeTag),
        FileType::WavPack => optional_tag!(WavPackFile, ape_mut, set_ape, ApeTag),
        FileType::Mpc => optional_tag!(MpcFile, ape_mut, set_ape, ApeTag),
        FileType::Opus => ogg_tag!(OpusFile),
        FileType::Vorbis => ogg_tag!(VorbisFile),
        FileType::Speex => ogg_tag!(SpeexFile),
        _ => Err(FileError::Format),
    }
}

/// Keeps an ID3v2 tag in the version it was read in, so a player that reads
/// only ID3v2.3 still reads the file. Other tags have one version.
fn write_options_for<T: std::any::Any>(tag: Option<&T>) -> WriteOptions {
    let mut options = WriteOptions::default();
    let v23 = tag
        .and_then(|t| (t as &dyn std::any::Any).downcast_ref::<lofty::id3::v2::Id3v2Tag>())
        .is_some_and(|t| t.original_version() == Id3v2Version::V3);
    options.use_id3v23(v23)
}

/// Splits a tag into the generic fields and the rest, changes the fields,
/// and merges them back, so the rest stays as it was.
fn edit_tag<T: SplitTag>(
    tag: T,
    values: &[(Field, Option<String>)],
) -> <T::Remainder as MergeTag>::Merged {
    let (rest, mut generic) = tag.split_tag();
    for (field, value) in values {
        set_field(&mut generic, *field, value.as_deref());
    }
    rest.merge_tag(generic)
}

/// Sets or clears one field in a generic tag. Values were parsed, so the
/// numbers parse.
fn set_field(tag: &mut Tag, field: Field, value: Option<&str>) {
    let number = |v: Option<&str>| v.and_then(|v| v.parse::<u32>().ok());
    let text_key = |tag: &mut Tag, key: ItemKey, value: Option<&str>| match value {
        Some(v) => {
            tag.insert_text(key, v.to_owned());
        }
        None => tag.remove_key(key),
    };
    match field {
        Field::Title => match value {
            Some(v) => tag.set_title(v.to_owned()),
            None => tag.remove_title(),
        },
        Field::Artist => match value {
            Some(v) => tag.set_artist(v.to_owned()),
            None => tag.remove_artist(),
        },
        Field::Album => match value {
            Some(v) => tag.set_album(v.to_owned()),
            None => tag.remove_album(),
        },
        Field::AlbumArtist => text_key(tag, ItemKey::AlbumArtist, value),
        Field::Track => match number(value) {
            Some(n) => tag.set_track(n),
            None => tag.remove_track(),
        },
        Field::Disc => match number(value) {
            Some(n) => tag.set_disk(n),
            None => tag.remove_disk(),
        },
        Field::Year => match value.and_then(|v| v.parse::<u16>().ok()) {
            Some(year) => tag.set_date(Timestamp {
                year,
                ..Timestamp::default()
            }),
            None => tag.remove_date(),
        },
        Field::RecordingMbid => text_key(tag, ItemKey::MusicBrainzRecordingId, value),
        Field::ReleaseMbid => text_key(tag, ItemKey::MusicBrainzReleaseId, value),
        Field::ArtistMbid => text_key(tag, ItemKey::MusicBrainzArtistId, value),
        Field::Bpm => {
            // The scan reads either key; keep them from disagreeing.
            tag.remove_key(ItemKey::Bpm);
            tag.remove_key(ItemKey::IntegerBpm);
            // The value is a whole number (see `Field::parse`); each format
            // holds at least one of the two keys.
            if let Some(v) = value {
                tag.insert_text(ItemKey::IntegerBpm, v.to_owned());
                tag.insert_text(ItemKey::Bpm, v.to_owned());
            }
        }
    }
}

#[cfg(test)]
mod tests;
