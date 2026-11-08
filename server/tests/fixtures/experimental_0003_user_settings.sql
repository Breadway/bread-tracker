-- Per-user settings replace the global key/value `settings` table.
--
-- The admin (user 1) inherits whatever the global table held, so an existing deployment keeps its
-- timezone, quiet hours and reminder switch. Every user created afterwards gets a row from the
-- trigger below with the column defaults: UTC, no quiet hours, reminders on. The signup flow can
-- set the phone's timezone on that row in the same transaction that creates the user.

CREATE TABLE user_settings (
  user_id           INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
  timezone          TEXT    NOT NULL DEFAULT 'UTC',
  quiet_start       TEXT    NOT NULL DEFAULT '',   -- local HH:MM, '' = no quiet hours
  quiet_end         TEXT    NOT NULL DEFAULT '',
  reminders_enabled INTEGER NOT NULL DEFAULT 1 CHECK (reminders_enabled IN (0, 1))
);

INSERT INTO user_settings (user_id, timezone, quiet_start, quiet_end, reminders_enabled)
SELECT
  u.id,
  COALESCE((SELECT value FROM settings WHERE key = 'timezone'), 'UTC'),
  COALESCE((SELECT value FROM settings WHERE key = 'quiet_start'), ''),
  COALESCE((SELECT value FROM settings WHERE key = 'quiet_end'), ''),
  CASE COALESCE((SELECT value FROM settings WHERE key = 'reminders_enabled'), '1') WHEN '1' THEN 1 ELSE 0 END
FROM users u
WHERE u.is_admin = 1;

-- Any non-admin user that already exists (none do before this migration, but be safe) gets defaults.
INSERT INTO user_settings (user_id) SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM user_settings);

CREATE TRIGGER users_seed_settings AFTER INSERT ON users
BEGIN
  INSERT OR IGNORE INTO user_settings (user_id) VALUES (NEW.id);
END;

DROP TABLE settings;
