-- Bread Tracker schema. Timestamps are unix milliseconds. `seq` is a server-assigned
-- monotonic change counter used as the sync cursor, so clock skew cannot lose rows.

CREATE TABLE meta (
  key TEXT PRIMARY KEY,
  val INTEGER NOT NULL
);
INSERT INTO meta (key, val) VALUES ('seq', 0);

CREATE TABLE items (
  id          TEXT PRIMARY KEY,
  kind        TEXT NOT NULL CHECK (kind IN ('chore', 'habit', 'todo')),
  name        TEXT NOT NULL,
  weekdays    INTEGER NOT NULL DEFAULT 127,   -- bit0 = Monday .. bit6 = Sunday
  remind_time TEXT,                           -- local HH:MM, NULL = no reminder
  points      INTEGER NOT NULL,
  due_date    TEXT,                           -- todos: YYYY-MM-DD
  sort        INTEGER NOT NULL DEFAULT 0,
  active      INTEGER NOT NULL DEFAULT 1,
  updated_at  INTEGER NOT NULL,
  deleted_at  INTEGER,
  seq         INTEGER NOT NULL
);
CREATE INDEX items_seq ON items (seq);

CREATE TABLE completions (
  item_id      TEXT NOT NULL,
  date         TEXT NOT NULL,                 -- local date YYYY-MM-DD
  completed_at INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  deleted_at   INTEGER,                       -- un-tick tombstone
  seq          INTEGER NOT NULL,
  PRIMARY KEY (item_id, date)
);
CREATE INDEX completions_seq ON completions (seq);

CREATE TABLE devices (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  fcm_token  TEXT,
  created_at INTEGER NOT NULL,
  last_seen  INTEGER NOT NULL
);

CREATE TABLE pairing_codes (
  code_hash  TEXT PRIMARY KEY,
  expires_at INTEGER NOT NULL
);

CREATE TABLE settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
INSERT INTO settings (key, value) VALUES
  ('timezone', 'Australia/Perth'),
  ('quiet_start', ''),
  ('quiet_end', ''),
  ('reminders_enabled', '1');

CREATE TABLE reminders_sent (
  item_id TEXT NOT NULL,
  date    TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (item_id, date)
);
