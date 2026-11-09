-- TRACK-72: an incremental ledger for XP, perfect days and streaks.
--
-- `/stats` used to load every retained item and completion on every request. The rules need
-- the whole history, so a scan cannot simply be bounded: instead this ledger keeps the exact
-- aggregates the rules produce and is updated in the same transaction as every sync write.
-- The tables hold no user-visible history and nothing here is ever pruned.
--
-- `stats_item_date` is the set of calendar dates on which an item has at least one live
-- completion, keyed by the canonical `YYYY-MM-DD` the shared parser yields. `live_rows` counts
-- the stored raw completion rows behind that date (an item may hold a tombstone and a live row
-- only for the same raw key, but two encodings of one date are still one day). A row exists only
-- while that count is positive.
--
-- `stats_day.done_scheduled` is the number of distinct live, active chore/habit items that are
-- scheduled on that date and done on it. A day is perfect when that count is positive and equals
-- the number of items scheduled on that exact date. A day row exists only while its count is
-- positive.
--
-- `stats_schedule` is the per-weekday count of live active chore/habit items, including items
-- that have a creation date. `stats_schedule_created` splits the same count by the item's
-- immutable `created_on`. An item counts toward a date only from the day AFTER it was created
-- (TRACK-65), so the scheduled count for date D is `stats_schedule[weekday(D)]` minus every
-- `stats_schedule_created` row for that weekday whose creation date is on or after D. Rows are
-- keyed by (weekday, created_on), not by item or completion, so the correction is exact and
-- bounded by the number of distinct live creation dates. A legacy item with NULL created_on
-- has no lower bound and never appears in `stats_schedule_created`. Because `created_on` is
-- immutable after creation, this bound never moves for an existing item.
--
-- `stats_user.xp_base` is the sum of `points * number of live completion dates` over live items.
-- Perfect-day bonuses are added at read time, so no bonus is ever stored twice. Base XP and
-- streaks use every live completion date; only the schedule membership is bounded by creation.
--
-- `stats_state` says whether a user's ledger has been built from their retained history. Every
-- user that exists when this migration runs starts at 0 and is rebuilt once at the next start,
-- in one transaction per user, before the server listens. A user created afterwards starts empty
-- and is maintained by sync from their first row, so the trigger marks them built. Nothing is
-- rebuilt on a normal start, and a failed or interrupted rebuild leaves the marker at 0 so the
-- next start retries it; no history is read or written by the rebuild beyond the ledger tables.
--
-- 0013 added `items.created_on`; this ledger applies the same creation bound to schedule
-- membership, so the two branches are reconciled rather than merged independently.

CREATE TABLE stats_item_date (
  user_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  item_id   TEXT NOT NULL,
  date      TEXT NOT NULL,                 -- canonical YYYY-MM-DD
  live_rows INTEGER NOT NULL CHECK (live_rows > 0),
  PRIMARY KEY (user_id, item_id, date)
);

CREATE TABLE stats_day (
  user_id        INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  date           TEXT NOT NULL,            -- canonical YYYY-MM-DD
  done_scheduled INTEGER NOT NULL CHECK (done_scheduled > 0),
  PRIMARY KEY (user_id, date)
);

CREATE TABLE stats_schedule (
  user_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  weekday   INTEGER NOT NULL CHECK (weekday BETWEEN 0 AND 6),  -- bit0 of an item's mask
  scheduled INTEGER NOT NULL CHECK (scheduled >= 0),
  PRIMARY KEY (user_id, weekday)
);

CREATE TABLE stats_schedule_created (
  user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  weekday    INTEGER NOT NULL CHECK (weekday BETWEEN 0 AND 6),
  created_on TEXT NOT NULL,                -- canonical YYYY-MM-DD, immutable per item
  scheduled  INTEGER NOT NULL CHECK (scheduled > 0),
  PRIMARY KEY (user_id, weekday, created_on)
);

CREATE TABLE stats_user (
  user_id  INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  xp_base  INTEGER NOT NULL
);

CREATE TABLE stats_state (
  user_id INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  built   INTEGER NOT NULL DEFAULT 0 CHECK (built IN (0, 1))
);

-- Every account that already has history must be backfilled once.
INSERT INTO stats_state (user_id, built) SELECT id, 0 FROM users;

-- A new account has no history, so its ledger is already exact (empty).
CREATE TRIGGER users_seed_stats AFTER INSERT ON users
BEGIN
  INSERT OR IGNORE INTO stats_state (user_id, built) VALUES (NEW.id, 1);
END;
