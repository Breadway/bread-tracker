CREATE INDEX IF NOT EXISTS items_kind_id ON items (kind, id);
CREATE INDEX IF NOT EXISTS completions_date_item ON completions (date, item_id);
CREATE INDEX IF NOT EXISTS completions_live_date ON completions (deleted_at, date);
