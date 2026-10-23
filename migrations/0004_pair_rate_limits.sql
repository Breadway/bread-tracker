-- Durable per-source pairing failure budgets. TRACK-17 reserves 0002 for the
-- users migration and TRACK-8 reserves 0003 for server hygiene on stacked PRs.
-- The API enforces a 1,024-row cap with BEGIN IMMEDIATE admission transactions.
CREATE TABLE pair_rate_limits (
  source_ip TEXT PRIMARY KEY,
  failures INTEGER NOT NULL CHECK (failures BETWEEN 1 AND 5),
  window_started_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  CHECK (window_started_at <= updated_at)
);

CREATE INDEX pair_rate_limits_window_started_at ON pair_rate_limits (window_started_at);
