-- Forward repair for the multi-user schema (0002). Additive only: no applied migration is
-- edited, and no table is rebuilt, so no row can be lost here.
--
-- Version 0005 on purpose: 0002 is users, 0003 is the hygiene indexes already on the default
-- branch, and 0004 is reserved for the durable pairing limiter. sqlx applies every version it
-- has not recorded, in version order, regardless of what is already recorded, so this file
-- runs after 0002 on every ledger shape:
--   0001 only; 0001+0003; 0001+0003+0004; and a fresh database (0001 to 0005 in order).
--
-- 1. Restore the hygiene indexes (0003). When 0003 was applied before 0002, the 0002 table
--    rebuild dropped them with the old tables, and sqlx never reruns an applied version.
--    IF NOT EXISTS makes this a no-op wherever 0003 ran after 0002 or the indexes survived.
CREATE INDEX IF NOT EXISTS items_kind_id ON items (kind, id);
CREATE INDEX IF NOT EXISTS items_reminder_candidates ON items (remind_time)
  WHERE active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL;
CREATE INDEX IF NOT EXISTS completions_date_item ON completions (date, item_id);
CREATE INDEX IF NOT EXISTS completions_live_date ON completions (deleted_at, date);

--    Also restore the access path the old completions primary key (item_id, date) gave the
--    scheduler's to-do suppression lookup (completions WHERE item_id IN (...)). The 0002
--    primary key is (user_id, item_id, date), which cannot serve a lookup by item_id alone,
--    so without this index that query scans every completion on every tick. Needed on every
--    ledger, so it is not conditional on 0003.
CREATE INDEX IF NOT EXISTS completions_item_date ON completions (item_id, date);

-- 2. Name uniqueness that matches the supported names. 0002 made users.name unique with the
--    NOCASE collation, which folds ASCII only, so 'Älex' and 'älex' were both accepted. The
--    application stores the Unicode caseless key of each name in name_key (see users.rs) and
--    this unique index enforces it. SQLite cannot compute that key, so the migration fills
--    it for printable ASCII names (where the key is plain lower case) and the server fills
--    the rest at start (users::backfill_name_keys), which refuses to start, naming both users,
--    if two existing names collide, rather than silently picking one to keep.
ALTER TABLE users ADD COLUMN name_key TEXT;
UPDATE users SET name_key = lower(name) WHERE name NOT GLOB '*[^ -~]*';
CREATE UNIQUE INDEX users_name_key ON users (name_key);

-- Guards, so a plain SQL write cannot leave an ASCII name without its key:
--  * an insert without a key gets lower(name) when the name is printable ASCII, and is refused
--    for any other name, because only the server can case fold it: create users through
--    users::create;
--  * a rename that does not touch the key re-derives it for ASCII names. Renames to other
--    names must go through users::rename, which sets the key itself.
-- Setting name_key to NULL together with a new name is allowed on purpose: it is how an
-- operator asks the server to recompute the key at the next start.
CREATE TRIGGER users_name_key_insert AFTER INSERT ON users
WHEN NEW.name_key IS NULL
BEGIN
  SELECT RAISE(ABORT, 'users.name_key is required for non-ASCII names: create users through users::create')
  WHERE NEW.name GLOB '*[^ -~]*';
  UPDATE users SET name_key = lower(NEW.name) WHERE id = NEW.id;
END;
CREATE TRIGGER users_name_key_rename AFTER UPDATE OF name ON users
WHEN NEW.name != OLD.name AND NEW.name_key IS OLD.name_key AND NEW.name_key IS NOT NULL
  AND NEW.name NOT GLOB '*[^ -~]*'
BEGIN
  UPDATE users SET name_key = lower(NEW.name) WHERE id = NEW.id;
END;

-- 3. Durable one-time naming of the first user. Before this, "still the placeholder" was
--    decided by comparing the name with 'admin', so choosing the name Admin (which NOCASE
--    treats as the placeholder) left the account open to renaming on every later start.
--    name_confirmed records that the name was chosen, whatever its text. Any user whose name
--    is not the untouched placeholder (compared byte for byte) is already established.
ALTER TABLE users ADD COLUMN name_confirmed INTEGER NOT NULL DEFAULT 0
  CHECK (name_confirmed IN (0, 1));
-- The comparison is forced to BINARY: the column is NOCASE, which would treat a chosen name
-- 'Admin' as the placeholder.
UPDATE users SET name_confirmed = 1 WHERE NOT (id = 1 AND name = 'admin' COLLATE BINARY);
