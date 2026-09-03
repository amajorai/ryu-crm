/// The complete v1 schema.
pub(super) const SCHEMA_VERSION: i32 = 1;
///
/// Collapsed into ONE statement batch rather than replayed as a migration history,
/// because there are no existing databases to migrate — this app has never shipped.
/// Every table is declared in its final shape.
pub(super) const V1_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS objects (
  id             TEXT PRIMARY KEY,
  slug           TEXT NOT NULL UNIQUE,
  singular       TEXT NOT NULL,
  plural         TEXT NOT NULL,
  icon           TEXT,
  description    TEXT,
  title_field_id TEXT,
  is_standard    INTEGER NOT NULL DEFAULT 0,
  position       INTEGER NOT NULL DEFAULT 0,
  created_at     TEXT NOT NULL,
  updated_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS fields (
  id          TEXT PRIMARY KEY,
  object_id   TEXT NOT NULL,
  -- NULL for an object field, set for a LIST-specific field. See models::Field.
  list_id     TEXT,
  slug        TEXT NOT NULL,
  name        TEXT NOT NULL,
  field_type  TEXT NOT NULL,
  config      TEXT NOT NULL DEFAULT '{}',
  description TEXT,
  is_required INTEGER NOT NULL DEFAULT 0,
  is_unique   INTEGER NOT NULL DEFAULT 0,
  is_system   INTEGER NOT NULL DEFAULT 0,
  position    INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
-- TWO PARTIAL indices, not one `UNIQUE(object_id, list_id, slug)`. SQLite treats
-- NULLs as distinct in a UNIQUE constraint, so the three-column version would happily
-- accept ('obj_deal', NULL, 'name') twice and enforce nothing at all for object
-- fields — which is every field that matters.
CREATE UNIQUE INDEX IF NOT EXISTS idx_fields_object_slug
  ON fields(object_id, slug) WHERE list_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_fields_list_slug
  ON fields(list_id, slug) WHERE list_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_fields_object ON fields(object_id, position);

CREATE TABLE IF NOT EXISTS records (
  id         TEXT PRIMARY KEY,
  object_id  TEXT NOT NULL,
  title      TEXT NOT NULL DEFAULT '',
  -- The value bag. Named `data`, not `values`: VALUES is a SQL keyword.
  data       TEXT NOT NULL DEFAULT '{}',
  deleted_at TEXT,
  created_by TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
-- The hot predicate for every table/board query.
CREATE INDEX IF NOT EXISTS idx_records_object_live
  ON records(object_id, deleted_at, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_records_object_title
  ON records(object_id, title COLLATE NOCASE);

CREATE TABLE IF NOT EXISTS record_links (
  id               TEXT PRIMARY KEY,
  field_id         TEXT NOT NULL,
  source_record_id TEXT NOT NULL,
  source_object_id TEXT NOT NULL,
  target_record_id TEXT NOT NULL,
  target_object_id TEXT NOT NULL,
  created_at       TEXT NOT NULL
);
-- ONE row per edge, read from both ends via these two indices. See
-- models::RecordLink for why a mirrored second row would be worse.
CREATE UNIQUE INDEX IF NOT EXISTS idx_record_links_edge
  ON record_links(field_id, source_record_id, target_record_id);
CREATE INDEX IF NOT EXISTS idx_record_links_source ON record_links(source_record_id);
CREATE INDEX IF NOT EXISTS idx_record_links_target ON record_links(target_record_id);

CREATE TABLE IF NOT EXISTS views (
  id                TEXT PRIMARY KEY,
  object_id         TEXT NOT NULL,
  name              TEXT NOT NULL,
  kind              TEXT NOT NULL DEFAULT 'table',
  filter            TEXT,
  sorts             TEXT NOT NULL DEFAULT '[]',
  visible_fields    TEXT NOT NULL DEFAULT '[]',
  group_by_field_id TEXT,
  is_default        INTEGER NOT NULL DEFAULT 0,
  position          INTEGER NOT NULL DEFAULT 0,
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_views_object ON views(object_id, position);

CREATE TABLE IF NOT EXISTS lists (
  id          TEXT PRIMARY KEY,
  object_id   TEXT NOT NULL,
  name        TEXT NOT NULL,
  description TEXT,
  icon        TEXT,
  position    INTEGER NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lists_object ON lists(object_id, position);

CREATE TABLE IF NOT EXISTS list_entries (
  id         TEXT PRIMARY KEY,
  list_id    TEXT NOT NULL,
  record_id  TEXT NOT NULL,
  -- Keyed by LIST-field slug, a separate namespace from the record's own bag.
  data       TEXT NOT NULL DEFAULT '{}',
  position   INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_list_entries_membership
  ON list_entries(list_id, record_id);
CREATE INDEX IF NOT EXISTS idx_list_entries_list ON list_entries(list_id, position);
CREATE INDEX IF NOT EXISTS idx_list_entries_record ON list_entries(record_id);

CREATE TABLE IF NOT EXISTS activities (
  id               TEXT PRIMARY KEY,
  record_id        TEXT,
  object_id        TEXT,
  kind             TEXT NOT NULL,
  title            TEXT NOT NULL DEFAULT '',
  body             TEXT,
  field_id         TEXT,
  from_value       TEXT,
  to_value         TEXT,
  assignee         TEXT,
  due_at           TEXT,
  completed_at     TEXT,
  due_notified_at  TEXT,
  author           TEXT,
  metadata         TEXT,
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_activities_record ON activities(record_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_activities_feed ON activities(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_activities_object_kind
  ON activities(object_id, kind, created_at DESC);
-- Serves the due sweep's `kind='task' AND completed_at IS NULL AND due_at <= ?`
-- scan. Without it that runs the whole activity table once a minute, forever.
CREATE INDEX IF NOT EXISTS idx_activities_due
  ON activities(kind, completed_at, due_notified_at, due_at);

CREATE TABLE IF NOT EXISTS import_jobs (
  id          TEXT PRIMARY KEY,
  object_id   TEXT NOT NULL,
  filename    TEXT,
  status      TEXT NOT NULL DEFAULT 'draft',
  delimiter   TEXT NOT NULL DEFAULT ',',
  has_header  INTEGER NOT NULL DEFAULT 1,
  row_count   INTEGER NOT NULL DEFAULT 0,
  columns     TEXT NOT NULL DEFAULT '[]',
  mappings    TEXT NOT NULL DEFAULT '[]',
  dedupe      TEXT NOT NULL DEFAULT '{}',
  preview     TEXT,
  result      TEXT,
  error       TEXT,
  -- The uploaded file, verbatim. Preview and apply MUST see identical bytes: a
  -- preview computed over a re-uploaded file is not a preview of what will happen.
  raw_csv     TEXT NOT NULL DEFAULT '',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_import_jobs_object ON import_jobs(object_id, created_at DESC);

-- Full-text index over every record. A CONTENTLESS-adjacent plain fts5 table whose
-- rowid is `records.rowid`, so delete-then-reinsert on every update is an O(log n)
-- rowid lookup rather than the full index scan a `WHERE record_id = ?` on an
-- UNINDEXED column would force. `records` must therefore never become WITHOUT ROWID.
CREATE VIRTUAL TABLE IF NOT EXISTS records_fts USING fts5(
  title,
  body,
  tokenize = 'unicode61 remove_diacritics 2'
);
"#;
