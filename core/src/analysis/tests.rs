use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Mutex, OnceLock};

use super::*;
use crate::db::tests::temp_db;
use crate::local_library::{LocalFilter, local_only_songs};

/// A full folksentia result for a file whose name contains `good`.
const GOOD: &str = r#"{"schema":2,"file":"x","duration":3.0,"bpm":120.5,"bpm_confidence":3.2,
"key_pitch":9,"key_mode":1,"key_strength":0.9,"tuning_frequency":440.0,"replay_gain_db":-3.5,
"silence":{"thresholdDb":-100,"leadMs":12,"tailMs":40},
"loudness":{"integratedLufs":-14.2,"truePeakDbtp":-1.0,"lraLu":8.5,"plrDb":13.2,"noiseFloorLufs":-60},
"waveform":{"bins":3,"peaks":[0,128,255]},"beats":[0.5,1.0,1.5,2.0],"downbeatMeter":2,
"downbeatIndexes":[0,2],"fingerprint":{"algorithm":"chromaprint","version":2,"durationSec":3.0,
"value":"AQAAEkmUaEmSJMkS"},"rhythm":null,"somethingNew":{"a":1}}"#;

/// A fake folksentia: its answer depends on the file name. It records its
/// arguments next to the file, and the pid of a slow run.
fn fake_script() -> String {
    format!(
        r#"#!/bin/sh
for last; do :; done
case "$1" in
  --version) printf 'folksentia 9.9.9+gtest\nbuilt 2026-11-13\nEssentia 2.1\nChromaprint yes\n'; exit 0 ;;
esac
echo "$@" > "$last.args"
case "$last" in
  *good*) printf '%s' '{}' ;;
  *broken*) printf '{{"schema":2,"file":"x","error":"no audio decoded from x"}}'; exit 1 ;;
  *crash*) kill -9 $$ ;;
  *slow*) echo $$ > "$last.pid"; exec /bin/sleep 30 ;;
  *old*) printf '{{"schema":1,"file":"x","bpm":100}}' ;;
  *noise*) echo 'not json'; echo 'Essentia exploded' >&2; exit 3 ;;
  *flood*) /usr/bin/head -c 70000000 /dev/zero ;;
esac
"#,
        GOOD.replace('\n', "")
    )
}

/// The fake program, written once for all tests: a file being written while
/// another test forks cannot then make exec fail with "text file busy".
fn fake_program() -> &'static Path {
    static PROGRAM: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &PROGRAM
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("folksentia");
            fs::write(&path, fake_script()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            (dir, path)
        })
        .1
}

/// The fake folksentia, retried while a fork elsewhere still holds the file.
async fn fake() -> Folksentia {
    let program = fake_program().to_owned();
    for _ in 0..50 {
        match Folksentia::find(Some(&program)).await {
            Ok(tool) => return tool,
            Err(AnalysisError::Run { source, .. }) if source.raw_os_error() == Some(26) => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("{error}"),
        }
    }
    panic!("the fake folksentia stayed busy");
}

/// A music folder with the given files, recorded as local tracks.
fn library(names: &[&str]) -> (tempfile::TempDir, Arc<Database>, PathBuf) {
    let (dir, db) = temp_db();
    let music = dir.path().join("music");
    fs::create_dir_all(&music).unwrap();
    let folder = crate::local_library::add_folder(&db, &music, 1).unwrap();
    db.write(|tx| {
        for (i, name) in names.iter().enumerate() {
            fs::write(folder.path.join(name), b"audio").unwrap();
            tx.execute(
                "INSERT INTO local_track (id, folder_id, rel_path, size_bytes, modified_ms,
                     fingerprint, title, bpm, scanned_at)
                 VALUES (?1, ?2, ?3, 5, 100, printf('%064d', ?1), ?3, 90.0, 1)",
                params![i64::try_from(i).unwrap() + 1, folder.id, name],
            )?;
        }
        Ok(())
    })
    .unwrap();
    (dir, Arc::new(db), folder.path)
}

/// Whether a process still runs; a zombie waiting to be reaped does not.
fn alive(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{}/stat", pid.trim())).is_ok_and(|stat| {
        stat.rsplit(')')
            .next()
            .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
    })
}

/// Waits up to a second for a process to end.
async fn ended(pid: &str) -> bool {
    for _ in 0..50 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn work_for(path: &Path) -> Work {
    Work {
        track_id: 1,
        path: path.to_owned(),
        size_bytes: 5,
        modified_ms: 100,
    }
}

#[test]
fn the_version_line_is_read_strictly() {
    assert_eq!(
        folksentia::parse_version("folksentia 0.5.0+g125f36d\nbuilt 2026-11-13\n").as_deref(),
        Some("0.5.0+g125f36d")
    );
    for bad in [
        "",
        "folksentia\n",
        "essentia 2.1\n",
        "folksentia 0.5 extra\n",
        "folksentia 0.5;rm\n",
        "{\"schema\":2,\"error\":\"--version: no such file\"}\n",
    ] {
        assert_eq!(folksentia::parse_version(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn folksentia_is_found_with_its_version() {
    let tool = fake().await;
    assert_eq!(tool.version(), "9.9.9+gtest");
    assert!(tool.has_chromaprint());
    assert!(tool.path().is_absolute());
    assert!(library_options(&tool).contains(&"--fingerprint".to_owned()));
}

#[tokio::test]
async fn a_program_that_is_not_folksentia_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        Folksentia::find(Some(&dir.path().join("missing"))).await,
        Err(AnalysisError::NotAProgram(_))
    ));
    assert!(matches!(
        Folksentia::find(Some(Path::new("folksentia"))).await,
        Err(AnalysisError::NotAProgram(_))
    ));
    // `true` prints no version, like a folksentia older than 0.1.0.
    assert!(matches!(
        Folksentia::find(Some(Path::new("/bin/true"))).await,
        Err(AnalysisError::NoVersion(_))
    ));
}

#[test]
fn output_is_read_and_checked() {
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(GOOD).unwrap();
    let Outcome::Analysed(values) = output::parse(&object).unwrap() else {
        panic!("not analysed");
    };
    assert_eq!(values.bpm, Some(120.5));
    assert_eq!(
        values.key,
        Some(Key {
            pitch: 9,
            minor: true
        })
    );
    assert_eq!(values.beats.as_deref(), Some(&[0.5, 1.0, 1.5, 2.0][..]));
    assert_eq!(values.downbeat_meter, Some(2));
    assert_eq!(values.downbeats.as_deref(), Some(&[0, 2][..]));
    assert_eq!(values.waveform.as_deref(), Some(&[0, 128, 255][..]));
    assert_eq!(values.silence_lead_ms, Some(12));
    assert_eq!(values.integrated_lufs, Some(-14.2));
    assert_eq!(values.loudness_range_lu, Some(8.5));
    assert_eq!(values.replay_gain_db, Some(-3.5));
    assert_eq!(
        values.fingerprint.as_ref().map(|f| f.value.as_str()),
        Some("AQAAEkmUaEmSJMkS")
    );

    // Values out of range are unknown, not stored.
    let odd = GOOD
        .replace("\"bpm\":120.5", "\"bpm\":900")
        .replace("\"key_pitch\":9", "\"key_pitch\":14")
        .replace("[0,2]", "[0,9]")
        .replace("[0,128,255]", "[0,128,300]")
        .replace("\"downbeatMeter\":2", "\"downbeatMeter\":40");
    let object = serde_json::from_str(&odd).unwrap();
    let Outcome::Analysed(values) = output::parse(&object).unwrap() else {
        panic!("not analysed");
    };
    assert_eq!(values.bpm, None);
    assert_eq!(values.key, None);
    assert_eq!(values.downbeats, None, "an index past the beats");
    assert_eq!(values.waveform, None);
    assert_eq!(values.downbeat_meter, None);
    assert_eq!(values.beats.map(|b| b.len()), Some(4), "the rest is kept");

    let error = serde_json::from_str(r#"{"schema":2,"file":"x","error":"no audio"}"#).unwrap();
    assert_eq!(
        output::parse(&error).unwrap(),
        Outcome::Failed("no audio".to_owned())
    );
    let old = serde_json::from_str(r#"{"schema":1,"file":"x","bpm":100}"#).unwrap();
    assert!(matches!(
        output::parse(&old),
        Err(AnalysisError::Schema(Some(1)))
    ));
    let none = serde_json::from_str(r#"{"file":"x"}"#).unwrap();
    assert!(matches!(
        output::parse(&none),
        Err(AnalysisError::Schema(None))
    ));
}

#[tokio::test]
async fn a_run_gives_the_object_or_why_it_failed() {
    let tool = fake().await;
    let dir = tempfile::tempdir().unwrap();
    let run = |name: &str| {
        let path = dir.path().join(name);
        fs::write(&path, b"audio").unwrap();
        let tool = tool.clone();
        async move {
            tool.analyse(&path, &["--downbeats".to_owned()], Duration::from_secs(20))
                .await
        }
    };
    let good = run("a good tune.flac").await.unwrap();
    assert_eq!(good.get("bpm"), Some(&serde_json::json!(120.5)));
    let args = fs::read_to_string(dir.path().join("a good tune.flac.args")).unwrap();
    assert!(
        args.starts_with("--downbeats -- /"),
        "options, then the file after --: {args}"
    );

    let broken = run("broken.flac").await.unwrap();
    assert_eq!(
        broken.get("error"),
        Some(&serde_json::json!("no audio decoded from x"))
    );

    let crash = run("crash.flac").await.unwrap_err();
    assert!(crash.contains("signal 9"), "{crash}");

    let noise = run("noise.flac").await.unwrap_err();
    assert_eq!(noise, "folksentia exited with 3: Essentia exploded");

    let flood = run("flood.flac").await.unwrap_err();
    assert!(flood.contains("more than 64 MB"), "{flood}");
}

#[tokio::test]
async fn a_run_that_takes_too_long_is_killed() {
    let tool = fake().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("slow.flac");
    fs::write(&path, b"audio").unwrap();
    let error = tool
        .analyse(&path, &[], Duration::from_millis(500))
        .await
        .unwrap_err();
    assert!(error.starts_with("folksentia timed out"), "{error}");
    let pid = fs::read_to_string(dir.path().join("slow.flac.pid")).unwrap();
    assert!(ended(&pid).await, "the child is gone");
}

#[tokio::test]
async fn the_library_is_analysed_and_results_are_kept() {
    let (_dir, db, _music) = library(&["a good.flac", "b broken.flac", "c crash.flac"]);
    let tool = fake().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let progress = analyse_library(
        Arc::clone(&db),
        tool.clone(),
        Selection::default(),
        2,
        move |p| log.lock().unwrap().push(p.done),
    )
    .await
    .unwrap();
    assert_eq!((progress.total, progress.done, progress.failed), (3, 3, 2));
    assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 3]);

    let conn = db.reader().unwrap();
    let Some(Ok(values)) = of_track(&conn, 1).unwrap() else {
        panic!("no result");
    };
    assert_eq!(values.bpm, Some(120.5));
    assert_eq!(values.beats.map(|b| b.len()), Some(4));
    assert_eq!(values.downbeats, Some(vec![0, 2]));
    assert_eq!(values.waveform, Some(vec![0, 128, 255]));
    assert_eq!(
        of_track(&conn, 2).unwrap(),
        Some(Err("no audio decoded from x".to_owned()))
    );
    assert!(matches!(of_track(&conn, 3).unwrap(), Some(Err(e)) if e.contains("signal 9")));
    let version: String = conn
        .query_row(
            "SELECT tool_version FROM local_analysis WHERE track_id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, "9.9.9+gtest");

    assert_eq!(
        status(&conn, Some("9.9.9+gtest")).unwrap(),
        Status {
            tracks: 3,
            analysed: 1,
            failed: 2,
            pending: 0,
            other_version: 0
        }
    );
    // Nothing is analysed twice; failures only when asked.
    assert!(
        work(&conn, "9.9.9+gtest", Selection::default())
            .unwrap()
            .is_empty()
    );
    let retry = Selection {
        retry_failed: true,
        ..Selection::default()
    };
    assert_eq!(work(&conn, "9.9.9+gtest", retry).unwrap().len(), 2);
    // Another folksentia version: only on request.
    assert_eq!(status(&conn, Some("10.0.0")).unwrap().other_version, 3);
    assert!(
        work(&conn, "10.0.0", Selection::default())
            .unwrap()
            .is_empty()
    );
    let newer = Selection {
        other_versions: true,
        ..Selection::default()
    };
    assert_eq!(work(&conn, "10.0.0", newer).unwrap().len(), 3);
}

#[tokio::test]
async fn a_changed_file_is_analysed_again() {
    let (_dir, db, _music) = library(&["a good.flac"]);
    let tool = fake().await;
    analyse_library(Arc::clone(&db), tool, Selection::default(), 1, |_| {})
        .await
        .unwrap();
    let conn = db.reader().unwrap();
    assert!(
        work(&conn, "9.9.9+gtest", Selection::default())
            .unwrap()
            .is_empty()
    );
    // A rescan records the new size and time.
    db.write(|tx| {
        tx.execute("UPDATE local_track SET modified_ms = 200 WHERE id = 1", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        of_track(&conn, 1).unwrap(),
        None,
        "the old result is not shown"
    );
    assert_eq!(status(&conn, None).unwrap().pending, 1);
    let again = work(&conn, "9.9.9+gtest", Selection::default()).unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].modified_ms, 200);
}

#[tokio::test]
async fn a_stopped_run_kills_folksentia_and_resumes() {
    let (_dir, db, music) = library(&["a good.flac", "b slow.flac", "c good.flac"]);
    let tool = fake().await;
    let run = analyse_library(
        Arc::clone(&db),
        tool.clone(),
        Selection::default(),
        1,
        |_| {},
    );
    // Stopped while the slow file runs: dropping the run stops it.
    assert!(
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .is_err()
    );
    let pid = fs::read_to_string(music.join("b slow.flac.pid")).unwrap();
    assert!(ended(&pid).await, "the slow folksentia was killed");

    let conn = db.reader().unwrap();
    assert!(matches!(of_track(&conn, 1).unwrap(), Some(Ok(_))), "kept");
    let left: Vec<i64> = work(&conn, "9.9.9+gtest", Selection::default())
        .unwrap()
        .into_iter()
        .map(|w| w.track_id)
        .collect();
    assert_eq!(left, [2, 3], "the next run starts where this one stopped");
}

#[tokio::test]
async fn another_output_schema_stops_the_run() {
    let (_dir, db, _music) = library(&["a old.flac", "b good.flac"]);
    let tool = fake().await;
    let result = analyse_library(Arc::clone(&db), tool, Selection::default(), 1, |_| {}).await;
    assert!(matches!(result, Err(AnalysisError::Schema(Some(1)))));
    let conn = db.reader().unwrap();
    assert_eq!(
        of_track(&conn, 1).unwrap(),
        None,
        "nothing misread is stored"
    );
}

#[test]
fn a_result_for_a_removed_track_is_dropped() {
    let (_dir, db, music) = library(&["a good.flac"]);
    db.write(|tx| {
        tx.execute("DELETE FROM local_track", [])?;
        Ok(())
    })
    .unwrap();
    store(
        &db,
        &work_for(&music.join("a good.flac")),
        "1",
        1,
        &Ok(Analysis::default()),
    )
    .unwrap();
    let rows: i64 = db
        .reader()
        .unwrap()
        .query_row("SELECT count(*) FROM local_analysis", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn local_only_tracks_show_the_analysed_bpm() {
    let (_dir, db, music) = library(&["a good.flac"]);
    let songs = |db: &Database| {
        local_only_songs(&db.reader().unwrap(), None, &LocalFilter::Id(1), 10).unwrap()
    };
    assert_eq!(songs(&db)[0].bpm, Some(90.0), "the tag's BPM");
    let item = work_for(&music.join("a good.flac"));
    let found = Analysis {
        bpm: Some(120.5),
        ..Analysis::default()
    };
    store(&db, &item, "1", 1, &Ok(found)).unwrap();
    assert_eq!(songs(&db)[0].bpm, Some(120.5), "folksentia's BPM");
    store(&db, &item, "1", 2, &Err("no audio".to_owned())).unwrap();
    assert_eq!(songs(&db)[0].bpm, Some(90.0), "a failed run hides nothing");
}

#[test]
fn settings_are_kept_within_bounds() {
    let (_dir, db) = temp_db();
    let wanted = Settings {
        folksentia: Some(PathBuf::from("/opt/folksentia/bin/folksentia")),
        parallel: 99,
    };
    set_settings(&db, &wanted).unwrap();
    let saved = settings(&db.reader().unwrap()).unwrap();
    assert_eq!(saved.folksentia, wanted.folksentia);
    assert_eq!(saved.parallel, MAX_PARALLEL);
    assert!(
        set_settings(
            &db,
            &Settings {
                folksentia: Some(PathBuf::from("relative/folksentia")),
                parallel: 1
            }
        )
        .is_err()
    );
    assert!(parallel_for(0) >= 1);
}
