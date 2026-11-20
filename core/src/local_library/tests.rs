use std::os::unix::fs::symlink;

use lofty::config::WriteOptions;
use lofty::tag::{Tag, TagExt, TagType};

use super::*;
use crate::catalog::{apply_changes, ensure_server, songs_of_album};
use crate::db::tests::temp_db;

const MBID: &str = "6e2a8f50-58c4-4c44-9a4c-0d7a3c8f1b11";

/// A WAV file of `seconds` of silence, with `extra` bytes so two files can
/// differ in content.
fn wav(path: &Path, seconds: u32, extra: u8) {
    let rate: u32 = 8_000;
    let mut data = vec![0u8; usize::try_from(rate * 2 * seconds).unwrap()];
    data[0] = extra;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + u32::try_from(data.len()).unwrap()).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(&data);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

/// A WAV file with an ID3v2 tag.
fn tagged(path: &Path, title: &str, artist: &str, album: &str, mbid: Option<&str>, bpm: &str) {
    wav(path, 1, title.len() as u8);
    let mut tag = Tag::new(TagType::Id3v2);
    tag.insert_text(ItemKey::TrackTitle, title.into());
    tag.insert_text(ItemKey::TrackArtist, artist.into());
    tag.insert_text(ItemKey::AlbumTitle, album.into());
    tag.insert_text(ItemKey::TrackNumber, "3".into());
    tag.insert_text(ItemKey::IntegerBpm, bpm.into());
    if let Some(mbid) = mbid {
        tag.insert_text(ItemKey::MusicBrainzRecordingId, mbid.into());
    }
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|s| s.title.as_str()).collect()
}

#[test]
fn tags_are_read_and_checked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("an dro.wav");
    tagged(
        &path,
        " An dro ",
        "Blowzabella",
        "Pingha",
        Some(MBID),
        "132",
    );
    let tags = read_tags(&path).unwrap();
    assert_eq!(tags.title.as_deref(), Some("An dro"));
    assert_eq!(tags.artist.as_deref(), Some("Blowzabella"));
    assert_eq!(tags.album.as_deref(), Some("Pingha"));
    assert_eq!(tags.track, Some(3));
    assert_eq!(tags.mbid.as_deref(), Some(MBID));
    assert_eq!(tags.bpm, Some(132.0));
    assert_eq!(tags.duration_ms, Some(1_000));

    let odd = dir.path().join("odd.wav");
    tagged(&odd, "Odd", "x", "y", Some("not-a-uuid"), "900");
    let tags = read_tags(&odd).unwrap();
    assert_eq!(tags.mbid, None, "an MBID must be a UUID");
    assert_eq!(tags.bpm, None, "a BPM outside 20–300 is dropped");

    let junk = dir.path().join("junk.mp3");
    fs::write(&junk, b"not audio at all").unwrap();
    assert!(read_tags(&junk).is_err());
}

#[test]
fn a_scan_adds_updates_follows_moves_and_drops_gone_files() {
    let (_db_dir, db) = temp_db();
    let music = tempfile::tempdir().unwrap();
    let root = music.path();
    tagged(&root.join("A/one.wav"), "One", "Alba", "First", None, "100");
    tagged(&root.join("A/two.wav"), "Two", "Alba", "First", None, "110");
    tagged(
        &root.join("B/three.wav"),
        "Three",
        "Bara",
        "Second",
        None,
        "120",
    );
    wav(&root.join("B/untagged.wav"), 1, 9);
    fs::write(root.join("B/cover.jpg"), b"jpeg").unwrap();
    fs::write(root.join("B/broken.mp3"), b"not audio").unwrap();
    fs::write(root.join(".hidden.wav"), b"x").unwrap();

    let folder = add_folder(&db, root, 1).unwrap();
    assert_eq!(
        add_folder(&db, &root.join("A/.."), 1).unwrap(),
        folder,
        "added once"
    );
    let report = scan_folder(&db, &folder, 2).unwrap();
    assert_eq!((report.added, report.unreadable), (5, 1), "{report:?}");

    let report = scan_folder(&db, &folder, 3).unwrap();
    assert_eq!(report.unchanged, 5);
    assert_eq!(
        (report.added, report.updated, report.moved, report.removed),
        (0, 0, 0, 0)
    );

    // Move and rename one, retag another, delete a third.
    let conn = db.reader().unwrap();
    let id_of = |rel: &str| -> i64 {
        conn.query_row(
            "SELECT id FROM local_track WHERE rel_path = ?1",
            [rel],
            |r| r.get(0),
        )
        .unwrap()
    };
    let moved_id = id_of("A/one.wav");
    fs::create_dir_all(root.join("C")).unwrap();
    fs::rename(root.join("A/one.wav"), root.join("C/renamed.wav")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    tagged(
        &root.join("A/two.wav"),
        "Two (live)",
        "Alba",
        "First",
        None,
        "111",
    );
    fs::remove_file(root.join("B/broken.mp3")).unwrap();

    let report = scan_folder(&db, &folder, 4).unwrap();
    assert_eq!(
        (report.moved, report.updated, report.removed),
        (1, 1, 1),
        "{report:?}"
    );
    assert_eq!(
        id_of("C/renamed.wav"),
        moved_id,
        "a moved file keeps its row"
    );
    assert!(
        root.join("C/renamed.wav").exists(),
        "files are never touched"
    );

    let songs = local_only_songs(&conn, None, &LocalFilter::Artist("alba".into()), 100).unwrap();
    assert_eq!(titles(&songs), ["One", "Two (live)"]);
    assert!(
        songs
            .iter()
            .all(|s| !s.on_server && s.id.starts_with(LOCAL_ID_PREFIX))
    );
    let found = local_only_songs(&conn, None, &LocalFilter::Search("untag".into()), 100).unwrap();
    assert_eq!(
        titles(&found),
        ["untagged"],
        "without a title the file name is used"
    );
    let none = local_only_songs(&conn, None, &LocalFilter::Search("%".into()), 100).unwrap();
    assert!(none.is_empty(), "LIKE wildcards are searched literally");

    remove_folder(&db, folder.id).unwrap();
    let left: i64 = conn
        .query_row("SELECT count(*) FROM local_track", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0);
    assert!(
        root.join("A/two.wav").exists(),
        "forgetting a folder keeps its files"
    );
}

#[test]
fn links_are_not_followed_and_paths_stay_inside() {
    let (_db_dir, db) = temp_db();
    let music = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    tagged(
        &outside.path().join("secret.wav"),
        "Secret",
        "x",
        "y",
        None,
        "100",
    );
    tagged(
        &music.path().join("in.wav"),
        "Inside",
        "x",
        "y",
        None,
        "100",
    );
    symlink(outside.path(), music.path().join("away")).unwrap();
    symlink(
        outside.path().join("secret.wav"),
        music.path().join("link.wav"),
    )
    .unwrap();

    let folder = add_folder(&db, music.path(), 1).unwrap();
    let report = scan_folder(&db, &folder, 2).unwrap();
    assert_eq!(report.added, 1, "{report:?}");

    assert!(resolve(music.path(), "in.wav").is_some());
    assert_eq!(
        resolve(music.path(), "link.wav"),
        None,
        "a link out is refused"
    );
    assert_eq!(resolve(music.path(), "away/secret.wav"), None);
    assert_eq!(resolve(music.path(), "../x.wav"), None);
    assert_eq!(resolve(music.path(), "/etc/passwd"), None);
    assert_eq!(resolve(music.path(), "gone.wav"), None);

    let refused = db.write(|tx| {
        tx.execute(
            "INSERT INTO local_track (folder_id, rel_path, size_bytes, modified_ms, fingerprint,
                 scanned_at) VALUES (?1, 'a/../../x.wav', 1, 1, ?2, 1)",
            params![folder.id, "0".repeat(64)],
        )?;
        Ok(())
    });
    assert!(refused.is_err(), "the schema refuses `..` too");
}

#[test]
fn local_tracks_join_server_songs_by_recording() {
    let (_db_dir, db) = temp_db();
    let server = ensure_server(&db, "https://music.example", 1).unwrap();
    let mut set = crate::catalog::tests::full_set();
    set.songs[0].mbid = Some(MBID.into());
    apply_changes(&db, server, &set, 5).unwrap();

    let music = tempfile::tempdir().unwrap();
    tagged(
        &music.path().join("same.wav"),
        "Same",
        "Blowzabella",
        "Bal",
        Some(MBID),
        "100",
    );
    tagged(
        &music.path().join("only.wav"),
        "Only here",
        "Blowzabella",
        "Bal",
        None,
        "100",
    );
    let folder = add_folder(&db, music.path(), 1).unwrap();
    scan_folder(&db, &folder, 2).unwrap();

    let conn = db.reader().unwrap();
    let album = set.songs[0].album_id.clone().unwrap();
    let mut songs = songs_of_album(&conn, server, &album).unwrap();
    resolve_paths(&conn, &mut songs).unwrap();
    let both = songs
        .iter()
        .find(|s| s.mbid.as_deref() == Some(MBID))
        .unwrap();
    assert!(both.on_server);
    assert_eq!(
        both.local_path.as_deref(),
        Some(
            fs::canonicalize(music.path().join("same.wav"))
                .unwrap()
                .as_path()
        )
    );
    assert!(
        songs
            .iter()
            .filter(|s| s.mbid.is_none())
            .all(|s| s.local_path.is_none())
    );

    // The matched track is not listed again as local-only.
    let artists = local_artists(&conn, Some(server)).unwrap();
    assert_eq!(
        artists,
        [LocalArtist {
            name: "Blowzabella".into(),
            albums: vec!["Bal".into()]
        }]
    );
    let only = local_only_songs(
        &conn,
        Some(server),
        &LocalFilter::Album {
            artist: "Blowzabella".into(),
            album: "Bal".into(),
        },
        100,
    )
    .unwrap();
    assert_eq!(titles(&only), ["Only here"]);
    // Without that server both are local-only.
    let all =
        local_only_songs(&conn, None, &LocalFilter::Artist("Blowzabella".into()), 100).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn the_watcher_reports_a_changed_folder_once_it_is_quiet() {
    let (_db_dir, db) = temp_db();
    let music = tempfile::tempdir().unwrap();
    let folder = add_folder(&db, music.path(), 1).unwrap();
    let (sender, receiver) = mpsc::channel();
    let watcher = FolderWatcher::start(
        std::slice::from_ref(&folder),
        Duration::from_millis(300),
        move |id| {
            let _ = sender.send(id);
        },
    )
    .unwrap();
    for n in 0..5 {
        wav(&music.path().join(format!("new{n}.wav")), 1, n);
    }
    assert_eq!(receiver.recv_timeout(Duration::from_secs(5)), Ok(folder.id));
    assert!(
        receiver.recv_timeout(Duration::from_millis(600)).is_err(),
        "a burst of changes gives one report"
    );
    drop(watcher);
}
