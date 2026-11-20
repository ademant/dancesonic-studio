//! Reporting plays to the server with `scrobble` (#31), so play counts are
//! the same on every device.
//!
//! A play counts once a track has played for half its length or four
//! minutes, whichever comes first. Only playback counts: a seek forward does
//! not. A counted play is queued in the database first and then sent, oldest
//! first, so plays made offline are sent later with their original time. The
//! server counts each (user, song, time) once, so a retry is harmless.

use std::sync::Arc;

use rusqlite::{Connection, Transaction, params};
use serde::Deserialize;

use crate::catalog::ServerId;
use crate::db::{Database, DbError};
use crate::queue::SongId;
use crate::subsonic::{Answer, ApiError, SubsonicClient};

/// A play counts after this much playback at most.
pub const MAX_THRESHOLD_MS: u64 = 4 * 60 * 1000;

/// Most plays queued per server; the oldest are dropped beyond it.
pub const MAX_QUEUED_PLAYS: i64 = 1000;

/// A larger jump of the position between two readings is a seek, not playback.
const MAX_STEP_MS: u64 = 3_000;

/// Plays read from the queue per round.
const SEND_BATCH: u32 = 50;

/// Subsonic error code for an unknown song: the play can never be counted.
const NOT_FOUND: u32 = 70;

/// One play of a song.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Play {
    pub song: SongId,
    /// When the play started, in epoch milliseconds.
    pub time_ms: i64,
}

/// Follows the playback position of the current track and says when it has
/// played long enough to count.
#[derive(Debug, Default)]
pub struct PlayCounter {
    current: Option<Current>,
}

#[derive(Debug)]
struct Current {
    play: Play,
    threshold_ms: u64,
    listened_ms: u64,
    last_position_ms: Option<u64>,
    counted: bool,
}

impl PlayCounter {
    /// A track started at `time_ms`. `length_ms` is its length, when known.
    pub fn start(&mut self, song: SongId, time_ms: i64, length_ms: Option<u64>) {
        let threshold_ms = length_ms
            .filter(|l| *l > 0)
            .map_or(MAX_THRESHOLD_MS, |l| (l / 2).min(MAX_THRESHOLD_MS));
        self.current = Some(Current {
            play: Play { song, time_ms },
            threshold_ms,
            listened_ms: 0,
            last_position_ms: None,
            counted: false,
        });
    }

    /// Playback stopped; nothing more counts until the next start.
    pub fn stop(&mut self) {
        self.current = None;
    }

    /// The playback position now. Returns the play once, when it has just
    /// reached the threshold.
    pub fn position(&mut self, position_ms: u64) -> Option<Play> {
        let current = self.current.as_mut()?;
        if let Some(last) = current.last_position_ms {
            let step = position_ms.saturating_sub(last);
            if position_ms > last && step <= MAX_STEP_MS {
                current.listened_ms += step;
            }
        }
        current.last_position_ms = Some(position_ms);
        if current.counted || current.listened_ms < current.threshold_ms {
            return None;
        }
        current.counted = true;
        Some(current.play.clone())
    }
}

/// A play waiting in the queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedPlay {
    seq: i64,
    pub song_id: String,
    pub time_ms: i64,
}

/// Queues a play. Returns how many old plays were dropped to stay within
/// [`MAX_QUEUED_PLAYS`].
pub fn queue_play(tx: &Transaction<'_>, server: ServerId, play: &Play) -> Result<usize, DbError> {
    tx.prepare_cached(
        "INSERT INTO play_queue (server_id, song_id, played_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (server_id, song_id, played_at) DO NOTHING",
    )?
    .execute(params![
        server.get(),
        play.song.as_str(),
        play.time_ms.max(0)
    ])?;
    let dropped = tx
        .prepare_cached(
            "DELETE FROM play_queue WHERE server_id = ?1 AND seq NOT IN (
                 SELECT seq FROM play_queue WHERE server_id = ?1 ORDER BY seq DESC LIMIT ?2)",
        )?
        .execute(params![server.get(), MAX_QUEUED_PLAYS])?;
    Ok(dropped)
}

/// The oldest queued plays of a server, in the order they were played.
pub fn queued_plays(
    conn: &Connection,
    server: ServerId,
    limit: u32,
) -> Result<Vec<QueuedPlay>, DbError> {
    let mut statement = conn.prepare_cached(
        "SELECT seq, song_id, played_at FROM play_queue
         WHERE server_id = ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows = statement.query_map(params![server.get(), limit], |r| {
        Ok(QueuedPlay {
            seq: r.get(0)?,
            song_id: r.get(1)?,
            time_ms: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Removes a play from the queue, once sent or when it can never be.
fn forget_play(db: &Database, play: &QueuedPlay) -> Result<(), DbError> {
    let seq = play.seq;
    db.write(|tx| {
        tx.prepare_cached("DELETE FROM play_queue WHERE seq = ?1")?
            .execute([seq])?;
        Ok(())
    })
}

#[derive(Debug, thiserror::Error)]
pub enum PlaysError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("the database task stopped")]
    Task,
}

#[derive(Deserialize)]
struct Nothing {}

impl SubsonicClient {
    /// Reports a play (`submission`) or that a song is playing now. The key
    /// goes in the header, as for every call.
    pub async fn scrobble(
        &self,
        song: &SongId,
        time_ms: i64,
        submission: bool,
    ) -> Result<(), ApiError> {
        let time = time_ms.max(0).to_string();
        let _: Answer<Nothing> = self
            .get(
                "scrobble",
                &[
                    ("id", song.as_str()),
                    ("time", &time),
                    ("submission", if submission { "true" } else { "false" }),
                ],
            )
            .await?;
        Ok(())
    }
}

/// Sends the queued plays of a server, oldest first, and removes each one the
/// server took. Stops at the first failure and leaves the rest for later. A
/// play of a song the server does not know is dropped. Returns how many were
/// sent.
pub async fn send_queued(
    client: &SubsonicClient,
    db: Arc<Database>,
    server: ServerId,
) -> Result<usize, PlaysError> {
    let mut sent = 0;
    loop {
        let reader = Arc::clone(&db);
        let batch = tokio::task::spawn_blocking(move || {
            queued_plays(&reader.reader()?, server, SEND_BATCH)
        })
        .await
        .map_err(|_| PlaysError::Task)??;
        if batch.is_empty() {
            return Ok(sent);
        }
        for play in batch {
            // The table's check keeps ids non-empty, so this only skips rows
            // a newer build might have written.
            if let Ok(song) = SongId::parse(&play.song_id) {
                match client.scrobble(&song, play.time_ms, true).await {
                    Ok(()) => sent += 1,
                    Err(ApiError::Refused {
                        code: Some(NOT_FOUND),
                        ..
                    }) => tracing::warn!("a queued play of an unknown song was dropped"),
                    Err(error) => return Err(error.into()),
                }
            }
            let writer = Arc::clone(&db);
            tokio::task::spawn_blocking(move || forget_play(&writer, &play))
                .await
                .map_err(|_| PlaysError::Task)??;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ensure_server;
    use crate::connection::{HttpPolicy, ServerUrl};
    use crate::db::tests::temp_db;
    use crate::http::{HttpSettings, client};
    use crate::subsonic::{ApiKey, Auth};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn song(id: &str) -> SongId {
        SongId::parse(id).unwrap()
    }

    fn subsonic(server: &MockServer) -> SubsonicClient {
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let http = client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap();
        SubsonicClient::new(http, url, Auth::ApiKey(ApiKey::new("tsk_k").unwrap()))
    }

    const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

    /// Plays the counter from `from` to `to` in quarter-second steps.
    fn play(counter: &mut PlayCounter, from: u64, to: u64) -> Vec<Play> {
        (from..=to)
            .step_by(250)
            .filter_map(|p| counter.position(p))
            .collect()
    }

    #[test]
    fn half_of_a_short_track_counts_once() {
        let mut counter = PlayCounter::default();
        counter.start(song("so-1"), 1_000, Some(180_000));
        assert!(play(&mut counter, 0, 89_750).is_empty());
        let counted = play(&mut counter, 90_000, 180_000);
        assert_eq!(
            counted,
            vec![Play {
                song: song("so-1"),
                time_ms: 1_000
            }]
        );
    }

    #[test]
    fn four_minutes_of_a_long_track_count() {
        let mut counter = PlayCounter::default();
        counter.start(song("so-1"), 1_000, Some(20 * 60 * 1000));
        assert!(play(&mut counter, 0, MAX_THRESHOLD_MS - 250).is_empty());
        assert_eq!(
            play(&mut counter, MAX_THRESHOLD_MS, MAX_THRESHOLD_MS).len(),
            1
        );
    }

    #[test]
    fn an_unknown_length_counts_after_four_minutes() {
        let mut counter = PlayCounter::default();
        counter.start(song("so-1"), 1_000, None);
        assert!(play(&mut counter, 0, MAX_THRESHOLD_MS - 250).is_empty());
        assert_eq!(
            play(&mut counter, MAX_THRESHOLD_MS, MAX_THRESHOLD_MS).len(),
            1
        );
    }

    #[test]
    fn seeking_forward_does_not_count() {
        let mut counter = PlayCounter::default();
        counter.start(song("so-1"), 1_000, Some(180_000));
        play(&mut counter, 0, 10_000);
        // A jump to near the end, then the rest plays.
        assert!(play(&mut counter, 170_000, 180_000).is_empty());
        // Seeking back and listening on counts.
        assert_eq!(play(&mut counter, 0, 75_000).len(), 1);
    }

    #[test]
    fn a_stopped_or_restarted_track_starts_over() {
        let mut counter = PlayCounter::default();
        counter.start(song("so-1"), 1_000, Some(180_000));
        play(&mut counter, 0, 80_000);
        counter.stop();
        assert!(play(&mut counter, 80_000, 180_000).is_empty());
        counter.start(song("so-1"), 2_000, Some(180_000));
        assert!(play(&mut counter, 0, 80_000).is_empty());
        assert_eq!(play(&mut counter, 80_250, 90_000)[0].time_ms, 2_000);
    }

    #[test]
    fn the_queue_is_bounded_and_drops_the_oldest() {
        let (_dir, db) = temp_db();
        let server = ensure_server(&db, "https://music.example", 1).unwrap();
        let other = ensure_server(&db, "https://other.example", 1).unwrap();
        db.write(|tx| {
            queue_play(
                tx,
                other,
                &Play {
                    song: song("so-x"),
                    time_ms: 0,
                },
            )?;
            for time in 0..MAX_QUEUED_PLAYS {
                assert_eq!(
                    queue_play(
                        tx,
                        server,
                        &Play {
                            song: song("so-1"),
                            time_ms: time
                        }
                    )?,
                    0
                );
            }
            // The same play again is not queued twice.
            assert_eq!(
                queue_play(
                    tx,
                    server,
                    &Play {
                        song: song("so-1"),
                        time_ms: 5
                    }
                )?,
                0
            );
            assert_eq!(
                queue_play(
                    tx,
                    server,
                    &Play {
                        song: song("so-2"),
                        time_ms: 9_999
                    }
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
        let conn = db.reader().unwrap();
        let queued = queued_plays(&conn, server, 2_000).unwrap();
        assert_eq!(queued.len() as i64, MAX_QUEUED_PLAYS);
        assert_eq!(queued[0].time_ms, 1, "the oldest play was dropped");
        assert_eq!(queued.last().unwrap().song_id, "so-2");
        assert_eq!(queued_plays(&conn, other, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_play_is_reported_with_its_time() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/scrobble.view"))
            .and(query_param("id", "so-1"))
            .and(query_param("time", "1700000000000"))
            .and(query_param("submission", "false"))
            .and(header("X-Api-Key", "tsk_k"))
            .respond_with(ResponseTemplate::new(200).set_body_string(OK))
            .expect(1)
            .mount(&server)
            .await;
        subsonic(&server)
            .scrobble(&song("so-1"), 1_700_000_000_000, false)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queued_plays_are_sent_in_order_and_kept_on_failure() {
        let (_dir, db) = temp_db();
        let db = Arc::new(db);
        let server_id = ensure_server(&db, "https://music.example", 1).unwrap();
        db.write(|tx| {
            for (id, time) in [("so-1", 10), ("so-gone", 20), ("so-2", 30), ("so-3", 40)] {
                queue_play(
                    tx,
                    server_id,
                    &Play {
                        song: song(id),
                        time_ms: time,
                    },
                )?;
            }
            Ok(())
        })
        .unwrap();

        let server = MockServer::start().await;
        for (id, time) in [("so-1", "10"), ("so-2", "30")] {
            Mock::given(method("GET"))
                .and(path("/rest/scrobble.view"))
                .and(query_param("id", id))
                .and(query_param("time", time))
                .and(query_param("submission", "true"))
                .and(header("X-Api-Key", "tsk_k"))
                .respond_with(ResponseTemplate::new(200).set_body_string(OK))
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/rest/scrobble.view"))
            .and(query_param("id", "so-gone"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"not found"}}}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/scrobble.view"))
            .and(query_param("id", "so-3"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let client = subsonic(&server);
        let result = send_queued(&client, Arc::clone(&db), server_id).await;
        assert!(matches!(result, Err(PlaysError::Api(ApiError::Http(503)))));
        let left = queued_plays(&db.reader().unwrap(), server_id, 10).unwrap();
        assert_eq!(left.len(), 1, "only the failed play stays");
        assert_eq!((left[0].song_id.as_str(), left[0].time_ms), ("so-3", 40));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_empty_queue_sends_nothing() {
        let (_dir, db) = temp_db();
        let db = Arc::new(db);
        let server_id = ensure_server(&db, "https://music.example", 1).unwrap();
        let server = MockServer::start().await;
        let sent = send_queued(&subsonic(&server), db, server_id)
            .await
            .unwrap();
        assert_eq!(sent, 0);
    }
}
