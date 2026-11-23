use std::fs;
use std::os::unix::fs::symlink;

use serde::Deserialize;

use super::*;
use crate::analysis::tests::{ended, fake};

/// The size and folksonic's modification time of a file.
fn stamp(path: &Path) -> (i64, i64) {
    let meta = fs::metadata(path).unwrap();
    (i64::try_from(meta.len()).unwrap(), mtime_ms(&meta))
}

/// A job as folksonic exports it.
fn job(id: &str, path: &str, (size, mtime): (i64, i64)) -> Value {
    json!({
        "id": id, "path": path, "size": size, "mtimeMs": mtime, "durationMs": 113000,
        "features": ["bpm", "beats", "key", "loudness", "silence", "waveform", "fingerprint"],
    })
}

fn list(jobs: Vec<Value>) -> Value {
    json!({
        "format": "tinysonic-analysis-jobs", "version": 1,
        "created": "2026-11-13T10:00:00Z",
        "tool": "folksentia", "schema": 1,
        "options": {"downbeats": true, "waveform": 150, "fingerprint": true, "onsets": true},
        "jobs": jobs,
    })
}

fn parsed(doc: &Value) -> Result<JobList, AnalysisError> {
    parse(&serde_json::to_vec(doc).unwrap())
}

fn lines(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_job_list_is_read_with_its_options() {
    let jobs = parsed(&list(vec![
        job("so-1", "A/one.flac", (1, 2)),
        job("so-2", "A/two.flac", (3, 4)),
        job("so-1", "A/again.flac", (5, 6)),
    ]))
    .unwrap();
    assert_eq!(jobs.created.as_deref(), Some("2026-11-13T10:00:00Z"));
    assert_eq!(
        jobs.options,
        ["--downbeats", "--waveform=150", "--fingerprint", "--onsets"]
    );
    let paths: Vec<&str> = jobs.jobs.iter().map(|j| j.path.as_str()).collect();
    assert_eq!(
        paths,
        ["A/one.flac", "A/two.flac"],
        "the first job of a song wins"
    );

    let mut plain = list(vec![]);
    plain["options"] = json!({"waveform": 0});
    assert!(parsed(&plain).unwrap().options.is_empty());
    plain.as_object_mut().unwrap().remove("options");
    assert!(parsed(&plain).unwrap().options.is_empty());
}

#[test]
fn other_files_are_refused() {
    let change = |f: &dyn Fn(&mut Value)| {
        let mut doc = list(vec![job("so-1", "a.flac", (1, 2))]);
        f(&mut doc);
        parsed(&doc)
    };
    for (what, result) in [
        ("format", change(&|d| d["format"] = json!("other"))),
        ("version", change(&|d| d["version"] = json!(2))),
        ("tool", change(&|d| d["tool"] = json!("essentia"))),
        ("schema", change(&|d| d["schema"] = json!(2))),
        (
            "no id",
            change(&|d| d["jobs"][0].as_object_mut().unwrap().clear()),
        ),
        ("not an object", change(&|d| d["jobs"][0] = json!("a.flac"))),
        (
            "waveform",
            change(&|d| d["options"]["waveform"] = json!(100_000)),
        ),
        (
            "waveform type",
            change(&|d| d["options"]["waveform"] = json!("150")),
        ),
    ] {
        assert!(
            matches!(result, Err(AnalysisError::Jobs(_))),
            "{what}: {result:?}"
        );
    }
    assert!(matches!(parse(b"{not json"), Err(AnalysisError::Jobs(_))));
}

#[test]
fn done_jobs_are_found_and_a_half_line_is_cut() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("results.jsonl");
    assert!(done_ids(&path).unwrap().is_empty(), "no file yet");
    let whole = "{\"job\":{\"id\":\"so-1\"},\"skipped\":\"missing\"}\nnot json\n\
                 {\"job\":{\"id\":\"so-2\"},\"skipped\":\"missing\"}\n";
    fs::write(&path, format!("{whole}{{\"job\":{{\"id\":\"so-3\"")).unwrap();
    let ids = done_ids(&path).unwrap();
    assert_eq!(ids, HashSet::from(["so-1".to_owned(), "so-2".to_owned()]));
    assert_eq!(fs::read_to_string(&path).unwrap(), whole);
}

/// A music folder and a job list for it: analysed, failed, crashed,
/// missing, changed, outside the folder.
fn setup() -> (tempfile::TempDir, PathBuf, JobList, Vec<Value>) {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("Musik");
    fs::create_dir_all(music.join("A")).unwrap();
    for name in [
        "A/good one.flac",
        "A/broken.flac",
        "A/crash.flac",
        "A/changed.flac",
    ] {
        fs::write(music.join(name), b"audio").unwrap();
    }
    fs::write(dir.path().join("secret good.flac"), b"audio").unwrap();
    symlink(
        dir.path().join("secret good.flac"),
        music.join("A/link good.flac"),
    )
    .unwrap();
    let (size, mtime) = stamp(&music.join("A/changed.flac"));
    let jobs = vec![
        job(
            "so-1",
            "A/good one.flac",
            stamp(&music.join("A/good one.flac")),
        ),
        job("so-2", "A/broken.flac", stamp(&music.join("A/broken.flac"))),
        job("so-3", "A/crash.flac", stamp(&music.join("A/crash.flac"))),
        job("so-4", "A/gone.flac", (5, 1)),
        job("so-5", "A/changed.flac", (size, mtime - 1000)),
        job("so-6", "../secret good.flac", (5, mtime)),
        job("so-7", "/etc/passwd", (5, mtime)),
        job("so-8", "A/link good.flac", (5, mtime)),
    ];
    let parsed = parsed(&list(jobs.clone())).unwrap();
    (dir, music, parsed, jobs)
}

fn job_run(dir: &Path, music: &Path) -> JobRun {
    JobRun {
        music_root: music.to_owned(),
        results: dir.join("results.jsonl"),
        size_only: false,
        parallel: 3,
    }
}

#[tokio::test]
async fn every_job_gets_one_line_in_folksonics_format() {
    let (dir, music, jobs, raw) = setup();
    let run = job_run(dir.path(), &music);
    let progress = super::run(fake().await, jobs, run.clone(), |_| {})
        .await
        .unwrap();
    assert_eq!(
        (progress.total, progress.done, progress.failed),
        (8, 8, 2),
        "{progress:?}"
    );
    assert_eq!((progress.missing, progress.changed), (4, 1));

    let written = lines(&run.results);
    assert_eq!(written.len(), 8);
    let by_id = |id: &str| {
        written
            .iter()
            .find(|l| l["job"]["id"] == id)
            .unwrap_or_else(|| panic!("{id}"))
            .clone()
    };
    for (i, job) in raw.iter().enumerate() {
        assert_eq!(
            &by_id(&format!("so-{}", i + 1))["job"],
            job,
            "the job unchanged"
        );
    }
    let good = by_id("so-1");
    assert_eq!(good["toolVersion"], "9.9.9+gtest");
    assert_eq!(good["result"]["bpm"], 120.5);
    assert_eq!(good["result"]["file"], "A/good one.flac", "the job's path");
    let (size, mtime) = stamp(&music.join("A/good one.flac"));
    assert_eq!(good["local"], json!({"size": size, "mtimeMs": mtime}));
    assert!(good.get("skipped").is_none());
    let args = fs::read_to_string(music.join("A/good one.flac.args")).unwrap();
    assert!(
        args.starts_with("--downbeats --waveform=150 --fingerprint --onsets -- /"),
        "{args}"
    );

    assert_eq!(by_id("so-2")["result"]["error"], "no audio decoded from x");
    assert_eq!(by_id("so-2")["result"]["file"], "A/broken.flac");
    let crash = &by_id("so-3")["result"];
    assert_eq!(crash["schema"], 2);
    assert_eq!(crash["file"], "A/crash.flac");
    assert!(crash["error"].as_str().unwrap().contains("signal 9"));
    for id in ["so-4", "so-6", "so-7", "so-8"] {
        let line = by_id(id);
        assert_eq!(line["skipped"], "missing", "{id}");
        assert!(line.get("result").is_none() && line.get("local").is_none());
    }
    assert!(
        !dir.path().join("secret good.flac.args").exists(),
        "nothing outside the music folder was opened"
    );
    let changed = by_id("so-5");
    assert_eq!(changed["skipped"], "changed");
    assert!(changed.get("local").is_some());

    // folksonic's import accepts every line.
    let report = import_check(&run.results);
    assert_eq!(report.stored, 3);
    assert_eq!(report.failed, 2);
    assert_eq!(report.skipped_missing, 4);
    assert_eq!(report.skipped_changed, 1);
    assert_eq!(report.invalid, Vec::<String>::new());

    // Running again does nothing: every job has its line.
    let again = super::run(
        fake().await,
        parsed(&list(raw)).unwrap(),
        run.clone(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!((again.already, again.total), (8, 0));
    assert_eq!(lines(&run.results).len(), 8);
}

#[tokio::test]
async fn size_only_accepts_a_copy_with_other_times() {
    let (dir, music, _, _) = setup();
    let (size, mtime) = stamp(&music.join("A/changed.flac"));
    let jobs = parsed(&list(vec![job(
        "so-5",
        "A/changed.flac",
        (size, mtime - 1000),
    )]))
    .unwrap();
    let run = JobRun {
        size_only: true,
        ..job_run(dir.path(), &music)
    };
    let progress = super::run(fake().await, jobs, run.clone(), |_| {})
        .await
        .unwrap();
    assert_eq!((progress.changed, progress.done), (0, 1));
    assert!(lines(&run.results)[0].get("result").is_some());
}

#[tokio::test]
async fn a_stopped_run_resumes_where_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("Musik");
    fs::create_dir_all(&music).unwrap();
    for name in ["a good.flac", "b slow.flac", "c good.flac"] {
        fs::write(music.join(name), b"audio").unwrap();
    }
    let raw: Vec<Value> = ["a good.flac", "b slow.flac", "c good.flac"]
        .iter()
        .enumerate()
        .map(|(i, name)| job(&format!("so-{i}"), name, stamp(&music.join(name))))
        .collect();
    let run = JobRun {
        parallel: 1,
        ..job_run(dir.path(), &music)
    };
    let first = super::run(
        fake().await,
        parsed(&list(raw.clone())).unwrap(),
        run.clone(),
        |_| {},
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .is_err()
    );
    let pid = fs::read_to_string(music.join("b slow.flac.pid")).unwrap();
    assert!(ended(&pid).await, "the slow folksentia was killed");
    assert_eq!(lines(&run.results).len(), 1, "whole lines only");

    // A stop in the middle of writing leaves part of a line; it is cut.
    let mut text = fs::read_to_string(&run.results).unwrap();
    text.push_str("{\"job\":{\"id\":\"so-1\",");
    fs::write(&run.results, text).unwrap();
    fs::remove_file(music.join("b slow.flac")).unwrap();
    let second = super::run(
        fake().await,
        parsed(&list(raw)).unwrap(),
        run.clone(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!((second.already, second.total, second.missing), (1, 2, 1));
    let ids: Vec<String> = lines(&run.results)
        .iter()
        .map(|l| l["job"]["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["so-0", "so-1", "so-2"]);
}

#[tokio::test]
async fn output_folksonic_cannot_import_stops_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("Musik");
    fs::create_dir_all(&music).unwrap();
    for name in ["a old.flac", "b noise.flac", "c future.flac"] {
        fs::write(music.join(name), b"audio").unwrap();
    }
    // Schema 1 is still imported, garbage output becomes a failed run, and
    // a schema folksonic does not read stops the run before its line.
    let raw: Vec<Value> = ["a old.flac", "b noise.flac", "c future.flac"]
        .iter()
        .enumerate()
        .map(|(i, name)| job(&format!("so-{i}"), name, stamp(&music.join(name))))
        .collect();
    let run = JobRun {
        parallel: 1,
        ..job_run(dir.path(), &music)
    };
    let result = super::run(
        fake().await,
        parsed(&list(raw)).unwrap(),
        run.clone(),
        |_| {},
    )
    .await;
    assert!(
        matches!(result, Err(AnalysisError::Schema(Some(3)))),
        "{result:?}"
    );
    let report = import_check(&run.results);
    assert_eq!((report.stored, report.failed), (2, 1));
    assert!(report.invalid.is_empty(), "{:?}", report.invalid);
}

/// What folksonic's `tinysonic analysis import` makes of a result file
/// (folksonic `internal/analysis/offline.go`, `ImportResults`), for files
/// that are unchanged on the server: the line types it decodes into, the
/// skip reasons it knows, and the output schemas it reads.
#[derive(Debug, Default)]
struct ImportReport {
    stored: usize,
    failed: usize,
    skipped_missing: usize,
    skipped_changed: usize,
    invalid: Vec<String>,
}

/// folksonic's `resultLine`, with Go's types: a number where Go expects a
/// string (or the reverse) fails the line.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GoLine {
    job: Option<GoJob>,
    local: Option<GoStamp>,
    #[serde(default)]
    tool_version: String,
    result: Option<Value>,
    #[serde(default)]
    skipped: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GoJob {
    #[serde(default)]
    id: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    mtime_ms: i64,
    #[serde(default)]
    duration_ms: i64,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GoStamp {
    size: i64,
    mtime_ms: i64,
}

fn import_check(results: &Path) -> ImportReport {
    let mut report = ImportReport::default();
    for (n, line) in fs::read_to_string(results).unwrap().lines().enumerate() {
        let line: GoLine = match serde_json::from_str(line) {
            Ok(line) => line,
            Err(error) => {
                report.invalid.push(format!("line {}: {error}", n + 1));
                continue;
            }
        };
        let Some(job) = line.job.filter(|j| !j.id.is_empty()) else {
            report.invalid.push(format!("line {}: no job", n + 1));
            continue;
        };
        match line.skipped.as_str() {
            "" => {}
            "missing" => {
                report.skipped_missing += 1;
                continue;
            }
            "changed" => {
                report.skipped_changed += 1;
                continue;
            }
            other => {
                report
                    .invalid
                    .push(format!("line {}: skipped {other:?}", n + 1));
                continue;
            }
        }
        let Some(result) = line.result.filter(|r| !r.is_null()) else {
            report.invalid.push(format!("line {}: no result", n + 1));
            continue;
        };
        let schema = result.get("schema").and_then(Value::as_u64);
        if !schema.is_some_and(|s| IMPORT_SCHEMAS.contains(&s)) {
            report
                .invalid
                .push(format!("line {}: schema {schema:?}", n + 1));
            continue;
        }
        assert!(!job.features.is_empty() && !job.path.is_empty());
        report.stored += 1;
        if result.get("error").is_some() {
            report.failed += 1;
        }
    }
    report
}
