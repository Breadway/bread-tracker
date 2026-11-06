-- TRACK-17 independently uses numeric SQLx version 0002 for users. Use the next forward
-- migration version to avoid a collision when the branches are integrated.
CREATE INDEX IF NOT EXISTS items_kind_id ON items (kind, id);
CREATE INDEX IF NOT EXISTS items_reminder_candidates ON items (remind_time)
  WHERE active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL;
CREATE INDEX IF NOT EXISTS completions_date_item ON completions (date, item_id);
CREATE INDEX IF NOT EXISTS completions_live_date ON completions (deleted_at, date);
