-- TRACK-61: durable queue for optional parent completion alerts.
--
-- When a child marks a parent-assigned item done and the link has `notify_on_complete` on, the
-- sync transaction inserts one row here. The row is the dedupe record: the unique key is the
-- completion identity, so a retried sync, a re-tick or a crash between enqueue and send can
-- never produce a second message for the same (parent, child, item, date).
--
-- `created_at` is the server time the completion was accepted. The sender groups every pending
-- row for one parent at or before a short deterministic cutoff into a single message, so a
-- child ticking several items in one sync sends one notification. `sent_at` is set when the
-- alert leaves the queue: after successful delivery to at least one parent device, or when the
-- send-time checks deliberately suppress it (link no longer active, notify flag off, the item
-- no longer parent-created, the parent's reminders off, or quiet hours). Suppression is final
-- rather than deferred, and the retained row keeps its dedupe key.
--
-- No tokens are stored here. The payload is built from the joined item name at send time and
-- never carries the child's own item names, so a private item cannot reach the parent.

CREATE TABLE family_alerts (
  id         INTEGER PRIMARY KEY,
  parent_id  INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  child_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  item_id    TEXT NOT NULL,
  date       TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  sent_at    INTEGER,
  UNIQUE (parent_id, child_id, item_id, date)
);

-- The sender asks only for one parent's unresolved rows in creation order.
CREATE INDEX family_alerts_pending ON family_alerts (parent_id, created_at)
  WHERE sent_at IS NULL;

-- The unique key already serves the parent side; this one serves the child's cascade and any
-- future per-child inspection.
CREATE INDEX family_alerts_child ON family_alerts (child_id);
