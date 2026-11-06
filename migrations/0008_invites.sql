-- Invite codes for signup. Only the SHA-256 of a code is stored, like pairing codes and tokens.
--
-- A code is single use: signup claims it with one UPDATE ... WHERE used_at IS NULL, so two
-- concurrent requests cannot both succeed. Rows are kept after use, expiry or revocation as an
-- audit trail; a revoked invite is marked, not deleted.
CREATE TABLE invites (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  code_hash  TEXT NOT NULL UNIQUE,
  created_by INTEGER REFERENCES users (id) ON DELETE SET NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  used_at    INTEGER,
  used_by    INTEGER REFERENCES users (id) ON DELETE SET NULL,
  revoked_at INTEGER
);
CREATE INDEX invites_expires ON invites (expires_at);
