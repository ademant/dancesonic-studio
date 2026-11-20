use std::os::unix::fs::PermissionsExt;

use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::catalog::tests::full_set;
use crate::catalog::{apply_changes, ensure_server, songs_of_album};
use crate::connection::HttpPolicy;
use crate::db::tests::temp_db;
use crate::http::{HttpSettings, streaming_client};
use crate::subsonic::ApiKey;

struct Setup {
    _db_dir: tempfile::TempDir,
    db: Arc<Database>,
    server: ServerId,
    source: Source,
    mock: MockServer,
    store: tempfile::TempDir,
}

async fn setup() -> Setup {
    let (db_dir, db) = temp_db();
    let db = Arc::new(db);
    let mock = MockServer::start().await;
    let url = ServerUrl::parse(&mock.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
    let server = ensure_server(&db, &url.key(), 1).unwrap();
    let mut set = full_set();
    for (song, size) in set.songs.iter_mut().zip([4u64, 5, 6]) {
        song.size_bytes = Some(size);
    }
    apply_changes(&db, server, &set, 2).unwrap();
    let http = streaming_client(&HttpSettings {
        policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
        pinned_certificate_pem: None,
    })
    .unwrap();
    Setup {
        _db_dir: db_dir,
        db,
        server,
        source: Source {
            http,
            server: url,
            auth: Auth::ApiKey(ApiKey::new("tsk_k").unwrap()),
        },
        mock,
        store: tempfile::tempdir().unwrap(),
    }
}

async fn audio(mock: &MockServer, id: &str, body: &[u8]) {
    Mock::given(method("GET"))
        .and(path("/rest/download.view"))
        .and(query_param("id", id))
        .and(header("X-Api-Key", "tsk_k"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/flac")
                .set_body_bytes(body.to_vec()),
        )
        .mount(mock)
        .await;
}

fn dir(setup: &Setup) -> PathBuf {
    setup.store.path().join("offline")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_album_is_downloaded_and_plays_from_disk() {
    let s = setup().await;
    audio(&s.mock, "s1", b"ONE!").await;
    audio(&s.mock, "s2", b"TWO!!").await;
    let base = s.source.server.key();
    set_pinned(&s.db, &base, PinKind::Album, "al1", true, 5).unwrap();
    assert!(is_pinned(&s.db.reader().unwrap(), &base, PinKind::Album, "al1").unwrap());

    let report = sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 1_000)
        .await
        .unwrap();
    assert_eq!((report.downloaded, report.used_bytes), (2, 9), "{report:?}");
    assert_eq!(
        fs::metadata(dir(&s)).unwrap().permissions().mode() & 0o777,
        0o700
    );

    let conn = s.db.reader().unwrap();
    let mut songs = songs_of_album(&conn, s.server, "al1").unwrap();
    resolve_paths(&conn, &dir(&s), &base, &mut songs).unwrap();
    for song in &songs {
        let path = song.local_path.as_ref().expect("a copy");
        assert!(song.kept_offline);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(is_copy_name(name), "named by a hash: {name}");
    }
    assert_eq!(
        fs::read(songs[0].local_path.as_ref().unwrap()).unwrap(),
        b"ONE!"
    );

    // A second sync keeps them; unpinning removes them.
    let again = sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 1_000)
        .await
        .unwrap();
    assert_eq!((again.downloaded, again.kept), (0, 2));
    set_pinned(&s.db, &base, PinKind::Album, "al1", false, 6).unwrap();
    let gone = sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 1_000)
        .await
        .unwrap();
    assert_eq!(gone.removed, 2);
    assert_eq!(fs::read_dir(dir(&s)).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_errors_and_broken_copies_are_handled() {
    let s = setup().await;
    let base = s.source.server.key();
    audio(&s.mock, "s3", b"MAZUR!").await;
    audio(&s.mock, "s1", b"ONE!").await;
    // The playlist holds s3 then s1; a limit of 7 bytes keeps only s3.
    set_pinned(&s.db, &base, PinKind::Playlist, "p1", true, 5).unwrap();
    let report = sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 7)
        .await
        .unwrap();
    assert_eq!((report.downloaded, report.over_limit), (1, 1), "{report:?}");

    // A copy changed on disk is caught by the check and fetched again.
    let conn = s.db.reader().unwrap();
    let name: String = conn
        .query_row("SELECT file_name FROM offline_track", [], |r| r.get(0))
        .unwrap();
    fs::write(dir(&s).join(&name), b"BROKEN").unwrap();
    assert_eq!(verify(&s.db, &dir(&s), Some(&base)).unwrap(), (1, 1));
    let report = sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 7)
        .await
        .unwrap();
    assert_eq!(report.downloaded, 1);
    assert_eq!(verify(&s.db, &dir(&s), None).unwrap(), (1, 0));
    assert_eq!(used_bytes(&s.db.reader().unwrap(), None).unwrap(), 6);

    // A Subsonic error with status 200 is not stored as audio.
    let other = setup().await;
    let base = other.source.server.key();
    Mock::given(method("GET"))
        .and(path("/rest/download.view"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(r#"{"subsonic-response":{"status":"failed"}}"#),
        )
        .mount(&other.mock)
        .await;
    set_pinned(&other.db, &base, PinKind::Album, "al2", true, 5).unwrap();
    let report = sync(
        &other.source,
        Arc::clone(&other.db),
        other.server,
        dir(&other),
        1_000,
    )
    .await
    .unwrap();
    assert_eq!((report.downloaded, report.failed), (0, 1));
    assert_eq!(
        fs::read_dir(dir(&other)).unwrap().count(),
        0,
        "no temp file left"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_larger_than_the_catalogue_says_is_refused() {
    let s = setup().await;
    let base = s.source.server.key();
    Mock::given(method("GET"))
        .and(path("/rest/download.view"))
        .and(query_param("id", "s3"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "audio/flac")
                .set_body_bytes(vec![1u8; 64]),
        )
        .mount(&s.mock)
        .await;
    set_pinned(&s.db, &base, PinKind::Album, "al2", true, 5).unwrap();
    let report = sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 32)
        .await
        .unwrap();
    assert_eq!(report.downloaded, 0, "{report:?}");
    assert_eq!(fs::read_dir(dir(&s)).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_cleanup_and_a_new_folder() {
    let s = setup().await;
    let base = s.source.server.key();
    audio(&s.mock, "s3", b"MAZUR!").await;
    set_pinned(&s.db, &base, PinKind::Album, "al2", true, 5).unwrap();
    sync(&s.source, Arc::clone(&s.db), s.server, dir(&s), 1_000)
        .await
        .unwrap();

    // Leftovers of a stopped download go; the user's own files stay.
    fs::write(
        dir(&s).join(format!("{}.audio.77.part", "a".repeat(64))),
        b"x",
    )
    .unwrap();
    fs::write(dir(&s).join(format!("{}.audio", "b".repeat(64))), b"x").unwrap();
    fs::write(dir(&s).join("mine.flac"), b"x").unwrap();
    assert_eq!(clean_up(&s.db, &dir(&s)).unwrap(), 2);
    assert!(dir(&s).join("mine.flac").exists());
    fs::remove_file(dir(&s).join("mine.flac")).unwrap();

    // Another folder: the old copies go and the next sync fills the new one.
    let default_root = dir(&s);
    let picked = tempfile::tempdir().unwrap();
    set_settings(
        &s.db,
        &default_root,
        &Settings {
            folder: Some(picked.path().to_owned()),
            limit_bytes: 1_000,
        },
    )
    .unwrap();
    assert_eq!(fs::read_dir(&default_root).unwrap().count(), 0);
    let settings = settings(&s.db.reader().unwrap()).unwrap();
    let new_dir = store_dir(&default_root, &settings);
    assert!(new_dir.ends_with(FOLDER_NAME));
    let report = sync(
        &s.source,
        Arc::clone(&s.db),
        s.server,
        new_dir.clone(),
        1_000,
    )
    .await
    .unwrap();
    assert_eq!(report.downloaded, 1);

    // Signing out with "remove offline copies".
    assert_eq!(remove_server(&s.db, &new_dir, &base).unwrap(), 1);
    assert_eq!(fs::read_dir(&new_dir).unwrap().count(), 0);
    assert!(!is_pinned(&s.db.reader().unwrap(), &base, PinKind::Album, "al2").unwrap());
}
