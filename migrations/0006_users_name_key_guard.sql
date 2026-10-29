-- Forward repair of the name-key guard added by 0005. Additive: 0005 is already applied on
-- real databases and is not edited. Only two triggers change.
--
-- The 0005 rename trigger skipped non-ASCII names, so a plain SQL rename such as
--   UPDATE users SET name = 'Älex' WHERE name = 'Sam'
-- left the old key ('sam') in place: the new name could not be found, a second 'älex' could
-- be created, and a restart accepted the duplicates. SQLite cannot case fold Unicode, so the
-- database cannot compute the right key itself. The supported behavior is now:
--
--   * rename to a printable ASCII name: the key is re-derived (plain lower case), as before;
--   * rename to any other name without also changing name_key: refused. Rename through
--     users::rename, which sets the key, or set name_key = NULL together with the new name
--     while the server is stopped; the server recomputes it at the next start;
--   * a key that is wrong anyway (written by hand, or stale from before this migration) is
--     found by the full validation the server runs at every start (users::reconcile_name_keys),
--     which repairs stale keys and refuses to start if two users would share one.
DROP TRIGGER users_name_key_rename;

CREATE TRIGGER users_name_key_rename_ascii AFTER UPDATE OF name ON users
WHEN NEW.name != OLD.name AND NEW.name_key IS OLD.name_key AND NEW.name_key IS NOT NULL
  AND NEW.name NOT GLOB '*[^ -~]*'
BEGIN
  UPDATE users SET name_key = lower(NEW.name) WHERE id = NEW.id;
END;

CREATE TRIGGER users_name_key_rename_unicode AFTER UPDATE OF name ON users
WHEN NEW.name != OLD.name AND NEW.name_key IS OLD.name_key AND NEW.name_key IS NOT NULL
  AND NEW.name GLOB '*[^ -~]*'
BEGIN
  SELECT RAISE(ABORT, 'users.name_key must change with a non-ASCII name: rename users through users::rename (or set name_key = NULL with the new name while the server is stopped)');
END;
