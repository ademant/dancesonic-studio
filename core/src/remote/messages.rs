//! The wire messages of `docs/remote-protocol.md`. Messages from the phone
//! are untrusted: their size is checked before parsing, unknown fields are
//! refused, and binary fields are decoded with a length bound.

use serde::{Deserialize, Serialize};

/// The largest counter: JSON numbers are exact only up to 2^53 - 1.
pub const MAX_COUNTER: u64 = (1 << 53) - 1;
/// Most song IDs in one `play_songs` or `enqueue`, and in `state.queue`.
pub const MAX_SONGS: usize = 1000;
/// Longest ID of a song, album, artist or playlist.
pub const MAX_ID_LEN: usize = 512;
/// Longest command body text.
pub const MAX_BODY_LEN: usize = 32 * 1024;

/// Which transport a session runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Ble,
    Network,
}

impl Transport {
    /// The value stored in the database and sent in `welcome`.
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Ble => "ble",
            Transport::Network => "network",
        }
    }

    /// The features this transport offers.
    pub fn features(self) -> &'static [&'static str] {
        match self {
            Transport::Ble => &["playback", "tempo", "dance", "queue", "network_hint"],
            Transport::Network => &["playback", "tempo", "dance", "queue", "library", "cover"],
        }
    }

    /// The largest message accepted, before parsing.
    pub fn max_message(self) -> usize {
        match self {
            Transport::Ble => 16 * 1024,
            Transport::Network => 64 * 1024,
        }
    }
}

/// A message from the phone.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Incoming {
    PairRequest {
        v: u8,
        name: String,
        device_key: String,
        noise_key: String,
        proof: String,
        sig: String,
    },
    Hello {
        device: String,
        counter: u64,
        sig: String,
    },
    Command {
        counter: u64,
        body: String,
        sig: String,
    },
}

/// Why a phone's message could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Too large, not JSON, or not shaped like any message.
    BadMessage,
    /// A well-formed message of a type this version does not know.
    UnknownType,
}

impl ParseError {
    /// The wire error code.
    pub fn code(self) -> &'static str {
        match self {
            ParseError::BadMessage => "bad_message",
            ParseError::UnknownType => "unknown_type",
        }
    }
}

const KNOWN_TYPES: [&str; 3] = ["pair_request", "hello", "command"];

impl Incoming {
    /// Parses one message of at most `max` bytes.
    pub fn parse(bytes: &[u8], max: usize) -> Result<Self, ParseError> {
        if bytes.len() > max {
            return Err(ParseError::BadMessage);
        }
        match serde_json::from_slice::<Incoming>(bytes) {
            Ok(message) => Ok(message),
            Err(_) => {
                #[derive(Deserialize)]
                struct Typed {
                    r#type: String,
                }
                match serde_json::from_slice::<Typed>(bytes) {
                    Ok(t) if !KNOWN_TYPES.contains(&t.r#type.as_str()) => {
                        Err(ParseError::UnknownType)
                    }
                    _ => Err(ParseError::BadMessage),
                }
            }
        }
    }
}

/// A command, the content of `command.body`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum Command {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    Seek {
        position_ms: u64,
    },
    SetRate {
        rate: f64,
    },
    SetBpm {
        bpm: f64,
    },
    PlaySongs {
        songs: Vec<String>,
        #[serde(default)]
        start: usize,
    },
    Enqueue {
        songs: Vec<String>,
    },
    PlayDance {
        tag_id: i64,
    },
    NextOfDance {
        tag_id: i64,
    },
    PlayPlaylist {
        playlist: String,
        #[serde(default)]
        start: usize,
    },
    GetState,
    LibraryArtists,
    LibraryPlaylists,
    LibraryDances,
    LibraryAlbums {
        artist: String,
    },
    LibrarySongs {
        #[serde(default)]
        album: Option<String>,
        #[serde(default)]
        artist: Option<String>,
        #[serde(default)]
        playlist: Option<String>,
        #[serde(default)]
        dance: Option<i64>,
        #[serde(default)]
        query: Option<String>,
    },
    Cover {
        id: String,
    },
}

impl Command {
    /// Parses and checks a command body: ranges, list sizes and ID lengths.
    pub fn parse(body: &str) -> Result<Self, ParseError> {
        if body.len() > MAX_BODY_LEN {
            return Err(ParseError::BadMessage);
        }
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(body).map_err(|_| ParseError::BadMessage)?;
        let fields = object.len();
        let command: Command = serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|_| ParseError::BadMessage)?;
        // serde ignores extra fields next to the tag of a variant without
        // fields; refuse them like any other unknown field.
        if command.takes_no_fields() && fields != 1 {
            return Err(ParseError::BadMessage);
        }
        if command.is_valid() {
            Ok(command)
        } else {
            Err(ParseError::BadMessage)
        }
    }

    fn takes_no_fields(&self) -> bool {
        matches!(
            self,
            Command::Play
                | Command::Pause
                | Command::Toggle
                | Command::Stop
                | Command::Next
                | Command::Previous
                | Command::GetState
                | Command::LibraryArtists
                | Command::LibraryPlaylists
                | Command::LibraryDances
        )
    }

    fn is_valid(&self) -> bool {
        let id_ok = |id: &str| !id.is_empty() && id.len() <= MAX_ID_LEN;
        let opt_ok = |id: &Option<String>| id.as_deref().is_none_or(|i| i.len() <= MAX_ID_LEN);
        match self {
            Command::SetRate { rate } => (0.5..=1.5).contains(rate),
            Command::SetBpm { bpm } => (20.0..=300.0).contains(bpm),
            Command::PlaySongs { songs, .. } | Command::Enqueue { songs } => {
                songs.len() <= MAX_SONGS && songs.iter().all(|s| id_ok(s))
            }
            Command::PlayPlaylist { playlist, .. } => id_ok(playlist),
            Command::LibraryAlbums { artist } => id_ok(artist),
            Command::Cover { id } => id_ok(id),
            Command::LibrarySongs {
                album,
                artist,
                playlist,
                dance,
                query,
            } => {
                let given = [
                    album.is_some(),
                    artist.is_some(),
                    playlist.is_some(),
                    dance.is_some(),
                    query.is_some(),
                ]
                .iter()
                .filter(|g| **g)
                .count();
                given == 1 && opt_ok(album) && opt_ok(artist) && opt_ok(playlist) && opt_ok(query)
            }
            _ => true,
        }
    }

    /// The feature a transport must offer for this command.
    pub fn feature(&self) -> &'static str {
        match self {
            Command::SetRate { .. } | Command::SetBpm { .. } => "tempo",
            Command::PlayDance { .. } | Command::NextOfDance { .. } => "dance",
            Command::PlaySongs { .. } | Command::Enqueue { .. } | Command::PlayPlaylist { .. } => {
                "queue"
            }
            Command::LibraryArtists
            | Command::LibraryPlaylists
            | Command::LibraryDances
            | Command::LibraryAlbums { .. }
            | Command::LibrarySongs { .. } => "library",
            Command::Cover { .. } => "cover",
            _ => "playback",
        }
    }
}

/// The dance that is playing, in `state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DanceRef {
    pub tag_id: i64,
    pub name: String,
}

/// Playback as the phone sees it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PlayState {
    pub playing: bool,
    pub song: Option<String>,
    pub index: Option<usize>,
    pub position_ms: u64,
    pub length_ms: Option<u64>,
    pub rate: f64,
    pub bpm: Option<f64>,
    pub queue: Vec<String>,
    pub dance: Option<DanceRef>,
}

/// A message to the phone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Outgoing {
    Identity {
        tls_spki: String,
    },
    PairPending,
    PairAccepted {
        device: String,
    },
    PairRejected {
        reason: String,
    },
    Welcome {
        protocol: u8,
        name: String,
        transport: Transport,
        features: Vec<String>,
    },
    Result {
        id: u64,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    State(PlayState),
    NetworkHint {
        addresses: Vec<String>,
        port: u16,
    },
    Error {
        code: String,
    },
    Bye {
        reason: String,
    },
}

impl Outgoing {
    /// An `error` message.
    pub fn error(code: &str) -> Self {
        Outgoing::Error {
            code: code.to_owned(),
        }
    }

    /// The JSON text of the message.
    pub fn to_json(&self) -> Vec<u8> {
        // Serialising these plain types cannot fail; an empty message would
        // be refused by the phone rather than misread.
        serde_json::to_vec(self).unwrap_or_default()
    }
}
