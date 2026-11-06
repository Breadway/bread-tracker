-- Shared bounded admission for pairing and invite signup, including malformed requests.
CREATE TABLE public_auth_limits (
  source_ip TEXT PRIMARY KEY,
  failures INTEGER NOT NULL CHECK (failures BETWEEN 1 AND 5),
  window_started_at INTEGER NOT NULL
);
CREATE INDEX public_auth_limits_expiry ON public_auth_limits(window_started_at);
