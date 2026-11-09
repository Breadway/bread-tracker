-- TRACK-56: family codes and their per-code attempt budget.
--
-- A family code is a short-lived, single-use offer from a child to any parent. The child
-- creates it from Settings; a parent redeems it to create a pending family link (see
-- 0015_family.sql). The link becomes active only when the child accepts it, so a parent can
-- never link unilaterally. Only the SHA-256 of a code is stored here, never the code itself.
--
-- Codes are deliberately separate from pairing_codes: a pairing code adds a device to an
-- existing account, while a family code authorises a parent/child link request and must not
-- be usable on /pair. Codes are single use (`used_at`), expire after 15 minutes (`expires_at`)
-- and can be withdrawn by the child (`revoked_at`). `used_by` is the redeeming parent; it is
-- cleared rather than cascading when that account is deleted, because the code row is a
-- history record and the child still owns it. Deleting the child removes their codes.
--
-- `family_code_attempts` is the durable per-code guessing budget. It is keyed by the hash of
-- the submitted code, not by source, so it cannot be confused with the shared per-source
-- pairing/signup admission in `public_auth_limits`. The row is deleted on a successful
-- redemption, so a success never spends a failure. `window_started_at` allows expired rows to
-- be swept and caps the table at one row per code hash that is currently guessing.

CREATE TABLE family_codes (
  code_hash   TEXT PRIMARY KEY,
  child_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  created_at  INTEGER NOT NULL,
  expires_at  INTEGER NOT NULL,
  used_at     INTEGER,
  used_by     INTEGER REFERENCES users (id) ON DELETE SET NULL,
  revoked_at  INTEGER,
  CHECK (expires_at > created_at)
);

CREATE INDEX family_codes_expires_at ON family_codes (expires_at);
CREATE INDEX family_codes_child ON family_codes (child_id, expires_at);

CREATE TABLE family_code_attempts (
  code_hash          TEXT PRIMARY KEY,
  failures           INTEGER NOT NULL CHECK (failures BETWEEN 1 AND 5),
  window_started_at  INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL,
  CHECK (window_started_at <= updated_at)
);

CREATE INDEX family_code_attempts_window ON family_code_attempts (window_started_at);
