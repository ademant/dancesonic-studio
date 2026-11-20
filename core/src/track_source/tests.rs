//! Tests of the track source, against a small fake server that answers byte
//! ranges and can be slowed down, held, cut off or drop connections.

use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use super::*;
use crate::connection::HttpPolicy;
use crate::db::Database;
use crate::http::{HttpSettings, streaming_client};

/// How the fake server behaves.
#[derive(Clone, Copy, Default)]
struct Pace {
    /// Pause between 16 KiB chunks of a whole track.
    delay: Duration,
    /// After this many bytes of a whole track, wait for [`Fake::release`].
    hold_after: Option<usize>,
    /// After this many bytes of a whole track, close the connection.
    cut_after: Option<usize>,
    /// Close the first connections without an answer.
    drop_first: usize,
}

struct Fake {
    url: ServerUrl,
    /// One line per request: `full so-1` or `range so-1 bytes=…`.
    requests: Arc<StdMutex<Vec<String>>>,
    /// The credential header of every request.
    credentials: Arc<StdMutex<Vec<String>>>,
    release: Arc<Notify>,
}

/// The bytes of a track: a pattern that differs per position and song.
fn track(song: &str, len: usize) -> Vec<u8> {
    let seed = song.bytes().fold(0u8, u8::wrapping_add);
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap().wrapping_add(seed))
        .collect()
}

/// Reads the request head, up to the blank line.
async fn read_head(socket: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn fake_server(rt: &tokio::runtime::Runtime, len: usize, pace: Pace) -> Fake {
    let listener = rt.block_on(TcpListener::bind(("127.0.0.1", 0))).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let credentials = Arc::new(StdMutex::new(Vec::new()));
    let release = Arc::new(Notify::new());
    let connections = Arc::new(AtomicUsize::new(0));
    let (seen, creds, gate) = (
        Arc::clone(&requests),
        Arc::clone(&credentials),
        Arc::clone(&release),
    );
    rt.spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let number = connections.fetch_add(1, Ordering::SeqCst) + 1;
            let (seen, creds, gate) = (Arc::clone(&seen), Arc::clone(&creds), Arc::clone(&gate));
            tokio::spawn(async move {
                let Some(head) = read_head(&mut socket).await else {
                    return;
                };
                let credential = header(&head, "dpop")
                    .or(header(&head, "x-api-key"))
                    .unwrap_or("")
                    .to_owned();
                creds.lock().unwrap().push(credential);
                if number <= pace.drop_first {
                    return;
                }
                let song = head
                    .split("id=")
                    .nth(1)
                    .and_then(|rest| rest.split(['&', ' ']).next())
                    .unwrap_or("")
                    .to_owned();
                let body = track(&song, len);
                let range = header(&head, "range").map(str::to_owned);
                // The download asks without a range.
                if range.is_none() {
                    seen.lock().unwrap().push(format!("full {song}"));
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/ogg\r\n\
                         Content-Length: {len}\r\nConnection: close\r\n\r\n"
                    );
                    socket.write_all(answer.as_bytes()).await.unwrap();
                    let mut sent = 0;
                    let mut held = false;
                    for chunk in body.chunks(16 * 1024) {
                        if pace.cut_after.is_some_and(|cut| sent >= cut) {
                            return;
                        }
                        if !held && pace.hold_after.is_some_and(|hold| sent >= hold) {
                            gate.notified().await;
                            held = true;
                        }
                        if socket.write_all(chunk).await.is_err() {
                            return;
                        }
                        sent += chunk.len();
                        tokio::time::sleep(pace.delay).await;
                    }
                    return;
                }
                let range = range.unwrap_or_default();
                seen.lock().unwrap().push(format!("range {song} {range}"));
                let spec = range.trim_start_matches("bytes=");
                let (first, last) = spec.split_once('-').unwrap();
                let first: usize = first.parse().unwrap();
                let last: usize = last.parse().unwrap_or(len - 1).min(len - 1);
                let part = &body[first..=last];
                let answer = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/ogg\r\n\
                     Content-Length: {}\r\nContent-Range: bytes {first}-{last}/{len}\r\n\
                     Connection: close\r\n\r\n",
                    part.len()
                );
                socket.write_all(answer.as_bytes()).await.unwrap();
                // A client that has what it needs may hang up.
                let _ = socket.write_all(part).await;
            });
        }
    });
    let url = ServerUrl::parse(
        &format!("http://127.0.0.1:{port}"),
        HttpPolicy::ALLOW_PRIVATE_HTTP,
    )
    .unwrap();
    Fake {
        url,
        requests,
        credentials,
        release,
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn http() -> reqwest::Client {
    streaming_client(&HttpSettings {
        policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
        pinned_certificate_pem: None,
    })
    .unwrap()
}

fn api_key() -> Auth {
    Auth::ApiKey(crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap())
}

struct Setup {
    _dir: tempfile::TempDir,
    cache: Arc<AudioCache>,
    tracks: Tracks,
    fake: Fake,
    /// Kept alive for the fake server and the downloads.
    _rt: tokio::runtime::Runtime,
}

fn setup(len: usize, pace: Pace) -> Setup {
    let rt = runtime();
    let fake = fake_server(&rt, len, pace);
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path().join("library.db")).unwrap());
    let cache = Arc::new(AudioCache::open(dir.path().join("audio"), db, &fake.url).unwrap());
    let tracks = Tracks::new(
        http(),
        fake.url.clone(),
        api_key(),
        Some(Arc::clone(&cache)),
        rt.handle().clone(),
    )
    .unwrap();
    Setup {
        _dir: dir,
        cache,
        tracks,
        fake,
        _rt: rt,
    }
}

fn song(id: &str) -> SongId {
    SongId::parse(id).unwrap()
}

fn never() -> bool {
    false
}

/// Reads `count` bytes from `pos`, as a player would, in 64 KiB reads.
fn read(source: &mut TrackSource, pos: u64, count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count);
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < count {
        let want = (count - out.len()).min(buf.len());
        let n = source
            .read_at(pos + out.len() as u64, &mut buf[..want], &never)
            .unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn until_cached(cache: &AudioCache, id: &str) {
    for _ in 0..400 {
        if cache.complete(&song(id)).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never reached the cache");
}

fn requests(fake: &Fake) -> Vec<String> {
    fake.requests.lock().unwrap().clone()
}

#[test]
fn the_first_read_plays_while_the_track_is_cached() {
    let len = 300_000;
    let s = setup(len, Pace::default());
    let mut source = s.tracks.open(&song("so-1"));
    assert_eq!(source.len(&never).unwrap(), len as u64);
    assert_eq!(read(&mut source, 0, len), track("so-1", len));
    let mut buf = [0u8; 16];
    assert_eq!(
        source.read_at(len as u64, &mut buf, &never),
        Ok(0),
        "end of track"
    );
    until_cached(&s.cache, "so-1");
    assert_eq!(
        requests(&s.fake),
        vec!["full so-1"],
        "one download, no more"
    );
}

#[test]
fn a_cached_track_is_read_from_disk() {
    let len = 100_000;
    let s = setup(len, Pace::default());
    read(&mut s.tracks.open(&song("so-1")), 0, len);
    until_cached(&s.cache, "so-1");

    let mut source = s.tracks.open(&song("so-1"));
    assert_eq!(source.len(&never).unwrap(), len as u64);
    assert_eq!(
        read(&mut source, 1000, 1000),
        track("so-1", len)[1000..2000]
    );
    assert_eq!(
        read(&mut source, (len - 10) as u64, 100),
        track("so-1", len)[len - 10..]
    );
    assert_eq!(requests(&s.fake), vec!["full so-1"], "all from disk");
}

#[test]
fn a_read_just_ahead_of_the_download_waits_for_it() {
    let len = 400_000;
    let pace = Pace {
        delay: Duration::from_millis(20),
        ..Pace::default()
    };
    let s = setup(len, pace);
    let mut source = s.tracks.open(&song("so-1"));
    source.len(&never).unwrap();
    assert_eq!(
        read(&mut source, 100_000, 1000),
        track("so-1", len)[100_000..101_000]
    );
    assert_eq!(
        requests(&s.fake),
        vec!["full so-1"],
        "the read waited instead of asking the server"
    );
}

#[test]
fn a_read_far_ahead_fetches_a_range_into_the_cache() {
    let len = 2_000_000;
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(len, pace);
    let mut source = s.tracks.open(&song("so-1"));
    assert_eq!(read(&mut source, 0, 100), track("so-1", len)[..100]);
    std::thread::sleep(Duration::from_millis(300));

    let far = 1_900_000;
    assert_eq!(
        read(&mut source, far, len - far as usize),
        track("so-1", len)[far as usize..]
    );
    assert!(
        requests(&s.fake).contains(&format!("range so-1 bytes={far}-")),
        "{:?}",
        requests(&s.fake)
    );

    // The tail is on disk now: a second reader does not go to the server.
    let before = requests(&s.fake).len();
    let mut other = s.tracks.open(&song("so-1"));
    assert_eq!(
        read(&mut other, far + 50_000, 1000),
        track("so-1", len)[far as usize + 50_000..][..1000]
    );
    assert_eq!(requests(&s.fake).len(), before);

    s.fake.release.notify_one();
    until_cached(&s.cache, "so-1");
    let cached = s.cache.complete(&song("so-1")).unwrap();
    assert_eq!(std::fs::read(cached.path).unwrap(), track("so-1", len));
}

#[test]
fn a_download_cut_off_falls_back_to_the_server() {
    let len = 200_000;
    let pace = Pace {
        cut_after: Some(64 * 1024),
        ..Pace::default()
    };
    let s = setup(len, pace);
    let mut source = s.tracks.open(&song("so-1"));
    assert_eq!(
        read(&mut source, 0, len),
        track("so-1", len),
        "every byte arrives"
    );
    assert!(
        s.cache.complete(&song("so-1")).is_none(),
        "nothing partial is kept"
    );
    std::thread::sleep(Duration::from_millis(100));
    let dir = s.cache.partial_path(&song("so-1"));
    let leftovers = std::fs::read_dir(dir.parent().unwrap()).unwrap().count();
    assert_eq!(leftovers, 0, "the partial file is deleted");
}

#[test]
fn the_next_track_is_fetched_after_the_current_one() {
    let len = 300_000;
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(len, pace);
    read(&mut s.tracks.open(&song("so-1")), 0, 100);
    s.tracks.prefetch(&song("so-2"));
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        requests(&s.fake),
        vec!["full so-1"],
        "the next track waits for the current one"
    );
    s.fake.release.notify_one();
    until_cached(&s.cache, "so-1");
    for _ in 0..40 {
        if requests(&s.fake).len() == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(requests(&s.fake), vec!["full so-1", "full so-2"]);
    s.fake.release.notify_one();
    until_cached(&s.cache, "so-2");
}

#[test]
fn dropping_the_tracks_stops_downloads_and_deletes_partial_files() {
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(300_000, pace);
    read(&mut s.tracks.open(&song("so-1")), 0, 100);
    let part = s.cache.partial_path(&song("so-1"));
    assert!(part.exists());
    drop(s.tracks);
    for _ in 0..40 {
        if !part.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!part.exists());
    assert!(s.cache.complete(&song("so-1")).is_none());
}

#[test]
fn a_waiting_read_is_cancelled_promptly() {
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(300_000, pace);
    let mut source = s.tracks.open(&song("so-1"));
    read(&mut source, 0, 100);
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        flag.store(true, Ordering::SeqCst);
    });
    let started = Instant::now();
    let mut buf = [0u8; 1024];
    // Just ahead of the held download: the read waits for it.
    let result = source.read_at(20_000, &mut buf, &|| cancelled.load(Ordering::SeqCst));
    assert_eq!(result, Err(SourceError::Cancelled));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    s.fake.release.notify_one();
}

#[test]
fn a_cache_for_another_server_is_refused() {
    let rt = runtime();
    let fake = fake_server(&rt, 10, Pace::default());
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path().join("library.db")).unwrap());
    let other = ServerUrl::parse("https://other.example", HttpPolicy::HTTPS_ONLY).unwrap();
    let cache = Arc::new(AudioCache::open(dir.path().join("audio"), db, &other).unwrap());
    assert!(matches!(
        Tracks::new(
            http(),
            fake.url,
            api_key(),
            Some(cache),
            rt.handle().clone()
        ),
        Err(SourceError::WrongCache)
    ));
}

#[test]
fn without_a_cache_reads_go_to_the_server_with_ranges() {
    let len = 100_000;
    let rt = runtime();
    let fake = fake_server(&rt, len, Pace::default());
    let tracks = Tracks::new(
        http(),
        fake.url.clone(),
        api_key(),
        None,
        rt.handle().clone(),
    )
    .unwrap();
    let mut source = tracks.open(&song("so-1"));
    assert_eq!(source.len(&never).unwrap(), len as u64);
    assert_eq!(read(&mut source, 0, 1000), track("so-1", len)[..1000]);
    // A seek: a new range from there.
    assert_eq!(
        read(&mut source, 50_000, 1000),
        track("so-1", len)[50_000..51_000]
    );
    let seen = requests(&fake);
    assert_eq!(seen, vec!["range so-1 bytes=0-", "range so-1 bytes=50000-"]);
    assert!(
        fake.credentials
            .lock()
            .unwrap()
            .iter()
            .all(|c| c == "tsk_test_key_12345"),
        "every request carries the key in its header"
    );
}

/// Regression (#6): a request sent on a connection the server had closed
/// failed with "error sending request", and playback stopped.
#[test]
fn a_request_dropped_before_any_answer_is_sent_again_with_a_fresh_proof() {
    let len = 1000;
    let rt = runtime();
    let pace = Pace {
        drop_first: 1,
        ..Pace::default()
    };
    let fake = fake_server(&rt, len, pace);
    let device = crate::dpop::DeviceKey::generate().unwrap();
    let tracks = Tracks::new(
        http(),
        fake.url.clone(),
        Auth::Device(device),
        None,
        rt.handle().clone(),
    )
    .unwrap();
    let mut source = tracks.open(&song("so-1"));
    assert_eq!(read(&mut source, 0, len), track("so-1", len));
    let proofs = fake.credentials.lock().unwrap().clone();
    assert_eq!(proofs.len(), 2, "one more attempt");
    assert_ne!(proofs[0], proofs[1], "a DPoP proof is never sent twice");
    assert!(proofs.iter().all(|p| !p.is_empty()));
}

#[test]
fn an_unreachable_server_is_an_error_not_a_hang() {
    let rt = runtime();
    // A port nobody listens on.
    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = ServerUrl::parse(
        &format!("http://127.0.0.1:{port}"),
        HttpPolicy::ALLOW_PRIVATE_HTTP,
    )
    .unwrap();
    let tracks = Tracks::new(http(), url, api_key(), None, rt.handle().clone()).unwrap();
    let mut source = tracks.open(&song("so-1"));
    assert_eq!(source.len(&never), Err(SourceError::Unreachable));
}

#[test]
fn content_ranges_and_types_are_read_strictly() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(CONTENT_RANGE, "bytes 5-9/10".parse().unwrap());
    assert_eq!(content_range(&headers), Some((5, 10)));
    headers.insert(CONTENT_RANGE, "bytes */10".parse().unwrap());
    assert_eq!(content_range(&headers), None);

    headers.insert(CONTENT_TYPE, "audio/ogg".parse().unwrap());
    assert_eq!(audio_content_type(&headers), "audio/ogg");
    headers.insert(CONTENT_TYPE, "text/html".parse().unwrap());
    assert_eq!(audio_content_type(&headers), FALLBACK_CONTENT_TYPE);
}
