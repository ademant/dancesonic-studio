use std::os::unix::fs::symlink;

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::id3::v2::Id3v2Tag;
use lofty::iff::wav::{RiffInfoList, WavFile};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Tag, TagExt, TagType};

use super::*;
use crate::catalog::{apply_changes, ensure_server};
use crate::db::tests::temp_db;
use crate::local_library::tests::{tagged, wav};
use crate::local_library::{LocalFolder, add_folder, read_tags, scan_folder};

const MBID: &str = "6e2a8f50-58c4-4c44-9a4c-0d7a3c8f1b11";
const OTHER: &str = "0f9d8d5e-1c4b-4a8e-9b0c-2d7e6f5a4b3c";

/// Bytes standing in for a cover image.
const COVER: &[u8] = b"\x89PNG\r\n\x1a\nnot really a picture";

/// A library with one folder holding the given files, scanned.
fn library(
    files: &[(&str, &str)],
) -> (tempfile::TempDir, Database, tempfile::TempDir, LocalFolder) {
    let (db_dir, db) = temp_db();
    let music = tempfile::tempdir().unwrap();
    for (name, title) in files {
        tagged(
            &music.path().join(name),
            title,
            "Blowzabella",
            "Pingha",
            None,
            "120",
        );
    }
    let folder = add_folder(&db, music.path(), 1).unwrap();
    scan_folder(&db, &folder, 2).unwrap();
    (db_dir, db, music, folder)
}

fn track_id(db: &Database, rel_path: &str) -> i64 {
    db.reader()
        .unwrap()
        .query_row(
            "SELECT id FROM local_track WHERE rel_path = ?1",
            [rel_path],
            |r| r.get(0),
        )
        .unwrap()
}

fn edit(changes: &[(Field, &str)]) -> Edit {
    let mut edit = Edit::new();
    for (field, text) in changes {
        edit.set(*field, text).unwrap();
    }
    edit
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// What a field makes of typed text: the value, or `Err(())` when refused.
fn parsed(field: Field, text: &str) -> Result<Option<String>, ()> {
    field.parse(text).map_err(|_| ())
}

#[test]
fn typed_values_are_parsed_or_refused() {
    let some = |v: &str| Ok(Some(v.to_owned()));
    assert_eq!(parsed(Field::Title, "  An dro "), some("An dro"));
    assert_eq!(parsed(Field::Title, "   "), Ok(None), "empty text clears");
    assert!(parsed(Field::Title, "two\nlines").is_err());
    assert!(parsed(Field::Album, &"x".repeat(1025)).is_err());
    assert_eq!(parsed(Field::Track, "7"), some("7"));
    assert!(parsed(Field::Track, "0").is_err());
    assert!(parsed(Field::Disc, "a").is_err());
    assert!(parsed(Field::Year, "10000").is_err());
    assert_eq!(
        parsed(Field::RecordingMbid, &MBID.to_uppercase()),
        some(MBID),
        "MBIDs in lower case"
    );
    assert!(parsed(Field::ReleaseMbid, "not-an-id").is_err());
    assert_eq!(parsed(Field::Bpm, "96.4"), some("96"), "whole BPMs");
    assert!(parsed(Field::Bpm, "19").is_err());
    assert!(parsed(Field::Bpm, "301").is_err());
    assert!(parsed(Field::Bpm, "NaN").is_err());
}

#[test]
fn a_preview_lists_the_changes_and_writes_nothing() {
    let (_d, db, music, _f) = library(&[("a.wav", "An dro"), ("b.wav", "Bourrée")]);
    let path = music.path().join("a.wav");
    let bytes = fs::read(&path).unwrap();
    let a = track_id(&db, "a.wav");
    let b = track_id(&db, "b.wav");
    let preview = preview(
        &db,
        &[
            (
                a,
                edit(&[(Field::Title, "An dro retourné"), (Field::Album, "")]),
            ),
            // Already holds these values.
            (b, edit(&[(Field::Artist, "Blowzabella")])),
        ],
    )
    .unwrap();
    assert_eq!(preview.unchanged, 1);
    assert!(preview.skipped.is_empty());
    assert_eq!(preview.files.len(), 1);
    let file = &preview.files[0];
    assert_eq!(file.name, "a.wav");
    assert!(!file.on_server);
    assert_eq!(
        file.changes,
        vec![
            FieldChange {
                field: Field::Title,
                before: Some("An dro".into()),
                after: Some("An dro retourné".into()),
            },
            FieldChange {
                field: Field::Album,
                before: Some("Pingha".into()),
                after: None,
            },
        ]
    );
    assert_eq!(fs::read(&path).unwrap(), bytes, "the file is untouched");
}

#[test]
fn a_write_changes_the_tags_keeps_the_rest_and_the_analysis() {
    let (_d, db, music, folder) = library(&[("a.wav", "An dro")]);
    let path = music.path().join("a.wav");
    // Things the editor does not know about, which must survive: a cover
    // and an ID3v2 frame of another program.
    let mut tag = Tag::new(TagType::Id3v2);
    tag.insert_text(ItemKey::TrackTitle, "An dro".into());
    tag.insert_text(ItemKey::AlbumTitle, "Pingha".into());
    tag.push_picture(
        Picture::unchecked(COVER.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    let mut id3 = Id3v2Tag::from(tag);
    id3.insert_user_text("DANCE_NOTE".into(), "for the bal".into());
    id3.save_to_path(&path, WriteOptions::default()).unwrap();
    scan_folder(&db, &folder, 3).unwrap();
    let id = track_id(&db, "a.wav");
    let (size, modified) = local_library::size_and_time(&path).unwrap();
    db.write(|tx| {
        tx.execute(
            "INSERT INTO local_analysis (track_id, tool_version, size_bytes, modified_ms,
                 analysed_at, bpm) VALUES (?1, '0.5.0', ?2, ?3, 1, 96.5)",
            params![id, i64::try_from(size).unwrap(), modified],
        )?;
        Ok(())
    })
    .unwrap();

    let preview = preview(
        &db,
        &[(
            id,
            edit(&[
                (Field::Title, "An dro retourné"),
                (Field::RecordingMbid, MBID),
                (Field::Year, "1983"),
                (Field::Bpm, "132"),
            ]),
        )],
    )
    .unwrap();
    let report = write(&db, &preview, 10).unwrap();
    assert_eq!(report.written, 1, "{report:?}");
    assert!(report.failed.is_empty());

    let tags = read_tags(&path).unwrap();
    assert_eq!(tags.title.as_deref(), Some("An dro retourné"));
    assert_eq!(tags.mbid.as_deref(), Some(MBID));
    assert_eq!(tags.year, Some(1983));
    assert_eq!(tags.bpm, Some(132.0));
    assert_eq!(tags.album.as_deref(), Some("Pingha"), "other fields kept");
    let file = WavFile::read_from(&mut File::open(&path).unwrap(), ParseOptions::new()).unwrap();
    let id3 = file.id3v2().unwrap();
    assert_eq!(id3.get_user_text("DANCE_NOTE"), Some("for the bal"));
    let file = lofty::read_from_path(&path).unwrap();
    let pictures = file.primary_tag().unwrap().pictures();
    assert_eq!(pictures.len(), 1);
    assert_eq!(pictures[0].data(), COVER);
    assert_eq!(files_in(music.path()), ["a.wav"], "no temporary file left");

    let conn = db.reader().unwrap();
    let (title, mbid): (String, String) = conn
        .query_row(
            "SELECT title, mbid FROM local_track WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((title.as_str(), mbid.as_str()), ("An dro retourné", MBID));
    let analysis = crate::analysis::of_track(&conn, id).unwrap();
    assert!(
        matches!(analysis, Some(Ok(ref a)) if a.bpm == Some(96.5)),
        "the audio did not change, so its analysis still counts: {analysis:?}"
    );
    let rescan = scan_folder(&db, &folder, 11).unwrap();
    assert_eq!((rescan.unchanged, rescan.updated), (1, 0));
    assert_eq!(undo_count(&conn).unwrap(), 1);
}

#[test]
fn a_file_without_its_main_tag_keeps_the_values_of_its_other_tag() {
    let (_d, db, music, folder) = library(&[]);
    let path = music.path().join("info.wav");
    wav(&path, 1, 9);
    let mut file =
        WavFile::read_from(&mut File::open(&path).unwrap(), ParseOptions::new()).unwrap();
    let mut info = RiffInfoList::default();
    info.insert("INAM".into(), "Scottish".into());
    info.insert("IART".into(), "Naragonia".into());
    file.set_riff_info(info);
    file.save_to_path(&path, WriteOptions::default()).unwrap();
    scan_folder(&db, &folder, 3).unwrap();
    let id = track_id(&db, "info.wav");
    assert_eq!(
        current_tags(&db, id).unwrap().unwrap().artist.as_deref(),
        Some("Naragonia")
    );

    let preview = preview(&db, &[(id, edit(&[(Field::Album, "Tempo")]))]).unwrap();
    assert_eq!(write(&db, &preview, 10).unwrap().written, 1);
    let tags = read_tags(&path).unwrap();
    assert_eq!(tags.album.as_deref(), Some("Tempo"));
    assert_eq!(tags.title.as_deref(), Some("Scottish"));
    assert_eq!(tags.artist.as_deref(), Some("Naragonia"));
}

#[test]
fn a_file_changed_since_the_preview_is_left_alone() {
    let (_d, db, music, _f) = library(&[("a.wav", "An dro")]);
    let path = music.path().join("a.wav");
    let id = track_id(&db, "a.wav");
    let preview = preview(&db, &[(id, edit(&[(Field::Title, "Mine")]))]).unwrap();
    // Someone else edits the file in between.
    tagged(&path, "Theirs", "x", "y", None, "100");
    let theirs = fs::read(&path).unwrap();

    let report = write(&db, &preview, 10).unwrap();
    assert_eq!(report.written, 0);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(
        report.failed[0].reason,
        "the file changed since the preview"
    );
    assert_eq!(fs::read(&path).unwrap(), theirs);
}

#[test]
fn undo_restores_the_last_write_but_not_a_file_changed_since() {
    let (_d, db, music, _f) = library(&[("a.wav", "An dro"), ("b.wav", "Bourrée")]);
    let a = track_id(&db, "a.wav");
    let b = track_id(&db, "b.wav");
    let change = edit(&[(Field::Album, "Bal"), (Field::AlbumArtist, "Various")]);
    let preview = preview(&db, &[(a, change.clone()), (b, change)]).unwrap();
    assert_eq!(write(&db, &preview, 10).unwrap().written, 2);
    // b is changed by another program after Studio wrote it.
    tagged(
        &music.path().join("b.wav"),
        "Bourrée",
        "x",
        "Theirs",
        None,
        "100",
    );

    let report = undo(&db, 20).unwrap();
    assert_eq!(report.written, 1);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].name, "b.wav");
    let a_tags = read_tags(&music.path().join("a.wav")).unwrap();
    assert_eq!(a_tags.album.as_deref(), Some("Pingha"));
    assert_eq!(
        a_tags.album_artist, None,
        "a cleared field is cleared again"
    );
    let b_tags = read_tags(&music.path().join("b.wav")).unwrap();
    assert_eq!(b_tags.album.as_deref(), Some("Theirs"));
    let album: String = db
        .reader()
        .unwrap()
        .query_row("SELECT album FROM local_track WHERE id = ?1", [a], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(album, "Pingha", "the library follows the undo");
    assert_eq!(undo_count(&db.reader().unwrap()).unwrap(), 0, "undo once");
}

#[test]
fn a_new_write_replaces_the_undo_of_the_last_one() {
    let (_d, db, _m, _f) = library(&[("a.wav", "An dro"), ("b.wav", "Bourrée")]);
    let a = track_id(&db, "a.wav");
    let b = track_id(&db, "b.wav");
    let first = preview(&db, &[(a, edit(&[(Field::Album, "One")]))]).unwrap();
    write(&db, &first, 10).unwrap();
    let second = preview(&db, &[(b, edit(&[(Field::Album, "Two")]))]).unwrap();
    write(&db, &second, 11).unwrap();
    let conn = db.reader().unwrap();
    let kept: Vec<i64> = conn
        .prepare("SELECT track_id FROM tag_undo")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(kept, [b]);
}

#[test]
fn the_bpm_of_a_server_track_is_not_changed_in_its_tag() {
    let (_d, db, music, folder) = library(&[]);
    tagged(
        &music.path().join("same.wav"),
        "Same",
        "x",
        "y",
        Some(MBID),
        "100",
    );
    tagged(&music.path().join("own.wav"), "Own", "x", "y", None, "100");
    let server = ensure_server(&db, "https://music.example", 1).unwrap();
    let mut set = crate::catalog::tests::full_set();
    set.songs[0].mbid = Some(MBID.into());
    set.songs[1].mbid = Some(OTHER.into());
    apply_changes(&db, server, &set, 5).unwrap();
    scan_folder(&db, &folder, 6).unwrap();
    let same = track_id(&db, "same.wav");
    let own = track_id(&db, "own.wav");

    let preview = preview(
        &db,
        &[
            (same, edit(&[(Field::Bpm, "110")])),
            // Becomes a server recording with this edit.
            (
                own,
                edit(&[(Field::RecordingMbid, OTHER), (Field::Bpm, "110")]),
            ),
        ],
    )
    .unwrap();
    assert!(preview.files.is_empty());
    assert_eq!(preview.skipped.len(), 2);
    assert!(
        preview.skipped[0]
            .reason
            .contains("tempo comes from folksonic")
    );

    let other = preview_of(&db, same, &[(Field::Album, "Bal")]);
    assert_eq!(other.files.len(), 1);
    assert!(
        other.files[0].on_server,
        "the server sees it on its next scan"
    );
}

fn preview_of(db: &Database, id: i64, changes: &[(Field, &str)]) -> Preview {
    preview(db, &[(id, edit(changes))]).unwrap()
}

#[test]
fn a_file_outside_its_folder_is_never_edited() {
    let (_d, db, music, folder) = library(&[("a.wav", "An dro")]);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("elsewhere.wav");
    tagged(&target, "Elsewhere", "x", "y", None, "100");
    let before = fs::read(&target).unwrap();
    // The scan never follows links, so put the row in by hand, as if the
    // file had been replaced by a link after the scan.
    let id = track_id(&db, "a.wav");
    fs::remove_file(music.path().join("a.wav")).unwrap();
    symlink(&target, music.path().join("a.wav")).unwrap();

    let preview = preview_of(&db, id, &[(Field::Title, "Mine")]);
    assert!(preview.files.is_empty());
    assert_eq!(
        preview.skipped[0].reason,
        "the file is missing or not inside its folder"
    );
    assert_eq!(fs::read(&target).unwrap(), before);
    let _ = folder;
}

#[test]
fn a_file_studio_cannot_read_is_skipped() {
    let (_d, db, music, folder) = library(&[]);
    fs::write(music.path().join("broken.mp3"), b"not audio at all").unwrap();
    scan_folder(&db, &folder, 3).unwrap();
    let id = track_id(&db, "broken.mp3");
    let preview = preview_of(&db, id, &[(Field::Title, "x")]);
    assert!(preview.files.is_empty());
    assert_eq!(preview.skipped.len(), 1);
    assert_eq!(
        fs::read(music.path().join("broken.mp3")).unwrap(),
        b"not audio at all"
    );
}

#[test]
fn an_undo_record_is_checked_before_it_is_used() {
    assert_eq!(
        old_values(r#"{"title":"An dro","album":null}"#),
        Some(vec![
            (Field::Album, None),
            (Field::Title, Some("An dro".to_owned()))
        ])
    );
    assert_eq!(old_values(r#"{"title":"two\nlines"}"#), None);
    assert_eq!(old_values(r#"{"path":"/etc/passwd"}"#), None);
    assert_eq!(old_values(r#"{"track":7}"#), None);
    assert_eq!(old_values("[]"), None);
}

#[test]
fn too_many_files_are_refused() {
    let (_d, db) = temp_db();
    let edits: Vec<(i64, Edit)> = (0..=i64::try_from(MAX_FILES).unwrap())
        .map(|id| (id, edit(&[(Field::Album, "x")])))
        .collect();
    assert!(matches!(preview(&db, &edits), Err(TagError::TooMany)));
}

#[test]
fn an_id3v2_3_tag_stays_version_2_3() {
    let (_d, db, music, folder) = library(&[]);
    let path = music.path().join("old.wav");
    wav(&path, 1, 5);
    let mut id3 = Id3v2Tag::new();
    id3.set_title("Old".into());
    let mut options = WriteOptions::default();
    id3.save_to_path(&path, options.use_id3v23(true)).unwrap();
    let version = |path: &Path| {
        WavFile::read_from(&mut File::open(path).unwrap(), ParseOptions::new())
            .unwrap()
            .id3v2()
            .unwrap()
            .original_version()
    };
    assert_eq!(version(&path), Id3v2Version::V3);
    scan_folder(&db, &folder, 3).unwrap();
    let id = track_id(&db, "old.wav");
    let preview = preview_of(&db, id, &[(Field::Album, "Older")]);
    assert_eq!(write(&db, &preview, 10).unwrap().written, 1);
    assert_eq!(version(&path), Id3v2Version::V3);
    assert_eq!(read_tags(&path).unwrap().album.as_deref(), Some("Older"));
}
