-- Schema version 3: the audio cache index (#30).
--
-- One row per complete track on disk. The audio itself lives in files under
-- the cache directory, named by `file_name`, never in the database. Partial
-- downloads have no row: they do not survive a restart.
--
-- Rows go with their server. Times are epoch milliseconds.

CREATE TABLE audio_cache (
    server_id     INTEGER NOT NULL REFERENCES server (id) ON DELETE CASCADE,
    song_id       TEXT    NOT NULL CHECK (length(song_id) BETWEEN 1 AND 512),
    -- A SHA-256 hex digest plus the extension, so it never comes from the server.
    file_name     TEXT    NOT NULL UNIQUE CHECK (length(file_name) = 70 AND file_name LIKE '%.audio'),
    size_bytes    INTEGER NOT NULL CHECK (size_bytes > 0),
    content_type  TEXT    NOT NULL CHECK (length(content_type) BETWEEN 1 AND 256),
    cached_at     INTEGER NOT NULL CHECK (cached_at >= 0),
    played_at     INTEGER NOT NULL CHECK (played_at >= 0),
    PRIMARY KEY (server_id, song_id)
) STRICT;
CREATE INDEX audio_cache_played ON audio_cache (played_at);
