-- Per-device budgets persist across server processes and restart. Device removal
-- deletes its budget with it; inactive windows are swept during admission.
CREATE TABLE authenticated_rate_limits (
  device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
  requests INTEGER NOT NULL CHECK(requests > 0),
  window_started_at INTEGER NOT NULL
);
CREATE INDEX authenticated_rate_limits_expiry ON authenticated_rate_limits(window_started_at);
