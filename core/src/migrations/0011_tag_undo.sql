-- Schema version 11: undo for the last tag write (#15).
--
-- The tag editor is the only part of Studio that changes music files. Before
-- each write it replaces this table with one row per file it changes: the
-- values the edited fields had before, and the size and modification time
-- the file had right after Studio wrote it. Undo writes the old values back,
-- but only to a file that is still exactly as Studio left it. Rows go with
-- their track. Times are epoch milliseconds.
--
-- `before` is a JSON object written by Studio: field name → old text, or
-- null for a field that was empty.

CREATE TABLE tag_undo (
    track_id     INTEGER PRIMARY KEY REFERENCES local_track (id) ON DELETE CASCADE,
    size_bytes   INTEGER NOT NULL CHECK (size_bytes >= 0),
    modified_ms  INTEGER NOT NULL,
    before       TEXT    NOT NULL CHECK (length(before) <= 65536 AND json_valid(before)
                                         AND json_type(before) = 'object'),
    written_at   INTEGER NOT NULL CHECK (written_at >= 0)
) STRICT;
