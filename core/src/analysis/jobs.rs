//! Offline analysis for folksonic (#12): a job list from `tinysonic analysis
//! export` in, a result file for `tinysonic analysis import` out.
//!
//! This does what folksentia's `folksentia-jobs` does (folksentia's README,
//! "Offline analysis for folksonic"; folksonic's README, "Offline
//! analysis"): every job runs folksentia on the local copy of the music
//! folder with the flags in the list's `options`, and gets one JSON line:
//!
//! ```text
//! {"job": …the job, unchanged…, "local": {"size", "mtimeMs"}, "toolVersion": "…", "result": {…}}
//! {"job": …, "skipped": "missing"}
//! {"job": …, "local": …, "skipped": "changed"}
//! ```
//!
//! The result file is appended to, one complete line at a time, so a run can
//! stop at any point and resume: jobs already in the file are skipped, and a
//! half-written last line is cut off first. (It is the one file Studio does
//! not replace atomically; the format is built for appending.)
//!
//! A job path is relative and `/`-separated. An absolute path, a `..`, or a
//! path that leads out of the music folder through a link counts as missing.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

use super::{AnalysisError, Folksentia, TRACK_TIMEOUT, output};

/// The job list format this reads.
pub const JOBS_FORMAT: &str = "tinysonic-analysis-jobs";
pub const JOBS_VERSION: u64 = 1;
/// The job list's own schema field, as folksonic exports it.
pub const JOBS_SCHEMA: u64 = 1;

/// The output schemas folksonic's import reads.
const IMPORT_SCHEMAS: [u64; 2] = [1, 2];

/// The largest job list read: a library of 100 000 tracks takes about 30 MB.
const MAX_JOBS_FILE: u64 = 256 << 20;

/// The longest result line read back when resuming; folksonic's import has
/// the same limit.
const MAX_LINE: usize = 64 << 20;

/// Most waveform bins a job list may ask for.
const MAX_WAVEFORM_BINS: u64 = 4096;

/// One track of a job list.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// folksonic's song id.
    pub id: String,
    /// Relative to the music folder, `/`-separated.
    pub path: String,
    size: Option<i64>,
    mtime_ms: Option<i64>,
    /// The job as it was in the list; it goes back unchanged.
    raw: Map<String, Value>,
}

/// A job list, read and checked.
#[derive(Clone, Debug, PartialEq)]
pub struct JobList {
    /// When folksonic exported it, as it says.
    pub created: Option<String>,
    /// The folksentia flags for every job.
    pub options: Vec<String>,
    /// Each song once; the first job of a song wins.
    pub jobs: Vec<Job>,
}

/// Reads a job list. A file of another format or version, or a job without
/// an id or path, refuses the whole list.
pub fn load(path: &Path) -> Result<JobList, AnalysisError> {
    let file = std::fs::File::open(path).map_err(|source| AnalysisError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut bytes = Vec::new();
    file.take(MAX_JOBS_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| AnalysisError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_JOBS_FILE {
        return Err(AnalysisError::Jobs(format!(
            "the file is larger than {} MB",
            MAX_JOBS_FILE >> 20
        )));
    }
    parse(&bytes)
}

/// Reads a job list from its bytes.
pub fn parse(bytes: &[u8]) -> Result<JobList, AnalysisError> {
    let refuse = |message: String| AnalysisError::Jobs(message);
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| refuse(format!("not JSON: {e}")))?;
    let field = |name: &str| doc.get(name);
    if field("format").and_then(Value::as_str) != Some(JOBS_FORMAT)
        || field("version").and_then(Value::as_u64) != Some(JOBS_VERSION)
    {
        return Err(refuse(format!(
            "not a {JOBS_FORMAT} version {JOBS_VERSION} file; export it with `tinysonic analysis export`"
        )));
    }
    if field("tool").and_then(Value::as_str) != Some("folksentia")
        || field("schema").and_then(Value::as_u64) != Some(JOBS_SCHEMA)
    {
        return Err(refuse(format!(
            "jobs for {} schema {}, expected folksentia schema {JOBS_SCHEMA}",
            field("tool").map_or_else(|| "no tool".to_owned(), Value::to_string),
            field("schema").map_or_else(|| "none".to_owned(), Value::to_string),
        )));
    }
    let options = folksentia_options(field("options"))?;
    let mut seen = HashSet::new();
    let mut jobs = Vec::new();
    for job in field("jobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(raw) = job.as_object() else {
            return Err(refuse("a job is not an object".to_owned()));
        };
        let (Some(id), Some(path)) = (
            raw.get("id").and_then(Value::as_str),
            raw.get("path").and_then(Value::as_str),
        ) else {
            return Err(refuse("a job has no id or no path".to_owned()));
        };
        if seen.insert(id.to_owned()) {
            jobs.push(Job {
                id: id.to_owned(),
                path: path.to_owned(),
                size: raw.get("size").and_then(Value::as_i64),
                mtime_ms: raw.get("mtimeMs").and_then(Value::as_i64),
                raw: raw.clone(),
            });
        }
    }
    Ok(JobList {
        created: field("created")
            .and_then(Value::as_str)
            .map(|c| c.chars().take(64).collect()),
        options,
        jobs,
    })
}

/// The flags folksonic passes for a local run, from the list's `options`.
fn folksentia_options(options: Option<&Value>) -> Result<Vec<String>, AnalysisError> {
    let flag = |name: &str| {
        options
            .and_then(|o| o.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let mut args = Vec::new();
    if flag("downbeats") {
        args.push("--downbeats".to_owned());
    }
    match options.and_then(|o| o.get("waveform")) {
        None | Some(Value::Null) => {}
        Some(bins) => match bins.as_u64() {
            Some(0) => {}
            Some(bins @ 1..=MAX_WAVEFORM_BINS) => args.push(format!("--waveform={bins}")),
            _ => {
                return Err(AnalysisError::Jobs(format!(
                    "the waveform bins must be 1 to {MAX_WAVEFORM_BINS}"
                )));
            }
        },
    }
    if flag("fingerprint") {
        args.push("--fingerprint".to_owned());
    }
    if flag("onsets") {
        args.push("--onsets".to_owned());
    }
    Ok(args)
}

/// The ids of the jobs already in a result file. A half-written last line
/// (from a run that was stopped while writing) is cut off, so new lines
/// start cleanly. Lines that do not read are left for the import to report.
pub fn done_ids(results: &Path) -> Result<HashSet<String>, AnalysisError> {
    let io = |source| AnalysisError::Io {
        path: results.to_owned(),
        source,
    };
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(results)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(io(error)),
    };
    let length = file.metadata().map_err(io)?.len();
    let mut reader = BufReader::new(&file);
    let mut ids = HashSet::new();
    let mut complete: u64 = 0;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 1)
            .read_until(b'\n', &mut line)
            .map_err(io)?;
        if read == 0 || line.last() != Some(&b'\n') {
            break;
        }
        complete += read as u64;
        if let Ok(value) = serde_json::from_slice::<Value>(&line)
            && let Some(id) = value
                .get("job")
                .and_then(|j| j.get("id"))
                .and_then(Value::as_str)
        {
            ids.insert(id.to_owned());
        }
    }
    if complete < length {
        file.set_len(complete).map_err(io)?;
        tracing::info!("a half-written last line was removed from the result file");
    }
    Ok(ids)
}

/// How a job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Analysed,
    Failed,
    Missing,
    Changed,
}

/// How far a job run is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobProgress {
    /// Jobs in the list.
    pub jobs: usize,
    /// Jobs already in the result file when the run started.
    pub already: usize,
    /// Jobs this run will do.
    pub total: usize,
    /// Jobs this run finished, whatever the outcome.
    pub done: usize,
    /// Jobs where folksentia failed; folksonic records them as failed runs.
    pub failed: usize,
    /// Jobs whose file is not in the music folder.
    pub missing: usize,
    /// Jobs whose file differs in size or modification time.
    pub changed: usize,
    /// The path of the last finished job.
    pub last: Option<String>,
    /// The error of the last failed job.
    pub last_error: Option<String>,
}

/// A job run: which list, against which folder, into which file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRun {
    pub music_root: PathBuf,
    pub results: PathBuf,
    /// Compare file sizes only, for copies that did not keep modification
    /// times (import with `-size-only` then).
    pub size_only: bool,
    /// folksentia processes at a time.
    pub parallel: usize,
}

/// Runs the jobs of `list` that are not yet in the result file and appends
/// a line for each. `progress` is called after every job. Drop the future to
/// stop; running folksentia processes are killed and the file stays usable.
/// Output that folksonic cannot import (another schema) stops the run.
pub async fn run(
    tool: Folksentia,
    list: JobList,
    run: JobRun,
    progress: impl Fn(&JobProgress) + Send + Sync + 'static,
) -> Result<JobProgress, AnalysisError> {
    let root = tokio::fs::canonicalize(&run.music_root)
        .await
        .map_err(|source| AnalysisError::Io {
            path: run.music_root.clone(),
            source,
        })?;
    if !root.is_dir() {
        return Err(AnalysisError::Jobs(format!(
            "{} is not a folder",
            run.music_root.display()
        )));
    }
    let done = {
        let results = run.results.clone();
        tokio::task::spawn_blocking(move || done_ids(&results))
            .await
            .map_err(|e| AnalysisError::Stopped(e.to_string()))??
    };
    let todo: Vec<Job> = list
        .jobs
        .iter()
        .filter(|job| !done.contains(&job.id))
        .cloned()
        .collect();
    let mut state = JobProgress {
        jobs: list.jobs.len(),
        already: list.jobs.len() - todo.len(),
        total: todo.len(),
        ..JobProgress::default()
    };
    progress(&state);
    tracing::info!(
        jobs = state.jobs,
        already = state.already,
        version = tool.version(),
        "offline analysis started"
    );

    let mut out = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&run.results)
        .await
        .map_err(|source| AnalysisError::Io {
            path: run.results.clone(),
            source,
        })?;
    let context = Arc::new(Context {
        tool,
        root,
        options: list.options,
        size_only: run.size_only,
        timeout: TRACK_TIMEOUT,
    });
    let mut queue = todo.into_iter();
    let mut running = JoinSet::new();
    loop {
        while running.len() < run.parallel.max(1) {
            let Some(job) = queue.next() else { break };
            let context = Arc::clone(&context);
            running.spawn(async move {
                let path = job.path.clone();
                let (line, status) = context.run_job(job).await;
                (path, line, status)
            });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let (path, line, status) = joined.map_err(|e| AnalysisError::Stopped(e.to_string()))?;
        if status == Status::Analysed || status == Status::Failed {
            let schema = line
                .get("result")
                .and_then(|r| r.get("schema"))
                .and_then(Value::as_u64);
            if !schema.is_some_and(|s| IMPORT_SCHEMAS.contains(&s)) {
                return Err(AnalysisError::Schema(schema));
            }
        }
        let mut text = serde_json::to_vec(&line)
            .map_err(|e| AnalysisError::Stopped(format!("a result could not be written: {e}")))?;
        text.push(b'\n');
        // One complete line at a time, so a stop leaves whole lines.
        let written = match out.write_all(&text).await {
            Ok(()) => out.flush().await,
            Err(error) => Err(error),
        };
        written.map_err(|source| AnalysisError::Io {
            path: run.results.clone(),
            source,
        })?;
        state.done += 1;
        match status {
            Status::Analysed => {}
            Status::Failed => {
                state.failed += 1;
                state.last_error = line
                    .get("result")
                    .and_then(|r| r.get("error"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Status::Missing => state.missing += 1,
            Status::Changed => state.changed += 1,
        }
        state.last = Some(path);
        progress(&state);
    }
    out.sync_all().await.map_err(|source| AnalysisError::Io {
        path: run.results.clone(),
        source,
    })?;
    tracing::info!(
        done = state.done,
        failed = state.failed,
        missing = state.missing,
        changed = state.changed,
        "offline analysis finished"
    );
    Ok(state)
}

/// What every job of a run shares.
struct Context {
    tool: Folksentia,
    /// The music folder, resolved.
    root: PathBuf,
    options: Vec<String>,
    size_only: bool,
    timeout: Duration,
}

impl Context {
    /// The result line for one job.
    async fn run_job(&self, job: Job) -> (Value, Status) {
        let Some(path) = self.file_of(&job.path).await else {
            return (
                json!({"job": job.raw, "skipped": "missing"}),
                Status::Missing,
            );
        };
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            return (
                json!({"job": job.raw, "skipped": "missing"}),
                Status::Missing,
            );
        };
        let size = i64::try_from(meta.len()).unwrap_or(i64::MAX);
        let mtime_ms = mtime_ms(&meta);
        let local = json!({"size": size, "mtimeMs": mtime_ms});
        if job.size != Some(size) || (!self.size_only && job.mtime_ms != Some(mtime_ms)) {
            return (
                json!({"job": job.raw, "local": local, "skipped": "changed"}),
                Status::Changed,
            );
        }
        let error = |message: &str| json!({"schema": output::SCHEMA, "file": job.path, "error": output::short(message)});
        let (result, status) = match self.tool.analyse(&path, &self.options, self.timeout).await {
            Ok(mut object) => {
                // The job's relative path, never this computer's.
                object.insert("file".to_owned(), Value::String(job.path.clone()));
                object.remove("file_base64");
                let status = if object.contains_key("error") {
                    Status::Failed
                } else {
                    Status::Analysed
                };
                (Value::Object(object), status)
            }
            Err(message) => (error(&message), Status::Failed),
        };
        (
            json!({
                "job": job.raw,
                "local": local,
                "toolVersion": self.tool.version(),
                "result": result,
            }),
            status,
        )
    }

    /// The file of a job path inside the music folder: relative, no `..`,
    /// no backslash, and not leading out of the folder through a link.
    async fn file_of(&self, relative: &str) -> Option<PathBuf> {
        let path = Path::new(relative);
        let plain = !relative.is_empty()
            && !relative.contains('\\')
            && path
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if !plain {
            return None;
        }
        let resolved = tokio::fs::canonicalize(self.root.join(path)).await.ok()?;
        (resolved.starts_with(&self.root) && resolved.is_file()).then_some(resolved)
    }
}

/// The modification time in milliseconds, truncated as Go's `UnixMilli`
/// does, which folksonic compares against.
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    match meta.modified() {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
            // Before 1970: rounded down, as Go does.
            Err(before) => {
                -i64::try_from(before.duration().as_nanos().div_ceil(1_000_000)).unwrap_or(i64::MAX)
            }
        },
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests;
