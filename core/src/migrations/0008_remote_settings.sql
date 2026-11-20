-- Schema version 8: remote control switches (#20, #24).
--
-- Both transports are off until the user turns them on. `port` is the one
-- Studio tries first for the network transport.

CREATE TABLE remote_settings (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    network     INTEGER NOT NULL DEFAULT 0 CHECK (network IN (0, 1)),
    bluetooth   INTEGER NOT NULL DEFAULT 0 CHECK (bluetooth IN (0, 1)),
    port        INTEGER NOT NULL DEFAULT 47800 CHECK (port BETWEEN 1024 AND 65535)
) STRICT;

INSERT INTO remote_settings (id) VALUES (1);
