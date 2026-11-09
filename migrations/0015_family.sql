-- TRACK-55: family links, item provenance and per-item permission overrides.
--
-- A family link is one row per parent/child pair. Only the child can accept it, so a link
-- starts pending; permissions are the link's default unless an item overrides it. The schema
-- here is deliberately behavior-neutral: no route reads or writes these tables yet, and the
-- existing sync path keeps working exactly as before.
--
-- `items.created_by` records who made an item. For every item that already exists the creator
-- is the owner, so the backfill below leaves every current behavior unchanged. The column is
-- nullable and SET NULL on user deletion, never CASCADE: deleting a parent must not delete a
-- child's chores, only forget who assigned them. A parent-added item is stored under the
-- child's `user_id` with `created_by` set to the parent; the child's own sync cursor delivers
-- it through the normal pull.
--
-- Clients never supply provenance or the override. The server sets `created_by` from the
-- authenticated owner on a normal sync write, and the dedicated family routes will set the
-- parent when they arrive. `permission_override` is NULL until an item override is set, which
-- means "use the link default". The items index serves a parent listing their assigned items
-- for one child and delta pulls through the same sequence cursor.

CREATE TABLE family_links (
  id                  INTEGER PRIMARY KEY,
  parent_id           INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  child_id            INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  status              TEXT NOT NULL CHECK (status IN ('pending', 'active', 'revoked')),
  default_permission  TEXT NOT NULL DEFAULT 'complete_only'
                        CHECK (default_permission IN ('complete_only', 'complete_and_reschedule', 'full')),
  notify_on_complete  INTEGER NOT NULL DEFAULT 0 CHECK (notify_on_complete IN (0, 1)),
  created_at          INTEGER NOT NULL,
  accepted_at         INTEGER,
  revoked_at          INTEGER,
  pending_expires_at  INTEGER,
  UNIQUE (parent_id, child_id),
  CHECK (parent_id != child_id)
);

-- The child's side lists who can assign to them; the unique pair index already serves the
-- parent's side.
CREATE INDEX family_links_child ON family_links (child_id, status);

ALTER TABLE items ADD COLUMN created_by INTEGER REFERENCES users (id) ON DELETE SET NULL;
ALTER TABLE items ADD COLUMN permission_override TEXT
  CHECK (permission_override IN ('complete_only', 'complete_and_reschedule', 'full'));

-- Every item that predates this migration was made by its owner, so nothing changes for
-- existing users. Later assignments are server-written provenance.
UPDATE items SET created_by = user_id;

CREATE INDEX items_user_created_by_seq ON items (user_id, created_by, seq);
