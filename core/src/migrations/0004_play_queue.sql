-- Schema version 4: plays waiting to be reported to the server (#31).
--
-- A play is queued as soon as it counts and removed once the server took it.
-- The play time is kept, so a retry sends the same time and the server counts
-- it once. The queue is bounded in code; the oldest plays go first.
--
-- Rows go with their server. Times are epoch milliseconds.

CREATE TABLE play_queue (
    seq        INTEGER PRIMARY KEY,
    server_id  INTEGER NOT NULL REFERENCES server (id) ON DELETE CASCADE,
    song_id    TEXT    NOT NULL CHECK (length(song_id) BETWEEN 1 AND 512),
    played_at  INTEGER NOT NULL CHECK (played_at >= 0),
    UNIQUE (server_id, song_id, played_at)
) STRICT;
CREATE INDEX play_queue_order ON play_queue (server_id, seq);
