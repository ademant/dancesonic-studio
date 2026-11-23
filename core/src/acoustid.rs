//! Matching untagged local files by their audio fingerprint (#14):
//! folksentia's Chromaprint fingerprint, looked up on AcoustID, gives
//! candidate MusicBrainz recordings.
//!
//! Studio only offers candidates. The user confirms one per file, and only
//! then is its MBID written, through the tag editor ([`crate::tag_editor`]),
//! with its preview and undo. A file with an MBID then matches the server's
//! song and DanceBrainz, like a file that was tagged by hand.
//!
//! Studio uses its own AcoustID application key, set when Studio is built
//! (`DANCESONIC_ACOUSTID_KEY`), never a user's key. It goes in the request
//! body, not the URL, and is never logged. AcoustID allows three requests a
//! second per application; Studio stays below that.
//!
//! Answers are untrusted: their size is bounded, MBIDs must parse, scores must
//! lie between 0 and 1, and texts are trimmed and bounded.

use std::collections::HashMap;
use std::time::Duration;

use rusqlite::Connection;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::analysis::Fingerprint;
use crate::connection::{HttpPolicy, ServerUrl};
use crate::dancebrainz::Mbid;
use crate::db::DbError;
use crate::http::{BodyError, read_body};

/// AcoustID's web service.
pub const SERVICE: &str = "https://api.acoustid.org/";

/// Least time between two requests: AcoustID allows three a second.
pub const MIN_INTERVAL: Duration = Duration::from_millis(400);

/// Wait before the one retry after AcoustID said it is busy.
const RETRY_AFTER: Duration = Duration::from_secs(2);

/// Largest answer read. Real answers are a few kilobytes.
const MAX_ANSWER: usize = 1024 * 1024;

/// Most candidates offered for one file.
pub const MAX_CANDIDATES: usize = 10;

/// Longest title or artist kept, in characters.
const MAX_TEXT: usize = 512;

/// Longest track AcoustID is asked about, in seconds.
const MAX_DURATION_SEC: u32 = 24 * 60 * 60;

/// AcoustID's error code for a wrong application key.
const INVALID_KEY: i64 = 4;

/// AcoustID's error codes for too many requests.
const RATE_LIMITED: [i64; 2] = [14, 15];

/// Studio's AcoustID application key, if this build has one.
pub fn app_key() -> Option<&'static str> {
    option_env!("DANCESONIC_ACOUSTID_KEY").filter(|key| !key.trim().is_empty())
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AcoustIdError {
    #[error("AcoustID could not be reached")]
    Transport,
    #[error("AcoustID answered with HTTP {0}")]
    Http(u16),
    #[error("AcoustID is busy; try again later")]
    RateLimited,
    #[error("AcoustID refused Studio's application key")]
    Key,
    #[error("AcoustID refused the request (error {0})")]
    Service(i64),
    #[error("AcoustID's answer does not read")]
    Invalid,
    #[error("AcoustID's answer is too large")]
    TooLarge,
    #[error("the track has no usable length")]
    NoDuration,
}

/// A recording AcoustID offers for a fingerprint.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub mbid: Mbid,
    /// How well the fingerprint matched, from 0 to 1.
    pub score: f64,
    pub title: Option<String>,
    /// The artist credit, joined as MusicBrainz shows it.
    pub artist: Option<String>,
    pub duration_sec: Option<u32>,
}

/// The AcoustID web service, with Studio's application key.
pub struct AcoustId {
    http: reqwest::Client,
    base: ServerUrl,
    key: SecretString,
    /// When the last request was sent, to keep below the rate limit.
    last: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for AcoustId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcoustId")
            .field("base", &self.base)
            .field("key", &"[redacted]")
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct AnswerDto {
    #[serde(default)]
    status: String,
    #[serde(default)]
    results: Vec<ResultDto>,
    #[serde(default)]
    error: Option<ErrorDto>,
}

#[derive(Deserialize)]
struct ErrorDto {
    #[serde(default)]
    code: Option<i64>,
}

#[derive(Deserialize)]
struct ResultDto {
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    recordings: Vec<RecordingDto>,
}

#[derive(Deserialize)]
struct RecordingDto {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    artists: Vec<ArtistDto>,
}

#[derive(Deserialize)]
struct ArtistDto {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    joinphrase: Option<String>,
}

impl AcoustId {
    /// The AcoustID service, through an HTTP client built with
    /// [`crate::http::client`], so https, no redirects and timeouts apply.
    pub fn new(http: reqwest::Client, key: &str) -> Result<Self, AcoustIdError> {
        let base = ServerUrl::parse(SERVICE, HttpPolicy::HTTPS_ONLY)
            .map_err(|_| AcoustIdError::Invalid)?;
        Ok(Self::at(http, base, key))
    }

    /// The service at another address, for tests.
    pub(crate) fn at(http: reqwest::Client, base: ServerUrl, key: &str) -> Self {
        Self {
            http,
            base,
            key: SecretString::from(key.to_owned()),
            last: Mutex::new(None),
        }
    }

    /// The recordings AcoustID knows for a fingerprint of a track of
    /// `duration_sec` seconds, best first, at most [`MAX_CANDIDATES`]. Empty
    /// when AcoustID knows none.
    pub async fn lookup(
        &self,
        fingerprint: &Fingerprint,
        duration_sec: u32,
    ) -> Result<Vec<Candidate>, AcoustIdError> {
        if !(1..=MAX_DURATION_SEC).contains(&duration_sec) {
            return Err(AcoustIdError::NoDuration);
        }
        match self.request(fingerprint, duration_sec).await {
            // A lookup changes nothing on AcoustID, so it may be sent again.
            Err(AcoustIdError::RateLimited) => {
                tokio::time::sleep(RETRY_AFTER).await;
                self.request(fingerprint, duration_sec).await
            }
            other => other,
        }
    }

    async fn request(
        &self,
        fingerprint: &Fingerprint,
        duration_sec: u32,
    ) -> Result<Vec<Candidate>, AcoustIdError> {
        let url = self
            .base
            .endpoint("v2/lookup")
            .map_err(|_| AcoustIdError::Invalid)?;
        // The fingerprint is too long for a URL, and the key must not be in
        // one: both go in the body.
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client", self.key.expose_secret())
            .append_pair("format", "json")
            .append_pair("meta", "recordings")
            .append_pair("duration", &duration_sec.to_string())
            .append_pair("fingerprint", &fingerprint.value)
            .finish();
        self.pace().await;
        let response = self
            .http
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| AcoustIdError::Transport)?;
        let status = response.status().as_u16();
        let bytes = read_body(response, MAX_ANSWER)
            .await
            .map_err(|error| match error {
                BodyError::TooLarge => AcoustIdError::TooLarge,
                BodyError::Transport(_) => AcoustIdError::Transport,
            })?;
        let answer: Option<AnswerDto> = serde_json::from_slice(&bytes).ok();
        let code = answer
            .as_ref()
            .and_then(|a| a.error.as_ref())
            .and_then(|e| e.code);
        match (status, code) {
            (_, Some(INVALID_KEY)) => return Err(AcoustIdError::Key),
            (429 | 503, _) => return Err(AcoustIdError::RateLimited),
            (_, Some(code)) if RATE_LIMITED.contains(&code) => {
                return Err(AcoustIdError::RateLimited);
            }
            (_, Some(code)) => return Err(AcoustIdError::Service(code)),
            (200, None) => {}
            (other, None) => return Err(AcoustIdError::Http(other)),
        }
        let answer = answer.ok_or(AcoustIdError::Invalid)?;
        if answer.status != "ok" {
            return Err(AcoustIdError::Invalid);
        }
        Ok(candidates(answer.results))
    }

    /// Waits until [`MIN_INTERVAL`] has passed since the last request.
    async fn pace(&self) {
        let mut last = self.last.lock().await;
        if let Some(at) = *last {
            tokio::time::sleep_until(at + MIN_INTERVAL).await;
        }
        *last = Some(Instant::now());
    }
}

/// The recordings of all results, each once with its best score, best first.
fn candidates(results: Vec<ResultDto>) -> Vec<Candidate> {
    let mut best: HashMap<Mbid, Candidate> = HashMap::new();
    for result in results {
        let Some(score) = result.score.filter(|s| (0.0..=1.0).contains(s)) else {
            continue;
        };
        for recording in result.recordings {
            let Ok(mbid) = Mbid::parse(&recording.id) else {
                continue;
            };
            let artist: String = recording
                .artists
                .iter()
                .map(|a| {
                    format!(
                        "{}{}",
                        a.name.as_deref().unwrap_or_default(),
                        a.joinphrase.as_deref().unwrap_or_default()
                    )
                })
                .collect();
            let candidate = Candidate {
                mbid: mbid.clone(),
                score,
                title: text(recording.title.as_deref()),
                artist: text(Some(&artist)),
                duration_sec: recording
                    .duration
                    .filter(|d| d.is_finite() && *d > 0.0 && *d < f64::from(MAX_DURATION_SEC))
                    .map(|d| d.round() as u32),
            };
            match best.get(&mbid) {
                Some(known) if known.score >= score => {}
                _ => {
                    best.insert(mbid, candidate);
                }
            }
        }
    }
    let mut list: Vec<Candidate> = best.into_values().collect();
    list.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.mbid.cmp(&b.mbid))
    });
    list.truncate(MAX_CANDIDATES);
    list
}

/// A text from AcoustID, trimmed, without control characters, and bounded.
fn text(value: Option<&str>) -> Option<String> {
    let value: String = value?
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// A local track without a recording MBID.
#[derive(Clone, Debug, PartialEq)]
pub struct Untagged {
    pub track_id: i64,
    /// The path inside its folder, for the user.
    pub name: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: Option<u64>,
    /// From the analysis of the file as it is now; `None` when it was not
    /// analysed with a folksentia that computes fingerprints.
    pub fingerprint: Option<Fingerprint>,
}

impl Untagged {
    /// The length AcoustID is asked about, in whole seconds: the file's, or
    /// the fingerprint's when the file's is not known.
    pub fn lookup_duration(&self) -> Option<u32> {
        let seconds = match self.duration_ms {
            Some(ms) => ms as f64 / 1000.0,
            None => self.fingerprint.as_ref()?.duration_sec,
        };
        (seconds.is_finite() && seconds >= 0.5 && seconds < f64::from(MAX_DURATION_SEC))
            .then(|| seconds.round() as u32)
    }
}

/// The local tracks without a recording MBID, with their fingerprint when
/// there is one, in folder and path order.
pub fn untagged(conn: &Connection) -> Result<Vec<Untagged>, DbError> {
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, t.rel_path, t.title, t.artist, t.duration_ms,
             a.fingerprint, a.fingerprint_sec
         FROM local_track t
         LEFT JOIN local_analysis a ON a.track_id = t.id AND a.error IS NULL
             AND a.size_bytes = t.size_bytes AND a.modified_ms = t.modified_ms
         WHERE t.mbid IS NULL
         ORDER BY t.folder_id, t.rel_path",
    )?;
    let rows = stmt.query_map([], |r| {
        let fingerprint = match (r.get::<_, Option<String>>(5)?, r.get(6)?) {
            (Some(value), Some(duration_sec)) => Some(Fingerprint {
                value,
                duration_sec,
            }),
            _ => None,
        };
        Ok(Untagged {
            track_id: r.get(0)?,
            name: r.get(1)?,
            title: r.get(2)?,
            artist: r.get(3)?,
            duration_ms: r
                .get::<_, Option<i64>>(4)?
                .and_then(|d| u64::try_from(d).ok()),
            fingerprint,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests;
