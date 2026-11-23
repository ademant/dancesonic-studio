-- Schema version 10: analysis of local files with folksentia (#11).
--
-- One row per local track that folksentia analysed, or failed to analyse
-- (`error` set, the values NULL). The row keeps the size and modification
-- time of the file it belongs to: a track whose file changed since then is
-- analysed again, an unchanged one is not, unless the user asks for a newer
-- folksentia version. Rows go with their track. Times are epoch milliseconds.
--
-- `beats` and `downbeats` are JSON arrays written by Studio: beat times in
-- seconds, and the indexes of the beats that start a bar. `waveform` holds
-- one byte (0–255) per bin.

CREATE TABLE local_analysis (
    track_id           INTEGER PRIMARY KEY REFERENCES local_track (id) ON DELETE CASCADE,
    tool_version       TEXT    NOT NULL CHECK (length(tool_version) BETWEEN 1 AND 200),
    size_bytes         INTEGER NOT NULL CHECK (size_bytes >= 0),
    modified_ms        INTEGER NOT NULL,
    analysed_at        INTEGER NOT NULL CHECK (analysed_at >= 0),
    error              TEXT    CHECK (error IS NULL OR length(error) BETWEEN 1 AND 1000),
    bpm                REAL    CHECK (bpm BETWEEN 20 AND 300),
    bpm_confidence     REAL    CHECK (bpm_confidence >= 0),
    beats              TEXT    CHECK (beats IS NULL OR beats LIKE '[%]'),
    downbeat_meter     INTEGER CHECK (downbeat_meter BETWEEN 2 AND 12),
    downbeats          TEXT    CHECK (downbeats IS NULL OR downbeats LIKE '[%]'),
    key_pitch          INTEGER CHECK (key_pitch BETWEEN 0 AND 11),
    key_mode           INTEGER CHECK (key_mode IN (0, 1)),
    key_strength       REAL    CHECK (key_strength BETWEEN 0 AND 1),
    integrated_lufs    REAL,
    true_peak_dbtp     REAL,
    loudness_range_lu  REAL    CHECK (loudness_range_lu >= 0),
    replay_gain_db     REAL,
    silence_lead_ms    INTEGER CHECK (silence_lead_ms >= 0),
    silence_tail_ms    INTEGER CHECK (silence_tail_ms >= 0),
    waveform           BLOB    CHECK (waveform IS NULL OR length(waveform) BETWEEN 1 AND 4096),
    fingerprint        TEXT    CHECK (fingerprint IS NULL OR length(fingerprint) BETWEEN 1 AND 65536),
    fingerprint_sec    REAL    CHECK (fingerprint_sec > 0),
    -- A key is a pitch and a mode, or nothing.
    CHECK ((key_pitch IS NULL) = (key_mode IS NULL)),
    -- A failed run stores no values.
    CHECK (error IS NULL OR (bpm IS NULL AND beats IS NULL AND key_pitch IS NULL
                             AND replay_gain_db IS NULL AND waveform IS NULL
                             AND fingerprint IS NULL))
) STRICT;

-- One row: which folksentia to run (NULL: look for it) and how many at a time.
CREATE TABLE analysis_settings (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    folksentia  TEXT    CHECK (folksentia IS NULL
                               OR (length(folksentia) BETWEEN 2 AND 4096 AND folksentia LIKE '/%')),
    parallel    INTEGER NOT NULL CHECK (parallel BETWEEN 1 AND 16)
) STRICT;
INSERT INTO analysis_settings (id, folksentia, parallel) VALUES (1, NULL, 2);
