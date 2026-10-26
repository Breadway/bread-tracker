-- Durable per-source pairing failure budgets. Admission, code validation,
-- and outcome accounting are serialized in one BEGIN IMMEDIATE transaction.
CREATE TABLE pair_rate_limits (
  source_ip TEXT PRIMARY KEY,
  failures INTEGER NOT NULL CHECK (failures BETWEEN 1 AND 5),
  window_started_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  CHECK (window_started_at <= updated_at)
);
CREATE INDEX pair_rate_limits_window_started_at ON pair_rate_limits (window_started_at);
