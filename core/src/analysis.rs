//! Analysis of local files with folksentia (#11).
//!
//! folksentia runs as a separate program ([`Folksentia`]), one process per
//! file, a few at a time ([`analyse_library`]). Each result is stored with the
//! folksentia version and the size and modification time of the file it
//! belongs to. A track is analysed again only when its file changed, or when
//! the user asks for a run with another folksentia version. Failed runs are
//! kept with their error and retried only on request.
//!
//! A run can be stopped at any time by dropping (aborting) its future: the
//! running folksentia processes are killed, and what was stored stays.
//! Starting again picks up the tracks that are still to do.
//!
//! Tempo has one source: a track that folksonic knows shows folksonic's BPM.
//! The BPM found here is used for local-only tracks (see
//! [`crate::local_library::local_only_songs`]) and as input for the round
//! trip to folksonic, never as a second BPM next to the server's.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, params};
use tokio::task::JoinSet;

use crate::db::{Database, DbError};
use crate::local_library;

pub mod folksentia;
pub mod output;

pub use folksentia::Folksentia;
pub use output::{Analysis, Fingerprint, Key, Outcome};

/// How long folksentia may take for one file before it is stopped.
pub const TRACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Waveform bins asked for, the same number folksonic asks for.
pub const WAVEFORM_BINS: u32 = 150;

/// Most folksentia processes at a time. Each takes about 1 GB for a
/// six-minute track.
pub const MAX_PARALLEL: u8 = 16;

#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(
        "folksentia was not found. Install it, or choose the program in the analysis settings."
    )]
    NotFound,
    #[error("{0} is not a program that can be run")]
    NotAProgram(PathBuf),
    #[error("{path} could not be run: {source}")]
    Run {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} did not report a folksentia version. Studio needs folksentia 0.1.0 or newer.")]
    NoVersion(PathBuf),
    #[error(
        "folksentia printed output schema {}; Studio reads schema 2. Install a newer folksentia.",
        .0.map_or_else(|| "none".to_owned(), |s| s.to_string())
    )]
    Schema(Option<u64>),
    #[error("the analysis stopped: {0}")]
    Stopped(String),
}

impl From<rusqlite::Error> for AnalysisError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::Sqlite(error))
    }
}

/// The analysis settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The folksentia program the user chose; `None` looks for it.
    pub folksentia: Option<PathBuf>,
    /// folksentia processes at a time, 1 to [`MAX_PARALLEL`].
    pub parallel: u8,
}

/// Reads the settings.
pub fn settings(conn: &Connection) -> Result<Settings, AnalysisError> {
    Ok(conn.query_row(
        "SELECT folksentia, parallel FROM analysis_settings WHERE id = 1",
        [],
        |r| {
            Ok(Settings {
                folksentia: r.get::<_, Option<String>>(0)?.map(PathBuf::from),
                parallel: u8::try_from(r.get::<_, i64>(1)?).unwrap_or(1),
            })
        },
    )?)
}

/// Saves the settings. The program must be an absolute path; the number of
/// processes is kept within 1 to [`MAX_PARALLEL`].
pub fn set_settings(db: &Database, settings: &Settings) -> Result<(), AnalysisError> {
    let program = match &settings.folksentia {
        Some(path) => Some(
            path.to_str()
                .filter(|p| path.is_absolute() && p.len() <= 4096)
                .ok_or_else(|| AnalysisError::NotAProgram(path.clone()))?
                .to_owned(),
        ),
        None => None,
    };
    let parallel = settings.parallel.clamp(1, MAX_PARALLEL);
    db.write(|tx| {
        tx.execute(
            "UPDATE analysis_settings SET folksentia = ?1, parallel = ?2 WHERE id = 1",
            params![program, parallel],
        )?;
        Ok(())
    })?;
    Ok(())
}

/// The processes to run when the settings ask for `wanted`: never more than
/// the machine has CPUs.
pub fn parallel_for(wanted: u8) -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    usize::from(wanted.clamp(1, MAX_PARALLEL)).min(cpus)
}

/// The folksentia flags for a library run.
pub fn library_options(tool: &Folksentia) -> Vec<String> {
    let mut options = vec![
        "--downbeats".to_owned(),
        format!("--waveform={WAVEFORM_BINS}"),
    ];
    if tool.has_chromaprint() {
        options.push("--fingerprint".to_owned());
    }
    options
}

/// How far the local library is analysed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Local tracks in all folders.
    pub tracks: usize,
    /// Tracks with a current result.
    pub analysed: usize,
    /// Tracks whose last run failed and whose file did not change since.
    pub failed: usize,
    /// Tracks without a result for their current file.
    pub pending: usize,
    /// Tracks with a current result or failure from another folksentia
    /// version than `version`.
    pub other_version: usize,
}

/// Counts the tracks by analysis state. `version` is the folksentia version
/// that would run now, if one was found.
pub fn status(conn: &Connection, version: Option<&str>) -> Result<Status, AnalysisError> {
    let (tracks, analysed, failed, other): (i64, i64, i64, i64) = conn
        .prepare_cached(
            "SELECT count(*),
                 count(*) FILTER (WHERE a.track_id IS NOT NULL AND a.error IS NULL),
                 count(*) FILTER (WHERE a.track_id IS NOT NULL AND a.error IS NOT NULL),
                 count(*) FILTER (WHERE a.track_id IS NOT NULL AND ?1 IS NOT NULL
                                  AND a.tool_version <> ?1)
             FROM local_track t
             LEFT JOIN local_analysis a ON a.track_id = t.id
                 AND a.size_bytes = t.size_bytes AND a.modified_ms = t.modified_ms",
        )?
        .query_row([version], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
    let count = |n: i64| usize::try_from(n).unwrap_or(0);
    Ok(Status {
        tracks: count(tracks),
        analysed: count(analysed),
        failed: count(failed),
        pending: count(tracks - analysed - failed),
        other_version: count(other),
    })
}

/// Which tracks a run analyses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Also tracks whose last run failed.
    pub retry_failed: bool,
    /// Also tracks analysed by another folksentia version than the one that
    /// runs.
    pub other_versions: bool,
}

/// One file to analyse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Work {
    pub track_id: i64,
    pub path: PathBuf,
    pub size_bytes: i64,
    pub modified_ms: i64,
}

/// The tracks to analyse with folksentia `version`: those without a result
/// for their current file, and what `selection` adds. Files that are no
/// longer inside their folder are left out; the next scan removes them.
pub fn work(
    conn: &Connection,
    version: &str,
    selection: Selection,
) -> Result<Vec<Work>, AnalysisError> {
    let mut stmt = conn.prepare_cached(
        "SELECT t.id, f.path, t.rel_path, t.size_bytes, t.modified_ms
         FROM local_track t
         JOIN local_folder f ON f.id = t.folder_id
         LEFT JOIN local_analysis a ON a.track_id = t.id
             AND a.size_bytes = t.size_bytes AND a.modified_ms = t.modified_ms
         WHERE a.track_id IS NULL
            OR (?1 AND a.error IS NOT NULL)
            OR (?2 AND a.tool_version <> ?3)
         ORDER BY f.path, t.rel_path",
    )?;
    let rows = stmt.query_map(
        params![selection.retry_failed, selection.other_versions, version],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        },
    )?;
    let mut work = Vec::new();
    for row in rows {
        let (track_id, root, rel_path, size_bytes, modified_ms) = row?;
        if let Some(path) = local_library::resolve(Path::new(&root), &rel_path) {
            work.push(Work {
                track_id,
                path,
                size_bytes,
                modified_ms,
            });
        }
    }
    Ok(work)
}

/// Stores the result of one run for the file state in `work`. A track that
/// was removed meanwhile is skipped.
pub fn store(
    db: &Database,
    work: &Work,
    version: &str,
    now_ms: i64,
    result: &Result<Analysis, String>,
) -> Result<(), AnalysisError> {
    let none = Analysis::default();
    let (values, error) = match result {
        Ok(values) => (values, None),
        Err(message) => (&none, Some(output::short(message))),
    };
    let json = |v: &Option<Vec<f64>>| v.as_ref().and_then(|v| serde_json::to_string(v).ok());
    let downbeats = values
        .downbeats
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok());
    let version: String = version.chars().take(200).collect();
    db.write(|tx| {
        tx.prepare_cached(
            "INSERT INTO local_analysis (track_id, tool_version, size_bytes, modified_ms,
                 analysed_at, error, bpm, bpm_confidence, beats, downbeat_meter, downbeats,
                 key_pitch, key_mode, key_strength, integrated_lufs, true_peak_dbtp,
                 loudness_range_lu, replay_gain_db, silence_lead_ms, silence_tail_ms,
                 waveform, fingerprint, fingerprint_sec)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20, ?21, ?22, ?23
             WHERE EXISTS (SELECT 1 FROM local_track WHERE id = ?1)
             ON CONFLICT (track_id) DO UPDATE SET
                 tool_version = excluded.tool_version, size_bytes = excluded.size_bytes,
                 modified_ms = excluded.modified_ms, analysed_at = excluded.analysed_at,
                 error = excluded.error, bpm = excluded.bpm,
                 bpm_confidence = excluded.bpm_confidence, beats = excluded.beats,
                 downbeat_meter = excluded.downbeat_meter, downbeats = excluded.downbeats,
                 key_pitch = excluded.key_pitch, key_mode = excluded.key_mode,
                 key_strength = excluded.key_strength,
                 integrated_lufs = excluded.integrated_lufs,
                 true_peak_dbtp = excluded.true_peak_dbtp,
                 loudness_range_lu = excluded.loudness_range_lu,
                 replay_gain_db = excluded.replay_gain_db,
                 silence_lead_ms = excluded.silence_lead_ms,
                 silence_tail_ms = excluded.silence_tail_ms, waveform = excluded.waveform,
                 fingerprint = excluded.fingerprint, fingerprint_sec = excluded.fingerprint_sec",
        )?
        .execute(params![
            work.track_id,
            version,
            work.size_bytes,
            work.modified_ms,
            now_ms,
            error,
            values.bpm,
            values.bpm_confidence,
            json(&values.beats),
            values.downbeat_meter,
            downbeats,
            values.key.map(|k| k.pitch),
            values.key.map(|k| u8::from(k.minor)),
            values.key_strength,
            values.integrated_lufs,
            values.true_peak_dbtp,
            values.loudness_range_lu,
            values.replay_gain_db,
            values.silence_lead_ms,
            values.silence_tail_ms,
            values.waveform,
            values.fingerprint.as_ref().map(|f| f.value.as_str()),
            values.fingerprint.as_ref().map(|f| f.duration_sec),
        ])?;
        Ok(())
    })?;
    Ok(())
}

/// The stored analysis of a local track, if its file has not changed since.
/// Failed runs give `Err` with their message.
pub fn of_track(
    conn: &Connection,
    track_id: i64,
) -> Result<Option<Result<Analysis, String>>, AnalysisError> {
    use rusqlite::OptionalExtension as _;
    let row = conn
        .prepare_cached(
            "SELECT a.error, a.bpm, a.bpm_confidence, a.beats, a.downbeat_meter, a.downbeats,
                 a.key_pitch, a.key_mode, a.key_strength, a.integrated_lufs, a.true_peak_dbtp,
                 a.loudness_range_lu, a.replay_gain_db, a.silence_lead_ms, a.silence_tail_ms,
                 a.waveform, a.fingerprint, a.fingerprint_sec
             FROM local_analysis a JOIN local_track t ON t.id = a.track_id
             WHERE a.track_id = ?1
               AND a.size_bytes = t.size_bytes AND a.modified_ms = t.modified_ms",
        )?
        .query_row([track_id], |r| {
            if let Some(error) = r.get::<_, Option<String>>(0)? {
                return Ok(Err(error));
            }
            let key = match (r.get::<_, Option<u8>>(6)?, r.get::<_, Option<u8>>(7)?) {
                (Some(pitch), Some(mode)) => Some(Key {
                    pitch,
                    minor: mode == 1,
                }),
                _ => None,
            };
            let fingerprint = match (r.get::<_, Option<String>>(16)?, r.get(17)?) {
                (Some(value), Some(duration_sec)) => Some(Fingerprint {
                    value,
                    duration_sec,
                }),
                _ => None,
            };
            Ok(Ok(Analysis {
                bpm: r.get(1)?,
                bpm_confidence: r.get(2)?,
                beats: json_list(r.get(3)?),
                downbeat_meter: r.get(4)?,
                downbeats: json_list(r.get(5)?),
                key,
                key_strength: r.get(8)?,
                integrated_lufs: r.get(9)?,
                true_peak_dbtp: r.get(10)?,
                loudness_range_lu: r.get(11)?,
                replay_gain_db: r.get(12)?,
                silence_lead_ms: r.get(13)?,
                silence_tail_ms: r.get(14)?,
                waveform: r.get(15)?,
                fingerprint,
            }))
        })
        .optional()?;
    Ok(row)
}

/// A list stored as JSON; `None` when there is none or it does not read.
fn json_list<T: serde::de::DeserializeOwned>(text: Option<String>) -> Option<Vec<T>> {
    text.and_then(|t| serde_json::from_str(&t).ok())
}

/// How far a run is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Files this run will analyse.
    pub total: usize,
    /// Files finished, failed ones included.
    pub done: usize,
    /// Files whose analysis failed.
    pub failed: usize,
    /// The file name of the last finished file.
    pub last: Option<String>,
    /// The error of the last failed file.
    pub last_error: Option<String>,
}

/// Analyses the local tracks that `selection` picks, `parallel` at a time,
/// and stores each result as soon as it is there. `progress` is called after
/// every file. Drop the future to stop: running folksentia processes are
/// killed. Another output schema stops the run with [`AnalysisError::Schema`].
pub async fn analyse_library(
    db: Arc<Database>,
    tool: Folksentia,
    selection: Selection,
    parallel: usize,
    progress: impl Fn(&Progress) + Send + Sync + 'static,
) -> Result<Progress, AnalysisError> {
    let version = tool.version().to_owned();
    let work = {
        let db = Arc::clone(&db);
        let version = version.clone();
        tokio::task::spawn_blocking(move || work(&db.reader()?, &version, selection))
            .await
            .map_err(|e| AnalysisError::Stopped(e.to_string()))??
    };
    let mut state = Progress {
        total: work.len(),
        ..Progress::default()
    };
    progress(&state);
    tracing::info!(files = work.len(), %version, "local analysis started");

    let tool = Arc::new(tool);
    let options = Arc::new(library_options(&tool));
    let mut queue = work.into_iter();
    let mut running = JoinSet::new();
    loop {
        while running.len() < parallel.max(1) {
            let Some(item) = queue.next() else { break };
            let tool = Arc::clone(&tool);
            let options = Arc::clone(&options);
            running.spawn(async move {
                let result = tool.analyse(&item.path, &options, TRACK_TIMEOUT).await;
                (item, result)
            });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let (item, result) = joined.map_err(|e| AnalysisError::Stopped(e.to_string()))?;
        let result = match result {
            Ok(object) => match output::parse(&object)? {
                Outcome::Analysed(values) => Ok(*values),
                Outcome::Failed(message) => Err(message),
            },
            Err(message) => Err(message),
        };
        state.done += 1;
        state.last = item
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        if let Err(message) = &result {
            state.failed += 1;
            state.last_error = Some(message.clone());
            tracing::debug!(error = %message, "a local file could not be analysed");
        }
        let db = Arc::clone(&db);
        let version = version.clone();
        tokio::task::spawn_blocking(move || {
            store(&db, &item, &version, crate::sync::now_ms(), &result)
        })
        .await
        .map_err(|e| AnalysisError::Stopped(e.to_string()))??;
        progress(&state);
    }
    tracing::info!(
        done = state.done,
        failed = state.failed,
        "local analysis finished"
    );
    Ok(state)
}

#[cfg(test)]
mod tests;
