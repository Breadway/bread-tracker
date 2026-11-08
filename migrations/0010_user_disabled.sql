-- A user can be disabled without deleting anything: their devices stay, their data stays, but
-- every authenticated request is refused and no reminders are sent until they are enabled again.
ALTER TABLE users ADD COLUMN disabled_at INTEGER;
