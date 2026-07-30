-- NookOS SQLite schema — the chat track's frozen 0001.
--
-- SCAFFOLDED ONCE from the schema the Postgres migrations actually produce
-- (MAIN-236), then hand-corrected and frozen. It is HAND-OWNED from here: the
-- generator that produced it was deleted in the same PR, and nothing
-- regenerates over this file. Forward changes are hand-authored SQLite deltas
-- (0002_…), the twin of the Postgres migration that goes with them.
--
-- Type map (docs/db-dialect-audit.md): uuid / timestamptz / jsonb / text -> TEXT,
-- bigint & friends -> INTEGER, boolean -> INTEGER (0/1), now() ->
-- CURRENT_TIMESTAMP, ::type casts stripped. SQLite's dynamic typing makes TEXT
-- for uuid/timestamptz faithful in practice: the values round-trip as the same
-- strings Postgres renders.

CREATE TABLE IF NOT EXISTS chat_channel_categories (
  id TEXT NOT NULL,
  owner_type TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  name TEXT NOT NULL,
  "position" INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS chat_channel_members (
  channel_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  joined_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (channel_id, user_id),
  FOREIGN KEY (channel_id) REFERENCES chat_channels (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS chat_channel_participants (
  channel_id TEXT NOT NULL,
  person_id TEXT NOT NULL,
  PRIMARY KEY (channel_id, person_id),
  FOREIGN KEY (channel_id) REFERENCES chat_channels (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS chat_channels (
  id TEXT NOT NULL,
  owner_type TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  archived_at TEXT,
  category_id TEXT,
  "position" INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  UNIQUE (owner_type, owner_id, slug),
  CHECK ((owner_type IN ('org', 'tenant', 'dm'))),
  FOREIGN KEY (category_id) REFERENCES chat_channel_categories (id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS chat_message_revisions (
  id TEXT NOT NULL,
  message_id TEXT NOT NULL,
  prior_content TEXT NOT NULL,
  action TEXT NOT NULL,
  acted_by TEXT NOT NULL,
  acted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  CHECK ((action IN ('edit', 'delete'))),
  FOREIGN KEY (message_id) REFERENCES chat_messages (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS chat_messages (
  id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  author_id TEXT NOT NULL,
  tenant_id TEXT NOT NULL,
  body TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  parent_message_id TEXT,
  edited_at TEXT,
  deleted_at TEXT,
  PRIMARY KEY (id),
  FOREIGN KEY (channel_id) REFERENCES chat_channels (id) ON DELETE CASCADE,
  FOREIGN KEY (parent_message_id) REFERENCES chat_messages (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS chat_reactions (
  message_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  emoji TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (message_id, user_id, emoji),
  FOREIGN KEY (message_id) REFERENCES chat_messages (id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS chat_read_cursors (
  channel_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  last_read_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (channel_id, user_id),
  FOREIGN KEY (channel_id) REFERENCES chat_channels (id) ON DELETE CASCADE
);

-- Indexes.
CREATE INDEX chat_channel_categories_owner_idx ON chat_channel_categories (owner_type, owner_id, "position");
CREATE INDEX chat_channel_participants_person_idx ON chat_channel_participants (person_id);
CREATE INDEX chat_message_revisions_message_idx ON chat_message_revisions (message_id, id);
CREATE INDEX chat_messages_channel_idx ON chat_messages (channel_id, id);
CREATE INDEX chat_messages_parent_idx ON chat_messages (parent_message_id, id) WHERE (parent_message_id IS NOT NULL);
CREATE INDEX chat_reactions_message_idx ON chat_reactions (message_id);
CREATE INDEX chat_read_cursors_user_idx ON chat_read_cursors (user_id, channel_id);

-- Seed rows the Postgres migrations insert.
