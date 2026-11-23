//! folksentia's output (schema 2), read into the values Studio keeps.
//!
//! The output is untrusted: unknown fields are ignored, every value is
//! checked for its range, and a value out of range counts as unknown. Lists
//! are bounded before they are stored.

use serde::Deserialize;

use super::AnalysisError;
use crate::tempo::BPM_RANGE;

/// The output schema Studio reads.
pub const SCHEMA: u64 = 2;

/// Most beats kept: 300 BPM for more than 10 hours.
const MAX_BEATS: usize = 200_000;

/// Most waveform bins kept.
const MAX_WAVEFORM: usize = 4096;

/// Longest fingerprint kept, in characters of base64.
const MAX_FINGERPRINT: usize = 65_536;

/// The values of one analysed file. `None` is unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Analysis {
    pub bpm: Option<f64>,
    pub bpm_confidence: Option<f64>,
    /// Beat times in seconds.
    pub beats: Option<Vec<f64>>,
    /// Beats per bar.
    pub downbeat_meter: Option<u8>,
    /// Indexes into `beats` of the beats that start a bar.
    pub downbeats: Option<Vec<u32>>,
    pub key: Option<Key>,
    pub key_strength: Option<f64>,
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    pub loudness_range_lu: Option<f64>,
    pub replay_gain_db: Option<f64>,
    pub silence_lead_ms: Option<u32>,
    pub silence_tail_ms: Option<u32>,
    /// One peak (0–255) per bin.
    pub waveform: Option<Vec<u8>>,
    pub fingerprint: Option<Fingerprint>,
}

/// A musical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    /// Pitch class, C = 0 … B = 11.
    pub pitch: u8,
    pub minor: bool,
}

/// A Chromaprint fingerprint as `fpcalc` prints it, for AcoustID (#14).
#[derive(Clone, Debug, PartialEq)]
pub struct Fingerprint {
    pub value: String,
    /// Seconds of audio it covers.
    pub duration_sec: f64,
}

/// What folksentia said about one file.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Analysed(Box<Analysis>),
    /// folksentia's error object, such as a file it could not decode.
    Failed(String),
}

/// The fields Studio reads; folksentia's names, snake case and camel case.
#[derive(Deserialize)]
struct Raw {
    bpm: Option<f64>,
    bpm_confidence: Option<f64>,
    beats: Option<Vec<f64>>,
    #[serde(rename = "downbeatMeter")]
    downbeat_meter: Option<i64>,
    #[serde(rename = "downbeatIndexes")]
    downbeat_indexes: Option<Vec<i64>>,
    key_pitch: Option<i64>,
    key_mode: Option<i64>,
    key_strength: Option<f64>,
    replay_gain_db: Option<f64>,
    silence: Option<RawSilence>,
    loudness: Option<RawLoudness>,
    waveform: Option<RawWaveform>,
    fingerprint: Option<RawFingerprint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSilence {
    lead_ms: Option<f64>,
    tail_ms: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLoudness {
    integrated_lufs: Option<f64>,
    true_peak_dbtp: Option<f64>,
    lra_lu: Option<f64>,
}

#[derive(Deserialize)]
struct RawWaveform {
    #[serde(default)]
    peaks: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFingerprint {
    algorithm: Option<String>,
    duration_sec: Option<f64>,
    value: Option<String>,
}

/// Reads one output object. Another schema is an error for the whole run,
/// since every file would be misread; a broken field only loses that value.
pub fn parse(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Outcome, AnalysisError> {
    let schema = object.get("schema").and_then(serde_json::Value::as_u64);
    if schema != Some(SCHEMA) {
        return Err(AnalysisError::Schema(schema));
    }
    if let Some(error) = object.get("error") {
        let message = error.as_str().unwrap_or("folksentia reported an error");
        return Ok(Outcome::Failed(short(message)));
    }
    let raw: Raw = match serde_json::from_value(serde_json::Value::Object(object.clone())) {
        Ok(raw) => raw,
        Err(error) => {
            return Ok(Outcome::Failed(short(&format!(
                "folksentia's output could not be read: {error}"
            ))));
        }
    };
    Ok(Outcome::Analysed(Box::new(values(raw))))
}

fn values(raw: Raw) -> Analysis {
    let beats = raw.beats.filter(|beats| {
        beats.len() <= MAX_BEATS && beats.iter().all(|t| t.is_finite() && *t >= 0.0)
    });
    let beat_count = beats.as_ref().map_or(0, Vec::len);
    let downbeats = raw.downbeat_indexes.and_then(|indexes| {
        indexes
            .into_iter()
            .map(|i| {
                usize::try_from(i)
                    .ok()
                    .filter(|i| *i < beat_count)
                    .and_then(|i| u32::try_from(i).ok())
            })
            .collect::<Option<Vec<u32>>>()
    });
    let key = match (raw.key_pitch, raw.key_mode) {
        (Some(pitch @ 0..=11), Some(mode @ 0..=1)) => Some(Key {
            pitch: u8::try_from(pitch).unwrap_or(0),
            minor: mode == 1,
        }),
        _ => None,
    };
    let silence = raw.silence;
    let loudness = raw.loudness;
    Analysis {
        bpm: finite(raw.bpm).filter(|bpm| BPM_RANGE.contains(bpm)),
        bpm_confidence: finite(raw.bpm_confidence).filter(|c| *c >= 0.0),
        beats,
        downbeat_meter: raw
            .downbeat_meter
            .filter(|m| (2..=12).contains(m))
            .and_then(|m| u8::try_from(m).ok()),
        downbeats,
        key,
        key_strength: finite(raw.key_strength).filter(|s| (0.0..=1.0).contains(s)),
        integrated_lufs: finite(loudness.as_ref().and_then(|l| l.integrated_lufs)),
        true_peak_dbtp: finite(loudness.as_ref().and_then(|l| l.true_peak_dbtp)),
        loudness_range_lu: finite(loudness.as_ref().and_then(|l| l.lra_lu)).filter(|l| *l >= 0.0),
        replay_gain_db: finite(raw.replay_gain_db),
        silence_lead_ms: millis(silence.as_ref().and_then(|s| s.lead_ms)),
        silence_tail_ms: millis(silence.as_ref().and_then(|s| s.tail_ms)),
        waveform: raw.waveform.and_then(|w| {
            (!w.peaks.is_empty() && w.peaks.len() <= MAX_WAVEFORM)
                .then(|| {
                    w.peaks
                        .into_iter()
                        .map(|p| u8::try_from(p).ok())
                        .collect::<Option<Vec<u8>>>()
                })
                .flatten()
        }),
        fingerprint: raw.fingerprint.and_then(|f| {
            let value = f.value?;
            let ok = f.algorithm.as_deref() == Some("chromaprint")
                && !value.is_empty()
                && value.len() <= MAX_FINGERPRINT
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"+/=-_".contains(&b));
            let duration_sec = finite(f.duration_sec).filter(|d| *d > 0.0)?;
            ok.then_some(Fingerprint {
                value,
                duration_sec,
            })
        }),
    }
}

fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite())
}

/// Milliseconds as a whole number, if they make sense.
fn millis(value: Option<f64>) -> Option<u32> {
    finite(value)
        .filter(|ms| (0.0..=f64::from(u32::MAX)).contains(ms))
        .map(|ms| ms.round() as u32)
}

/// An error message, trimmed and at most 300 characters.
pub(crate) fn short(message: &str) -> String {
    let message = message.trim();
    let message = if message.is_empty() {
        "folksentia reported an error"
    } else {
        message
    };
    message
        .chars()
        .take(super::folksentia::MAX_ERROR_CHARS)
        .collect()
}
