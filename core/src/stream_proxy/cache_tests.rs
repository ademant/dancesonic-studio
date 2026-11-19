//! Tests of the proxy with an audio cache (#30), against a small fake server
//! that answers byte ranges and can be slowed down, held or cut off.

use std::sync::Mutex as StdMutex;

use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use super::*;
use crate::connection::HttpPolicy;
use crate::db::Database;
use crate::http::{HttpSettings, streaming_client};

/// How the fake server sends a whole track (a request without a range).
#[derive(Clone, Copy, Default)]
struct Pace {
    /// Pause between 16 KiB chunks.
    delay: Duration,
    /// After this many bytes, wait until [`Fake::release`] is notified.
    hold_after: Option<usize>,
    /// After this many bytes, close the connection.
    cut_after: Option<usize>,
}

struct Fake {
    url: ServerUrl,
    /// One line per request: `full so-1` or `range so-1 bytes=…`.
    requests: Arc<StdMutex<Vec<String>>>,
    release: Arc<Notify>,
}

/// The bytes of a track: a pattern that differs per position and song.
fn track(song: &str, len: usize) -> Vec<u8> {
    let seed = song.bytes().fold(0u8, u8::wrapping_add);
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap().wrapping_add(seed))
        .collect()
}

async fn fake_server(len: usize, pace: Pace) -> Fake {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let release = Arc::new(Notify::new());
    let (seen, gate) = (Arc::clone(&requests), Arc::clone(&release));
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (seen, gate) = (Arc::clone(&seen), Arc::clone(&gate));
            tokio::spawn(async move {
                let Ok(head) = read_head(&mut socket).await else {
                    return;
                };
                let head = String::from_utf8_lossy(&head).into_owned();
                let song = head
                    .split("id=")
                    .nth(1)
                    .and_then(|rest| rest.split('&').next())
                    .unwrap_or("")
                    .to_owned();
                let body = track(&song, len);
                let range = head
                    .lines()
                    .find_map(|l| l.strip_prefix("range: ").or(l.strip_prefix("Range: ")))
                    .map(str::to_owned);
                let Some(range) = range else {
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
                };
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
        release,
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    cache: Arc<AudioCache>,
    proxy: StreamProxy,
    fake: Fake,
}

async fn setup(len: usize, pace: Pace) -> Setup {
    let fake = fake_server(len, pace).await;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path().join("library.db")).unwrap());
    let cache = Arc::new(AudioCache::open(dir.path().join("audio"), db, &fake.url).unwrap());
    let http = streaming_client(&HttpSettings {
        policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
        pinned_certificate_pem: None,
    })
    .unwrap();
    let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
    let proxy = StreamProxy::start_with_cache(
        http,
        fake.url.clone(),
        Auth::ApiKey(key),
        Arc::clone(&cache),
    )
    .await
    .unwrap();
    Setup {
        _dir: dir,
        cache,
        proxy,
        fake,
    }
}

fn song(id: &str) -> SongId {
    SongId::parse(id).unwrap()
}

/// Asks the proxy for `id`, optionally a range; returns the head and body.
async fn get(proxy: &StreamProxy, verb: &str, id: &str, range: Option<&str>) -> (String, Vec<u8>) {
    let mut socket = TcpStream::connect(("127.0.0.1", proxy.port()))
        .await
        .unwrap();
    let range = range.map_or(String::new(), |r| format!("Range: {r}\r\n"));
    let request = format!(
        "{verb} /{}/stream?id={id} HTTP/1.1\r\nHost: x\r\n{range}\r\n",
        proxy.token
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), socket.read_to_end(&mut answer))
        .await
        .unwrap()
        .unwrap();
    let split = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let body = answer.split_off(split);
    (String::from_utf8(answer).unwrap(), body)
}

async fn until_cached(cache: &AudioCache, id: &str) {
    for _ in 0..400 {
        if cache.complete(&song(id)).is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{id} never reached the cache");
}

fn requests(fake: &Fake) -> Vec<String> {
    fake.requests.lock().unwrap().clone()
}

#[tokio::test]
async fn the_first_request_plays_while_the_track_is_cached() {
    let len = 300_000;
    let s = setup(len, Pace::default()).await;
    let (head, body) = get(&s.proxy, "GET", "so-1", None).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.contains(&format!("Content-Length: {len}")), "{head}");
    assert!(head.contains("Content-Type: audio/ogg"), "{head}");
    assert_eq!(body, track("so-1", len));
    until_cached(&s.cache, "so-1").await;
    assert_eq!(
        requests(&s.fake),
        vec!["full so-1"],
        "one download, no more"
    );
}

#[tokio::test]
async fn a_cached_track_is_served_from_disk_with_ranges() {
    let len = 100_000;
    let s = setup(len, Pace::default()).await;
    get(&s.proxy, "GET", "so-1", None).await;
    until_cached(&s.cache, "so-1").await;

    let (head, body) = get(&s.proxy, "GET", "so-1", Some("bytes=1000-1999")).await;
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert!(
        head.contains(&format!("Content-Range: bytes 1000-1999/{len}")),
        "{head}"
    );
    assert_eq!(body, track("so-1", len)[1000..2000]);

    let (head, body) = get(&s.proxy, "GET", "so-1", Some("bytes=-10")).await;
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(body, track("so-1", len)[len - 10..]);

    let (head, body) = get(&s.proxy, "HEAD", "so-1", None).await;
    assert!(head.contains(&format!("Content-Length: {len}")), "{head}");
    assert!(body.is_empty());

    let (head, _) = get(&s.proxy, "GET", "so-1", Some(&format!("bytes={len}-"))).await;
    assert!(head.starts_with("HTTP/1.1 416"), "{head}");
    assert!(
        head.contains(&format!("Content-Range: bytes */{len}")),
        "{head}"
    );

    assert_eq!(requests(&s.fake), vec!["full so-1"], "all from disk");
}

#[tokio::test]
async fn a_range_just_ahead_of_the_download_waits_for_it() {
    let len = 400_000;
    let pace = Pace {
        delay: Duration::from_millis(20),
        ..Pace::default()
    };
    let s = setup(len, pace).await;
    // Starts the download; the answer arrives slowly.
    let first = tokio::spawn({
        let port = s.proxy.port();
        let token = s.proxy.token.clone();
        async move {
            let mut socket = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let request = format!("GET /{token}/stream?id=so-1 HTTP/1.1\r\nHost: x\r\n\r\n");
            socket.write_all(request.as_bytes()).await.unwrap();
            let mut answer = Vec::new();
            socket.read_to_end(&mut answer).await.unwrap();
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (head, body) = get(&s.proxy, "GET", "so-1", Some("bytes=100000-100999")).await;
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(body, track("so-1", len)[100_000..101_000]);
    first.await.unwrap();
    assert_eq!(
        requests(&s.fake),
        vec!["full so-1"],
        "the request waited instead of asking the server"
    );
}

#[tokio::test]
async fn a_range_far_ahead_goes_to_the_server_and_fills_the_cache() {
    let len = 2_000_000;
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(len, pace).await;
    // Starts the download, which stops after its first chunk.
    let (head, body) = get(&s.proxy, "GET", "so-1", Some("bytes=0-99")).await;
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(body, track("so-1", len)[..100]);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let far = 1_900_000;
    let (head, body) = get(&s.proxy, "GET", "so-1", Some(&format!("bytes={far}-"))).await;
    assert!(head.starts_with("HTTP/1.1 206"), "{head}");
    assert_eq!(body, track("so-1", len)[far..]);
    assert!(
        requests(&s.fake).contains(&format!("range so-1 bytes={far}-")),
        "{:?}",
        requests(&s.fake)
    );

    // The tail is on disk now: asking again does not go to the server.
    let before = requests(&s.fake).len();
    let (_, again) = get(&s.proxy, "GET", "so-1", Some(&format!("bytes={far}-"))).await;
    assert_eq!(again, body);
    assert_eq!(requests(&s.fake).len(), before);

    s.fake.release.notify_one();
    until_cached(&s.cache, "so-1").await;
    let cached = s.cache.complete(&song("so-1")).unwrap();
    assert_eq!(std::fs::read(cached.path).unwrap(), track("so-1", len));
}

#[tokio::test]
async fn a_download_cut_off_falls_back_to_the_server() {
    let len = 200_000;
    let pace = Pace {
        cut_after: Some(64 * 1024),
        ..Pace::default()
    };
    let s = setup(len, pace).await;
    let (head, body) = get(&s.proxy, "GET", "so-1", None).await;
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert_eq!(body, track("so-1", len), "the player still gets every byte");
    assert!(
        s.cache.complete(&song("so-1")).is_none(),
        "nothing partial is kept"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let leftovers = std::fs::read_dir(s.cache.partial_path(&song("so-1")).parent().unwrap())
        .unwrap()
        .count();
    assert_eq!(leftovers, 0, "the partial file is deleted");
}

#[tokio::test]
async fn the_next_track_is_fetched_after_the_current_one() {
    let len = 300_000;
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(len, pace).await;
    get(&s.proxy, "GET", "so-1", Some("bytes=0-99")).await;
    s.proxy.prefetch(&song("so-2"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        requests(&s.fake),
        vec!["full so-1"],
        "the next track waits for the current one"
    );
    s.fake.release.notify_one();
    until_cached(&s.cache, "so-1").await;
    // The second full download holds as well; let it go.
    for _ in 0..40 {
        if requests(&s.fake).len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(requests(&s.fake), vec!["full so-1", "full so-2"]);
    s.fake.release.notify_one();
    until_cached(&s.cache, "so-2").await;
}

#[tokio::test]
async fn dropping_the_proxy_stops_downloads_and_deletes_partial_files() {
    let pace = Pace {
        hold_after: Some(16 * 1024),
        ..Pace::default()
    };
    let s = setup(300_000, pace).await;
    get(&s.proxy, "GET", "so-1", Some("bytes=0-99")).await;
    let part = s.cache.partial_path(&song("so-1"));
    assert!(part.exists());
    drop(s.proxy);
    for _ in 0..40 {
        if !part.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!part.exists());
    assert!(s.cache.complete(&song("so-1")).is_none());
}

#[tokio::test]
async fn a_cache_for_another_server_is_refused() {
    let fake = fake_server(10, Pace::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path().join("library.db")).unwrap());
    let other = ServerUrl::parse("https://other.example", HttpPolicy::HTTPS_ONLY).unwrap();
    let cache = Arc::new(AudioCache::open(dir.path().join("audio"), db, &other).unwrap());
    let key = crate::subsonic::ApiKey::new("tsk_test_key_12345".to_owned()).unwrap();
    let http = streaming_client(&HttpSettings::default()).unwrap();
    assert!(matches!(
        StreamProxy::start_with_cache(http, fake.url, Auth::ApiKey(key), cache).await,
        Err(ProxyError::Address)
    ));
}

#[test]
fn byte_ranges_parse_and_resolve() {
    assert_eq!(ByteRange::parse("bytes=5-"), Some(ByteRange::From(5)));
    assert_eq!(
        ByteRange::parse("bytes=5-9"),
        Some(ByteRange::Between(5, 9))
    );
    assert_eq!(ByteRange::parse("bytes=-3"), Some(ByteRange::Last(3)));
    assert_eq!(ByteRange::parse("bytes=9-5"), None);
    assert_eq!(ByteRange::parse("bytes=0-1,5-6"), None);
    assert_eq!(ByteRange::parse("bytes=-"), None);
    assert_eq!(ByteRange::parse("bytes=x-"), None);

    assert_eq!(resolve(None, 10), Some((0, 9)));
    assert_eq!(resolve(Some(ByteRange::From(4)), 10), Some((4, 9)));
    assert_eq!(resolve(Some(ByteRange::From(10)), 10), None);
    assert_eq!(resolve(Some(ByteRange::Between(2, 50)), 10), Some((2, 9)));
    assert_eq!(resolve(Some(ByteRange::Last(3)), 10), Some((7, 9)));
    assert_eq!(resolve(Some(ByteRange::Last(30)), 10), Some((0, 9)));
    assert_eq!(resolve(Some(ByteRange::Last(0)), 10), None);
    assert_eq!(resolve(None, 0), None);
}
