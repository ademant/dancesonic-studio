//! Handing the play queue between the phone and the desktop (#18).
//!
//! Studio saves its queue, the song that plays and the position on the
//! server with the Subsonic `savePlayQueue` call, and reads a saved queue
//! back with `getPlayQueue`. Any client of the same account can then go on
//! where the other stopped: the DanceSonic app on the phone, or Studio on
//! another computer.
//!
//! Only songs the server has can be saved; local-only tracks are left out.
//! A long queue is saved around the song that plays, so the request stays
//! small. The saved queue is untrusted like every answer: song ids are
//! parsed, the list is bounded, and the position must lie within a day.

use serde::Deserialize;

use crate::queue::SongId;
use crate::subsonic::{Answer, ApiError, SubsonicClient};

/// Most songs saved. The ids go in the request's address, which a proxy
/// limits to a few kilobytes.
pub const MAX_SAVED: usize = 200;

/// Songs kept before the one that plays, when the queue is longer than
/// [`MAX_SAVED`].
const SAVED_BEFORE: usize = 20;

/// Most songs read back from a saved queue.
const MAX_READ: usize = 5_000;

/// Latest position accepted, in milliseconds: a day.
const MAX_POSITION_MS: u64 = 24 * 60 * 60 * 1000;

/// Longest client name kept.
const MAX_NAME: usize = 64;

/// A queue saved on the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedQueue {
    pub songs: Vec<SongId>,
    /// The index in `songs` of the song to go on with.
    pub current: usize,
    /// Where in that song, in milliseconds.
    pub position_ms: u64,
    /// The client that saved it, as it named itself.
    pub changed_by: Option<String>,
    /// When it was saved, as the server wrote it.
    pub changed: Option<String>,
}

/// The part of a queue to save: at most [`MAX_SAVED`] songs, with the one at
/// `current` in it. Returns the range of `len` songs to save and the index
/// of `current` within it.
pub fn saved_part(len: usize, current: usize) -> Option<(std::ops::Range<usize>, usize)> {
    if current >= len {
        return None;
    }
    let start = if len <= MAX_SAVED {
        0
    } else {
        current.saturating_sub(SAVED_BEFORE).min(len - MAX_SAVED)
    };
    let end = (start + MAX_SAVED).min(len);
    Some((start..end, current - start))
}

/// What to save of a play queue: the songs the server has (ids without
/// `local_prefix`), at most [`MAX_SAVED`] around the one at `current`, the
/// index to go on with and the position in it. When the song at `current` is
/// local-only, the next server song is current and starts at its beginning.
/// `None` when no server song plays or follows.
pub fn to_save(
    queue: &[String],
    current: usize,
    position_ms: u64,
    local_prefix: &str,
) -> Option<(Vec<SongId>, usize, u64)> {
    let mut songs = Vec::new();
    let mut at = None;
    let mut position = position_ms;
    for (index, id) in queue.iter().enumerate() {
        if id.starts_with(local_prefix) {
            continue;
        }
        let Ok(song) = SongId::parse(id) else {
            continue;
        };
        if at.is_none() && index >= current {
            if index > current {
                position = 0;
            }
            at = Some(songs.len());
        }
        songs.push(song);
    }
    let (range, at) = saved_part(songs.len(), at?)?;
    Some((songs[range].to_vec(), at, position))
}

#[derive(Deserialize)]
struct Nothing {}

#[derive(Deserialize)]
struct QueueAnswer {
    #[serde(default, rename = "playQueue")]
    play_queue: Option<QueueDto>,
}

#[derive(Deserialize)]
struct QueueDto {
    #[serde(default)]
    current: Option<serde_json::Value>,
    #[serde(default)]
    position: Option<serde_json::Value>,
    #[serde(default, rename = "changedBy")]
    changed_by: Option<String>,
    #[serde(default)]
    changed: Option<String>,
    #[serde(default)]
    entry: Vec<EntryDto>,
}

#[derive(Deserialize)]
struct EntryDto {
    #[serde(default)]
    id: Option<serde_json::Value>,
}

/// An id or a number as text: servers send either.
fn id_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A short text from the server, without control characters.
fn short(text: Option<String>) -> Option<String> {
    let text: String = text?
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

impl SubsonicClient {
    /// Saves `songs` as the account's play queue, going on with the one at
    /// `current` at `position_ms`. Saving again replaces it, so a retry does
    /// no harm.
    pub async fn save_play_queue(
        &self,
        songs: &[SongId],
        current: usize,
        position_ms: u64,
    ) -> Result<(), ApiError> {
        let current = songs
            .get(current)
            .ok_or(ApiError::Invalid("the current song is not in the queue"))?;
        let position = position_ms.min(MAX_POSITION_MS).to_string();
        let mut query: Vec<(&str, &str)> = songs.iter().map(|s| ("id", s.as_str())).collect();
        query.push(("current", current.as_str()));
        query.push(("position", &position));
        let _: Answer<Nothing> = self.get("savePlayQueue", &query).await?;
        Ok(())
    }

    /// The account's saved play queue; `None` when there is none.
    pub async fn play_queue(&self) -> Result<Option<SavedQueue>, ApiError> {
        let answer: Answer<QueueAnswer> = self.get("getPlayQueue", &[]).await?;
        let Some(saved) = answer.payload.play_queue else {
            return Ok(None);
        };
        let songs: Vec<SongId> = saved
            .entry
            .iter()
            .take(MAX_READ)
            .filter_map(|e| e.id.as_ref().and_then(id_text))
            .filter_map(|id| SongId::parse(&id).ok())
            .collect();
        if songs.is_empty() {
            return Ok(None);
        }
        let current = saved
            .current
            .as_ref()
            .and_then(id_text)
            .and_then(|id| songs.iter().position(|s| s.as_str() == id));
        let position_ms = match (current, &saved.position) {
            (Some(_), Some(serde_json::Value::Number(n))) => n
                .as_u64()
                .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64))
                .filter(|p| *p <= MAX_POSITION_MS)
                .unwrap_or(0),
            _ => 0,
        };
        Ok(Some(SavedQueue {
            songs,
            current: current.unwrap_or(0),
            position_ms,
            changed_by: short(saved.changed_by),
            changed: short(saved.changed),
        }))
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::connection::{HttpPolicy, ServerUrl};
    use crate::http::{HttpSettings, client};
    use crate::subsonic::{ApiKey, Auth};

    fn subsonic(server: &MockServer) -> SubsonicClient {
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let http = client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap();
        SubsonicClient::new(http, url, Auth::ApiKey(ApiKey::new("tsk_k").unwrap()))
    }

    fn ids(names: &[&str]) -> Vec<SongId> {
        names.iter().map(|n| SongId::parse(n).unwrap()).collect()
    }

    fn answer(queue: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "subsonic-response": {"status": "ok", "version": "1.16.1", "playQueue": queue}
        }))
    }

    #[test]
    fn a_long_queue_is_saved_around_the_song_that_plays() {
        assert_eq!(saved_part(3, 1), Some((0..3, 1)));
        assert_eq!(saved_part(3, 3), None);
        assert_eq!(saved_part(0, 0), None);
        let len = MAX_SAVED + 100;
        assert_eq!(saved_part(len, 5), Some((0..MAX_SAVED, 5)));
        assert_eq!(
            saved_part(len, 50),
            Some((30..30 + MAX_SAVED, SAVED_BEFORE))
        );
        assert_eq!(
            saved_part(len, len - 1),
            Some((100..len, MAX_SAVED - 1)),
            "the end of the queue stays in"
        );
    }

    #[test]
    fn local_only_songs_are_not_saved() {
        let queue: Vec<String> = ["so-1", "local:4", "so-2", "local:5"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(
            to_save(&queue, 2, 700, "local:"),
            Some((ids(&["so-1", "so-2"]), 1, 700))
        );
        assert_eq!(
            to_save(&queue, 1, 700, "local:"),
            Some((ids(&["so-1", "so-2"]), 1, 0)),
            "a local song that plays hands on to the next server song"
        );
        assert_eq!(to_save(&queue, 3, 700, "local:"), None);
        assert_eq!(to_save(&[], 0, 0, "local:"), None);
    }

    #[tokio::test]
    async fn the_queue_is_saved_with_its_current_song_and_position() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/savePlayQueue.view"))
            .and(query_param("current", "so-2"))
            .and(query_param("position", "61500"))
            .and(header("X-Api-Key", "tsk_k"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = subsonic(&server);
        client
            .save_play_queue(&ids(&["so-1", "so-2", "so-3"]), 1, 61_500)
            .await
            .unwrap();
        let sent = &server.received_requests().await.unwrap()[0];
        let saved: Vec<String> = sent
            .url
            .query_pairs()
            .filter(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(saved, ["so-1", "so-2", "so-3"], "in queue order");
        assert!(matches!(
            client.save_play_queue(&ids(&["so-1"]), 1, 0).await,
            Err(ApiError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn a_saved_queue_is_read_and_checked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getPlayQueue.view"))
            .respond_with(answer(serde_json::json!({
                "current": "so-2",
                "position": 1500,
                "changedBy": "DanceSonic\u{7}",
                "changed": "2026-11-24T10:00:00Z",
                "entry": [{"id": "so-1"}, {"id": ""}, {"id": "so-2"}, {"title": "no id"}, {"id": 3}]
            })))
            .mount(&server)
            .await;
        let saved = subsonic(&server).play_queue().await.unwrap().unwrap();
        assert_eq!(saved.songs, ids(&["so-1", "so-2", "3"]));
        assert_eq!(saved.current, 1);
        assert_eq!(saved.position_ms, 1500);
        assert_eq!(saved.changed_by.as_deref(), Some("DanceSonic"));
        assert_eq!(saved.changed.as_deref(), Some("2026-11-24T10:00:00Z"));
    }

    #[tokio::test]
    async fn an_odd_saved_queue_starts_at_its_beginning() {
        for queue in [
            serde_json::json!({"current": "gone", "position": 9000, "entry": [{"id": "so-1"}]}),
            serde_json::json!({"current": "so-1", "position": -5, "entry": [{"id": "so-1"}]}),
            serde_json::json!({"current": "so-1", "position": 1e12, "entry": [{"id": "so-1"}]}),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(answer(queue))
                .mount(&server)
                .await;
            let saved = subsonic(&server).play_queue().await.unwrap().unwrap();
            assert_eq!((saved.current, saved.position_ms), (0, 0));
        }
    }

    #[tokio::test]
    async fn no_saved_queue_is_none() {
        for body in [
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","playQueue":{"entry":[]}}}"#,
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
            assert_eq!(subsonic(&server).play_queue().await.unwrap(), None);
        }
    }
}
