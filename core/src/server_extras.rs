//! Server calls the main window needs beyond the catalog (#8): a track's
//! waveform for the seek bar, cover art, and adding a song to a playlist.
//!
//! Every answer is untrusted: sizes are bounded before reading, and the
//! waveform is checked value by value.

use serde::Deserialize;

use crate::queue::SongId;
use crate::subsonic::{ApiError, SubsonicClient, credential};

/// The OpenSubsonic extension that offers `GET /api/analysis` (folksonic).
pub const ANALYSIS_EXTENSION: &str = "tinysonicAnalysis";

/// Most waveform bins accepted. folksonic sends a few hundred.
pub const MAX_WAVEFORM_BINS: usize = 8192;

/// Largest analysis answer read. It carries beats and onsets too.
const MAX_ANALYSIS_BYTES: usize = 4 * 1024 * 1024;

/// Largest cover image read.
pub const MAX_COVER_BYTES: usize = 4 * 1024 * 1024;

/// The envelope of a track: one peak per bin, 0 (silence) to 255 (full scale).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waveform {
    peaks: Vec<u8>,
}

impl Waveform {
    /// Checks peaks from the server: at least one, at most
    /// [`MAX_WAVEFORM_BINS`], each 0 to 255.
    pub fn new(peaks: &[i64]) -> Result<Self, ApiError> {
        if peaks.is_empty() || peaks.len() > MAX_WAVEFORM_BINS {
            return Err(ApiError::Invalid("waveform size"));
        }
        let peaks = peaks
            .iter()
            .map(|&p| u8::try_from(p).map_err(|_| ApiError::Invalid("waveform peak")))
            .collect::<Result<_, _>>()?;
        Ok(Self { peaks })
    }

    pub fn peaks(&self) -> &[u8] {
        &self.peaks
    }
}

#[derive(Deserialize)]
struct AnalysisDto {
    #[serde(default)]
    waveform: Option<WaveformDto>,
}

#[derive(Deserialize)]
struct WaveformDto {
    #[serde(default)]
    bins: Option<usize>,
    #[serde(default)]
    peaks: Vec<i64>,
}

/// An empty Subsonic payload: only the status matters.
#[derive(Deserialize)]
struct Nothing {}

impl SubsonicClient {
    /// The waveform of `song` from folksonic's analysis. `None` when the
    /// server knows the song but has no waveform yet, or does not know it.
    pub async fn waveform(&self, song: &SongId) -> Result<Option<Waveform>, ApiError> {
        let mut url = self
            .server()
            .endpoint("api/analysis")
            .map_err(|_| ApiError::Invalid("the endpoint address"))?;
        url.query_pairs_mut().append_pair("id", song.as_str());
        let body = match self.fetch(url, MAX_ANALYSIS_BYTES).await {
            Ok((body, _)) => body,
            Err(ApiError::Http(404)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let dto: AnalysisDto =
            serde_json::from_slice(&body).map_err(|_| ApiError::Invalid("analysis JSON"))?;
        let Some(waveform) = dto.waveform else {
            return Ok(None);
        };
        if waveform
            .bins
            .is_some_and(|bins| bins != waveform.peaks.len())
        {
            return Err(ApiError::Invalid("waveform size"));
        }
        Waveform::new(&waveform.peaks).map(Some)
    }

    /// The cover image `cover_id`, scaled by the server to about `size`
    /// pixels. Returns the image bytes; the caller decodes them.
    pub async fn cover_art(&self, cover_id: &str, size: u32) -> Result<Vec<u8>, ApiError> {
        let mut url = self
            .server()
            .endpoint("rest/getCoverArt.view")
            .map_err(|_| ApiError::Invalid("the endpoint address"))?;
        url.query_pairs_mut()
            .append_pair("c", crate::subsonic::CLIENT_NAME)
            .append_pair("v", crate::subsonic::API_VERSION)
            .append_pair("id", cover_id)
            .append_pair("size", &size.to_string());
        let (body, content_type) = self.fetch(url, MAX_COVER_BYTES).await?;
        // An error comes back as Subsonic XML or JSON with status 200.
        if !content_type.is_some_and(|t| t.starts_with("image/")) {
            return Err(ApiError::Invalid("cover art is not an image"));
        }
        Ok(body)
    }

    /// Adds `song` to the end of the playlist `playlist_id` on the server.
    /// Not retried: a second call would add the song twice.
    pub async fn add_to_playlist(&self, playlist_id: &str, song: &SongId) -> Result<(), ApiError> {
        let _: crate::subsonic::Answer<Nothing> = self
            .get(
                "updatePlaylist",
                &[("playlistId", playlist_id), ("songIdToAdd", song.as_str())],
            )
            .await?;
        Ok(())
    }

    /// One signed GET, read up to `limit` bytes. Returns the body and its
    /// content type.
    async fn fetch(
        &self,
        url: url::Url,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<String>), ApiError> {
        let header = credential(self.auth(), "GET", &url)?;
        let response = self
            .http()
            .get(url)
            .header(header.0, header.1)
            .send()
            .await
            .map_err(crate::subsonic::map_transport)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ApiError::Http(status.as_u16()));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_ascii_lowercase);
        let body = crate::subsonic::read_limited_to(response, limit).await?;
        Ok((body, content_type))
    }
}

/// Writes cover bytes to `dir` for the desktop's media controls (#7), which
/// need a file to show. The name is a hash of the server and cover id, so a
/// server value never becomes part of a path. The directory is private
/// (`0700`), the file `0600`, written to a temporary name and renamed. An
/// existing file is reused.
pub fn store_cover(
    dir: &std::path::Path,
    server_key: &str,
    cover_id: &str,
    bytes: &[u8],
) -> std::io::Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other("the cover directory is a symlink"));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut hash = Sha256::new();
    hash.update(server_key.as_bytes());
    hash.update([0]);
    hash.update(cover_id.as_bytes());
    let name: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{name}.img"));
    if path.is_file() {
        return Ok(path);
    }
    let temp = dir.join(format!("{name}.{}.part", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_are_stored_privately_under_a_hashed_name() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("covers");
        let path = store_cover(&dir, "https://a.example", "../../etc/passwd", b"img").unwrap();
        assert_eq!(path.parent(), Some(dir.as_path()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 64 + 4);
        assert!(name.chars().take(64).all(|c| c.is_ascii_hexdigit()));
        assert_eq!(std::fs::read(&path).unwrap(), b"img");
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        // Same server and cover: the same file. Another server: another file.
        assert_eq!(
            store_cover(&dir, "https://a.example", "../../etc/passwd", b"x").unwrap(),
            path
        );
        assert_ne!(
            store_cover(&dir, "https://b.example", "../../etc/passwd", b"x").unwrap(),
            path
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "no temporary files left"
        );
    }
    use crate::connection::{HttpPolicy, ServerUrl};
    use crate::http::{HttpSettings, client};
    use crate::subsonic::{ApiKey, Auth};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn subsonic(server: &MockServer) -> SubsonicClient {
        let url = ServerUrl::parse(&server.uri(), HttpPolicy::ALLOW_PRIVATE_HTTP).unwrap();
        let http = client(&HttpSettings {
            policy: HttpPolicy::ALLOW_PRIVATE_HTTP,
            pinned_certificate_pem: None,
        })
        .unwrap();
        SubsonicClient::new(http, url, Auth::ApiKey(ApiKey::new("tsk_k").unwrap()))
    }

    fn song() -> SongId {
        SongId::parse("so-1").unwrap()
    }

    async fn analysis(server: &MockServer, status: u16, body: &str) {
        Mock::given(method("GET"))
            .and(path("/api/analysis"))
            .and(query_param("id", "so-1"))
            .and(header("X-Api-Key", "tsk_k"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_waveform_is_read_from_the_analysis() {
        let server = MockServer::start().await;
        analysis(
            &server,
            200,
            r#"{"id":"so-1","waveform":{"bins":3,"peaks":[0,12,255]},"bpm":null}"#,
        )
        .await;
        let wave = subsonic(&server).waveform(&song()).await.unwrap().unwrap();
        assert_eq!(wave.peaks(), &[0, 12, 255]);
    }

    #[tokio::test]
    async fn a_pending_analysis_still_gives_what_is_known() {
        let server = MockServer::start().await;
        analysis(
            &server,
            202,
            r#"{"id":"so-1","waveform":{"bins":1,"peaks":[9]},"pending":["beats"]}"#,
        )
        .await;
        let wave = subsonic(&server).waveform(&song()).await.unwrap().unwrap();
        assert_eq!(wave.peaks(), &[9]);
    }

    #[tokio::test]
    async fn no_waveform_or_unknown_song_is_none() {
        let server = MockServer::start().await;
        analysis(&server, 200, r#"{"id":"so-1","waveform":null}"#).await;
        assert_eq!(subsonic(&server).waveform(&song()).await.unwrap(), None);

        let server = MockServer::start().await;
        analysis(&server, 404, r#"{"error":"song not found"}"#).await;
        assert_eq!(subsonic(&server).waveform(&song()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn bad_waveforms_are_refused() {
        for body in [
            r#"{"waveform":{"bins":1,"peaks":[300]}}"#,
            r#"{"waveform":{"bins":1,"peaks":[-1]}}"#,
            r#"{"waveform":{"bins":2,"peaks":[1]}}"#,
            r#"{"waveform":{"peaks":[]}}"#,
            r#"not json"#,
        ] {
            let server = MockServer::start().await;
            analysis(&server, 200, body).await;
            let result = subsonic(&server).waveform(&song()).await;
            assert!(
                matches!(result, Err(ApiError::Invalid(_))),
                "{body}: {result:?}"
            );
        }
        let too_many = vec![1i64; MAX_WAVEFORM_BINS + 1];
        assert!(Waveform::new(&too_many).is_err());
    }

    #[tokio::test]
    async fn cover_art_must_be_an_image_of_bounded_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/getCoverArt.view"))
            .and(query_param("id", "al-1"))
            .and(query_param("size", "256"))
            .and(header("X-Api-Key", "tsk_k"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/jpeg")
                    .set_body_bytes(vec![0xff, 0xd8, 0xff]),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/getCoverArt.view"))
            .and(query_param("id", "al-2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"subsonic-response":{"status":"failed"}}"#),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/getCoverArt.view"))
            .and(query_param("id", "al-3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(vec![0u8; MAX_COVER_BYTES + 1]),
            )
            .mount(&server)
            .await;
        let client = subsonic(&server);
        assert_eq!(
            client.cover_art("al-1", 256).await.unwrap(),
            vec![0xff, 0xd8, 0xff]
        );
        assert!(matches!(
            client.cover_art("al-2", 256).await,
            Err(ApiError::Invalid(_))
        ));
        assert!(matches!(
            client.cover_art("al-3", 256).await,
            Err(ApiError::TooLarge)
        ));
    }

    #[tokio::test]
    async fn a_song_is_added_to_a_playlist() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/updatePlaylist.view"))
            .and(query_param("playlistId", "pl-1"))
            .and(query_param("songIdToAdd", "so-1"))
            .and(header("X-Api-Key", "tsk_k"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#),
            )
            .expect(1)
            .mount(&server)
            .await;
        subsonic(&server)
            .add_to_playlist("pl-1", &song())
            .await
            .unwrap();
    }
}
