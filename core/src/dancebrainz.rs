//! DanceBrainz: which dances and meter a recording is for (#9).
//!
//! Enrichment only. A lookup adds dance tags to a track; it never decides
//! tempo (that comes from folksonic) and never blocks playback or the library.
//! A track without an MBID stays playable and visible, just without dances.
//!
//! The dance-type model lives here and nowhere else in core: a [`DanceTag`] is
//! one tag of one recording, with its family path, and [`Qualifier`] says
//! whether a dance is for learning, for the disco, or both.
//!
//! Lookups are anonymous: DanceBrainz serves its public reads without a token,
//! so no credential is sent to it.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::connection::ServerUrl;
use crate::db::{Database, DbError};
use crate::http::{BodyError, read_body};
use crate::sync::now_ms;

/// Most recordings one batch lookup may name (DanceBrainz's own limit).
pub const LOOKUP_BATCH: usize = 500;

/// Largest lookup answer read. A batch of 500 recordings with their chapters is
/// far below this; the limit only stops a broken or hostile server.
pub const MAX_LOOKUP_BYTES: usize = 16 * 1024 * 1024;

/// How long a looked-up recording is trusted before it is asked for again.
pub const REFRESH_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

const MAX_NAME_BYTES: usize = 256;
const MAX_KIND_BYTES: usize = 64;
const MAX_UPDATED_BYTES: usize = 64;
const MAX_PATH_ITEMS: usize = 16;

/// A MusicBrainz recording id: 36 characters, five groups of hex digits, kept
/// in lower case so the same recording always has one spelling.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mbid(String);

impl Mbid {
    /// Parses an MBID, in either case. Anything else is refused.
    pub fn parse(text: &str) -> Result<Self, MbidError> {
        let bytes = text.as_bytes();
        let shape_ok = bytes.len() == 36
            && bytes.iter().enumerate().all(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte == b'-',
                _ => byte.is_ascii_hexdigit(),
            });
        if shape_ok {
            Ok(Self(text.to_ascii_lowercase()))
        } else {
            Err(MbidError::Invalid)
        }
    }

    /// The lower-case text form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Mbid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The text is not an MBID.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MbidError {
    #[error("not a MusicBrainz recording id")]
    Invalid,
}

/// Whether a dance is for learning, for the disco, or both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Qualifier {
    Learn,
    Disco,
    Both,
}

impl Qualifier {
    /// The text DanceBrainz and the database use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Learn => "learn",
            Self::Disco => "disco",
            Self::Both => "both",
        }
    }

    /// Reads a qualifier. An unknown one is dropped rather than failing the
    /// whole answer: the tag itself is still true.
    fn parse(text: &str) -> Option<Self> {
        match text {
            "learn" => Some(Self::Learn),
            "disco" => Some(Self::Disco),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

/// One dance-type tag of one recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DanceTag {
    /// DanceBrainz's identity for the tag; key on this, not on the name.
    pub tag_id: i64,
    /// The tag type, such as `dance`, `region` or `meter`.
    pub kind: String,
    /// The name to show.
    pub name: String,
    /// The names from the family down to this tag, ending with it.
    pub path: Vec<String>,
    pub qualifier: Option<Qualifier>,
    /// When DanceBrainz last changed the tag, as it sent it.
    pub updated_at: String,
}

/// What DanceBrainz says about one recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordingDances {
    pub mbid: Mbid,
    pub tags: Vec<DanceTag>,
}

/// Errors from a lookup. Carry no response content.
#[derive(Debug, thiserror::Error)]
pub enum LookupError {
    #[error("DanceBrainz could not be reached")]
    Unreachable,
    #[error("DanceBrainz did not answer in time")]
    Timeout,
    #[error("DanceBrainz is limiting requests; try again later")]
    RateLimited,
    #[error("DanceBrainz refused the request (HTTP {0})")]
    Http(u16),
    #[error("DanceBrainz sent an answer that is too large")]
    TooLarge,
    #[error("DanceBrainz sent an invalid answer")]
    Invalid,
    #[error("the address for DanceBrainz is not usable")]
    Address,
}

/// Errors from refreshing the cache.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error(transparent)]
    Lookup(#[from] LookupError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("the background task stopped")]
    Task,
}

/// A DanceBrainz server, read anonymously.
#[derive(Clone, Debug)]
pub struct DanceBrainz {
    http: reqwest::Client,
    base: ServerUrl,
}

#[derive(Serialize)]
struct LookupBody<'a> {
    mbids: Vec<&'a str>,
}

#[derive(Deserialize)]
struct LookupDto {
    #[serde(default)]
    recordings: Vec<RecordingDto>,
}

#[derive(Deserialize)]
struct RecordingDto {
    mbid: String,
    #[serde(default)]
    tags: Vec<TagDto>,
}

#[derive(Deserialize)]
struct TagDto {
    tag_id: i64,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: Vec<PathDto>,
    #[serde(default)]
    qualifier: Option<String>,
    #[serde(default)]
    updated_at: String,
}

#[derive(Deserialize)]
struct PathDto {
    #[serde(default)]
    name: String,
}

impl DanceBrainz {
    /// A client for the server at `base`, with the HTTP client the caller built
    /// from its settings, so the https and pinning rules apply.
    pub fn new(http: reqwest::Client, base: ServerUrl) -> Self {
        Self { http, base }
    }

    /// Looks up many recordings, in batches of [`LOOKUP_BATCH`]. Returns the
    /// recordings DanceBrainz answered for, in no particular order. A
    /// recording it does not answer for is left out, so it can be asked again.
    pub async fn lookup(&self, mbids: &[Mbid]) -> Result<Vec<RecordingDances>, LookupError> {
        let mut found = Vec::new();
        for batch in mbids.chunks(LOOKUP_BATCH) {
            found.extend(self.lookup_batch(batch).await?);
        }
        Ok(found)
    }

    async fn lookup_batch(&self, batch: &[Mbid]) -> Result<Vec<RecordingDances>, LookupError> {
        let url = self
            .base
            .endpoint("api/v1/recordings/lookup")
            .map_err(|_| LookupError::Address)?;
        let body = LookupBody {
            mbids: batch.iter().map(Mbid::as_str).collect(),
        };
        let response = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(map_transport)?;
        match response.status().as_u16() {
            200 => {}
            429 => return Err(LookupError::RateLimited),
            other => return Err(LookupError::Http(other)),
        }
        let bytes = read_body(response, MAX_LOOKUP_BYTES)
            .await
            .map_err(|error| match error {
                BodyError::TooLarge => LookupError::TooLarge,
                BodyError::Transport(err) => map_transport(err),
            })?;
        let answer: LookupDto = serde_json::from_slice(&bytes).map_err(|_| LookupError::Invalid)?;
        let requested: HashSet<&str> = batch.iter().map(Mbid::as_str).collect();
        Ok(answer
            .recordings
            .into_iter()
            .filter_map(|dto| recording_from(dto, &requested))
            .collect())
    }

    /// Looks up the given recordings that the cache does not hold or holds too
    /// long, and stores the answers. Returns how many recordings were stored.
    /// A failed lookup changes nothing, so the next call tries again.
    pub async fn refresh(
        &self,
        db: Arc<Database>,
        mbids: Vec<Mbid>,
    ) -> Result<usize, RefreshError> {
        let now = now_ms();
        let reader = Arc::clone(&db);
        let wanted = tokio::task::spawn_blocking(move || {
            let conn = reader.reader()?;
            stale(&conn, &mbids, now)
        })
        .await
        .map_err(|_| RefreshError::Task)??;
        if wanted.is_empty() {
            return Ok(0);
        }
        let answers = self.lookup(&wanted).await?;
        let count = answers.len();
        tokio::task::spawn_blocking(move || db.write(|tx| store(tx, &answers, now)))
            .await
            .map_err(|_| RefreshError::Task)??;
        Ok(count)
    }
}

fn map_transport(error: reqwest::Error) -> LookupError {
    if error.is_timeout() {
        LookupError::Timeout
    } else {
        tracing::debug!(error = %error.without_url(), "DanceBrainz request failed");
        LookupError::Unreachable
    }
}

/// Converts one recording of an answer. Recordings nobody asked for, and tags
/// that do not parse, are dropped; the rest is kept.
fn recording_from(dto: RecordingDto, requested: &HashSet<&str>) -> Option<RecordingDances> {
    let mbid = Mbid::parse(&dto.mbid).ok()?;
    if !requested.contains(mbid.as_str()) {
        return None;
    }
    let tags = dto.tags.into_iter().filter_map(tag_from).collect();
    Some(RecordingDances { mbid, tags })
}

fn tag_from(dto: TagDto) -> Option<DanceTag> {
    if dto.tag_id <= 0 {
        return None;
    }
    let kind = clip(dto.kind.trim(), MAX_KIND_BYTES);
    let name = clip(dto.name.trim(), MAX_NAME_BYTES);
    if kind.is_empty() || name.is_empty() {
        return None;
    }
    let mut path: Vec<String> = dto
        .path
        .into_iter()
        .take(MAX_PATH_ITEMS)
        .map(|item| clip(item.name.trim(), MAX_NAME_BYTES))
        .filter(|name| !name.is_empty())
        .collect();
    if path.is_empty() {
        path.push(name.clone());
    }
    Some(DanceTag {
        tag_id: dto.tag_id,
        kind,
        name,
        path,
        qualifier: dto.qualifier.as_deref().and_then(Qualifier::parse),
        updated_at: clip(&dto.updated_at, MAX_UPDATED_BYTES),
    })
}

/// Cuts text to at most `max` bytes, on a character boundary.
fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Stores answers, replacing each recording's tags and stamping it fetched now.
pub fn store(tx: &Transaction<'_>, answers: &[RecordingDances], now: i64) -> Result<(), DbError> {
    let mut stamp = tx.prepare_cached(
        "INSERT INTO dance_lookup (mbid, fetched_at) VALUES (?1, ?2)
         ON CONFLICT (mbid) DO UPDATE SET fetched_at = excluded.fetched_at",
    )?;
    let mut clear = tx.prepare_cached("DELETE FROM dance_tag WHERE mbid = ?1")?;
    let mut insert = tx.prepare_cached(
        "INSERT INTO dance_tag (mbid, tag_id, kind, name, path, qualifier, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for answer in answers {
        stamp.execute(params![answer.mbid.as_str(), now])?;
        clear.execute([answer.mbid.as_str()])?;
        for tag in &answer.tags {
            let path = serde_json::to_string(&tag.path)
                .map_err(|_| DbError::Invalid("dance path".to_owned()))?;
            insert.execute(params![
                answer.mbid.as_str(),
                tag.tag_id,
                tag.kind,
                tag.name,
                path,
                tag.qualifier.map(Qualifier::as_str),
                tag.updated_at,
            ])?;
        }
    }
    Ok(())
}

/// The recordings that are not cached, or were fetched more than
/// [`REFRESH_AFTER_MS`] before `now`.
pub fn stale(conn: &Connection, mbids: &[Mbid], now: i64) -> Result<Vec<Mbid>, DbError> {
    let mut fetched = conn.prepare_cached("SELECT fetched_at FROM dance_lookup WHERE mbid = ?1")?;
    let mut out = Vec::new();
    for mbid in mbids {
        let at: Option<i64> = fetched
            .query_row([mbid.as_str()], |row| row.get(0))
            .optional()?;
        if at.is_none_or(|at| now.saturating_sub(at) >= REFRESH_AFTER_MS) {
            out.push(mbid.clone());
        }
    }
    Ok(out)
}

/// The cached dance tags of one recording, in tag-id order. Empty when the
/// recording was never looked up or has no tags.
pub fn dances_of(conn: &Connection, mbid: &Mbid) -> Result<Vec<DanceTag>, DbError> {
    let mut select = conn.prepare_cached(
        "SELECT tag_id, kind, name, path, qualifier, updated_at
         FROM dance_tag WHERE mbid = ?1 ORDER BY tag_id",
    )?;
    let rows = select.query_map([mbid.as_str()], |row| {
        let path: String = row.get(3)?;
        let qualifier: Option<String> = row.get(4)?;
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            path,
            qualifier,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut tags = Vec::new();
    for row in rows {
        let (tag_id, kind, name, path, qualifier, updated_at) = row?;
        let path: Vec<String> =
            serde_json::from_str(&path).map_err(|_| DbError::Invalid("dance path".to_owned()))?;
        tags.push(DanceTag {
            tag_id,
            kind,
            name,
            path,
            qualifier: qualifier.as_deref().and_then(Qualifier::parse),
            updated_at,
        });
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::HttpPolicy;
    use crate::http::{HttpSettings, client};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const A: &str = "5ec2d0c5-03e3-4d8b-9ad2-0c11a6b7a5d1";
    const B: &str = "00000000-0000-4000-8000-000000000002";

    fn mbid(text: &str) -> Mbid {
        Mbid::parse(text).unwrap()
    }

    fn http() -> reqwest::Client {
        client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap()
    }

    async fn dancebrainz(server: &MockServer) -> DanceBrainz {
        let base = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        DanceBrainz::new(http(), base)
    }

    fn answer_for(mbids: &[&str]) -> serde_json::Value {
        let recordings: Vec<_> = mbids
            .iter()
            .map(|id| {
                json!({
                    "mbid": id,
                    "tags": [{
                        "tag_id": 41,
                        "type": "dance",
                        "name": "Ton simpl",
                        "path": [{"id": 40, "name": "Suite Gavotte de Montagne"},
                                 {"id": 41, "name": "Ton simpl"}],
                        "qualifier": "disco",
                        "updated_at": "2026-11-19T10:00:00Z"
                    }],
                    "chapters": [],
                    "chapters_version": "abc"
                })
            })
            .collect();
        json!({ "recordings": recordings })
    }

    #[test]
    fn mbids_are_checked_and_lower_cased() {
        assert_eq!(
            Mbid::parse("5EC2D0C5-03E3-4D8B-9AD2-0C11A6B7A5D1")
                .unwrap()
                .as_str(),
            A
        );
        assert!(Mbid::parse("not-an-mbid").is_err());
        assert!(Mbid::parse("5ec2d0c503e34d8b9ad20c11a6b7a5d1").is_err());
        assert!(Mbid::parse("5ec2d0c5-03e3-4d8b-9ad2-0c11a6b7a5dg").is_err());
    }

    #[test]
    fn qualifiers_are_read_and_unknown_ones_dropped() {
        assert_eq!(Qualifier::parse("learn"), Some(Qualifier::Learn));
        assert_eq!(Qualifier::parse("both"), Some(Qualifier::Both));
        assert_eq!(Qualifier::parse("party"), None);
    }

    #[tokio::test]
    async fn a_lookup_returns_the_dance_tags_with_their_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer_for(&[A])))
            .expect(1)
            .mount(&server)
            .await;
        let found = dancebrainz(&server).await.lookup(&[mbid(A)]).await.unwrap();
        assert_eq!(found.len(), 1);
        let tag = &found[0].tags[0];
        assert_eq!(tag.tag_id, 41);
        assert_eq!(tag.kind, "dance");
        assert_eq!(tag.name, "Ton simpl");
        assert_eq!(tag.path, ["Suite Gavotte de Montagne", "Ton simpl"]);
        assert_eq!(tag.qualifier, Some(Qualifier::Disco));
    }

    #[tokio::test]
    async fn the_request_carries_no_credential() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"recordings": []})))
            .mount(&server)
            .await;
        dancebrainz(&server).await.lookup(&[mbid(A)]).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get("authorization").is_none());
        assert!(requests[0].url.query().is_none());
    }

    #[tokio::test]
    async fn large_lists_are_sent_in_batches_of_500() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"recordings": []})))
            .expect(2)
            .mount(&server)
            .await;
        let ids: Vec<Mbid> = (0..501)
            .map(|n| mbid(&format!("00000000-0000-4000-8000-{n:012}")))
            .collect();
        dancebrainz(&server).await.lookup(&ids).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let sizes: Vec<usize> = requests
            .iter()
            .map(|r| {
                serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["mbids"]
                    .as_array()
                    .unwrap()
                    .len()
            })
            .collect();
        assert_eq!(sizes, [500, 1]);
    }

    #[tokio::test]
    async fn answers_for_unrequested_or_malformed_recordings_are_ignored() {
        let server = MockServer::start().await;
        let body = json!({"recordings": [
            {"mbid": A, "tags": []},
            {"mbid": B, "tags": []},
            {"mbid": "garbage", "tags": []},
            {"mbid": A, "tags": [
                {"tag_id": 0, "type": "dance", "name": "x", "path": []},
                {"tag_id": 9, "type": "", "name": "x", "path": []},
                {"tag_id": 10, "type": "dance", "name": "Tamm", "path": [], "qualifier": "party"}
            ]}
        ]});
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let found = dancebrainz(&server).await.lookup(&[mbid(A)]).await.unwrap();
        assert_eq!(found.len(), 2, "B is not requested and garbage is dropped");
        let tags = &found[1].tags;
        assert_eq!(tags.len(), 1, "tags with no id or type are dropped");
        assert_eq!(tags[0].tag_id, 10);
        assert_eq!(tags[0].qualifier, None);
        assert_eq!(
            tags[0].path,
            ["Tamm"],
            "an empty path becomes the tag itself"
        );
    }

    #[tokio::test]
    async fn rate_limits_server_errors_and_oversized_answers_are_refused() {
        let server = MockServer::start().await;
        let dance = dancebrainz(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        assert!(matches!(
            dance.lookup(&[mbid(A)]).await,
            Err(LookupError::RateLimited)
        ));
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        assert!(matches!(
            dance.lookup(&[mbid(A)]).await,
            Err(LookupError::Http(502))
        ));
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_LOOKUP_BYTES + 1]),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            dance.lookup(&[mbid(A)]).await,
            Err(LookupError::TooLarge)
        ));
    }

    fn temp_database() -> (tempfile::TempDir, Arc<Database>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("data").join("library.db")).unwrap();
        (dir, Arc::new(db))
    }

    #[test]
    fn stored_dances_come_back_and_stale_ones_are_asked_again() {
        let (_dir, db) = temp_database();
        let answer = RecordingDances {
            mbid: mbid(A),
            tags: vec![DanceTag {
                tag_id: 41,
                kind: "dance".into(),
                name: "Ton simpl".into(),
                path: vec!["Suite".into(), "Ton simpl".into()],
                qualifier: Some(Qualifier::Both),
                updated_at: "2026-11-19T10:00:00Z".into(),
            }],
        };
        let now = 1_000_000_000_000;
        db.write(|tx| store(tx, std::slice::from_ref(&answer), now))
            .unwrap();
        let conn = db.reader().unwrap();
        assert_eq!(dances_of(&conn, &mbid(A)).unwrap(), answer.tags);
        assert!(dances_of(&conn, &mbid(B)).unwrap().is_empty());

        let wanted = [mbid(A), mbid(B)];
        assert_eq!(stale(&conn, &wanted, now + 1).unwrap(), [mbid(B)]);
        assert_eq!(
            stale(&conn, &wanted, now + REFRESH_AFTER_MS).unwrap(),
            [mbid(A), mbid(B)]
        );
    }

    #[tokio::test]
    async fn refresh_asks_only_for_what_is_stale() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer_for(&[A])))
            .expect(1)
            .mount(&server)
            .await;
        let (_dir, db) = temp_database();
        let dance = dancebrainz(&server).await;

        assert_eq!(
            dance.refresh(Arc::clone(&db), vec![mbid(A)]).await.unwrap(),
            1
        );
        // Fresh now, so the second call sends nothing: `expect(1)` checks it.
        assert_eq!(
            dance.refresh(Arc::clone(&db), vec![mbid(A)]).await.unwrap(),
            0
        );
        let conn = db.reader().unwrap();
        assert_eq!(dances_of(&conn, &mbid(A)).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_refresh_stores_nothing_and_is_not_fatal_to_the_library() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/recordings/lookup"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let (_dir, db) = temp_database();
        let result = dancebrainz(&server)
            .await
            .refresh(Arc::clone(&db), vec![mbid(A)])
            .await;
        assert!(matches!(
            result,
            Err(RefreshError::Lookup(LookupError::Http(502)))
        ));
        let conn = db.reader().unwrap();
        assert_eq!(stale(&conn, &[mbid(A)], now_ms()).unwrap(), [mbid(A)]);
    }
}
