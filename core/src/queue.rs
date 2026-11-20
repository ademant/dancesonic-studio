//! The play queue: the songs to play, in order, and where playback is (#6).
//!
//! A pure model with no GStreamer. The player asks [`PlayQueue::upcoming`]
//! ahead of time, so the next track can start without a gap.

use std::fmt;

/// Most songs a queue holds. A library is far bigger; this only bounds what a
/// single request can add.
pub const MAX_QUEUE_LEN: usize = 10_000;

/// Longest song id accepted, in bytes. Server ids are short.
pub const MAX_SONG_ID_BYTES: usize = 512;

/// A server's id for one song. Opaque to the queue; only the server reads it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SongId(String);

impl SongId {
    /// Parses a song id. Empty, too long or control characters are refused.
    pub fn parse(text: &str) -> Result<Self, QueueError> {
        if text.is_empty() || text.len() > MAX_SONG_ID_BYTES || text.chars().any(char::is_control) {
            return Err(QueueError::BadSong);
        }
        Ok(Self(text.to_owned()))
    }

    /// The id as the server sent it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SongId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Errors from changing the queue.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QueueError {
    #[error("a song id is empty, too long or has control characters")]
    BadSong,
    #[error("the queue would hold more than {MAX_QUEUE_LEN} songs")]
    TooLong,
    #[error("there is no song at that position")]
    NoSuchIndex,
}

/// The songs to play and the position of the current one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlayQueue {
    songs: Vec<SongId>,
    current: Option<usize>,
}

impl PlayQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every song in order.
    pub fn songs(&self) -> &[SongId] {
        &self.songs
    }

    /// Number of songs.
    pub fn len(&self) -> usize {
        self.songs.len()
    }

    /// Whether the queue holds no songs.
    pub fn is_empty(&self) -> bool {
        self.songs.is_empty()
    }

    /// Position of the current song, if any.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The song playing now.
    pub fn current(&self) -> Option<&SongId> {
        self.current.and_then(|index| self.songs.get(index))
    }

    /// The song after the current one, without moving to it. The player
    /// queues this one ahead of time for gapless playback.
    pub fn upcoming(&self) -> Option<&SongId> {
        let next = self.current? + 1;
        self.songs.get(next)
    }

    /// Replaces the queue and starts at `start`. An empty list leaves nothing
    /// playing.
    pub fn replace(&mut self, songs: Vec<SongId>, start: usize) -> Result<(), QueueError> {
        if songs.len() > MAX_QUEUE_LEN {
            return Err(QueueError::TooLong);
        }
        if songs.is_empty() {
            self.clear();
            return Ok(());
        }
        if start >= songs.len() {
            return Err(QueueError::NoSuchIndex);
        }
        self.songs = songs;
        self.current = Some(start);
        Ok(())
    }

    /// Adds songs to the end. Playback does not move.
    pub fn append(&mut self, songs: Vec<SongId>) -> Result<(), QueueError> {
        if self.songs.len() + songs.len() > MAX_QUEUE_LEN {
            return Err(QueueError::TooLong);
        }
        self.songs.extend(songs);
        Ok(())
    }

    /// Removes the song at `index`. If it was the current one, the next song
    /// becomes current, or nothing does when it was the last.
    pub fn remove(&mut self, index: usize) -> Result<SongId, QueueError> {
        if index >= self.songs.len() {
            return Err(QueueError::NoSuchIndex);
        }
        let removed = self.songs.remove(index);
        self.current = match self.current {
            _ if self.songs.is_empty() => None,
            Some(current) if index < current => Some(current - 1),
            Some(current) if index == current => (index < self.songs.len()).then_some(index),
            other => other,
        };
        Ok(removed)
    }

    /// Moves to the next song and returns it, or `None` at the end.
    pub fn advance(&mut self) -> Option<&SongId> {
        let next = self.current.map_or(0, |current| current + 1);
        if next >= self.songs.len() {
            self.current = None;
            return None;
        }
        self.current = Some(next);
        self.songs.get(next)
    }

    /// Moves to the previous song and returns it, or `None` at the start.
    pub fn back(&mut self) -> Option<&SongId> {
        let previous = self.current?.checked_sub(1)?;
        self.current = Some(previous);
        self.songs.get(previous)
    }

    /// Stops and empties the queue.
    pub fn clear(&mut self) {
        self.songs.clear();
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn songs(names: &[&str]) -> Vec<SongId> {
        names.iter().map(|n| SongId::parse(n).unwrap()).collect()
    }

    fn names(queue: &PlayQueue) -> Vec<&str> {
        queue.songs().iter().map(SongId::as_str).collect()
    }

    #[test]
    fn song_ids_are_checked() {
        assert!(SongId::parse("so-1").is_ok());
        assert_eq!(SongId::parse("").err(), Some(QueueError::BadSong));
        assert_eq!(SongId::parse("a\nb").err(), Some(QueueError::BadSong));
        assert_eq!(
            SongId::parse(&"x".repeat(MAX_SONG_ID_BYTES + 1)).err(),
            Some(QueueError::BadSong)
        );
    }

    #[test]
    fn replace_starts_at_the_given_song() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a", "b", "c"]), 1).unwrap();
        assert_eq!(queue.current().unwrap().as_str(), "b");
        assert_eq!(queue.upcoming().unwrap().as_str(), "c");
        assert_eq!(
            queue.replace(songs(&["a"]), 3),
            Err(QueueError::NoSuchIndex)
        );
    }

    #[test]
    fn advancing_past_the_end_stops_at_nothing() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a", "b"]), 0).unwrap();
        assert_eq!(queue.advance().unwrap().as_str(), "b");
        assert!(queue.upcoming().is_none());
        assert!(queue.advance().is_none());
        assert!(queue.current().is_none());
    }

    #[test]
    fn back_stops_at_the_first_song() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a", "b"]), 1).unwrap();
        assert_eq!(queue.back().unwrap().as_str(), "a");
        assert!(queue.back().is_none());
        assert_eq!(queue.current().unwrap().as_str(), "a");
    }

    #[test]
    fn removing_before_the_current_song_keeps_it_current() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a", "b", "c"]), 2).unwrap();
        queue.remove(0).unwrap();
        assert_eq!(queue.current().unwrap().as_str(), "c");
        assert_eq!(queue.current_index(), Some(1));
    }

    #[test]
    fn removing_the_current_song_moves_to_the_next_one() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a", "b", "c"]), 1).unwrap();
        queue.remove(1).unwrap();
        assert_eq!(queue.current().unwrap().as_str(), "c");
        queue.remove(1).unwrap();
        assert!(queue.current().is_none(), "no song after the last one");
        assert_eq!(names(&queue), ["a"]);
    }

    #[test]
    fn removing_the_last_song_empties_the_current_position() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a"]), 0).unwrap();
        queue.remove(0).unwrap();
        assert!(queue.is_empty());
        assert!(queue.current().is_none());
    }

    #[test]
    fn append_does_not_move_playback() {
        let mut queue = PlayQueue::new();
        queue.replace(songs(&["a"]), 0).unwrap();
        queue.append(songs(&["b", "c"])).unwrap();
        assert_eq!(queue.current().unwrap().as_str(), "a");
        assert_eq!(queue.upcoming().unwrap().as_str(), "b");
    }

    #[test]
    fn the_queue_has_a_length_limit() {
        let mut queue = PlayQueue::new();
        let many = vec![SongId::parse("a").unwrap(); MAX_QUEUE_LEN + 1];
        assert_eq!(queue.replace(many, 0), Err(QueueError::TooLong));
        queue
            .append(vec![SongId::parse("a").unwrap(); MAX_QUEUE_LEN])
            .unwrap();
        assert_eq!(queue.append(songs(&["b"])), Err(QueueError::TooLong));
    }
}
