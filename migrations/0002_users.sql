-- Multi-user schema. Every owned row gets a user_id, and keys that were global become per user,
-- so one user pushing another user's item id can never overwrite it.
--
-- The existing data is adopted by the first user (id 1, the admin). Its name starts as 'admin'
-- and is set from BREADTRACKER_ADMIN_NAME at server start (a migration cannot read the
-- environment). Fresh databases get the same admin row, so first-run pairing still has an owner.
--
-- SQLite cannot change a primary key in place, so items, completions, devices, reminders_sent
-- and pairing_codes are rebuilt: create the new table, copy every row, drop the old one, rename.
-- sqlx runs this file in one transaction, so a failure leaves the 0001 schema untouched.
--
-- meta.seq stays one global counter. A user's own seq values have gaps, which is harmless
-- because a cursor only means "everything with a larger seq".
--
-- The settings table is left alone here; it becomes per-user in a later migration.

CREATE TABLE users (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,   -- never reused after a user is removed
  name       TEXT NOT NULL COLLATE NOCASE UNIQUE CHECK (length(name) BETWEEN 1 AND 40),
  is_admin   INTEGER NOT NULL DEFAULT 0 CHECK (is_admin IN (0, 1)),
  created_at INTEGER NOT NULL
);

INSERT INTO users (id, name, is_admin, created_at)
VALUES (
  1, 'admin', 1,
  COALESCE((SELECT MIN(created_at) FROM devices), CAST(strftime('%s', 'now') AS INTEGER) * 1000)
);

-- items: primary key (user_id, id)
CREATE TABLE items_new (
  user_id     INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  id          TEXT NOT NULL,
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
  seq         INTEGER NOT NULL,
  PRIMARY KEY (user_id, id)
);
INSERT INTO items_new (user_id, id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq)
  SELECT 1, id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq FROM items;
DROP TABLE items;
ALTER TABLE items_new RENAME TO items;
CREATE INDEX items_user_seq ON items (user_id, seq);

-- completions: primary key (user_id, item_id, date)
CREATE TABLE completions_new (
  user_id      INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  item_id      TEXT NOT NULL,
  date         TEXT NOT NULL,                 -- local date YYYY-MM-DD
  completed_at INTEGER NOT NULL,
  updated_at   INTEGER NOT NULL,
  deleted_at   INTEGER,                       -- un-tick tombstone
  seq          INTEGER NOT NULL,
  PRIMARY KEY (user_id, item_id, date)
);
INSERT INTO completions_new (user_id, item_id, date, completed_at, updated_at, deleted_at, seq)
  SELECT 1, item_id, date, completed_at, updated_at, deleted_at, seq FROM completions;
DROP TABLE completions;
ALTER TABLE completions_new RENAME TO completions;
CREATE INDEX completions_user_seq ON completions (user_id, seq);

-- devices: each belongs to one user; the token hash stays globally unique
CREATE TABLE devices_new (
  id         TEXT PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  fcm_token  TEXT,
  created_at INTEGER NOT NULL,
  last_seen  INTEGER NOT NULL
);
INSERT INTO devices_new (id, user_id, name, token_hash, fcm_token, created_at, last_seen)
  SELECT id, 1, name, token_hash, fcm_token, created_at, last_seen FROM devices;
DROP TABLE devices;
ALTER TABLE devices_new RENAME TO devices;
CREATE INDEX devices_user ON devices (user_id);

-- reminders_sent: unique per (user_id, item_id, date)
CREATE TABLE reminders_sent_new (
  user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  item_id TEXT NOT NULL,
  date    TEXT NOT NULL,
  sent_at INTEGER NOT NULL,
  PRIMARY KEY (user_id, item_id, date)
);
INSERT INTO reminders_sent_new (user_id, item_id, date, sent_at)
  SELECT 1, item_id, date, sent_at FROM reminders_sent;
DROP TABLE reminders_sent;
ALTER TABLE reminders_sent_new RENAME TO reminders_sent;

-- pairing_codes: a code is bound to the user whose device it will create
CREATE TABLE pairing_codes_new (
  code_hash  TEXT PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL
);
INSERT INTO pairing_codes_new (code_hash, user_id, expires_at)
  SELECT code_hash, 1, expires_at FROM pairing_codes;
DROP TABLE pairing_codes;
ALTER TABLE pairing_codes_new RENAME TO pairing_codes;
