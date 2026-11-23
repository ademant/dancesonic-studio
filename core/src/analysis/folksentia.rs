//! Running the folksentia program (ademant/folksentia): finding it, asking
//! its version, and analysing one file in a child process of its own.
//!
//! folksentia is never linked in. Each file gets its own process, started
//! from an argument list (never a shell), with an empty environment, a
//! timeout, capped output, and the lowest CPU and I/O priority where `nice`
//! and `ionice` exist. A child whose run is dropped (cancelled) is killed.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::AnalysisError;

/// How long `folksentia --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// Most bytes read from folksentia's output for one file. A long track's
/// beats, onsets, waveform and fingerprint stay far below it.
pub const MAX_OUTPUT: usize = 64 << 20;

/// Most bytes kept of folksentia's error output.
const MAX_STDERR: usize = 16 << 10;

/// Longest error message kept from a failed run, in characters.
pub const MAX_ERROR_CHARS: usize = 300;

/// Folders searched for folksentia when no program was chosen, after the
/// absolute entries of `PATH`.
const SEARCH_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/opt/folksentia/bin"];

/// Folders the priority helpers are looked up in. They are part of every
/// Ubuntu installation (coreutils and util-linux).
const HELPER_DIRS: &[&str] = &["/usr/bin", "/bin"];

/// A folksentia program that answered `--version`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folksentia {
    path: PathBuf,
    version: String,
    chromaprint: bool,
    /// `nice` and `ionice`, when found: the child runs through them.
    nice: Option<PathBuf>,
    ionice: Option<PathBuf>,
}

impl Folksentia {
    /// Finds folksentia and asks its version. `chosen` is the program the user
    /// picked; without one, the absolute folders of `PATH`, `~/.local/bin` and
    /// a few usual places are searched.
    pub async fn find(chosen: Option<&Path>) -> Result<Self, AnalysisError> {
        let path = match chosen {
            Some(path) => {
                if !is_program(path) {
                    return Err(AnalysisError::NotAProgram(path.to_owned()));
                }
                path.to_owned()
            }
            None => search().ok_or(AnalysisError::NotFound)?,
        };
        let path = std::fs::canonicalize(&path).map_err(|source| AnalysisError::Run {
            path: path.clone(),
            source,
        })?;
        Self::with_version(path).await
    }

    /// Runs `path --version` and keeps the program when it is folksentia.
    async fn with_version(path: PathBuf) -> Result<Self, AnalysisError> {
        let mut command = Command::new(&path);
        command
            .arg("--version")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
            .await
            .map_err(|_| AnalysisError::NoVersion(path.clone()))?
            .map_err(|source| AnalysisError::Run {
                path: path.clone(),
                source,
            })?;
        let text = String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(4096)]);
        let version = parse_version(&text).ok_or_else(|| AnalysisError::NoVersion(path.clone()))?;
        let chromaprint = text
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("chromaprint yes"));
        Ok(Self {
            path,
            version,
            chromaprint,
            nice: helper("nice"),
            ionice: helper("ionice"),
        })
    }

    /// The program's absolute path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The version from `--version`, such as `0.5.0+g125f36d`.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Whether this build can compute fingerprints (`--fingerprint`).
    pub fn has_chromaprint(&self) -> bool {
        self.chromaprint
    }

    /// Analyses one file with `options` (folksentia flags). Gives
    /// folksentia's JSON object, which may be its error object, or an error
    /// message when the run itself failed: the program could not start, it
    /// timed out, crashed, or printed something that is not one JSON object.
    pub async fn analyse(
        &self,
        file: &Path,
        options: &[String],
        timeout: Duration,
    ) -> Result<serde_json::Map<String, serde_json::Value>, String> {
        let mut command = self.command();
        command
            .args(options)
            .arg("--")
            .arg(file)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Its own process group: a Ctrl-C in Studio's terminal is not
            // passed on, Studio decides when to stop it.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("folksentia could not be started: {e}"))?;
        if let Some(pid) = child.id() {
            // The kernel picks folksentia before Studio when memory runs out.
            // Raising one's own child's score needs no privilege; where /proc
            // is not writable the run goes on without it.
            let _ = std::fs::write(format!("/proc/{pid}/oom_score_adj"), "1000");
        }
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err("folksentia's output could not be read".to_owned());
        };
        let run = async {
            tokio::try_join!(
                read_capped(stdout, MAX_OUTPUT),
                read_capped(stderr, MAX_STDERR),
                async { child.wait().await.map_err(|e| e.to_string()) },
            )
        };
        let finished = tokio::time::timeout(timeout, run).await;
        let (out, err, status) = match finished {
            Ok(Ok(done)) => done,
            Ok(Err(message)) => {
                stop(&mut child).await;
                return Err(message);
            }
            Err(_) => {
                stop(&mut child).await;
                return Err(format!(
                    "folksentia timed out after {} s",
                    timeout.as_secs()
                ));
            }
        };
        match serde_json::from_slice::<serde_json::Value>(&out) {
            Ok(serde_json::Value::Object(object)) => Ok(object),
            _ => Err(failure(status, &err)),
        }
    }

    /// The command that starts folksentia, through `ionice` and `nice` when
    /// both are there. Both replace themselves with the program, so the
    /// child's process is folksentia's.
    fn command(&self) -> Command {
        match (&self.ionice, &self.nice) {
            (Some(ionice), Some(nice)) => {
                let mut command = Command::new(ionice);
                command
                    // `-t`: run anyway where the idle class cannot be set.
                    .args(["-c", "3", "-t", "--"])
                    .arg(nice)
                    .args(["-n", "19", "--"])
                    .arg(&self.path);
                command
            }
            (None, Some(nice)) => {
                let mut command = Command::new(nice);
                command.args(["-n", "19", "--"]).arg(&self.path);
                command
            }
            _ => Command::new(&self.path),
        }
    }
}

/// The version from the first line of `--version`: `folksentia VERSION`.
pub(crate) fn parse_version(text: &str) -> Option<String> {
    let mut words = text.lines().next()?.split_whitespace();
    let (Some("folksentia"), Some(version), None) = (words.next(), words.next(), words.next())
    else {
        return None;
    };
    let ok = version.len() <= 100
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._".contains(c));
    ok.then(|| version.to_owned())
}

/// Looks for `folksentia` in the absolute folders of `PATH`, then in
/// `~/.local/bin` and [`SEARCH_DIRS`]. Relative `PATH` entries are skipped,
/// so the current folder never supplies the program.
fn search() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.extend(SEARCH_DIRS.iter().map(PathBuf::from));
    dirs.into_iter()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("folksentia"))
        .find(|path| is_program(path))
}

/// A priority helper from [`HELPER_DIRS`], if installed.
fn helper(name: &str) -> Option<PathBuf> {
    HELPER_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| is_program(path))
}

/// Whether `path` is an absolute path to an executable file.
fn is_program(path: &Path) -> bool {
    path.is_absolute()
        && std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Reads at most `limit` bytes; more is an error, so the caller stops the
/// child instead of letting it fill memory.
async fn read_capped(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    reader
        .take(cap)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("folksentia's output could not be read: {e}"))?;
    if bytes.len() > limit {
        return Err(format!("folksentia printed more than {} MB", limit >> 20));
    }
    Ok(bytes)
}

/// Kills a child that is still running and reaps it.
async fn stop(child: &mut tokio::process::Child) {
    // An error means it has already ended.
    let _ = child.kill().await;
}

/// The message for a run that printed no result.
fn failure(status: ExitStatus, stderr: &[u8]) -> String {
    if let Some(signal) = status.signal() {
        let mut message = format!("folksentia was killed by signal {signal}");
        if signal == 9 {
            message.push_str(" (out of memory?)");
        }
        return message;
    }
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let tail: String = {
        let chars: Vec<char> = text.chars().collect();
        chars[chars.len().saturating_sub(MAX_ERROR_CHARS)..]
            .iter()
            .collect()
    };
    match status.code() {
        Some(code) if tail.is_empty() => format!("folksentia exited with {code}"),
        Some(code) => format!("folksentia exited with {code}: {tail}"),
        None => "folksentia ended without a result".to_owned(),
    }
}
