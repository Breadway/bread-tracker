-- Migration 0001 seeded the owner's own timezone (Australia/Perth) for everyone, and 0003 copied
-- it to the admin's per-user settings. On a brand new installation that is wrong for whoever runs
-- it, so a database that has never had a device (it was created just now, by running every
-- migration at once) gets the neutral default instead. A deployment that is in use always has a
-- device and keeps whatever timezone it had.
UPDATE user_settings
SET timezone = 'UTC'
WHERE user_id = 1
  AND timezone = 'Australia/Perth'
  AND NOT EXISTS (SELECT 1 FROM devices);
