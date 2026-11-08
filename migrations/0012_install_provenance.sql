-- TRACK-82: record whether this database was an empty file before the migration set ran, and
-- use that provenance for the neutral timezone default.
--
-- The rejected 0005_neutral_default_timezone.sql decided "new installation" from the current
-- device count. That is not provenance: a household can revoke or lose every device and still
-- keep its timezone, quiet hours and reminder history. An owner with no devices left is not a
-- new installation.
--
-- db::prepare records the only reliable fresh-vs-populated fact, whether the database had no
-- user tables when migrations began, BEFORE any migration applies. A ledger built without that
-- helper (a direct sqlx::migrate!, an older release, a test fixture) has no such record; treat
-- it as populated so no adopted settings are overwritten. This file only reads that record and
-- applies the repair, so the decision is a durable migration marker rather than a per-start
-- guess at a schema that migrations have already rewritten.
--
-- Versions 0008 to 0011 belong to the multi-user stack, 0004 is reserved for the durable
-- pairing limiter and is not shipped, and the owner-approved rule in DESIGN.md is that a later
-- schema change takes 0012 or above. This file therefore uses 0012 and reuses no version that
-- an experimental 003/004 ledger already recorded.

CREATE TABLE IF NOT EXISTS install_provenance (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL CHECK (value IN ('fresh', 'populated'))
);

-- A database whose prepared start did not record provenance is conservatively populated.
INSERT OR IGNORE INTO install_provenance (key, value) VALUES ('install_kind', 'populated');

-- Only a database that existed solely as an empty file when this set began gets the neutral
-- default. Everything else keeps the values 0007 adopted from it, including an owner with no
-- devices left. 0007's already published bytes are never edited; this is the forward repair.
UPDATE user_settings
SET timezone = 'UTC'
WHERE user_id = 1
  AND timezone = 'Australia/Perth'
  AND (SELECT value FROM install_provenance WHERE key = 'install_kind') = 'fresh';
