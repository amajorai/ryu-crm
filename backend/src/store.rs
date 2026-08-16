//! SQLite persistence for the whole Harbor spine (`~/.ryu/crm.db`).
//!
//! ## The store owns the rules, not the routers
//!
//! Six later-owned router modules sit on top of this file. Every rule that could be
//! got subtly wrong twice — value validation, relation edge maintenance, FTS index
//! maintenance, dedupe matching, merge resolution — lives HERE, once, and the
//! routers are thin. That is why `dry_run_import` / `apply_import` and
//! `merge_records` are store functions rather than handler logic: an import that
//! validates differently from a form save is a data-corruption bug that no test in
//! the handler would catch.
//!
//! ## Validation is non-throwing: the `Validated<T>` shape
//!
//! Write functions return [`Validated<T>`] = `Result<Result<T, Vec<FieldValidationError>>>`.
//! The OUTER `Result` is infrastructure failure (→ 500). The INNER one is "your
//! values were rejected" (→ 422, via `ApiError::validation`). They are separated
//! because a CSV import needs to collect per-row rejections for 10 000 rows without
//! unwinding, and a record form needs every bad cell at once — not the first one.
//!
//! ## No foreign keys — a deliberate choice, not an omission
//!
//! `PRAGMA foreign_keys` is **per-connection and not persisted in the file**, so a
//! schema with real `ON DELETE CASCADE` behaves differently depending on which code
//! path opened the connection — silent orphans on one, cascades on the other. That
//! failure mode is invisible until data is already lost. Deletes instead run an
//! explicit ordered cascade inside a transaction, which is auditable and
//! connection-independent.
//!
//! ## Uniqueness is checked in the transaction, not by an index
//!
//! A `is_unique` field could be enforced with `CREATE UNIQUE INDEX … ON records(
//! json_extract(data, '$.slug'))`. It is not, for two reasons: that is runtime DDL
//! driven by a user-supplied expression, and it cannot express "ignore empty values"
//! or "ignore soft-deleted rows" without a partial-index predicate that has to be
//! rebuilt whenever either rule changes. Instead the write transaction does a
//! `SELECT … LIMIT 1` before inserting. The race that would normally make this
//! unsound is closed by the single-writer mutex below: no second writer exists.
//!
//! ## Locking
//!
//! One `Arc<tokio::sync::Mutex<Connection>>` (the async mutex, matching
//! `ryu-social` / `ryu-teams`) — a single writer with WAL underneath. `busy_timeout`
//! still matters because WAL admits readers from OTHER processes (a `sqlite3` shell,
//! a backup), about which this process's mutex knows nothing.
//!
//! **Every public method locks exactly once.** The internal helpers all take
//! `&Connection` (a `Transaction` derefs to one), so a public method never calls
//! another public method — `tokio::sync::Mutex` is not reentrant and doing so would
//! deadlock the process, not error.
//!
//! ## Why the value column is called `data`
//!
//! `VALUES` is a SQL keyword. Naming the column `values` would mean quoting it in
//! every statement in this file, and the one place someone forgot would be a syntax
//! error found at runtime. The wire name stays `values`; only the column differs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::models::*;

/// Outer `Result` = infrastructure failure. Inner = the caller's values were
/// rejected. See the module docs.
pub type Validated<T> = Result<std::result::Result<T, Vec<FieldValidationError>>>;

/// The schema version this build expects. Bump it and add a `v<N>` arm in
/// [`CrmStore::migrate`] when the shape changes.
const SCHEMA_VERSION: i32 = 1;

/// SQLite-backed store for the CRM spine. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct CrmStore {
    conn: Arc<Mutex<Connection>>,
}

impl CrmStore {
    /// Open (creating if needed) the DB at `path`, migrate it, and seed the standard
    /// schema. The path is injected by the caller (`paths::crm_db_path()`) so this
    /// module has no opinion about where the node's data lives.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating parent dir for crm.db")?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("opening crm db at {}", path.display()))?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory store. A plain `pub fn`, not `#[cfg(test)]`, so the later agents'
    /// module tests can build a real seeded store without a temp file.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::prepare(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Pragmas, then migrations, then seeds. Both open paths call this so an
    /// in-memory store is byte-for-byte the same schema as a real one — a divergence
    /// here would make every module test a lie.
    fn prepare(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )
        .context("applying crm db pragmas")?;
        Self::migrate(conn)?;
        seed_standard_schema(conn).context("seeding the standard CRM schema")
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current < 1 {
            conn.execute_batch(V1_DDL)
                .context("applying crm schema v1")?;
        }
        if current < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .context("stamping crm schema version")?;
        }
        Ok(())
    }
}

/// The complete v1 schema.
///
/// Collapsed into ONE statement batch rather than replayed as a migration history,
/// because there are no existing databases to migrate — this app has never shipped.
/// Every table is declared in its final shape.
const V1_DDL: &str = r#"
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

// ── Seeds ──────────────────────────────────────────────────────────────────────
//
// Every seeded row uses a DETERMINISTIC id from `models` and is written with
// `INSERT OR IGNORE`, so seeding is idempotent: it runs on every open and is a
// no-op on an existing database. That is also what makes it safe for the seed set
// to GROW in a later version — new rows appear, existing ones are never rewritten,
// and a user's edits to a standard field's name or options survive an upgrade.

/// One seed field, in the compact form the tables below are written in.
struct SeedField {
    id: &'static str,
    slug: &'static str,
    name: &'static str,
    field_type: FieldType,
    required: bool,
    unique: bool,
    system: bool,
    config: FieldConfig,
}

fn sf(
    id: &'static str,
    slug: &'static str,
    name: &'static str,
    field_type: FieldType,
) -> SeedField {
    SeedField {
        id,
        slug,
        name,
        field_type,
        required: false,
        unique: false,
        system: false,
        config: FieldConfig::default(),
    }
}

impl SeedField {
    /// The object's display-name field: required and undeletable.
    fn title(mut self) -> Self {
        self.required = true;
        self.system = true;
        self
    }

    fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    fn required(mut self) -> Self {
        self.required = true;
        self
    }

    fn options(mut self, options: Vec<SelectOption>) -> Self {
        self.config.options = options;
        self
    }

    fn relation(mut self, target: &str, multiple: bool, inverse: &str) -> Self {
        self.config.relation_object_id = Some(target.to_string());
        self.config.relation_multiple = multiple;
        self.config.relation_inverse_label = Some(inverse.to_string());
        self
    }

    fn currency(mut self, code: &str) -> Self {
        self.config.currency_code = Some(code.to_string());
        self
    }
}

fn seed_objects() -> Vec<(Object, Vec<SeedField>, Vec<View>)> {
    let now = now_rfc3339();
    let object = |id: &str,
                  slug: &str,
                  singular: &str,
                  plural: &str,
                  icon: &str,
                  title: &str,
                  position: i64| Object {
        id: id.to_string(),
        slug: slug.to_string(),
        singular: singular.to_string(),
        plural: plural.to_string(),
        icon: Some(icon.to_string()),
        description: None,
        title_field_id: Some(title.to_string()),
        is_standard: true,
        position,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let view = |id: &str,
                object_id: &str,
                name: &str,
                kind: ViewKind,
                visible: &[&str],
                group_by: Option<&str>,
                is_default: bool,
                position: i64| View {
        id: id.to_string(),
        object_id: object_id.to_string(),
        name: name.to_string(),
        kind,
        filter: None,
        sorts: vec![ViewSort::desc("updated_at")],
        visible_field_ids: visible.iter().map(|s| (*s).to_string()).collect(),
        group_by_field_id: group_by.map(str::to_string),
        is_default,
        position,
        created_at: now.clone(),
        updated_at: now.clone(),
    };

    vec![
        (
            object(
                OBJ_COMPANY,
                "company",
                "Company",
                "Companies",
                "building-2",
                FLD_COMPANY_NAME,
                0,
            ),
            vec![
                sf(FLD_COMPANY_NAME, "name", "Name", FieldType::Text).title(),
                sf(FLD_COMPANY_DOMAIN, "domain", "Domain", FieldType::Text),
                sf(FLD_COMPANY_WEBSITE, "website", "Website", FieldType::Url),
                sf(FLD_COMPANY_STATUS, "status", "Status", FieldType::Status).options(vec![
                    SelectOption::new(OPT_COMPANY_STATUS_LEAD, "Lead", 0).with_color("slate"),
                    SelectOption::new(OPT_COMPANY_STATUS_PROSPECT, "Prospect", 1)
                        .with_color("blue"),
                    SelectOption::new(OPT_COMPANY_STATUS_CUSTOMER, "Customer", 2)
                        .with_color("green")
                        .won(),
                    SelectOption::new(OPT_COMPANY_STATUS_CHURNED, "Churned", 3)
                        .with_color("red")
                        .lost(),
                ]),
                sf(
                    FLD_COMPANY_INDUSTRY,
                    "industry",
                    "Industry",
                    FieldType::Select,
                )
                .options(vec![
                    SelectOption::new("opt_company_industry_saas", "SaaS", 0),
                    SelectOption::new("opt_company_industry_fintech", "Fintech", 1),
                    SelectOption::new("opt_company_industry_healthcare", "Healthcare", 2),
                    SelectOption::new("opt_company_industry_ecommerce", "E-commerce", 3),
                    SelectOption::new("opt_company_industry_manufacturing", "Manufacturing", 4),
                    SelectOption::new("opt_company_industry_media", "Media", 5),
                    SelectOption::new("opt_company_industry_education", "Education", 6),
                    SelectOption::new("opt_company_industry_nonprofit", "Nonprofit", 7),
                    SelectOption::new("opt_company_industry_other", "Other", 8),
                ]),
                sf(
                    FLD_COMPANY_EMPLOYEES,
                    "employees",
                    "Employees",
                    FieldType::Number,
                ),
                sf(FLD_COMPANY_ARR, "arr", "ARR", FieldType::Currency).currency("USD"),
                sf(FLD_COMPANY_PHONE, "phone", "Phone", FieldType::Phone),
                sf(
                    FLD_COMPANY_LOCATION,
                    "location",
                    "Location",
                    FieldType::Text,
                ),
                sf(
                    FLD_COMPANY_DESCRIPTION,
                    "description",
                    "Description",
                    FieldType::LongText,
                ),
                sf(FLD_COMPANY_OWNER, "owner", "Owner", FieldType::User),
                sf(FLD_COMPANY_TAGS, "tags", "Tags", FieldType::MultiSelect),
            ],
            vec![view(
                VIEW_COMPANY_ALL,
                OBJ_COMPANY,
                "All companies",
                ViewKind::Table,
                &[
                    FLD_COMPANY_NAME,
                    FLD_COMPANY_DOMAIN,
                    FLD_COMPANY_STATUS,
                    FLD_COMPANY_INDUSTRY,
                    FLD_COMPANY_ARR,
                    FLD_COMPANY_OWNER,
                ],
                None,
                true,
                0,
            )],
        ),
        (
            object(
                OBJ_PERSON,
                "person",
                "Person",
                "People",
                "user-round",
                FLD_PERSON_NAME,
                1,
            ),
            vec![
                sf(FLD_PERSON_NAME, "name", "Name", FieldType::Text).title(),
                // The canonical dedupe key, and the one seeded unique field.
                // Uniqueness ignores empty values, so a roster with blank emails
                // still imports.
                sf(FLD_PERSON_EMAIL, "email", "Email", FieldType::Email).unique(),
                sf(FLD_PERSON_PHONE, "phone", "Phone", FieldType::Phone),
                sf(
                    FLD_PERSON_JOB_TITLE,
                    "job_title",
                    "Job title",
                    FieldType::Text,
                ),
                sf(
                    FLD_PERSON_COMPANY,
                    "company",
                    "Company",
                    FieldType::Relation,
                )
                .relation(OBJ_COMPANY, false, "People"),
                sf(FLD_PERSON_LINKEDIN, "linkedin", "LinkedIn", FieldType::Url),
                sf(FLD_PERSON_LOCATION, "location", "Location", FieldType::Text),
                sf(FLD_PERSON_OWNER, "owner", "Owner", FieldType::User),
                sf(FLD_PERSON_TAGS, "tags", "Tags", FieldType::MultiSelect),
                sf(FLD_PERSON_NOTES, "notes", "Notes", FieldType::LongText),
            ],
            vec![view(
                VIEW_PERSON_ALL,
                OBJ_PERSON,
                "All people",
                ViewKind::Table,
                &[
                    FLD_PERSON_NAME,
                    FLD_PERSON_EMAIL,
                    FLD_PERSON_JOB_TITLE,
                    FLD_PERSON_COMPANY,
                    FLD_PERSON_OWNER,
                ],
                None,
                true,
                0,
            )],
        ),
        (
            object(
                OBJ_DEAL,
                "deal",
                "Deal",
                "Deals",
                "target",
                FLD_DEAL_NAME,
                2,
            ),
            vec![
                sf(FLD_DEAL_NAME, "name", "Name", FieldType::Text).title(),
                // THE pipeline field: `PipelineRequest` defaults to the object's
                // first status field by position, which is this one.
                sf(FLD_DEAL_STAGE, "stage", "Stage", FieldType::Status)
                    .required()
                    .options(vec![
                        SelectOption::new(OPT_DEAL_STAGE_LEAD, "Lead", 0).with_color("slate"),
                        SelectOption::new(OPT_DEAL_STAGE_QUALIFIED, "Qualified", 1)
                            .with_color("blue"),
                        SelectOption::new(OPT_DEAL_STAGE_PROPOSAL, "Proposal", 2)
                            .with_color("indigo"),
                        SelectOption::new(OPT_DEAL_STAGE_NEGOTIATION, "Negotiation", 3)
                            .with_color("amber"),
                        SelectOption::new(OPT_DEAL_STAGE_WON, "Won", 4)
                            .with_color("green")
                            .won(),
                        SelectOption::new(OPT_DEAL_STAGE_LOST, "Lost", 5)
                            .with_color("red")
                            .lost(),
                    ]),
                sf(FLD_DEAL_AMOUNT, "amount", "Amount", FieldType::Currency).currency("USD"),
                sf(
                    FLD_DEAL_PROBABILITY,
                    "probability",
                    "Probability",
                    FieldType::Percent,
                ),
                sf(
                    FLD_DEAL_CLOSE_DATE,
                    "close_date",
                    "Close date",
                    FieldType::Date,
                ),
                sf(FLD_DEAL_COMPANY, "company", "Company", FieldType::Relation).relation(
                    OBJ_COMPANY,
                    false,
                    "Deals",
                ),
                sf(FLD_DEAL_CONTACT, "contact", "Contacts", FieldType::Relation)
                    .relation(OBJ_PERSON, true, "Deals"),
                sf(FLD_DEAL_OWNER, "owner", "Owner", FieldType::User),
                sf(FLD_DEAL_SOURCE, "source", "Source", FieldType::Select).options(vec![
                    SelectOption::new(OPT_DEAL_SOURCE_INBOUND, "Inbound", 0),
                    SelectOption::new(OPT_DEAL_SOURCE_OUTBOUND, "Outbound", 1),
                    SelectOption::new(OPT_DEAL_SOURCE_REFERRAL, "Referral", 2),
                    SelectOption::new(OPT_DEAL_SOURCE_PARTNER, "Partner", 3),
                    SelectOption::new(OPT_DEAL_SOURCE_EVENT, "Event", 4),
                    SelectOption::new(OPT_DEAL_SOURCE_OTHER, "Other", 5),
                ]),
                sf(
                    FLD_DEAL_DESCRIPTION,
                    "description",
                    "Description",
                    FieldType::LongText,
                ),
            ],
            vec![
                view(
                    VIEW_DEAL_ALL,
                    OBJ_DEAL,
                    "All deals",
                    ViewKind::Table,
                    &[
                        FLD_DEAL_NAME,
                        FLD_DEAL_STAGE,
                        FLD_DEAL_AMOUNT,
                        FLD_DEAL_CLOSE_DATE,
                        FLD_DEAL_COMPANY,
                        FLD_DEAL_OWNER,
                    ],
                    None,
                    true,
                    0,
                ),
                view(
                    VIEW_DEAL_PIPELINE,
                    OBJ_DEAL,
                    "Pipeline",
                    ViewKind::Board,
                    &[
                        FLD_DEAL_NAME,
                        FLD_DEAL_AMOUNT,
                        FLD_DEAL_CLOSE_DATE,
                        FLD_DEAL_COMPANY,
                    ],
                    Some(FLD_DEAL_STAGE),
                    false,
                    1,
                ),
            ],
        ),
        (
            object(
                OBJ_NOTE,
                "note",
                "Note",
                "Notes",
                "sticky-note",
                FLD_NOTE_SUBJECT,
                3,
            ),
            vec![
                // `subject`, not `title`: `title` is a RESERVED_SLUG (it is a real
                // column on `records`).
                sf(FLD_NOTE_SUBJECT, "subject", "Subject", FieldType::Text).title(),
                sf(FLD_NOTE_BODY, "body", "Body", FieldType::LongText),
                sf(FLD_NOTE_AUTHOR, "author", "Author", FieldType::User),
                sf(FLD_NOTE_PINNED, "pinned", "Pinned", FieldType::Checkbox),
            ],
            vec![view(
                VIEW_NOTE_ALL,
                OBJ_NOTE,
                "All notes",
                ViewKind::Table,
                &[FLD_NOTE_SUBJECT, FLD_NOTE_AUTHOR, FLD_NOTE_PINNED],
                None,
                true,
                0,
            )],
        ),
        (
            object(
                OBJ_TASK,
                "task",
                "Task",
                "Tasks",
                "circle-check",
                FLD_TASK_NAME,
                4,
            ),
            vec![
                sf(FLD_TASK_NAME, "name", "Name", FieldType::Text).title(),
                sf(FLD_TASK_STATUS, "status", "Status", FieldType::Status).options(vec![
                    SelectOption::new(OPT_TASK_STATUS_TODO, "To do", 0).with_color("slate"),
                    SelectOption::new(OPT_TASK_STATUS_IN_PROGRESS, "In progress", 1)
                        .with_color("blue"),
                    SelectOption::new(OPT_TASK_STATUS_DONE, "Done", 2)
                        .with_color("green")
                        .won(),
                    SelectOption::new(OPT_TASK_STATUS_CANCELLED, "Cancelled", 3)
                        .with_color("red")
                        .lost(),
                ]),
                sf(FLD_TASK_ASSIGNEE, "assignee", "Assignee", FieldType::User),
                sf(FLD_TASK_DUE_DATE, "due_date", "Due", FieldType::Datetime),
                sf(FLD_TASK_PRIORITY, "priority", "Priority", FieldType::Select).options(vec![
                    SelectOption::new(OPT_TASK_PRIORITY_LOW, "Low", 0),
                    SelectOption::new(OPT_TASK_PRIORITY_MEDIUM, "Medium", 1),
                    SelectOption::new(OPT_TASK_PRIORITY_HIGH, "High", 2),
                    SelectOption::new(OPT_TASK_PRIORITY_URGENT, "Urgent", 3),
                ]),
                sf(FLD_TASK_NOTES, "notes", "Notes", FieldType::LongText),
            ],
            vec![
                view(
                    VIEW_TASK_ALL,
                    OBJ_TASK,
                    "All tasks",
                    ViewKind::Table,
                    &[
                        FLD_TASK_NAME,
                        FLD_TASK_STATUS,
                        FLD_TASK_ASSIGNEE,
                        FLD_TASK_DUE_DATE,
                        FLD_TASK_PRIORITY,
                    ],
                    None,
                    true,
                    0,
                ),
                view(
                    VIEW_TASK_BOARD,
                    OBJ_TASK,
                    "Task board",
                    ViewKind::Board,
                    &[
                        FLD_TASK_NAME,
                        FLD_TASK_ASSIGNEE,
                        FLD_TASK_DUE_DATE,
                        FLD_TASK_PRIORITY,
                    ],
                    Some(FLD_TASK_STATUS),
                    false,
                    1,
                ),
            ],
        ),
    ]
}

fn seed_standard_schema(conn: &Connection) -> Result<()> {
    let now = now_rfc3339();
    for (object, fields, views) in seed_objects() {
        conn.execute(
            "INSERT OR IGNORE INTO objects
               (id, slug, singular, plural, icon, description, title_field_id, is_standard, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10)",
            params![
                object.id,
                object.slug,
                object.singular,
                object.plural,
                object.icon,
                object.description,
                object.title_field_id,
                object.position,
                now,
                now
            ],
        )?;
        for (position, field) in fields.iter().enumerate() {
            conn.execute(
                "INSERT OR IGNORE INTO fields
                   (id, object_id, list_id, slug, name, field_type, config, description,
                    is_required, is_unique, is_system, position, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, NULL, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    field.id,
                    object.id,
                    field.slug,
                    field.name,
                    field.field_type.as_str(),
                    field.config.encode(),
                    i64::from(field.required),
                    i64::from(field.unique),
                    i64::from(field.system),
                    position as i64,
                    now,
                    now
                ],
            )?;
        }
        for view in &views {
            conn.execute(
                "INSERT OR IGNORE INTO views
                   (id, object_id, name, kind, filter, sorts, visible_fields,
                    group_by_field_id, is_default, position, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    view.id,
                    view.object_id,
                    view.name,
                    view.kind.as_str(),
                    serde_json::to_string(&view.sorts)?,
                    serde_json::to_string(&view.visible_field_ids)?,
                    view.group_by_field_id,
                    i64::from(view.is_default),
                    view.position,
                    now,
                    now
                ],
            )?;
        }
    }
    Ok(())
}

// ── Column lists + row decoders ────────────────────────────────────────────────
//
// Declared once so a decoder and its SELECTs cannot drift apart. Every decoder is
// TOLERANT: a value that fails to parse degrades to a documented default rather
// than failing the whole query, so one corrupt row cannot blank a table.

const COLS_OBJECT: &str = "id, slug, singular, plural, icon, description, title_field_id, \
                           is_standard, position, created_at, updated_at";
const COLS_FIELD: &str = "id, object_id, list_id, slug, name, field_type, config, description, \
                          is_required, is_unique, is_system, position, created_at, updated_at";
const COLS_RECORD: &str =
    "id, object_id, title, data, deleted_at, created_by, created_at, updated_at";
const COLS_LINK: &str = "id, field_id, source_record_id, source_object_id, target_record_id, \
                         target_object_id, created_at";
const COLS_VIEW: &str = "id, object_id, name, kind, filter, sorts, visible_fields, \
                         group_by_field_id, is_default, position, created_at, updated_at";
const COLS_LIST: &str = "id, object_id, name, description, icon, position, created_at, updated_at";
const COLS_LIST_ENTRY: &str = "id, list_id, record_id, data, position, created_at, updated_at";
const COLS_ACTIVITY: &str = "id, record_id, object_id, kind, title, body, field_id, from_value, \
                             to_value, assignee, due_at, completed_at, due_notified_at, author, \
                             metadata, created_at, updated_at";
const COLS_IMPORT: &str = "id, object_id, filename, status, delimiter, has_header, row_count, \
                           columns, mappings, dedupe, preview, result, error, created_at, updated_at";

/// Decode a JSON TEXT column, falling back to a default on anything unparseable.
fn decode_json<T: serde::de::DeserializeOwned + Default>(raw: Option<String>) -> T {
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn encode_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn row_to_object(row: &Row<'_>) -> rusqlite::Result<Object> {
    Ok(Object {
        id: row.get(0)?,
        slug: row.get(1)?,
        singular: row.get(2)?,
        plural: row.get(3)?,
        icon: row.get(4)?,
        description: row.get(5)?,
        title_field_id: row.get(6)?,
        is_standard: row.get::<_, i64>(7)? != 0,
        position: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn row_to_field(row: &Row<'_>) -> rusqlite::Result<Field> {
    let field_type: String = row.get(5)?;
    let config: String = row.get(6)?;
    Ok(Field {
        id: row.get(0)?,
        object_id: row.get(1)?,
        list_id: row.get(2)?,
        slug: row.get(3)?,
        name: row.get(4)?,
        field_type: FieldType::from_db(&field_type),
        config: FieldConfig::decode(&config),
        description: row.get(7)?,
        is_required: row.get::<_, i64>(8)? != 0,
        is_unique: row.get::<_, i64>(9)? != 0,
        is_system: row.get::<_, i64>(10)? != 0,
        position: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn row_to_record(row: &Row<'_>) -> rusqlite::Result<Record> {
    let data: String = row.get(3)?;
    Ok(Record {
        id: row.get(0)?,
        object_id: row.get(1)?,
        title: row.get(2)?,
        values: serde_json::from_str(&data).unwrap_or_default(),
        deleted_at: row.get(4)?,
        created_by: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_link(row: &Row<'_>) -> rusqlite::Result<RecordLink> {
    Ok(RecordLink {
        id: row.get(0)?,
        field_id: row.get(1)?,
        source_record_id: row.get(2)?,
        source_object_id: row.get(3)?,
        target_record_id: row.get(4)?,
        target_object_id: row.get(5)?,
        created_at: row.get(6)?,
    })
}

fn row_to_view(row: &Row<'_>) -> rusqlite::Result<View> {
    let kind: String = row.get(3)?;
    let filter: Option<String> = row.get(4)?;
    Ok(View {
        id: row.get(0)?,
        object_id: row.get(1)?,
        name: row.get(2)?,
        kind: ViewKind::from_db(&kind),
        filter: filter.and_then(|f| serde_json::from_str(&f).ok()),
        sorts: decode_json(row.get(5)?),
        visible_field_ids: decode_json(row.get(6)?),
        group_by_field_id: row.get(7)?,
        is_default: row.get::<_, i64>(8)? != 0,
        position: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

fn row_to_list(row: &Row<'_>) -> rusqlite::Result<List> {
    Ok(List {
        id: row.get(0)?,
        object_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        icon: row.get(4)?,
        position: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_list_entry(row: &Row<'_>) -> rusqlite::Result<ListEntry> {
    let data: String = row.get(3)?;
    Ok(ListEntry {
        id: row.get(0)?,
        list_id: row.get(1)?,
        record_id: row.get(2)?,
        values: serde_json::from_str(&data).unwrap_or_default(),
        position: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_activity(row: &Row<'_>) -> rusqlite::Result<Activity> {
    let kind: String = row.get(3)?;
    let from_value: Option<String> = row.get(7)?;
    let to_value: Option<String> = row.get(8)?;
    let metadata: Option<String> = row.get(14)?;
    Ok(Activity {
        id: row.get(0)?,
        record_id: row.get(1)?,
        object_id: row.get(2)?,
        kind: ActivityKind::from_db(&kind),
        title: row.get(4)?,
        body: row.get(5)?,
        field_id: row.get(6)?,
        from_value: from_value.and_then(|v| serde_json::from_str(&v).ok()),
        to_value: to_value.and_then(|v| serde_json::from_str(&v).ok()),
        assignee: row.get(9)?,
        due_at: row.get(10)?,
        completed_at: row.get(11)?,
        due_notified_at: row.get(12)?,
        author: row.get(13)?,
        metadata: metadata.and_then(|v| serde_json::from_str(&v).ok()),
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn row_to_import(row: &Row<'_>) -> rusqlite::Result<ImportJob> {
    let status: String = row.get(3)?;
    let preview: Option<String> = row.get(10)?;
    let result: Option<String> = row.get(11)?;
    Ok(ImportJob {
        id: row.get(0)?,
        object_id: row.get(1)?,
        filename: row.get(2)?,
        status: ImportStatus::from_db(&status),
        delimiter: row.get(4)?,
        has_header: row.get::<_, i64>(5)? != 0,
        row_count: row.get::<_, i64>(6)?.max(0) as usize,
        columns: decode_json(row.get(7)?),
        mappings: decode_json(row.get(8)?),
        dedupe: decode_json(row.get(9)?),
        preview: preview.and_then(|p| serde_json::from_str(&p).ok()),
        result: result.and_then(|r| serde_json::from_str(&r).ok()),
        error: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

// ── Internal loaders (all take `&Connection`, never `&self`) ───────────────────
//
// See the module docs on locking: a public method that called another public method
// would deadlock on the non-reentrant async mutex. Everything shared lives here.

/// Resolve an object by id OR slug. The tolerance is what lets an agent tool say
/// `"deal"` where the panel says `"obj_deal"`.
fn load_object(conn: &Connection, id_or_slug: &str) -> Result<Option<Object>> {
    let sql = format!("SELECT {COLS_OBJECT} FROM objects WHERE id = ?1 OR slug = ?1");
    Ok(conn
        .query_row(&sql, params![id_or_slug], row_to_object)
        .optional()?)
}

/// An object's own fields (`list_id IS NULL`), in position order.
fn load_fields(conn: &Connection, object_id: &str) -> Result<Vec<Field>> {
    let sql = format!(
        "SELECT {COLS_FIELD} FROM fields WHERE object_id = ?1 AND list_id IS NULL
         ORDER BY position ASC, created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![object_id], row_to_field)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One list's extra fields, in position order.
fn load_list_fields(conn: &Connection, list_id: &str) -> Result<Vec<Field>> {
    let sql = format!(
        "SELECT {COLS_FIELD} FROM fields WHERE list_id = ?1
         ORDER BY position ASC, created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![list_id], row_to_field)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn load_field(conn: &Connection, field_id: &str) -> Result<Option<Field>> {
    let sql = format!("SELECT {COLS_FIELD} FROM fields WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![field_id], row_to_field)
        .optional()?)
}

fn load_record(conn: &Connection, record_id: &str) -> Result<Option<Record>> {
    let sql = format!("SELECT {COLS_RECORD} FROM records WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![record_id], row_to_record)
        .optional()?)
}

fn load_list(conn: &Connection, list_id: &str) -> Result<Option<List>> {
    let sql = format!("SELECT {COLS_LIST} FROM lists WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![list_id], row_to_list)
        .optional()?)
}

/// An id-AND-slug lookup table over a field set, so a filter/sort/mapping may name
/// either. Both keys point at the same `Field`.
fn field_index(fields: &[Field]) -> HashMap<String, Field> {
    let mut index = HashMap::with_capacity(fields.len() * 2);
    for field in fields {
        index.insert(field.id.clone(), field.clone());
        index.insert(field.slug.clone(), field.clone());
    }
    index
}

/// The next free `position` for a new field/view/list/entry in a scope.
fn next_position(conn: &Connection, sql: &str, key: &str) -> Result<i64> {
    let max: Option<i64> = conn
        .query_row(sql, params![key], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(max.unwrap_or(-1) + 1)
}

// ── Value validation ───────────────────────────────────────────────────────────
//
// TWO LAYERS, and downstream code needs to know which is which:
//
//   * [`validate_field_value`] is PURE — type/shape/range normalization only. It
//     turns "$1,234.56" into 123456 cents, "Proposal" into `opt_deal_stage_proposal`,
//     "yes" into `true`, "31/03/2026"-shaped input into `2026-03-31`. It cannot
//     check that a relation target exists or that a unique value is free, because it
//     has no database.
//   * [`validate_bag`] runs that over a whole value bag AND adds the two checks
//     that need the connection: relation-target existence and uniqueness. It also
//     enforces `is_required` (for a non-partial write).
//
// The import path calls both, per row. The merge path calls the pure layer on any
// value a user typed into the resolution dialog.

/// Read a JSON value as a trimmed string, coercing numbers and booleans. `None` for
/// null, an empty/blank string, or a container.
fn as_text(raw: &Value) -> Option<String> {
    match raw {
        Value::Null => None,
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Read a JSON value as an f64, tolerating a formatted string ("1,234.56", "$1 200",
/// "45%"). This is the CSV path: a spreadsheet exports money with separators.
fn as_number(raw: &Value) -> Option<f64> {
    match raw {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let cleaned: String = s
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
                .collect();
            cleaned.parse().ok()
        }
        _ => None,
    }
}

/// Read a JSON value as a list of trimmed strings: an array, or a comma-separated
/// string (which is what a CSV cell holding several tags looks like).
fn as_list(raw: &Value) -> Vec<String> {
    match raw {
        Value::Array(items) => items.iter().filter_map(as_text).collect(),
        Value::String(s) => s
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Null => Vec::new(),
        other => as_text(other).into_iter().collect(),
    }
}

/// Whether a normalized value counts as "set". Used by required, by unique, by
/// `fill_blanks` import and by merge's default resolution.
pub fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Normalize ONE value against ONE field's type. `Ok(None)` means "this clears the
/// field"; `Ok(Some(v))` is the canonical stored form.
///
/// Currency deserves its own note, because the rule is not guessable: **a JSON
/// INTEGER is already cents; a JSON string or float is a major-unit amount and gets
/// multiplied by 100.** That split is what lets the panel round-trip `12345` →
/// `12345` losslessly while a CSV cell reading `123.45` and an agent writing
/// `"$123.45"` both land on the same 12345.
pub fn validate_field_value(
    field: &Field,
    raw: &Value,
) -> std::result::Result<Option<Value>, FieldValidationError> {
    let invalid = |message: &str| {
        FieldValidationError::coded(&field.id, &field.slug, ValidationCode::Invalid, message)
    };
    let out_of_range = |message: &str| {
        FieldValidationError::coded(&field.id, &field.slug, ValidationCode::OutOfRange, message)
    };

    if raw.is_null() {
        return Ok(None);
    }

    match field.field_type {
        FieldType::Text | FieldType::LongText | FieldType::User => {
            Ok(as_text(raw).map(Value::String))
        }
        FieldType::Email => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            let lowered = text.to_lowercase();
            let ok = !lowered.contains(char::is_whitespace)
                && lowered.matches('@').count() == 1
                && lowered.split_once('@').is_some_and(|(user, host)| {
                    !user.is_empty()
                        && host.contains('.')
                        && !host.starts_with('.')
                        && !host.ends_with('.')
                });
            if !ok {
                return Err(invalid("not a valid email address"));
            }
            Ok(Some(Value::String(lowered)))
        }
        FieldType::Phone => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            // Deliberately permissive: phone formats are a swamp, and rejecting a
            // number a user can read is worse than storing an odd one. Only the
            // obviously-not-a-number case fails.
            if !text.chars().any(|c| c.is_ascii_digit()) {
                return Err(invalid("a phone number must contain at least one digit"));
            }
            Ok(Some(Value::String(text)))
        }
        FieldType::Url => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            if text.contains(char::is_whitespace) {
                return Err(invalid("a URL must not contain spaces"));
            }
            let normalized = if text.starts_with("http://") || text.starts_with("https://") {
                text
            } else if text.contains('.') {
                // "acme.com" is what a human types and what every CSV holds.
                format!("https://{text}")
            } else {
                return Err(invalid("not a valid URL"));
            };
            Ok(Some(Value::String(normalized)))
        }
        FieldType::Number => {
            let Some(n) = as_number(raw) else {
                return Err(invalid("expected a number"));
            };
            Ok(Some(number_value(n)))
        }
        FieldType::Currency => {
            // See the fn docs: integer ⇒ already cents, anything else ⇒ major units.
            let cents = match raw {
                Value::Number(n) if n.is_i64() => n.as_i64().unwrap_or_default(),
                other => {
                    let Some(major) = as_number(other) else {
                        return Err(invalid("expected an amount"));
                    };
                    (major * 100.0).round() as i64
                }
            };
            Ok(Some(Value::Number(cents.into())))
        }
        FieldType::Percent => {
            let Some(n) = as_number(raw) else {
                return Err(invalid("expected a percentage"));
            };
            if !(0.0..=100.0).contains(&n) {
                return Err(out_of_range("a percentage must be between 0 and 100"));
            }
            Ok(Some(number_value(n)))
        }
        FieldType::Rating => {
            let Some(n) = as_number(raw) else {
                return Err(invalid("expected a rating"));
            };
            let max = i64::from(field.config.max_rating());
            let rounded = n.round() as i64;
            if rounded < 0 || rounded > max {
                return Err(out_of_range(&format!(
                    "a rating must be between 0 and {max}"
                )));
            }
            Ok(Some(Value::Number(rounded.into())))
        }
        FieldType::Checkbox => {
            let value = match raw {
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().unwrap_or_default() != 0.0,
                Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "yes" | "y" | "1" | "on" | "checked" => true,
                    "false" | "no" | "n" | "0" | "off" | "" => false,
                    _ => return Err(invalid("expected true or false")),
                },
                _ => return Err(invalid("expected true or false")),
            };
            Ok(Some(Value::Bool(value)))
        }
        FieldType::Date => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            normalize_date(&text)
                .map(Value::String)
                .map(Some)
                .ok_or_else(|| invalid("not a valid date"))
        }
        FieldType::Datetime => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            normalize_datetime(&text)
                .map(Value::String)
                .map(Some)
                .ok_or_else(|| invalid("not a valid date and time"))
        }
        FieldType::Select | FieldType::Status => {
            let Some(text) = as_text(raw) else {
                return Ok(None);
            };
            match field.config.resolve_option(&text) {
                Some(option) => Ok(Some(Value::String(option.id.clone()))),
                None => Err(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::UnknownOption,
                    format!("\"{text}\" is not one of this field's options"),
                )),
            }
        }
        FieldType::MultiSelect => {
            let raw_items = as_list(raw);
            if raw_items.is_empty() {
                return Ok(None);
            }
            let mut ids = Vec::with_capacity(raw_items.len());
            for item in raw_items {
                let Some(option) = field.config.resolve_option(&item) else {
                    return Err(FieldValidationError::coded(
                        &field.id,
                        &field.slug,
                        ValidationCode::UnknownOption,
                        format!("\"{item}\" is not one of this field's options"),
                    ));
                };
                if !ids.contains(&option.id) {
                    ids.push(option.id.clone());
                }
            }
            Ok(Some(json!(ids)))
        }
        FieldType::Relation => {
            let ids = as_list(raw);
            if ids.is_empty() {
                return Ok(None);
            }
            if field.config.relation_object_id.is_none() {
                return Err(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::BadRelationTarget,
                    "this relation field has no target object configured",
                ));
            }
            if !field.config.relation_multiple && ids.len() > 1 {
                return Err(out_of_range("this relation accepts a single record"));
            }
            let mut unique = Vec::with_capacity(ids.len());
            for id in ids {
                if !unique.contains(&id) {
                    unique.push(id);
                }
            }
            Ok(Some(json!(unique)))
        }
    }
}

/// Store an f64 as an integer when it is one, so a whole number round-trips as `5`
/// rather than `5.0` and `json_extract` comparisons stay integral.
fn number_value(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::Number((n as i64).into())
    } else {
        serde_json::Number::from_f64(n)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    }
}

/// Validate a whole bag against an object's fields, adding the two checks that need
/// the database.
///
/// `partial` = the caller sent a MERGE update, so absent required fields are fine.
/// `exclude_record_id` is the record being updated, excluded from its own uniqueness
/// check — without it every save of an unchanged unique field would fail.
fn validate_bag(
    conn: &Connection,
    object_id: &str,
    fields: &[Field],
    incoming: &ValueBag,
    partial: bool,
    exclude_record_id: Option<&str>,
) -> Result<ValidatedValues> {
    let index = field_index(fields);
    let mut out = ValidatedValues::default();

    for (key, raw) in incoming {
        let Some(field) = index.get(key.as_str()) else {
            out.errors.push(FieldValidationError::unknown_field(key));
            continue;
        };
        match validate_field_value(field, raw) {
            Ok(Some(value)) => {
                out.values.insert(field.slug.clone(), value);
            }
            // An explicit clear: recorded as JSON null so a MERGE update can tell
            // "clear this" from "do not mention this".
            Ok(None) => {
                out.values.insert(field.slug.clone(), Value::Null);
            }
            Err(error) => out.errors.push(error),
        }
    }

    // Relation targets: exist, are live, and are on the right object.
    for field in fields
        .iter()
        .filter(|f| f.field_type == FieldType::Relation)
    {
        let Some(Value::Array(targets)) = out.values.get(&field.slug) else {
            continue;
        };
        let Some(target_object) = field.config.relation_object_id.as_deref() else {
            continue;
        };
        for target in targets {
            let Some(id) = target.as_str() else { continue };
            let ok: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM records WHERE id = ?1 AND object_id = ?2 AND deleted_at IS NULL",
                    params![id, target_object],
                    |r| r.get(0),
                )
                .optional()?;
            if ok.is_none() {
                out.errors.push(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::BadRelationTarget,
                    format!("no live record \"{id}\" on the target object"),
                ));
            }
        }
    }

    // Uniqueness. Empty values never collide — a hundred people with no email are
    // not a hundred duplicates.
    for field in fields.iter().filter(|f| f.is_unique) {
        let Some(value) = out.values.get(&field.slug) else {
            continue;
        };
        if is_empty_value(value) {
            continue;
        }
        let Some(text) = as_text(value) else { continue };
        let sql = format!(
            "SELECT id FROM records
              WHERE object_id = ?1 AND deleted_at IS NULL AND id <> ?2
                AND lower(trim(CAST(json_extract(data, '$.{}') AS TEXT))) = lower(trim(?3))
              LIMIT 1",
            field.slug
        );
        let clash: Option<String> = conn
            .query_row(
                &sql,
                params![object_id, exclude_record_id.unwrap_or(""), text],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = clash {
            out.errors.push(FieldValidationError::coded(
                &field.id,
                &field.slug,
                ValidationCode::NotUnique,
                format!(
                    "another record ({other}) already has this {}",
                    field.name.to_lowercase()
                ),
            ));
        }
    }

    // Required, for a full write only.
    if !partial {
        for field in fields.iter().filter(|f| f.is_required) {
            let present = out
                .values
                .get(&field.slug)
                .is_some_and(|v| !is_empty_value(v));
            if !present {
                out.errors.push(FieldValidationError::coded(
                    &field.id,
                    &field.slug,
                    ValidationCode::Required,
                    format!("{} is required", field.name),
                ));
            }
        }
    }

    Ok(out)
}

/// Apply a field's `default_value` to every field the bag does not mention. Called
/// on create only — a default that reasserted itself on every update would make a
/// deliberately cleared field un-clearable.
fn apply_defaults(fields: &[Field], values: &mut ValueBag) {
    for field in fields {
        let Some(default) = field.config.default_value.as_ref() else {
            continue;
        };
        let missing = values.get(&field.slug).map_or(true, |v| v.is_null());
        if missing {
            values.insert(field.slug.clone(), default.clone());
        }
    }
}

/// Drop the explicit-clear nulls a MERGE update produced, since a stored bag holds
/// only set values. Keeping nulls would make `json_extract` return SQL NULL either
/// way but bloat every row.
fn prune_nulls(values: &mut ValueBag) {
    values.retain(|_, v| !v.is_null());
}

/// The record's display name: the object's `title_field_id` value, falling back to
/// the first text-ish field, then to a placeholder.
fn compute_title(object: &Object, fields: &[Field], values: &ValueBag) -> String {
    let from_field =
        |field: &Field| -> Option<String> { values.get(&field.slug).and_then(as_text) };
    if let Some(title_field) = object
        .title_field_id
        .as_deref()
        .and_then(|id| fields.iter().find(|f| f.id == id))
    {
        if let Some(text) = from_field(title_field) {
            return text;
        }
    }
    for field in fields
        .iter()
        .filter(|f| matches!(f.field_type, FieldType::Text | FieldType::Email))
    {
        if let Some(text) = from_field(field) {
            return text;
        }
    }
    format!("Untitled {}", object.singular.to_lowercase())
}

/// The text FTS indexes for a record: every searchable field's value, space-joined.
/// See [`FieldType::is_searchable`] for why numbers, dates and option ids are out.
fn fts_body(fields: &[Field], values: &ValueBag) -> String {
    let mut parts: Vec<String> = Vec::new();
    for field in fields.iter().filter(|f| f.field_type.is_searchable()) {
        if let Some(text) = values.get(&field.slug).and_then(as_text) {
            parts.push(text);
        }
    }
    parts.join(" ")
}

/// Replace a record's FTS row. Delete-then-insert keyed on `records.rowid`, which is
/// an O(log n) lookup — see the DDL comment on `records_fts`.
fn fts_reindex(conn: &Connection, record_id: &str, title: &str, body: &str) -> Result<()> {
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM records WHERE id = ?1",
            params![record_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(rowid) = rowid else { return Ok(()) };
    conn.execute("DELETE FROM records_fts WHERE rowid = ?1", params![rowid])?;
    conn.execute(
        "INSERT INTO records_fts(rowid, title, body) VALUES (?1, ?2, ?3)",
        params![rowid, title, body],
    )?;
    Ok(())
}

fn fts_delete(conn: &Connection, record_id: &str) -> Result<()> {
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM records WHERE id = ?1",
            params![record_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(rowid) = rowid {
        conn.execute("DELETE FROM records_fts WHERE rowid = ?1", params![rowid])?;
    }
    Ok(())
}

/// Turn arbitrary user input into a safe FTS5 MATCH expression.
///
/// FTS5's query language has operators (`AND`, `NEAR`, `*`, `"`, `:`), and passing
/// raw input straight through turns a search box into a syntax-error generator at
/// best. Every token is quoted, which makes it a literal; the last one gets a `*` so
/// typing narrows as you go.
fn fts_match_expression(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric() && c != '@' && c != '.' && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return None;
    }
    let last = tokens.len() - 1;
    Some(
        tokens
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == last {
                    format!("\"{t}\"*")
                } else {
                    format!("\"{t}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

// ── Objects ────────────────────────────────────────────────────────────────────

impl CrmStore {
    pub async fn list_objects(&self) -> Result<Vec<Object>> {
        let conn = self.conn.lock().await;
        let sql =
            format!("SELECT {COLS_OBJECT} FROM objects ORDER BY position ASC, created_at ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_object)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Objects with the counts the sidebar renders.
    pub async fn list_object_summaries(&self) -> Result<Vec<ObjectSummary>> {
        let conn = self.conn.lock().await;
        object_summaries(&conn)
    }

    /// By id OR slug.
    pub async fn get_object(&self, id_or_slug: &str) -> Result<Option<Object>> {
        let conn = self.conn.lock().await;
        load_object(&conn, id_or_slug)
    }

    /// The whole schema in one lock: objects, their fields, their views, and every
    /// list. This is the panel's boot call.
    pub async fn schema(&self) -> Result<SchemaResponse> {
        let conn = self.conn.lock().await;
        let objects = {
            let sql =
                format!("SELECT {COLS_OBJECT} FROM objects ORDER BY position ASC, created_at ASC");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], row_to_object)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::with_capacity(objects.len());
        for object in objects {
            let fields = load_fields(&conn, &object.id)?;
            let views = load_views(&conn, &object.id)?;
            let record_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM records WHERE object_id = ?1 AND deleted_at IS NULL",
                params![object.id],
                |r| r.get(0),
            )?;
            out.push(ObjectWithFields {
                object,
                fields,
                views,
                record_count,
            });
        }
        let lists = {
            let sql =
                format!("SELECT {COLS_LIST} FROM lists ORDER BY position ASC, created_at ASC");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], row_to_list)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(SchemaResponse {
            objects: out,
            lists,
        })
    }

    /// Create a custom object.
    ///
    /// Also creates its `name` title field and an "All …" default table view, in the
    /// same transaction. A bare object with no fields and no view is unusable, and
    /// making the panel do three calls to reach a usable state is how you get objects
    /// stuck half-created.
    pub async fn create_object(&self, req: &CreateObjectRequest) -> Validated<Object> {
        let slug = req.slug.trim().to_lowercase();
        if !is_valid_slug(&slug) {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "slug",
                ValidationCode::Invalid,
                "an object slug must be lowercase letters, digits and underscores, and must not be a reserved word",
            )]));
        }
        let singular = req.singular.trim().to_string();
        if singular.is_empty() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "singular",
                ValidationCode::Required,
                "a name is required",
            )]));
        }
        let plural = req
            .plural
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{singular}s"));

        let mut conn = self.conn.lock().await;
        if load_object(&conn, &slug)?.is_some() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "slug",
                ValidationCode::NotUnique,
                format!("an object with the slug \"{slug}\" already exists"),
            )]));
        }
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        let object_id = new_id(ID_OBJECT);
        let title_field_id = new_id(ID_FIELD);
        let position = next_position(
            &tx,
            "SELECT MAX(position) FROM objects WHERE ?1 IS NOT NULL",
            "x",
        )?;
        tx.execute(
            "INSERT INTO objects
               (id, slug, singular, plural, icon, description, title_field_id, is_standard, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?9)",
            params![
                object_id,
                slug,
                singular,
                plural,
                req.icon,
                req.description,
                title_field_id,
                position,
                now
            ],
        )?;
        tx.execute(
            "INSERT INTO fields
               (id, object_id, list_id, slug, name, field_type, config, description,
                is_required, is_unique, is_system, position, created_at, updated_at)
             VALUES (?1, ?2, NULL, 'name', 'Name', 'text', '{}', NULL, 1, 0, 1, 0, ?3, ?3)",
            params![title_field_id, object_id, now],
        )?;
        tx.execute(
            "INSERT INTO views
               (id, object_id, name, kind, filter, sorts, visible_fields, group_by_field_id,
                is_default, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'table', NULL, ?4, ?5, NULL, 1, 0, ?6, ?6)",
            params![
                new_id(ID_VIEW),
                object_id,
                format!("All {}", plural.to_lowercase()),
                encode_json(&vec![ViewSort::desc("updated_at")]),
                encode_json(&vec![title_field_id.clone()]),
                now
            ],
        )?;
        let object = load_object(&tx, &object_id)?.context("re-reading the object just created")?;
        tx.commit()?;
        Ok(Ok(object))
    }

    /// Rename / re-icon / re-title an object. Its `slug` is immutable.
    pub async fn update_object(
        &self,
        object_id: &str,
        req: &UpdateObjectRequest,
    ) -> Result<Option<Object>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_object(&conn, object_id)? else {
            return Ok(None);
        };
        let now = now_rfc3339();
        conn.execute(
            "UPDATE objects SET singular = ?2, plural = ?3, icon = ?4, description = ?5,
                                title_field_id = ?6, position = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                existing.id,
                req.singular
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&existing.singular),
                req.plural
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&existing.plural),
                req.icon.as_ref().or(existing.icon.as_ref()),
                req.description.as_ref().or(existing.description.as_ref()),
                req.title_field_id
                    .as_ref()
                    .or(existing.title_field_id.as_ref()),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        load_object(&conn, &existing.id)
    }

    /// Delete a custom object and everything under it.
    ///
    /// An explicit ordered cascade in ONE transaction (see the module docs for why
    /// not `ON DELETE CASCADE`). Refuses a standard object: `Ok(false)` when there
    /// was nothing to delete, `Err` when the caller asked for something the product
    /// does not permit — the two are different HTTP answers.
    pub async fn delete_object(&self, object_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(false);
        };
        if object.is_standard {
            bail!("the standard \"{}\" object cannot be deleted", object.slug);
        }
        let tx = conn.transaction()?;
        // FTS first: once the records are gone their rowids are unrecoverable.
        tx.execute(
            "DELETE FROM records_fts WHERE rowid IN (SELECT rowid FROM records WHERE object_id = ?1)",
            params![object.id],
        )?;
        tx.execute(
            "DELETE FROM record_links WHERE source_object_id = ?1 OR target_object_id = ?1",
            params![object.id],
        )?;
        tx.execute(
            "DELETE FROM list_entries WHERE list_id IN (SELECT id FROM lists WHERE object_id = ?1)",
            params![object.id],
        )?;
        tx.execute(
            "DELETE FROM activities WHERE object_id = ?1",
            params![object.id],
        )?;
        for table in ["records", "fields", "views", "lists", "import_jobs"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE object_id = ?1"),
                params![object.id],
            )?;
        }
        let n = tx.execute("DELETE FROM objects WHERE id = ?1", params![object.id])?;
        tx.commit()?;
        Ok(n > 0)
    }
}

fn object_summaries(conn: &Connection) -> Result<Vec<ObjectSummary>> {
    let sql = format!("SELECT {COLS_OBJECT} FROM objects ORDER BY position ASC, created_at ASC");
    let objects = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_object)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::with_capacity(objects.len());
    for object in objects {
        let field_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM fields WHERE object_id = ?1 AND list_id IS NULL",
            params![object.id],
            |r| r.get(0),
        )?;
        let record_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM records WHERE object_id = ?1 AND deleted_at IS NULL",
            params![object.id],
            |r| r.get(0),
        )?;
        let default_view_id: Option<String> = conn
            .query_row(
                "SELECT id FROM views WHERE object_id = ?1 AND is_default = 1 LIMIT 1",
                params![object.id],
                |r| r.get(0),
            )
            .optional()?;
        out.push(ObjectSummary {
            object,
            field_count,
            record_count,
            default_view_id,
        });
    }
    Ok(out)
}

// ── Fields ─────────────────────────────────────────────────────────────────────

impl CrmStore {
    pub async fn list_fields(&self, object_id: &str) -> Result<Vec<Field>> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(Vec::new());
        };
        load_fields(&conn, &object.id)
    }

    pub async fn get_field(&self, field_id: &str) -> Result<Option<Field>> {
        let conn = self.conn.lock().await;
        load_field(&conn, field_id)
    }

    /// Resolve a field on one object by id OR slug. The tolerant lookup every
    /// filter, sort, import mapping and agent tool call goes through.
    pub async fn resolve_field(&self, object_id: &str, id_or_slug: &str) -> Result<Option<Field>> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        Ok(fields
            .into_iter()
            .find(|f| f.id == id_or_slug || f.slug == id_or_slug))
    }

    /// Add a field to an object (`list_id = None`) or to a list (`Some`).
    pub async fn create_field(
        &self,
        object_id: &str,
        list_id: Option<&str>,
        req: &CreateFieldRequest,
    ) -> Validated<Field> {
        let slug = req.slug.trim().to_lowercase();
        let mut errors = Vec::new();
        if !is_valid_slug(&slug) {
            errors.push(FieldValidationError::coded(
                "",
                "slug",
                ValidationCode::Invalid,
                "a field slug must start with a lowercase letter and contain only lowercase letters, digits and underscores, and must not be one of the reserved names",
            ));
        }
        if req.name.trim().is_empty() {
            errors.push(FieldValidationError::coded(
                "",
                "name",
                ValidationCode::Required,
                "a field name is required",
            ));
        }
        if let Some(error) = validate_config(&slug, req.field_type, &req.config) {
            errors.push(error);
        }
        // Rejected rather than documented-as-inert: `validate_bag` enforces uniqueness
        // with a `SELECT … FROM records`, but a list field's values live in
        // `list_entries.data`, so a unique list field would enforce NOTHING, forever,
        // with no error anywhere. A guard is one line; a silent lie the UI can switch
        // on is a support ticket nobody can reproduce.
        if req.is_unique && list_id.is_some() {
            errors.push(FieldValidationError::coded(
                "",
                &slug,
                ValidationCode::Invalid,
                "list-specific fields cannot be unique",
            ));
        }
        if !errors.is_empty() {
            return Ok(Err(errors));
        }

        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let taken = match list_id {
            Some(list) => load_list_fields(&conn, list)?
                .iter()
                .any(|f| f.slug == slug),
            None => load_fields(&conn, &object.id)?
                .iter()
                .any(|f| f.slug == slug),
        };
        if taken {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                &slug,
                ValidationCode::NotUnique,
                format!("a field with the slug \"{slug}\" already exists here"),
            )]));
        }

        let mut config = req.config.clone();
        assign_option_ids(&slug, &mut config);
        let now = now_rfc3339();
        let id = new_id(ID_FIELD);
        let position = match req.position {
            Some(p) => p,
            None => match list_id {
                Some(list) => next_position(
                    &conn,
                    "SELECT MAX(position) FROM fields WHERE list_id = ?1",
                    list,
                )?,
                None => next_position(
                    &conn,
                    "SELECT MAX(position) FROM fields WHERE object_id = ?1 AND list_id IS NULL",
                    &object.id,
                )?,
            },
        };
        conn.execute(
            "INSERT INTO fields
               (id, object_id, list_id, slug, name, field_type, config, description,
                is_required, is_unique, is_system, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, ?12, ?12)",
            params![
                id,
                object.id,
                list_id,
                slug,
                req.name.trim(),
                req.field_type.as_str(),
                config.encode(),
                req.description,
                i64::from(req.is_required),
                i64::from(req.is_unique),
                position,
                now
            ],
        )?;
        let field = load_field(&conn, &id)?.context("re-reading the field just created")?;
        Ok(Ok(field))
    }

    /// Rename / reconfigure a field. `slug` and `field_type` are immutable — see the
    /// models module docs.
    pub async fn update_field(
        &self,
        field_id: &str,
        req: &UpdateFieldRequest,
    ) -> Validated<Option<Field>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_field(&conn, field_id)? else {
            return Ok(Ok(None));
        };
        let mut config = req
            .config
            .clone()
            .unwrap_or_else(|| existing.config.clone());
        if let Some(error) = validate_config(&existing.slug, existing.field_type, &config) {
            return Ok(Err(vec![error]));
        }
        assign_option_ids(&existing.slug, &mut config);
        let now = now_rfc3339();
        conn.execute(
            "UPDATE fields SET name = ?2, config = ?3, description = ?4, is_required = ?5,
                               is_unique = ?6, position = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                existing.id,
                req.name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(&existing.name),
                config.encode(),
                req.description.as_ref().or(existing.description.as_ref()),
                i64::from(req.is_required.unwrap_or(existing.is_required)),
                i64::from(req.is_unique.unwrap_or(existing.is_unique)),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        Ok(Ok(load_field(&conn, &existing.id)?))
    }

    /// Delete a field and strip its values from every record.
    ///
    /// Refuses a system field. The value strip is not optional housekeeping: a bag
    /// entry whose field is gone is invisible in the UI, still matched by FTS, and
    /// would silently reappear if a new field ever took the same slug.
    pub async fn delete_field(&self, field_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(field) = load_field(&conn, field_id)? else {
            return Ok(false);
        };
        if field.is_system {
            bail!("the system field \"{}\" cannot be deleted", field.slug);
        }
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM fields WHERE id = ?1", params![field.id])?;
        tx.execute(
            "DELETE FROM record_links WHERE field_id = ?1",
            params![field.id],
        )?;
        match field.list_id.as_deref() {
            Some(list_id) => {
                tx.execute(
                    &format!("UPDATE list_entries SET data = json_remove(data, '$.{}') WHERE list_id = ?1", field.slug),
                    params![list_id],
                )?;
            }
            None => {
                tx.execute(
                    &format!(
                        "UPDATE records SET data = json_remove(data, '$.{}') WHERE object_id = ?1",
                        field.slug
                    ),
                    params![field.object_id],
                )?;
                // A view that still lists this column, or groups by it, would render
                // a ghost. Both are cheap to repair here and impossible to notice
                // later.
                tx.execute(
                    "UPDATE views SET group_by_field_id = NULL WHERE group_by_field_id = ?1",
                    params![field.id],
                )?;
                if field.field_type.is_searchable() {
                    reindex_object(&tx, &field.object_id)?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Give the listed field ids positions `0..n`. Ids not listed keep their relative
    /// order after them.
    pub async fn reorder_fields(
        &self,
        object_id: &str,
        list_id: Option<&str>,
        ids: &[String],
    ) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        for (position, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE fields SET position = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, position as i64, now],
            )?;
        }
        // Push everything unlisted past the explicit block, preserving its order.
        let offset = ids.len() as i64;
        match list_id {
            Some(list) => tx.execute(
                "UPDATE fields SET position = position + ?2 WHERE list_id = ?1 AND id NOT IN (SELECT value FROM json_each(?3))",
                params![list, offset, encode_json(&ids)],
            )?,
            None => tx.execute(
                "UPDATE fields SET position = position + ?2 WHERE object_id = ?1 AND list_id IS NULL AND id NOT IN (SELECT value FROM json_each(?3))",
                params![object_id, offset, encode_json(&ids)],
            )?,
        };
        tx.commit()?;
        Ok(())
    }
}

/// Type-specific config sanity, run before a field is written. Returns the first
/// problem, because a config with two problems is one badly-filled form.
fn validate_config(
    slug: &str,
    field_type: FieldType,
    config: &FieldConfig,
) -> Option<FieldValidationError> {
    if field_type.is_option_backed() {
        let mut seen = HashSet::new();
        for option in &config.options {
            if option.label.trim().is_empty() {
                return Some(FieldValidationError::coded(
                    "",
                    slug,
                    ValidationCode::Invalid,
                    "every option needs a label",
                ));
            }
            if !option.id.is_empty() && !seen.insert(option.id.clone()) {
                return Some(FieldValidationError::coded(
                    "",
                    slug,
                    ValidationCode::NotUnique,
                    format!("duplicate option id \"{}\"", option.id),
                ));
            }
        }
    }
    if field_type == FieldType::Relation
        && config
            .relation_object_id
            .as_deref()
            .is_none_or(|t| t.trim().is_empty())
    {
        return Some(FieldValidationError::coded(
            "",
            slug,
            ValidationCode::BadRelationTarget,
            "a relation field needs a target object",
        ));
    }
    None
}

/// Give every option without an id a deterministic one derived from the field slug
/// and label, and normalize positions to `0..n`.
///
/// Derived rather than random so the same option added twice on two machines does
/// not produce two ids for one concept, and so a seeded option keeps the id the
/// panel hardcodes.
fn assign_option_ids(field_slug: &str, config: &mut FieldConfig) {
    let mut taken: HashSet<String> = config
        .options
        .iter()
        .filter(|o| !o.id.is_empty())
        .map(|o| o.id.clone())
        .collect();
    for (position, option) in config.options.iter_mut().enumerate() {
        option.position = position as i64;
        if !option.id.is_empty() {
            continue;
        }
        let base = slugify(&option.label).unwrap_or_else(|| "option".to_string());
        let mut candidate = format!("{ID_OPTION}{field_slug}_{base}");
        let mut n = 2;
        while taken.contains(&candidate) {
            candidate = format!("{ID_OPTION}{field_slug}_{base}_{n}");
            n += 1;
        }
        taken.insert(candidate.clone());
        option.id = candidate;
    }
}

/// Rebuild the FTS rows for one object. Used after a schema change that alters what
/// is searchable.
fn reindex_object(conn: &Connection, object_id: &str) -> Result<usize> {
    let fields = load_fields(conn, object_id)?;
    let sql = format!("SELECT {COLS_RECORD} FROM records WHERE object_id = ?1");
    let records = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![object_id], row_to_record)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for record in &records {
        fts_reindex(
            conn,
            &record.id,
            &record.title,
            &fts_body(&fields, &record.values),
        )?;
    }
    Ok(records.len())
}

// ── Query builder ──────────────────────────────────────────────────────────────
//
// Every filter/sort in this app funnels through here. Two rules make it safe:
//
//   * User VALUES are always bound parameters, never interpolated.
//   * The only interpolated text is a field SLUG, and `models::is_valid_slug`
//     restricts slugs to `[a-z][a-z0-9_]*` — there is no quote, `$`, `.` or `[` in
//     the alphabet, so a JSON path built from one cannot be broken out of. That is
//     the whole reason the slug validator is as strict as it is.

type SqlParams = Vec<rusqlite::types::Value>;

fn sql_value(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as S;
    match value {
        Value::Null => S::Null,
        Value::Bool(b) => S::Integer(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(S::Integer)
            .or_else(|| n.as_f64().map(S::Real))
            .unwrap_or(S::Null),
        Value::String(s) => S::Text(s.clone()),
        other => S::Text(other.to_string()),
    }
}

/// The SQL expression that yields one field's value, for a row aliased `alias`.
/// `None` when the key names nothing.
fn value_expr(
    index: &HashMap<String, Field>,
    key: &str,
    alias: &str,
) -> Option<(String, Option<Field>)> {
    if ViewSort::is_intrinsic(key) {
        return Some((format!("{alias}.{key}"), None));
    }
    let field = index.get(key)?;
    Some((
        format!("json_extract({alias}.data, '$.{}')", field.slug),
        Some(field.clone()),
    ))
}

/// One leaf condition. Returns `None` for a condition naming an unknown field, which
/// the caller treats as "no constraint" — a saved view whose field was deleted must
/// degrade to showing everything, not to a 500.
fn build_condition(
    condition: &FilterCondition,
    index: &HashMap<String, Field>,
    alias: &str,
    params: &mut SqlParams,
) -> Option<String> {
    let (expr, field) = value_expr(index, &condition.field_id, alias)?;
    let multi = field.as_ref().is_some_and(|f| f.field_type.is_multi());
    let numeric = field.as_ref().is_some_and(|f| f.field_type.is_numeric());

    // Membership over a stored JSON array.
    let any_of = |values: &[Value], params: &mut SqlParams| -> String {
        if values.is_empty() {
            return "0".to_string();
        }
        let placeholders = values
            .iter()
            .map(|v| {
                params.push(sql_value(v));
                "?"
            })
            .collect::<Vec<_>>()
            .join(", ");
        if multi {
            format!(
                "EXISTS (SELECT 1 FROM json_each({expr}) je WHERE je.value IN ({placeholders}))"
            )
        } else {
            format!("{expr} IN ({placeholders})")
        }
    };

    let scalar = |params: &mut SqlParams| {
        params.push(sql_value(&condition.value));
    };

    let sql = match condition.op {
        FilterOperator::Eq => {
            if multi {
                any_of(std::slice::from_ref(&condition.value), params)
            } else {
                scalar(params);
                format!("{expr} = ?")
            }
        }
        FilterOperator::NotEq => {
            if multi {
                let inner = any_of(std::slice::from_ref(&condition.value), params);
                format!("NOT ({inner})")
            } else {
                scalar(params);
                format!("({expr} IS NULL OR {expr} <> ?)")
            }
        }
        FilterOperator::Contains => {
            scalar(params);
            format!("lower(CAST({expr} AS TEXT)) LIKE '%' || lower(?) || '%'")
        }
        FilterOperator::NotContains => {
            scalar(params);
            format!(
                "({expr} IS NULL OR lower(CAST({expr} AS TEXT)) NOT LIKE '%' || lower(?) || '%')"
            )
        }
        FilterOperator::StartsWith => {
            scalar(params);
            format!("lower(CAST({expr} AS TEXT)) LIKE lower(?) || '%'")
        }
        FilterOperator::EndsWith => {
            scalar(params);
            format!("lower(CAST({expr} AS TEXT)) LIKE '%' || lower(?)")
        }
        FilterOperator::Gt | FilterOperator::Gte | FilterOperator::Lt | FilterOperator::Lte => {
            let op = match condition.op {
                FilterOperator::Gt => ">",
                FilterOperator::Gte => ">=",
                FilterOperator::Lt => "<",
                _ => "<=",
            };
            scalar(params);
            // Text comparison is lexicographic, which is CORRECT for this app's
            // dates because they are fixed-width RFC-3339 (see models::now_rfc3339).
            if numeric {
                format!("CAST({expr} AS REAL) {op} CAST(? AS REAL)")
            } else {
                format!("{expr} {op} ?")
            }
        }
        FilterOperator::Between => {
            let bounds = condition.value.as_array().cloned().unwrap_or_default();
            if bounds.len() != 2 {
                return None;
            }
            params.push(sql_value(&bounds[0]));
            params.push(sql_value(&bounds[1]));
            if numeric {
                format!("CAST({expr} AS REAL) BETWEEN CAST(? AS REAL) AND CAST(? AS REAL)")
            } else {
                format!("{expr} BETWEEN ? AND ?")
            }
        }
        FilterOperator::IsEmpty => {
            if multi {
                format!("({expr} IS NULL OR json_array_length({expr}) = 0)")
            } else {
                format!("({expr} IS NULL OR CAST({expr} AS TEXT) = '')")
            }
        }
        FilterOperator::IsNotEmpty => {
            if multi {
                format!("({expr} IS NOT NULL AND json_array_length({expr}) > 0)")
            } else {
                format!("({expr} IS NOT NULL AND CAST({expr} AS TEXT) <> '')")
            }
        }
        FilterOperator::IsAnyOf => {
            let values = condition
                .value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![condition.value.clone()]);
            any_of(&values, params)
        }
        FilterOperator::IsNoneOf => {
            let values = condition
                .value
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![condition.value.clone()]);
            let inner = any_of(&values, params);
            format!("NOT ({inner})")
        }
        FilterOperator::IsTrue => format!("{expr} = 1"),
        FilterOperator::IsFalse => format!("({expr} IS NULL OR {expr} = 0)"),
    };
    Some(sql)
}

/// Compile a filter tree. Always returns a valid boolean expression; an empty node
/// compiles to `1`.
fn build_filter(
    filter: &ViewFilter,
    index: &HashMap<String, Field>,
    alias: &str,
    params: &mut SqlParams,
) -> String {
    match filter {
        ViewFilter::And { filters } | ViewFilter::Or { filters } => {
            let joiner = if matches!(filter, ViewFilter::And { .. }) {
                " AND "
            } else {
                " OR "
            };
            let parts: Vec<String> = filters
                .iter()
                .map(|f| build_filter(f, index, alias, params))
                .filter(|p| p != "1")
                .collect();
            if parts.is_empty() {
                "1".to_string()
            } else {
                format!("({})", parts.join(joiner))
            }
        }
        ViewFilter::Not { filter } => {
            let inner = build_filter(filter, index, alias, params);
            if inner == "1" {
                "1".to_string()
            } else {
                format!("NOT ({inner})")
            }
        }
        ViewFilter::Condition(condition) => {
            build_condition(condition, index, alias, params).unwrap_or_else(|| "1".to_string())
        }
    }
}

/// Compile a sort list into an `ORDER BY` body, always ending in `id` so pagination
/// cannot repeat or skip a row when two rows tie.
fn build_order_by(sorts: &[ViewSort], index: &HashMap<String, Field>, alias: &str) -> String {
    let mut parts = Vec::new();
    for sort in sorts.iter().take(MAX_SORTS) {
        let Some((expr, field)) = value_expr(index, &sort.field_id, alias) else {
            continue;
        };
        let text = field.as_ref().is_none_or(|f| !f.field_type.is_numeric());
        let collate = if text { " COLLATE NOCASE" } else { "" };
        // NULLs last in both directions: an unset field is not "smallest", it is
        // absent, and a table whose blank rows float to the top is unusable.
        parts.push(format!(
            "({expr} IS NULL) ASC, {expr}{collate} {}",
            sort.direction.as_sql()
        ));
    }
    parts.push(format!("{alias}.id ASC"));
    parts.join(", ")
}

/// More than this many sorts is a UI bug, and each one is an unindexed
/// `json_extract` on every candidate row.
const MAX_SORTS: usize = 4;

// ── Records ────────────────────────────────────────────────────────────────────

impl CrmStore {
    pub async fn get_record(&self, record_id: &str) -> Result<Option<Record>> {
        let conn = self.conn.lock().await;
        load_record(&conn, record_id)
    }

    /// Everything the record drawer renders, in one lock.
    pub async fn get_record_detail(&self, record_id: &str) -> Result<Option<RecordDetail>> {
        let conn = self.conn.lock().await;
        let Some(record) = load_record(&conn, record_id)? else {
            return Ok(None);
        };
        let Some(object) = load_object(&conn, &record.object_id)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        let links = link_views(&conn, &record.id)?;
        let activities = {
            let sql = format!(
                "SELECT {COLS_ACTIVITY} FROM activities WHERE record_id = ?1
                 ORDER BY created_at DESC, id DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![record.id, RecordDetail::TIMELINE_LIMIT as i64],
                row_to_activity,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let lists = {
            let mut stmt = conn.prepare(
                "SELECT l.id, l.name, e.id FROM list_entries e
                 JOIN lists l ON l.id = e.list_id
                 WHERE e.record_id = ?1 ORDER BY l.position ASC",
            )?;
            let rows = stmt.query_map(params![record.id], |row| {
                Ok(ListMembership {
                    list_id: row.get(0)?,
                    list_name: row.get(1)?,
                    entry_id: row.get(2)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Some(RecordDetail {
            record,
            object,
            fields,
            links,
            activities,
            lists,
        }))
    }

    /// Create a record.
    ///
    /// `object_id` must already be resolved by the caller — a handler resolves the
    /// object anyway (it needs it for the event payload), and having the store
    /// re-resolve it would double every lookup. An unknown id is an internal error,
    /// not a validation failure.
    pub async fn create_record(
        &self,
        object_id: &str,
        req: &CreateRecordRequest,
    ) -> Validated<Record> {
        let mut conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let fields = load_fields(&conn, &object.id)?;
        let mut incoming = req.values.clone();
        apply_defaults(&fields, &mut incoming);
        let validated = validate_bag(&conn, &object.id, &fields, &incoming, false, None)?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut values = validated.values;
        prune_nulls(&mut values);

        let tx = conn.transaction()?;
        let record = insert_record(&tx, &object, &fields, values, req.created_by.as_deref())?;
        tx.commit()?;
        Ok(Ok(record))
    }

    /// Update a record's values. `Ok(Ok(None))` = no such record.
    ///
    /// Returns the diff alongside the row; an empty `changed` means the write was a
    /// no-op and the caller must NOT emit `record.updated`.
    pub async fn update_record(
        &self,
        record_id: &str,
        req: &UpdateRecordRequest,
    ) -> Validated<Option<RecordUpdate>> {
        let mut conn = self.conn.lock().await;
        let Some(existing) = load_record(&conn, record_id)? else {
            return Ok(Ok(None));
        };
        let Some(object) = load_object(&conn, &existing.object_id)? else {
            bail!("record {record_id} points at a missing object");
        };
        let fields = load_fields(&conn, &object.id)?;
        let partial = req.mode == UpdateMode::Merge;
        let validated = validate_bag(
            &conn,
            &object.id,
            &fields,
            &req.values,
            partial,
            Some(&existing.id),
        )?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }

        let mut next = match req.mode {
            UpdateMode::Merge => {
                let mut merged = existing.values.clone();
                for (slug, value) in validated.values {
                    if value.is_null() {
                        merged.remove(&slug);
                    } else {
                        merged.insert(slug, value);
                    }
                }
                merged
            }
            UpdateMode::Replace => validated.values,
        };
        prune_nulls(&mut next);

        let tx = conn.transaction()?;
        let update = write_record_values(&tx, &object, &fields, &existing, next)?;
        tx.commit()?;
        Ok(Ok(Some(update)))
    }

    /// Soft delete. The row survives so its timeline, links and list memberships
    /// stay explicable and so an accidental delete is one restore away.
    pub async fn delete_record(&self, record_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE records SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![record_id, now],
        )?;
        Ok(n > 0)
    }

    pub async fn restore_record(&self, record_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE records SET deleted_at = NULL, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![record_id, now],
        )?;
        Ok(n > 0)
    }

    /// Hard delete, with the ordered cascade. Irreversible.
    pub async fn purge_record(&self, record_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        if load_record(&conn, record_id)?.is_none() {
            return Ok(false);
        }
        let tx = conn.transaction()?;
        fts_delete(&tx, record_id)?;
        tx.execute(
            "DELETE FROM record_links WHERE source_record_id = ?1 OR target_record_id = ?1",
            params![record_id],
        )?;
        tx.execute(
            "DELETE FROM list_entries WHERE record_id = ?1",
            params![record_id],
        )?;
        tx.execute(
            "DELETE FROM activities WHERE record_id = ?1",
            params![record_id],
        )?;
        let n = tx.execute("DELETE FROM records WHERE id = ?1", params![record_id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// The one paginated record query. Filters, sorts, FTS pre-filter, list scoping
    /// and explicit id sets all compose here.
    pub async fn query_records(
        &self,
        query: &RecordQuery,
        limit: usize,
        offset: usize,
    ) -> Result<RecordPage> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &query.object_id)? else {
            return Ok(Page::empty(limit, offset));
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let (where_sql, mut params) = build_record_where(query, &object.id, &index);

        let count_sql = format!("SELECT COUNT(*) FROM records r WHERE {where_sql}");
        let total: i64 =
            conn.query_row(&count_sql, params_from_iter(params.clone()), |r| r.get(0))?;

        let order_by = build_order_by(&query.sorts, &index, "r");
        let sql = format!(
            "SELECT {COLS_RECORD} FROM records r WHERE {where_sql} ORDER BY {order_by} LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), row_to_record)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }

    /// Count only — the board's per-column totals and the summary strip.
    pub async fn count_records(&self, query: &RecordQuery) -> Result<i64> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &query.object_id)? else {
            return Ok(0);
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let (where_sql, params) = build_record_where(query, &object.id, &index);
        let sql = format!("SELECT COUNT(*) FROM records r WHERE {where_sql}");
        Ok(conn.query_row(&sql, params_from_iter(params), |r| r.get(0))?)
    }

    /// Validate a bag without writing. The panel's inline-edit path calls this to
    /// show an error before it commits a cell.
    pub async fn validate_values(
        &self,
        object_id: &str,
        values: &ValueBag,
        partial: bool,
        exclude_record_id: Option<&str>,
    ) -> Result<ValidatedValues> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let fields = load_fields(&conn, &object.id)?;
        validate_bag(
            &conn,
            &object.id,
            &fields,
            values,
            partial,
            exclude_record_id,
        )
    }
}

/// Assemble the `WHERE` body shared by `query_records` and `count_records`, so the
/// count can never disagree with the page it describes.
fn build_record_where(
    query: &RecordQuery,
    object_id: &str,
    index: &HashMap<String, Field>,
) -> (String, SqlParams) {
    let mut params: SqlParams = Vec::new();
    let mut clauses = vec!["r.object_id = ?".to_string()];
    params.push(rusqlite::types::Value::Text(object_id.to_string()));

    if !query.include_deleted {
        clauses.push("r.deleted_at IS NULL".to_string());
    }
    if let Some(list_id) = query.list_id.as_deref().filter(|l| !l.is_empty()) {
        clauses.push(
            "EXISTS (SELECT 1 FROM list_entries le WHERE le.record_id = r.id AND le.list_id = ?)"
                .to_string(),
        );
        params.push(rusqlite::types::Value::Text(list_id.to_string()));
    }
    if let Some(ids) = query.record_ids.as_ref() {
        if ids.is_empty() {
            clauses.push("0".to_string());
        } else {
            let placeholders = ids
                .iter()
                .map(|id| {
                    params.push(rusqlite::types::Value::Text(id.clone()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("r.id IN ({placeholders})"));
        }
    }
    if let Some(expression) = query.search.as_deref().and_then(fts_match_expression) {
        clauses.push(
            "r.rowid IN (SELECT rowid FROM records_fts WHERE records_fts MATCH ?)".to_string(),
        );
        params.push(rusqlite::types::Value::Text(expression));
    }
    if let Some(filter) = query.filter.as_ref().filter(|f| !f.is_empty()) {
        clauses.push(build_filter(filter, index, "r", &mut params));
    }
    (clauses.join(" AND "), params)
}

/// Insert one record, maintaining links and the FTS index. Takes an ALREADY
/// VALIDATED bag.
fn insert_record(
    conn: &Connection,
    object: &Object,
    fields: &[Field],
    values: ValueBag,
    created_by: Option<&str>,
) -> Result<Record> {
    let now = now_rfc3339();
    let record = Record {
        id: new_id(ID_RECORD),
        object_id: object.id.clone(),
        title: compute_title(object, fields, &values),
        values,
        deleted_at: None,
        created_by: created_by.map(str::to_string),
        created_at: now.clone(),
        updated_at: now,
    };
    conn.execute(
        "INSERT INTO records (id, object_id, title, data, deleted_at, created_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7)",
        params![
            record.id,
            record.object_id,
            record.title,
            encode_json(&record.values),
            record.created_by,
            record.created_at,
            record.updated_at
        ],
    )?;
    sync_links(conn, object, fields, &record.id, &record.values)?;
    fts_reindex(
        conn,
        &record.id,
        &record.title,
        &fts_body(fields, &record.values),
    )?;
    Ok(record)
}

/// Write a new value bag over an existing record, computing the diff, maintaining
/// links and FTS, and writing the automatic timeline entries.
fn write_record_values(
    conn: &Connection,
    object: &Object,
    fields: &[Field],
    existing: &Record,
    next: ValueBag,
) -> Result<RecordUpdate> {
    let changed = diff_values(fields, &existing.values, &next);
    if changed.is_empty() {
        return Ok(RecordUpdate {
            record: existing.clone(),
            changed,
            stage_change: None,
        });
    }
    let now = now_rfc3339();
    let title = compute_title(object, fields, &next);
    conn.execute(
        "UPDATE records SET title = ?2, data = ?3, updated_at = ?4 WHERE id = ?1",
        params![existing.id, title, encode_json(&next), now],
    )?;
    sync_links(conn, object, fields, &existing.id, &next)?;
    fts_reindex(conn, &existing.id, &title, &fts_body(fields, &next))?;

    let stage_change = stage_change_from(fields, &changed);
    log_change_activities(conn, object, &existing.id, &changed, stage_change.as_ref())?;

    Ok(RecordUpdate {
        record: Record {
            title,
            values: next,
            updated_at: now,
            ..existing.clone()
        },
        changed,
        stage_change,
    })
}

/// Per-field before/after, in field position order so a timeline reads top-to-bottom
/// like the form does.
fn diff_values(fields: &[Field], before: &ValueBag, after: &ValueBag) -> Vec<FieldChange> {
    let mut changes = Vec::new();
    for field in fields {
        let from = before.get(&field.slug).cloned().unwrap_or(Value::Null);
        let to = after.get(&field.slug).cloned().unwrap_or(Value::Null);
        if from == to {
            continue;
        }
        changes.push(FieldChange {
            field_id: field.id.clone(),
            field_slug: field.slug.clone(),
            field_name: field.name.clone(),
            from,
            to,
        });
    }
    changes
}

/// Extract the first `status`-field transition from a diff, with both option ids and
/// both labels resolved.
fn stage_change_from(fields: &[Field], changes: &[FieldChange]) -> Option<StageChange> {
    for change in changes {
        let field = fields
            .iter()
            .find(|f| f.id == change.field_id && f.field_type == FieldType::Status)?;
        let label = |value: &Value| -> Option<String> {
            value
                .as_str()
                .and_then(|id| field.config.option(id))
                .map(|o| o.label.clone())
        };
        return Some(StageChange {
            field_id: field.id.clone(),
            field_slug: field.slug.clone(),
            from_label: label(&change.from),
            from: change.from.as_str().map(str::to_string),
            to_label: label(&change.to),
            to: change.to.as_str().map(str::to_string),
        });
    }
    None
}

/// Write the automatic `field_change` entry and, when a status field moved, the
/// `stage_change` entry the pipeline/funnel report reads.
fn log_change_activities(
    conn: &Connection,
    object: &Object,
    record_id: &str,
    changes: &[FieldChange],
    stage: Option<&StageChange>,
) -> Result<()> {
    let now = now_rfc3339();
    // ONE `field_change` per update, not one per field: a five-field save is one
    // edit, and five timeline rows for it is noise that buries the note above them.
    let summary = if changes.len() == 1 {
        format!("changed {}", changes[0].field_name)
    } else {
        format!("changed {} fields", changes.len())
    };
    conn.execute(
        "INSERT INTO activities
           (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
            assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'field_change', ?4, NULL, ?5, ?6, ?7, NULL, NULL, NULL, NULL, NULL, ?8, ?9, ?9)",
        params![
            new_id(ID_ACTIVITY),
            record_id,
            object.id,
            summary,
            changes.first().map(|c| c.field_id.clone()),
            changes.first().map(|c| encode_json(&c.from)),
            changes.first().map(|c| encode_json(&c.to)),
            encode_json(&json!({ "changes": changes })),
            now
        ],
    )?;
    if let Some(stage) = stage {
        conn.execute(
            "INSERT INTO activities
               (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
                assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'stage_change', ?4, NULL, ?5, ?6, ?7, NULL, NULL, NULL, NULL, NULL, NULL, ?8, ?8)",
            params![
                new_id(ID_ACTIVITY),
                record_id,
                object.id,
                format!(
                    "{} → {}",
                    stage.from_label.clone().unwrap_or_else(|| "—".to_string()),
                    stage.to_label.clone().unwrap_or_else(|| "—".to_string())
                ),
                stage.field_id,
                stage.from.as_ref().map(|v| encode_json(&json!(v))),
                stage.to.as_ref().map(|v| encode_json(&json!(v))),
                now
            ],
        )?;
    }
    Ok(())
}

// ── Relations ──────────────────────────────────────────────────────────────────

/// Bring the materialised edges for a record in line with its value bag.
///
/// The bag is authoritative and the edge table is a projection of it. Reconciling
/// (delete the gone, insert the new) rather than delete-all-then-reinsert keeps
/// `created_at` meaningful — "when did we link this company to this deal" is a
/// question people ask, and rewriting every edge on every unrelated save would
/// answer it with the time of the last edit to anything.
fn sync_links(
    conn: &Connection,
    object: &Object,
    fields: &[Field],
    record_id: &str,
    values: &ValueBag,
) -> Result<()> {
    let now = now_rfc3339();
    for field in fields
        .iter()
        .filter(|f| f.field_type == FieldType::Relation)
    {
        let Some(target_object) = field.config.relation_object_id.as_deref() else {
            continue;
        };
        let wanted: Vec<String> = values.get(&field.slug).map(as_list).unwrap_or_default();

        let existing: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT target_record_id FROM record_links WHERE field_id = ?1 AND source_record_id = ?2",
            )?;
            let rows = stmt.query_map(params![field.id, record_id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for gone in existing.iter().filter(|id| !wanted.contains(id)) {
            conn.execute(
                "DELETE FROM record_links WHERE field_id = ?1 AND source_record_id = ?2 AND target_record_id = ?3",
                params![field.id, record_id, gone],
            )?;
        }
        for added in wanted.iter().filter(|id| !existing.contains(id)) {
            conn.execute(
                "INSERT OR IGNORE INTO record_links
                   (id, field_id, source_record_id, source_object_id, target_record_id, target_object_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    new_id(ID_LINK),
                    field.id,
                    record_id,
                    object.id,
                    added,
                    target_object,
                    now
                ],
            )?;
        }
    }
    Ok(())
}

/// Every edge touching `record_id`, from that record's point of view, with the other
/// end's title resolved and the direction-appropriate label chosen.
fn link_views(conn: &Connection, record_id: &str) -> Result<Vec<RecordLinkView>> {
    let sql = format!(
        "SELECT {COLS_LINK} FROM record_links
          WHERE source_record_id = ?1 OR target_record_id = ?1
          ORDER BY created_at ASC, id ASC"
    );
    let links = {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![record_id], row_to_link)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut out = Vec::with_capacity(links.len());
    for link in links {
        let outgoing = link.source_record_id == record_id;
        let other_id = if outgoing {
            &link.target_record_id
        } else {
            &link.source_record_id
        };
        let other_object = if outgoing {
            &link.target_object_id
        } else {
            &link.source_object_id
        };
        let title: Option<String> = conn
            .query_row(
                "SELECT title FROM records WHERE id = ?1 AND deleted_at IS NULL",
                params![other_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(title) = title else { continue };
        let field = load_field(conn, &link.field_id)?;
        let label = if outgoing {
            field
                .as_ref()
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "Related".to_string())
        } else {
            // The inverse name, or the SOURCE object's plural. Without this the
            // company's page would label the edge with the person's field name and
            // read "Company: Jane Doe".
            field
                .as_ref()
                .and_then(|f| f.config.relation_inverse_label.clone())
                .or_else(|| {
                    load_object(conn, &link.source_object_id)
                        .ok()
                        .flatten()
                        .map(|o| o.plural)
                })
                .unwrap_or_else(|| "Related".to_string())
        };
        out.push(RecordLinkView {
            link_id: link.id,
            field_id: link.field_id,
            label,
            direction: if outgoing {
                LinkDirection::Outgoing
            } else {
                LinkDirection::Incoming
            },
            record_id: other_id.clone(),
            object_id: other_object.clone(),
            title,
            created_at: link.created_at,
        });
    }
    Ok(out)
}

impl CrmStore {
    pub async fn list_links(&self, record_id: &str) -> Result<Vec<RecordLinkView>> {
        let conn = self.conn.lock().await;
        link_views(&conn, record_id)
    }

    /// Add relation targets. Writes the record's value bag — the bag is
    /// authoritative and `sync_links` projects it — so a link and an inline edit of
    /// the same field can never disagree.
    pub async fn link_records(
        &self,
        record_id: &str,
        req: &LinkRequest,
    ) -> Validated<Option<RecordUpdate>> {
        self.mutate_links(record_id, req, true).await
    }

    pub async fn unlink_records(
        &self,
        record_id: &str,
        req: &LinkRequest,
    ) -> Validated<Option<RecordUpdate>> {
        self.mutate_links(record_id, req, false).await
    }

    async fn mutate_links(
        &self,
        record_id: &str,
        req: &LinkRequest,
        add: bool,
    ) -> Validated<Option<RecordUpdate>> {
        let mut conn = self.conn.lock().await;
        let Some(existing) = load_record(&conn, record_id)? else {
            return Ok(Ok(None));
        };
        let Some(object) = load_object(&conn, &existing.object_id)? else {
            bail!("record {record_id} points at a missing object");
        };
        let fields = load_fields(&conn, &object.id)?;
        let Some(field) = fields
            .iter()
            .find(|f| f.id == req.field_id || f.slug == req.field_id)
            .filter(|f| f.field_type == FieldType::Relation)
            .cloned()
        else {
            return Ok(Err(vec![FieldValidationError::coded(
                &req.field_id,
                &req.field_id,
                ValidationCode::UnknownField,
                "not a relation field on this object",
            )]));
        };

        let mut targets: Vec<String> = existing
            .values
            .get(&field.slug)
            .map(as_list)
            .unwrap_or_default();
        for id in &req.target_record_ids {
            if add {
                if !targets.contains(id) {
                    targets.push(id.clone());
                }
            } else {
                targets.retain(|t| t != id);
            }
        }
        let mut patch = ValueBag::new();
        patch.insert(field.slug.clone(), json!(targets));
        let validated = validate_bag(&conn, &object.id, &fields, &patch, true, Some(&existing.id))?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut next = existing.values.clone();
        for (slug, value) in validated.values {
            if value.is_null() {
                next.remove(&slug);
            } else {
                next.insert(slug, value);
            }
        }
        prune_nulls(&mut next);

        let tx = conn.transaction()?;
        let update = write_record_values(&tx, &object, &fields, &existing, next)?;
        tx.commit()?;
        Ok(Ok(Some(update)))
    }

    /// Records on the other end of this record's edges.
    pub async fn related_records(
        &self,
        record_id: &str,
        query: &RelatedQuery,
        limit: usize,
        offset: usize,
    ) -> Result<RecordPage> {
        let conn = self.conn.lock().await;
        let mut views = link_views(&conn, record_id)?;
        if let Some(field_id) = query.field_id.as_deref().filter(|f| !f.is_empty()) {
            views.retain(|v| v.field_id == field_id);
        }
        let total = views.len() as i64;
        let ids: Vec<String> = views
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|v| v.record_id)
            .collect();
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(record) = load_record(&conn, &id)? {
                items.push(record);
            }
        }
        Ok(Page::new(items, total, limit, offset))
    }
}

// ── Dedupe + merge ─────────────────────────────────────────────────────────────

impl CrmStore {
    /// Records that share a normalized value on one of the match fields.
    ///
    /// With no fields named, the scan picks them: `is_unique` fields first, then
    /// email fields, then the title field. Returning WHICH fields it chose is part
    /// of the contract — a duplicate list the user cannot explain is a list they
    /// will not act on.
    pub async fn merge_candidates(
        &self,
        object_id: &str,
        req: &DuplicateScanRequest,
        limit: usize,
    ) -> Result<DuplicateScanResponse> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(DuplicateScanResponse {
                candidates: Vec::new(),
                field_ids: Vec::new(),
            });
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let chosen: Vec<Field> = if req.field_ids.is_empty() {
            let unique: Vec<Field> = fields.iter().filter(|f| f.is_unique).cloned().collect();
            let emails: Vec<Field> = fields
                .iter()
                .filter(|f| f.field_type == FieldType::Email)
                .cloned()
                .collect();
            if !unique.is_empty() {
                unique
            } else if !emails.is_empty() {
                emails
            } else {
                object
                    .title_field_id
                    .as_deref()
                    .and_then(|id| index.get(id).cloned())
                    .into_iter()
                    .collect()
            }
        } else {
            req.field_ids
                .iter()
                .filter_map(|id| index.get(id.as_str()).cloned())
                .collect()
        };

        let mut candidates = Vec::new();
        for field in &chosen {
            let sql = format!(
                "SELECT lower(trim(CAST(json_extract(data, '$.{slug}') AS TEXT))) AS k,
                        group_concat(id, char(10)), COUNT(*)
                   FROM records
                  WHERE object_id = ?1 AND deleted_at IS NULL
                    AND json_extract(data, '$.{slug}') IS NOT NULL
                    AND trim(CAST(json_extract(data, '$.{slug}') AS TEXT)) <> ''
                  GROUP BY k HAVING COUNT(*) > 1
                  ORDER BY COUNT(*) DESC LIMIT ?2",
                slug = field.slug
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![object.id, limit as i64], |row| {
                let value: String = row.get(0)?;
                let ids: String = row.get(1)?;
                Ok((value, ids))
            })?;
            for row in rows {
                let (value, joined) = row?;
                // group_concat gives no ordering guarantee; sorting the ULID-ish ids
                // makes `record_ids[0]` deterministically the OLDEST, which is what
                // the UI suggests as the survivor.
                let mut record_ids: Vec<String> = joined
                    .split('\n')
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .collect();
                record_ids.sort();
                let mut titles = Vec::with_capacity(record_ids.len());
                for id in &record_ids {
                    let title: Option<String> = conn
                        .query_row(
                            "SELECT title FROM records WHERE id = ?1",
                            params![id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    titles.push(title.unwrap_or_default());
                }
                candidates.push(MergeCandidate {
                    record_ids,
                    field_id: field.id.clone(),
                    field_slug: field.slug.clone(),
                    value,
                    score: 1.0,
                    titles,
                });
            }
        }
        candidates.truncate(limit);
        Ok(DuplicateScanResponse {
            candidates,
            field_ids: chosen.into_iter().map(|f| f.id).collect(),
        })
    }

    /// Dry run of a merge. Writes nothing.
    pub async fn plan_merge(&self, plan: &MergePlan) -> Result<Option<MergePreview>> {
        let conn = self.conn.lock().await;
        let Some((survivor, losers, fields, resolved, conflicts)) = resolve_merge(&conn, plan)?
        else {
            return Ok(None);
        };
        let _ = fields;
        let mut activity_count = 0i64;
        let mut link_count = 0i64;
        let mut list_entry_count = 0i64;
        for loser in &losers {
            activity_count += conn.query_row(
                "SELECT COUNT(*) FROM activities WHERE record_id = ?1",
                params![loser.id],
                |r| r.get::<_, i64>(0),
            )?;
            link_count += conn.query_row(
                "SELECT COUNT(*) FROM record_links WHERE source_record_id = ?1 OR target_record_id = ?1",
                params![loser.id],
                |r| r.get::<_, i64>(0),
            )?;
            list_entry_count += conn.query_row(
                "SELECT COUNT(*) FROM list_entries WHERE record_id = ?1",
                params![loser.id],
                |r| r.get::<_, i64>(0),
            )?;
        }
        Ok(Some(MergePreview {
            survivor,
            losers,
            resolved_values: resolved,
            conflicts,
            activity_count,
            link_count,
            list_entry_count,
        }))
    }

    /// Perform the merge: resolve values onto the survivor, move history, links and
    /// list memberships, then retire the losers. One transaction — a half-merged
    /// pair is worse than either outcome.
    pub async fn merge_records(&self, plan: &MergePlan) -> Result<Option<MergeOutcome>> {
        let mut conn = self.conn.lock().await;
        let Some((survivor, losers, fields, resolved, _)) = resolve_merge(&conn, plan)? else {
            return Ok(None);
        };
        let Some(object) = load_object(&conn, &survivor.object_id)? else {
            bail!("record {} points at a missing object", survivor.id);
        };
        let now = now_rfc3339();
        let tx = conn.transaction()?;

        let mut moved_activities = 0i64;
        let mut moved_links = 0i64;
        let mut moved_list_entries = 0i64;
        for loser in &losers {
            moved_activities += tx.execute(
                "UPDATE activities SET record_id = ?2, updated_at = ?3 WHERE record_id = ?1",
                params![loser.id, survivor.id, now],
            )? as i64;
            // `INSERT OR IGNORE`-style repointing: the unique edge index rejects a
            // duplicate, so re-point what can move and drop what would collide.
            moved_links += tx.execute(
                "UPDATE OR IGNORE record_links SET source_record_id = ?2 WHERE source_record_id = ?1",
                params![loser.id, survivor.id],
            )? as i64;
            moved_links += tx.execute(
                "UPDATE OR IGNORE record_links SET target_record_id = ?2 WHERE target_record_id = ?1",
                params![loser.id, survivor.id],
            )? as i64;
            tx.execute(
                "DELETE FROM record_links WHERE source_record_id = ?1 OR target_record_id = ?1",
                params![loser.id],
            )?;
            moved_list_entries += tx.execute(
                "UPDATE OR IGNORE list_entries SET record_id = ?2, updated_at = ?3 WHERE record_id = ?1",
                params![loser.id, survivor.id, now],
            )? as i64;
            tx.execute(
                "DELETE FROM list_entries WHERE record_id = ?1",
                params![loser.id],
            )?;

            if plan.soft_delete_losers {
                tx.execute(
                    "UPDATE records SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![loser.id, now],
                )?;
            } else {
                fts_delete(&tx, &loser.id)?;
                tx.execute("DELETE FROM records WHERE id = ?1", params![loser.id])?;
            }
        }

        let update = write_record_values(&tx, &object, &fields, &survivor, resolved)?;
        // A dedicated timeline entry, because "why does this record now say what the
        // other one said" is the first question after any merge.
        tx.execute(
            "INSERT INTO activities
               (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
                assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'note', ?4, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, ?5, ?6, ?6)",
            params![
                new_id(ID_ACTIVITY),
                survivor.id,
                object.id,
                format!("Merged {} record(s) into this one", losers.len()),
                encode_json(&json!({ "merged_record_ids": losers.iter().map(|l| l.id.clone()).collect::<Vec<_>>() })),
                now
            ],
        )?;
        tx.commit()?;

        Ok(Some(MergeOutcome {
            survivor: update.record,
            merged_record_ids: losers.into_iter().map(|l| l.id).collect(),
            moved_activities,
            moved_links,
            moved_list_entries,
            changed: update.changed,
        }))
    }
}

/// Shared by the preview and the apply, so the dry run cannot describe a different
/// merge from the one that happens.
#[allow(clippy::type_complexity)]
fn resolve_merge(
    conn: &Connection,
    plan: &MergePlan,
) -> Result<
    Option<(
        Record,
        Vec<Record>,
        Vec<Field>,
        ValueBag,
        Vec<MergeConflict>,
    )>,
> {
    let Some(survivor) = load_record(conn, &plan.survivor_id)? else {
        return Ok(None);
    };
    let mut losers = Vec::new();
    for id in &plan.loser_ids {
        if id == &survivor.id {
            bail!("a record cannot be merged into itself");
        }
        let Some(loser) = load_record(conn, id)? else {
            continue;
        };
        if loser.object_id != survivor.object_id {
            bail!("records on different objects cannot be merged");
        }
        losers.push(loser);
    }
    if losers.is_empty() {
        bail!("a merge needs at least one record to merge in");
    }
    let fields = load_fields(conn, &survivor.object_id)?;
    let explicit: HashMap<&str, &MergeSource> = plan
        .resolutions
        .iter()
        .map(|r| (r.field_id.as_str(), &r.source))
        .collect();

    let mut resolved = survivor.values.clone();
    let mut conflicts = Vec::new();
    for field in &fields {
        let survivor_value = survivor
            .values
            .get(&field.slug)
            .cloned()
            .unwrap_or(Value::Null);
        let differing: Vec<MergeLoserValue> = losers
            .iter()
            .filter_map(|loser| {
                let value = loser
                    .values
                    .get(&field.slug)
                    .cloned()
                    .unwrap_or(Value::Null);
                (!is_empty_value(&value) && value != survivor_value).then(|| MergeLoserValue {
                    record_id: loser.id.clone(),
                    title: loser.title.clone(),
                    value,
                })
            })
            .collect();

        let source = explicit
            .get(field.id.as_str())
            .or_else(|| explicit.get(field.slug.as_str()));
        let chosen = match source {
            Some(MergeSource::Survivor) => survivor_value.clone(),
            Some(MergeSource::Loser { record_id }) => losers
                .iter()
                .find(|l| &l.id == record_id)
                .and_then(|l| l.values.get(&field.slug).cloned())
                .unwrap_or(Value::Null),
            Some(MergeSource::Value { value }) => match validate_field_value(field, value) {
                Ok(v) => v.unwrap_or(Value::Null),
                Err(_) => survivor_value.clone(),
            },
            // The default: keep what the survivor has, and only FILL A BLANK from a
            // loser. Never silently overwrite — that is the behaviour every CRM gets
            // complained about.
            None => {
                if is_empty_value(&survivor_value) {
                    differing
                        .first()
                        .map(|l| l.value.clone())
                        .unwrap_or(Value::Null)
                } else {
                    survivor_value.clone()
                }
            }
        };

        if !differing.is_empty() && !is_empty_value(&survivor_value) {
            conflicts.push(MergeConflict {
                field_id: field.id.clone(),
                field_slug: field.slug.clone(),
                field_name: field.name.clone(),
                survivor_value: survivor_value.clone(),
                loser_values: differing,
            });
        }
        if is_empty_value(&chosen) {
            resolved.remove(&field.slug);
        } else {
            resolved.insert(field.slug.clone(), chosen);
        }
    }
    Ok(Some((survivor, losers, fields, resolved, conflicts)))
}

// ── Views ──────────────────────────────────────────────────────────────────────

fn load_views(conn: &Connection, object_id: &str) -> Result<Vec<View>> {
    let sql = format!(
        "SELECT {COLS_VIEW} FROM views WHERE object_id = ?1 ORDER BY position ASC, created_at ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![object_id], row_to_view)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn load_view(conn: &Connection, view_id: &str) -> Result<Option<View>> {
    let sql = format!("SELECT {COLS_VIEW} FROM views WHERE id = ?1");
    Ok(conn
        .query_row(&sql, params![view_id], row_to_view)
        .optional()?)
}

impl CrmStore {
    pub async fn list_views(&self, object_id: &str) -> Result<Vec<View>> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            return Ok(Vec::new());
        };
        load_views(&conn, &object.id)
    }

    pub async fn get_view(&self, view_id: &str) -> Result<Option<View>> {
        let conn = self.conn.lock().await;
        load_view(&conn, view_id)
    }

    pub async fn create_view(&self, object_id: &str, req: &CreateViewRequest) -> Result<View> {
        let mut conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, object_id)? else {
            bail!("unknown object \"{object_id}\"");
        };
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        let id = new_id(ID_VIEW);
        let position = next_position(
            &tx,
            "SELECT MAX(position) FROM views WHERE object_id = ?1",
            &object.id,
        )?;
        if req.is_default {
            tx.execute(
                "UPDATE views SET is_default = 0 WHERE object_id = ?1",
                params![object.id],
            )?;
        }
        tx.execute(
            "INSERT INTO views
               (id, object_id, name, kind, filter, sorts, visible_fields, group_by_field_id,
                is_default, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                id,
                object.id,
                req.name.trim(),
                req.kind.as_str(),
                req.filter.as_ref().map(encode_json),
                encode_json(&req.sorts),
                encode_json(&req.visible_field_ids),
                req.group_by_field_id,
                i64::from(req.is_default),
                position,
                now
            ],
        )?;
        let view = load_view(&tx, &id)?.context("re-reading the view just created")?;
        tx.commit()?;
        Ok(view)
    }

    pub async fn update_view(
        &self,
        view_id: &str,
        req: &UpdateViewRequest,
    ) -> Result<Option<View>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_view(&conn, view_id)? else {
            return Ok(None);
        };
        let now = now_rfc3339();
        conn.execute(
            "UPDATE views SET name = ?2, kind = ?3, filter = ?4, sorts = ?5, visible_fields = ?6,
                              group_by_field_id = ?7, position = ?8, updated_at = ?9
             WHERE id = ?1",
            params![
                existing.id,
                req.name
                    .as_deref()
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(&existing.name),
                req.kind.unwrap_or(existing.kind).as_str(),
                req.filter
                    .as_ref()
                    .or(existing.filter.as_ref())
                    .map(encode_json),
                encode_json(req.sorts.as_ref().unwrap_or(&existing.sorts)),
                encode_json(
                    req.visible_field_ids
                        .as_ref()
                        .unwrap_or(&existing.visible_field_ids)
                ),
                req.group_by_field_id
                    .as_ref()
                    .or(existing.group_by_field_id.as_ref()),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        load_view(&conn, &existing.id)
    }

    /// Delete a view. Refuses the last one on an object — an object with no view has
    /// no way to open it.
    pub async fn delete_view(&self, view_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(view) = load_view(&conn, view_id)? else {
            return Ok(false);
        };
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM views WHERE object_id = ?1",
            params![view.object_id],
            |r| r.get(0),
        )?;
        if remaining <= 1 {
            bail!("an object must keep at least one view");
        }
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM views WHERE id = ?1", params![view.id])?;
        if view.is_default {
            // Promote the next one rather than leaving the object with no default.
            tx.execute(
                "UPDATE views SET is_default = 1 WHERE id = (
                     SELECT id FROM views WHERE object_id = ?1 ORDER BY position ASC LIMIT 1)",
                params![view.object_id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    pub async fn set_default_view(&self, view_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        let Some(view) = load_view(&conn, view_id)? else {
            return Ok(false);
        };
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE views SET is_default = 0 WHERE object_id = ?1",
            params![view.object_id],
        )?;
        tx.execute(
            "UPDATE views SET is_default = 1 WHERE id = ?1",
            params![view.id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Run a saved view: the flat page always, plus board columns when the view is a
    /// board with a valid grouping field.
    pub async fn run_view(
        &self,
        view_id: &str,
        overrides: &ViewQueryOverrides,
        limit: usize,
        offset: usize,
    ) -> Result<Option<ViewResult>> {
        let conn = self.conn.lock().await;
        let Some(view) = load_view(&conn, view_id)? else {
            return Ok(None);
        };
        let Some(object) = load_object(&conn, &view.object_id)? else {
            return Ok(None);
        };
        let all_fields = load_fields(&conn, &object.id)?;
        let index = field_index(&all_fields);
        let fields: Vec<Field> = if view.visible_field_ids.is_empty() {
            all_fields.clone()
        } else {
            view.visible_field_ids
                .iter()
                .filter_map(|id| index.get(id.as_str()).cloned())
                .collect()
        };

        // The view's own filter is ANDed with the override, never replaced — see
        // `ViewQueryOverrides::filter`.
        let filter = match (view.filter.clone(), overrides.filter.clone()) {
            (Some(saved), Some(extra)) => Some(ViewFilter::And {
                filters: vec![saved, extra],
            }),
            (Some(saved), None) => Some(saved),
            (None, extra) => extra,
        };
        let sorts = overrides
            .sorts
            .clone()
            .unwrap_or_else(|| view.sorts.clone());
        let query = RecordQuery {
            object_id: object.id.clone(),
            filter,
            sorts,
            search: overrides.search.clone(),
            include_deleted: overrides.include_deleted,
            ..Default::default()
        };
        let (where_sql, mut params) = build_record_where(&query, &object.id, &index);
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM records r WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let order_by = build_order_by(&query.sorts, &index, "r");
        let items = {
            let sql = format!(
                "SELECT {COLS_RECORD} FROM records r WHERE {where_sql} ORDER BY {order_by} LIMIT ? OFFSET ?"
            );
            params.push(rusqlite::types::Value::Integer(limit as i64));
            params.push(rusqlite::types::Value::Integer(offset as i64));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), row_to_record)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let page = Page::new(items, total, limit, offset);

        let groups = if view.kind == ViewKind::Board {
            build_board_groups(&conn, &view, &object, &all_fields, &index, &query, limit)?
        } else {
            None
        };

        Ok(Some(ViewResult {
            view,
            fields,
            page,
            groups,
        }))
    }
}

/// One query per column. `N` is the option count (under twenty in practice), and the
/// alternative — one grouped query plus a windowed per-group limit — is materially
/// harder to read for no measurable gain at CRM scale.
fn build_board_groups(
    conn: &Connection,
    view: &View,
    object: &Object,
    all_fields: &[Field],
    index: &HashMap<String, Field>,
    base: &RecordQuery,
    per_group: usize,
) -> Result<Option<Vec<BoardGroup>>> {
    let Some(group_field) = view
        .group_by_field_id
        .as_deref()
        .and_then(|id| index.get(id))
        .filter(|f| f.field_type.is_option_backed())
    else {
        // A board mid-configuration must degrade, not 500.
        return Ok(None);
    };
    let value_field = all_fields
        .iter()
        .find(|f| f.field_type == FieldType::Currency)
        .cloned();

    let mut groups = Vec::new();
    let mut buckets: Vec<(Option<SelectOption>, i64)> = group_field
        .config
        .sorted_options()
        .into_iter()
        .map(|o| {
            let position = o.position;
            (Some(o), position)
        })
        .collect();
    // The "no value" column always exists and always sorts last: records with no
    // stage are the ones that need attention, and hiding them loses them.
    buckets.push((None, i64::MAX));

    for (option, position) in buckets {
        let condition = FilterCondition {
            field_id: group_field.id.clone(),
            op: match &option {
                Some(_) => FilterOperator::Eq,
                None => FilterOperator::IsEmpty,
            },
            value: option.as_ref().map(|o| json!(o.id)).unwrap_or(Value::Null),
        };
        let filter = match base.filter.clone() {
            Some(existing) => ViewFilter::And {
                filters: vec![existing, ViewFilter::Condition(condition)],
            },
            None => ViewFilter::Condition(condition),
        };
        let query = RecordQuery {
            filter: Some(filter),
            ..base.clone()
        };
        let (where_sql, mut params) = build_record_where(&query, &object.id, index);
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM records r WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let value_cents = match value_field.as_ref() {
            Some(field) => {
                let sum: Option<i64> = conn.query_row(
                    &format!(
                        "SELECT CAST(SUM(COALESCE(json_extract(r.data, '$.{}'), 0)) AS INTEGER)
                           FROM records r WHERE {where_sql}",
                        field.slug
                    ),
                    params_from_iter(params.clone()),
                    |r| r.get(0),
                )?;
                Some(sum.unwrap_or(0))
            }
            None => None,
        };
        let order_by = build_order_by(&query.sorts, index, "r");
        let records = {
            let sql = format!(
                "SELECT {COLS_RECORD} FROM records r WHERE {where_sql} ORDER BY {order_by} LIMIT ?"
            );
            params.push(rusqlite::types::Value::Integer(per_group as i64));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(params), row_to_record)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        groups.push(BoardGroup {
            option_id: option.as_ref().map(|o| o.id.clone()),
            label: option
                .as_ref()
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "No value".to_string()),
            color: option.as_ref().and_then(|o| o.color.clone()),
            position,
            total,
            value_cents,
            records,
        });
    }
    Ok(Some(groups))
}

// ── Lists ──────────────────────────────────────────────────────────────────────

impl CrmStore {
    pub async fn list_lists(&self, object_id: Option<&str>) -> Result<Vec<List>> {
        let conn = self.conn.lock().await;
        match object_id.filter(|o| !o.is_empty()) {
            Some(object_ref) => {
                let Some(object) = load_object(&conn, object_ref)? else {
                    return Ok(Vec::new());
                };
                let sql = format!(
                    "SELECT {COLS_LIST} FROM lists WHERE object_id = ?1 ORDER BY position ASC, created_at ASC"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![object.id], row_to_list)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
            None => {
                let sql =
                    format!("SELECT {COLS_LIST} FROM lists ORDER BY position ASC, created_at ASC");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], row_to_list)?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            }
        }
    }

    pub async fn get_list(&self, list_id: &str) -> Result<Option<List>> {
        let conn = self.conn.lock().await;
        load_list(&conn, list_id)
    }

    pub async fn create_list(&self, req: &CreateListRequest) -> Result<List> {
        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &req.object_id)? else {
            bail!("unknown object \"{}\"", req.object_id);
        };
        let now = now_rfc3339();
        let id = new_id(ID_LIST);
        let position = next_position(
            &conn,
            "SELECT MAX(position) FROM lists WHERE object_id = ?1",
            &object.id,
        )?;
        conn.execute(
            "INSERT INTO lists (id, object_id, name, description, icon, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
            params![id, object.id, req.name.trim(), req.description, req.icon, position, now],
        )?;
        load_list(&conn, &id)?.context("re-reading the list just created")
    }

    pub async fn update_list(
        &self,
        list_id: &str,
        req: &UpdateListRequest,
    ) -> Result<Option<List>> {
        let conn = self.conn.lock().await;
        let Some(existing) = load_list(&conn, list_id)? else {
            return Ok(None);
        };
        let now = now_rfc3339();
        conn.execute(
            "UPDATE lists SET name = ?2, description = ?3, icon = ?4, position = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                existing.id,
                req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&existing.name),
                req.description.as_ref().or(existing.description.as_ref()),
                req.icon.as_ref().or(existing.icon.as_ref()),
                req.position.unwrap_or(existing.position),
                now
            ],
        )?;
        load_list(&conn, &existing.id)
    }

    /// Delete a list, its entries and its list-specific fields. The RECORDS survive:
    /// a list is a set, and removing the set must not remove its members.
    pub async fn delete_list(&self, list_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().await;
        if load_list(&conn, list_id)?.is_none() {
            return Ok(false);
        }
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM list_entries WHERE list_id = ?1",
            params![list_id],
        )?;
        tx.execute("DELETE FROM fields WHERE list_id = ?1", params![list_id])?;
        let n = tx.execute("DELETE FROM lists WHERE id = ?1", params![list_id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    pub async fn list_list_fields(&self, list_id: &str) -> Result<Vec<Field>> {
        let conn = self.conn.lock().await;
        load_list_fields(&conn, list_id)
    }

    /// Add a record to a list, with its list-specific values.
    pub async fn add_list_entry(
        &self,
        list_id: &str,
        req: &AddListEntryRequest,
    ) -> Validated<ListEntry> {
        let conn = self.conn.lock().await;
        let Some(list) = load_list(&conn, list_id)? else {
            bail!("unknown list \"{list_id}\"");
        };
        let Some(record) = load_record(&conn, &req.record_id)? else {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "record_id",
                ValidationCode::BadRelationTarget,
                "no such record",
            )]));
        };
        if record.object_id != list.object_id {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "record_id",
                ValidationCode::BadRelationTarget,
                "that record is not on this list's object",
            )]));
        }
        let list_fields = load_list_fields(&conn, &list.id)?;
        let validated = validate_bag(
            &conn,
            &list.object_id,
            &list_fields,
            &req.values,
            false,
            None,
        )?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut values = validated.values;
        prune_nulls(&mut values);
        let now = now_rfc3339();
        let id = new_id(ID_LIST_ENTRY);
        let position = next_position(
            &conn,
            "SELECT MAX(position) FROM list_entries WHERE list_id = ?1",
            &list.id,
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO list_entries (id, list_id, record_id, data, position, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, list.id, record.id, encode_json(&values), position, now],
        )?;
        // `INSERT OR IGNORE` means re-adding an existing member is a no-op rather
        // than an error; return whichever row is now the membership.
        let sql = format!(
            "SELECT {COLS_LIST_ENTRY} FROM list_entries WHERE list_id = ?1 AND record_id = ?2"
        );
        let entry = conn
            .query_row(&sql, params![list.id, record.id], row_to_list_entry)
            .optional()?
            .context("re-reading the list entry just written")?;
        Ok(Ok(entry))
    }

    pub async fn update_list_entry(
        &self,
        entry_id: &str,
        req: &UpdateListEntryRequest,
    ) -> Validated<Option<ListEntry>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_LIST_ENTRY} FROM list_entries WHERE id = ?1");
        let Some(existing) = conn
            .query_row(&sql, params![entry_id], row_to_list_entry)
            .optional()?
        else {
            return Ok(Ok(None));
        };
        let Some(list) = load_list(&conn, &existing.list_id)? else {
            bail!("list entry {entry_id} points at a missing list");
        };
        let list_fields = load_list_fields(&conn, &list.id)?;
        let partial = req.mode == UpdateMode::Merge;
        let validated = validate_bag(
            &conn,
            &list.object_id,
            &list_fields,
            &req.values,
            partial,
            None,
        )?;
        if !validated.is_ok() {
            return Ok(Err(validated.errors));
        }
        let mut next = match req.mode {
            UpdateMode::Merge => {
                let mut merged = existing.values.clone();
                for (slug, value) in validated.values {
                    if value.is_null() {
                        merged.remove(&slug);
                    } else {
                        merged.insert(slug, value);
                    }
                }
                merged
            }
            UpdateMode::Replace => validated.values,
        };
        prune_nulls(&mut next);
        let now = now_rfc3339();
        conn.execute(
            "UPDATE list_entries SET data = ?2, updated_at = ?3 WHERE id = ?1",
            params![existing.id, encode_json(&next), now],
        )?;
        Ok(Ok(Some(ListEntry {
            values: next,
            updated_at: now,
            ..existing
        })))
    }

    pub async fn remove_list_entry(&self, entry_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM list_entries WHERE id = ?1", params![entry_id])?;
        Ok(n > 0)
    }

    pub async fn reorder_list_entries(&self, list_id: &str, ids: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        let now = now_rfc3339();
        for (position, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE list_entries SET position = ?2, updated_at = ?3 WHERE id = ?1 AND list_id = ?4",
                params![id, position as i64, now, list_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// One list's entries with their records resolved.
    ///
    /// Filters and sorts may name the record's own fields OR the list's extra
    /// fields; the two namespaces are kept apart by looking a key up in the list
    /// fields first, then in the object's.
    pub async fn query_list_entries(
        &self,
        query: &ListEntryQuery,
        limit: usize,
        offset: usize,
    ) -> Result<ListEntryPage> {
        let conn = self.conn.lock().await;
        let Some(list) = load_list(&conn, &query.list_id)? else {
            return Ok(Page::empty(limit, offset));
        };
        let record_fields = load_fields(&conn, &list.object_id)?;
        let list_fields = load_list_fields(&conn, &list.id)?;
        let record_index = field_index(&record_fields);
        let list_index = field_index(&list_fields);

        let mut params: SqlParams = vec![rusqlite::types::Value::Text(list.id.clone())];
        let mut clauses = vec![
            "e.list_id = ?".to_string(),
            "r.deleted_at IS NULL".to_string(),
        ];
        if let Some(expression) = query.search.as_deref().and_then(fts_match_expression) {
            clauses.push(
                "r.rowid IN (SELECT rowid FROM records_fts WHERE records_fts MATCH ?)".to_string(),
            );
            params.push(rusqlite::types::Value::Text(expression));
        }
        if let Some(filter) = query.filter.as_ref().filter(|f| !f.is_empty()) {
            // A key that names a list field binds to `e`; anything else to `r`.
            clauses.push(build_scoped_filter(
                filter,
                &list_index,
                &record_index,
                &mut params,
            ));
        }
        let where_sql = clauses.join(" AND ");

        let total: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM list_entries e JOIN records r ON r.id = e.record_id WHERE {where_sql}"
            ),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;

        let order_by = if query.sorts.is_empty() {
            "e.position ASC, e.id ASC".to_string()
        } else {
            build_scoped_order_by(&query.sorts, &list_index, &record_index)
        };
        let entry_cols = COLS_LIST_ENTRY
            .split(", ")
            .map(|c| format!("e.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let record_cols = COLS_RECORD
            .split(", ")
            .map(|c| format!("r.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {entry_cols}, {record_cols} FROM list_entries e
               JOIN records r ON r.id = e.record_id
              WHERE {where_sql} ORDER BY {order_by} LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), |row| {
            let entry = row_to_list_entry(row)?;
            // The record's columns start after the entry's seven.
            let record = Record {
                id: row.get(7)?,
                object_id: row.get(8)?,
                title: row.get(9)?,
                values: serde_json::from_str::<ValueBag>(&row.get::<_, String>(10)?)
                    .unwrap_or_default(),
                deleted_at: row.get(11)?,
                created_by: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
            };
            Ok(ListEntryView { entry, record })
        })?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }
}

/// Compile a filter whose keys may name EITHER a list field (bound to `e`) or a
/// record field (bound to `r`).
fn build_scoped_filter(
    filter: &ViewFilter,
    list_index: &HashMap<String, Field>,
    record_index: &HashMap<String, Field>,
    params: &mut SqlParams,
) -> String {
    match filter {
        ViewFilter::And { filters } | ViewFilter::Or { filters } => {
            let joiner = if matches!(filter, ViewFilter::And { .. }) {
                " AND "
            } else {
                " OR "
            };
            let parts: Vec<String> = filters
                .iter()
                .map(|f| build_scoped_filter(f, list_index, record_index, params))
                .filter(|p| p != "1")
                .collect();
            if parts.is_empty() {
                "1".to_string()
            } else {
                format!("({})", parts.join(joiner))
            }
        }
        ViewFilter::Not { filter } => {
            let inner = build_scoped_filter(filter, list_index, record_index, params);
            if inner == "1" {
                "1".to_string()
            } else {
                format!("NOT ({inner})")
            }
        }
        ViewFilter::Condition(condition) => if list_index.contains_key(&condition.field_id) {
            build_condition(condition, list_index, "e", params)
        } else {
            build_condition(condition, record_index, "r", params)
        }
        .unwrap_or_else(|| "1".to_string()),
    }
}

fn build_scoped_order_by(
    sorts: &[ViewSort],
    list_index: &HashMap<String, Field>,
    record_index: &HashMap<String, Field>,
) -> String {
    let mut parts = Vec::new();
    for sort in sorts.iter().take(MAX_SORTS) {
        let resolved = if list_index.contains_key(&sort.field_id) {
            value_expr(list_index, &sort.field_id, "e")
        } else {
            value_expr(record_index, &sort.field_id, "r")
        };
        let Some((expr, field)) = resolved else {
            continue;
        };
        let collate = if field.as_ref().is_none_or(|f| !f.field_type.is_numeric()) {
            " COLLATE NOCASE"
        } else {
            ""
        };
        parts.push(format!(
            "({expr} IS NULL) ASC, {expr}{collate} {}",
            sort.direction.as_sql()
        ));
    }
    parts.push("e.id ASC".to_string());
    parts.join(", ")
}

// ── Activities + tasks ─────────────────────────────────────────────────────────

impl CrmStore {
    pub async fn get_activity(&self, activity_id: &str) -> Result<Option<Activity>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![activity_id], row_to_activity)
            .optional()?)
    }

    pub async fn query_activities(
        &self,
        query: &ActivityQuery,
        limit: usize,
        offset: usize,
    ) -> Result<ActivityPage> {
        let conn = self.conn.lock().await;
        let mut clauses = vec!["1".to_string()];
        let mut params: SqlParams = Vec::new();
        if let Some(record_id) = query.record_id.as_deref().filter(|r| !r.is_empty()) {
            clauses.push("a.record_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(record_id.to_string()));
        }
        if let Some(object_ref) = query.object_id.as_deref().filter(|o| !o.is_empty()) {
            let Some(object) = load_object(&conn, object_ref)? else {
                return Ok(Page::empty(limit, offset));
            };
            clauses.push("a.object_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(object.id));
        }
        if !query.kinds.is_empty() {
            let placeholders = query
                .kinds
                .iter()
                .map(|k| {
                    params.push(rusqlite::types::Value::Text(k.as_str().to_string()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("a.kind IN ({placeholders})"));
        }
        if let Some(assignee) = query.assignee.as_deref().filter(|a| !a.is_empty()) {
            clauses.push("a.assignee = ?".to_string());
            params.push(rusqlite::types::Value::Text(assignee.to_string()));
        }
        if let Some(search) = query.search.as_deref().filter(|s| !s.trim().is_empty()) {
            clauses.push("(lower(a.title) LIKE '%' || lower(?) || '%' OR lower(COALESCE(a.body, '')) LIKE '%' || lower(?) || '%')".to_string());
            params.push(rusqlite::types::Value::Text(search.to_string()));
            params.push(rusqlite::types::Value::Text(search.to_string()));
        }
        // Fixed-width RFC-3339 makes these correct as TEXT range scans.
        if let Some(since) = query.since.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("a.created_at >= ?".to_string());
            params.push(rusqlite::types::Value::Text(since.to_string()));
        }
        if let Some(until) = query.until.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("a.created_at <= ?".to_string());
            params.push(rusqlite::types::Value::Text(until.to_string()));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM activities a WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let cols = COLS_ACTIVITY
            .split(", ")
            .map(|c| format!("a.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols} FROM activities a WHERE {where_sql}
              ORDER BY a.created_at DESC, a.id DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), row_to_activity)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }

    /// Create a user-authored timeline entry.
    ///
    /// Refuses the two automatic kinds: a hand-forged `field_change` is an audit
    /// trail that lies, which is worse than not having one.
    pub async fn create_activity(&self, req: &CreateActivityRequest) -> Validated<Activity> {
        if !req.kind.is_user_authored() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "kind",
                ValidationCode::Invalid,
                format!(
                    "\"{}\" entries are written automatically and cannot be created directly",
                    req.kind.as_str()
                ),
            )]));
        }
        let conn = self.conn.lock().await;
        let mut object_id: Option<String> = None;
        if let Some(record_id) = req.record_id.as_deref().filter(|r| !r.is_empty()) {
            let Some(record) = load_record(&conn, record_id)? else {
                return Ok(Err(vec![FieldValidationError::coded(
                    "",
                    "record_id",
                    ValidationCode::BadRelationTarget,
                    "no such record",
                )]));
            };
            object_id = Some(record.object_id);
        }
        let due_at = match req.due_at.as_deref().filter(|d| !d.trim().is_empty()) {
            Some(raw) => match normalize_datetime(raw) {
                Some(normalized) => Some(normalized),
                None => {
                    return Ok(Err(vec![FieldValidationError::coded(
                        "",
                        "due_at",
                        ValidationCode::Invalid,
                        "not a valid date and time",
                    )]))
                }
            },
            None => None,
        };
        let now = now_rfc3339();
        let id = new_id(ID_ACTIVITY);
        conn.execute(
            "INSERT INTO activities
               (id, record_id, object_id, kind, title, body, field_id, from_value, to_value,
                assignee, due_at, completed_at, due_notified_at, author, metadata, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, ?7, ?8, NULL, NULL, ?9, ?10, ?11, ?11)",
            params![
                id,
                req.record_id,
                object_id,
                req.kind.as_str(),
                req.title.trim(),
                req.body,
                req.assignee,
                due_at,
                req.author,
                req.metadata.as_ref().map(encode_json),
                now
            ],
        )?;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        let activity = conn.query_row(&sql, params![id], row_to_activity)?;
        Ok(Ok(activity))
    }

    pub async fn update_activity(
        &self,
        activity_id: &str,
        req: &UpdateActivityRequest,
    ) -> Validated<Option<Activity>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        let Some(existing) = conn
            .query_row(&sql, params![activity_id], row_to_activity)
            .optional()?
        else {
            return Ok(Ok(None));
        };
        let due_at = match req.due_at.as_deref() {
            Some(raw) if raw.trim().is_empty() => None,
            Some(raw) => match normalize_datetime(raw) {
                Some(normalized) => Some(normalized),
                None => {
                    return Ok(Err(vec![FieldValidationError::coded(
                        "",
                        "due_at",
                        ValidationCode::Invalid,
                        "not a valid date and time",
                    )]))
                }
            },
            None => existing.due_at.clone(),
        };
        let now = now_rfc3339();
        let completed_at = match req.completed {
            Some(true) => Some(existing.completed_at.clone().unwrap_or_else(|| now.clone())),
            Some(false) => None,
            None => existing.completed_at.clone(),
        };
        conn.execute(
            "UPDATE activities SET title = ?2, body = ?3, assignee = ?4, due_at = ?5,
                                   completed_at = ?6, metadata = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                existing.id,
                req.title
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or(&existing.title),
                req.body.as_ref().or(existing.body.as_ref()),
                req.assignee.as_ref().or(existing.assignee.as_ref()),
                due_at,
                completed_at,
                req.metadata
                    .as_ref()
                    .map(encode_json)
                    .or_else(|| existing.metadata.as_ref().map(encode_json)),
                now
            ],
        )?;
        Ok(Ok(conn
            .query_row(&sql, params![existing.id], row_to_activity)
            .optional()?))
    }

    pub async fn delete_activity(&self, activity_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM activities WHERE id = ?1", params![activity_id])?;
        Ok(n > 0)
    }

    /// Complete or reopen a task.
    pub async fn complete_task(
        &self,
        activity_id: &str,
        completed: bool,
    ) -> Result<Option<Activity>> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE activities SET completed_at = ?2, updated_at = ?3 WHERE id = ?1 AND kind = 'task'",
            params![activity_id, completed.then(|| now.clone()), now],
        )?;
        let sql = format!("SELECT {COLS_ACTIVITY} FROM activities WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![activity_id], row_to_activity)
            .optional()?)
    }

    pub async fn list_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        offset: usize,
    ) -> Result<ActivityPage> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let mut clauses = vec!["a.kind = 'task'".to_string()];
        let mut params: SqlParams = Vec::new();
        match query.filter {
            TaskFilter::Open => clauses.push("a.completed_at IS NULL".to_string()),
            TaskFilter::Completed => clauses.push("a.completed_at IS NOT NULL".to_string()),
            TaskFilter::Overdue => {
                clauses.push(
                    "a.completed_at IS NULL AND a.due_at IS NOT NULL AND a.due_at <= ?".to_string(),
                );
                params.push(rusqlite::types::Value::Text(now.clone()));
            }
            TaskFilter::All => {}
        }
        if let Some(assignee) = query.assignee.as_deref().filter(|a| !a.is_empty()) {
            clauses.push("a.assignee = ?".to_string());
            params.push(rusqlite::types::Value::Text(assignee.to_string()));
        }
        if let Some(record_id) = query.record_id.as_deref().filter(|r| !r.is_empty()) {
            clauses.push("a.record_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(record_id.to_string()));
        }
        if let Some(object_ref) = query.object_id.as_deref().filter(|o| !o.is_empty()) {
            let Some(object) = load_object(&conn, object_ref)? else {
                return Ok(Page::empty(limit, offset));
            };
            clauses.push("a.object_id = ?".to_string());
            params.push(rusqlite::types::Value::Text(object.id));
        }
        if let Some(before) = query.due_before.as_deref().filter(|d| !d.is_empty()) {
            clauses.push("a.due_at <= ?".to_string());
            params.push(rusqlite::types::Value::Text(before.to_string()));
        }
        if let Some(after) = query.due_after.as_deref().filter(|d| !d.is_empty()) {
            clauses.push("a.due_at >= ?".to_string());
            params.push(rusqlite::types::Value::Text(after.to_string()));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM activities a WHERE {where_sql}"),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        let cols = COLS_ACTIVITY
            .split(", ")
            .map(|c| format!("a.{c}"))
            .collect::<Vec<_>>()
            .join(", ");
        // Undated tasks last: a task with a due date is the one that needs doing.
        let sql = format!(
            "SELECT {cols} FROM activities a WHERE {where_sql}
              ORDER BY (a.due_at IS NULL) ASC, a.due_at ASC, a.created_at DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), row_to_activity)?;
        let items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Page::new(items, total, limit, offset))
    }

    /// Claim overdue tasks for the `task.due` sweep.
    ///
    /// The claim and the selection are ONE statement: `UPDATE … WHERE id IN (SELECT
    /// …) RETURNING …` stamps `due_notified_at` on exactly the rows it hands back, so
    /// a crash between "found it" and "announced it" cannot re-announce, and two
    /// sweeps cannot both claim the same task. A read-then-blind-update pair has no
    /// claim semantics at all — both callers would "win".
    pub async fn claim_due_tasks(&self, limit: usize) -> Result<Vec<Activity>> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let sql = format!(
            "UPDATE activities SET due_notified_at = ?1, updated_at = ?1
              WHERE id IN (
                SELECT id FROM activities
                 WHERE kind = 'task' AND completed_at IS NULL AND due_notified_at IS NULL
                   AND due_at IS NOT NULL AND due_at <= ?1
                 ORDER BY due_at ASC LIMIT ?2)
            RETURNING {COLS_ACTIVITY}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![now, limit as i64], row_to_activity)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

// ── CSV import ─────────────────────────────────────────────────────────────────

/// RFC-4180 CSV, hand-rolled.
///
/// A dependency would churn the shared `Cargo.lock` for every other job building
/// this tree, and the grammar that actually matters is small: quoted fields, `""`
/// as an escaped quote, embedded newlines inside quotes, and `\r\n` normalised to
/// `\n`. Rows are NOT padded or truncated to a common width here — a short row is a
/// real signal the mapper needs to see.
pub fn parse_csv(raw: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(ch);
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => in_quotes = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    // A trailing newline produces one empty row; drop rows that are entirely blank.
    rows.retain(|r| r.iter().any(|c| !c.trim().is_empty()));
    rows
}

/// Pick the delimiter by counting candidates on the first line. Comma-first, because
/// a tie on a single-column file should read as CSV.
pub fn sniff_delimiter(raw: &str) -> char {
    let first = raw.lines().next().unwrap_or_default();
    [',', ';', '\t', '|']
        .into_iter()
        .max_by_key(|d| first.matches(*d).count())
        .filter(|d| first.contains(*d))
        .unwrap_or(',')
}

/// A first row is a header when every cell is non-empty, non-numeric and distinct.
/// Getting this wrong in either direction is recoverable — the caller can override
/// `has_header` — but guessing well is what makes the common case one click.
fn looks_like_header(row: &[String]) -> bool {
    let mut seen = HashSet::new();
    row.iter().all(|cell| {
        let trimmed = cell.trim();
        !trimmed.is_empty()
            && trimmed.parse::<f64>().is_err()
            && seen.insert(trimmed.to_lowercase())
    })
}

impl CrmStore {
    /// Upload a CSV: parse it, infer the columns, suggest a mapping. Writes nothing
    /// to `records`.
    pub async fn create_import(
        &self,
        req: &CreateImportRequest,
        max_bytes: usize,
    ) -> Validated<ImportJob> {
        if req.csv.len() > max_bytes {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "csv",
                ValidationCode::OutOfRange,
                format!("the file is larger than the {max_bytes}-byte import limit"),
            )]));
        }
        let delimiter = req
            .delimiter
            .as_deref()
            .and_then(|d| d.chars().next())
            .unwrap_or_else(|| sniff_delimiter(&req.csv));
        let rows = parse_csv(&req.csv, delimiter);
        if rows.is_empty() {
            return Ok(Err(vec![FieldValidationError::coded(
                "",
                "csv",
                ValidationCode::Invalid,
                "that file has no rows",
            )]));
        }
        let has_header = req
            .has_header
            .unwrap_or_else(|| looks_like_header(&rows[0]));

        let conn = self.conn.lock().await;
        let Some(object) = load_object(&conn, &req.object_id)? else {
            bail!("unknown object \"{}\"", req.object_id);
        };
        let fields = load_fields(&conn, &object.id)?;

        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        let data_rows = if has_header { &rows[1..] } else { &rows[..] };
        let mut columns = Vec::with_capacity(width);
        for index in 0..width {
            let name = if has_header {
                rows[0]
                    .get(index)
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty())
            } else {
                None
            }
            .unwrap_or_else(|| format!("Column {}", index + 1));
            let samples: Vec<String> = data_rows
                .iter()
                .filter_map(|r| r.get(index))
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty())
                .take(ImportColumn::SAMPLE_ROWS)
                .collect();
            columns.push(ImportColumn {
                index,
                suggested_field_id: suggest_field(&name, &fields).map(|f| f.id),
                name,
                samples,
            });
        }
        // Pre-fill the mapping from the suggestions: the commonest import is a file
        // exported from another CRM whose headers already match.
        let mappings: Vec<ImportMapping> = columns
            .iter()
            .map(|c| ImportMapping {
                column_index: c.index,
                field_id: c.suggested_field_id.clone(),
            })
            .collect();

        let now = now_rfc3339();
        let id = new_id(ID_IMPORT);
        conn.execute(
            "INSERT INTO import_jobs
               (id, object_id, filename, status, delimiter, has_header, row_count, columns,
                mappings, dedupe, preview, result, error, raw_csv, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'draft', ?4, ?5, ?6, ?7, ?8, '{}', NULL, NULL, NULL, ?9, ?10, ?10)",
            params![
                id,
                object.id,
                req.filename,
                delimiter.to_string(),
                i64::from(has_header),
                data_rows.len() as i64,
                encode_json(&columns),
                encode_json(&mappings),
                req.csv,
                now
            ],
        )?;
        let sql = format!("SELECT {COLS_IMPORT} FROM import_jobs WHERE id = ?1");
        Ok(Ok(conn.query_row(&sql, params![id], row_to_import)?))
    }

    pub async fn get_import(&self, import_id: &str) -> Result<Option<ImportJob>> {
        let conn = self.conn.lock().await;
        let sql = format!("SELECT {COLS_IMPORT} FROM import_jobs WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![import_id], row_to_import)
            .optional()?)
    }

    pub async fn list_imports(
        &self,
        object_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ImportJob>> {
        let conn = self.conn.lock().await;
        let sql = format!(
            "SELECT {COLS_IMPORT} FROM import_jobs
              WHERE (?1 IS NULL OR object_id = ?1) ORDER BY created_at DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![object_id, limit as i64], row_to_import)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Save the column → field mapping and the dedupe rule. Clears any stale preview:
    /// a preview that describes a different mapping is worse than no preview.
    pub async fn set_import_mapping(
        &self,
        import_id: &str,
        req: &SetImportMappingRequest,
    ) -> Result<Option<ImportJob>> {
        let conn = self.conn.lock().await;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE import_jobs SET mappings = ?2, dedupe = ?3, preview = NULL, status = 'draft', updated_at = ?4
             WHERE id = ?1 AND status <> 'applied'",
            params![import_id, encode_json(&req.mappings), encode_json(&req.dedupe), now],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let sql = format!("SELECT {COLS_IMPORT} FROM import_jobs WHERE id = ?1");
        Ok(conn
            .query_row(&sql, params![import_id], row_to_import)
            .optional()?)
    }

    pub async fn delete_import(&self, import_id: &str) -> Result<bool> {
        let conn = self.conn.lock().await;
        let n = conn.execute("DELETE FROM import_jobs WHERE id = ?1", params![import_id])?;
        Ok(n > 0)
    }

    /// The dry run. Computes what every row WOULD do, writes nothing to `records`,
    /// and stores the report on the job.
    pub async fn dry_run_import(&self, import_id: &str) -> Result<Option<ImportPreview>> {
        let conn = self.conn.lock().await;
        let Some((job, raw, object, fields)) = load_import_context(&conn, import_id)? else {
            return Ok(None);
        };
        let (plans, conflicts) = plan_import(&conn, &job, &raw, &object, &fields)?;

        let mut preview = ImportPreview {
            total_rows: plans.len(),
            unmapped_columns: job
                .columns
                .iter()
                .filter(|c| {
                    job.mappings
                        .iter()
                        .find(|m| m.column_index == c.index)
                        .and_then(|m| m.field_id.as_deref())
                        .is_none()
                })
                .map(|c| c.name.clone())
                .collect(),
            ..Default::default()
        };
        for plan in &plans {
            match plan.action {
                ImportAction::Create => preview.create_count += 1,
                ImportAction::Update => preview.update_count += 1,
                ImportAction::Skip => preview.skip_count += 1,
                ImportAction::Error => preview.error_count += 1,
            }
        }
        // Errors first, then the head of the file: a preview that hides the failures
        // is worse than no preview.
        let mut samples: Vec<ImportRowPlan> = plans
            .iter()
            .filter(|p| p.action == ImportAction::Error)
            .take(ImportPreview::SAMPLE_LIMIT)
            .cloned()
            .collect();
        for plan in plans.iter().take(ImportPreview::SAMPLE_LIMIT) {
            if samples.len() >= ImportPreview::SAMPLE_LIMIT {
                break;
            }
            if !samples.iter().any(|s| s.row_index == plan.row_index) {
                samples.push(plan.clone());
            }
        }
        preview.truncated = plans.len() > samples.len();
        preview.samples = samples;
        preview.conflicts = conflicts
            .into_iter()
            .take(ImportPreview::CONFLICT_LIMIT)
            .collect();

        let now = now_rfc3339();
        conn.execute(
            "UPDATE import_jobs SET preview = ?2, status = 'previewed', updated_at = ?3 WHERE id = ?1",
            params![job.id, encode_json(&preview), now],
        )?;
        Ok(Some(preview))
    }

    /// Apply the mapping. ONE transaction over the whole file: a half-imported CSV is
    /// a reconciliation problem with no tooling, whereas a failed one is a retry.
    ///
    /// Returns the created/updated ids so the CALLER raises the `record.created` /
    /// `record.updated` events — the store never emits.
    pub async fn apply_import(&self, import_id: &str) -> Result<Option<ImportResult>> {
        let mut conn = self.conn.lock().await;
        let Some((job, raw, object, fields)) = load_import_context(&conn, import_id)? else {
            return Ok(None);
        };
        if job.status == ImportStatus::Applied {
            bail!("this import has already been applied");
        }
        let (plans, _) = plan_import(&conn, &job, &raw, &object, &fields)?;

        let tx = conn.transaction()?;
        let mut result = ImportResult::default();
        for plan in plans {
            match plan.action {
                ImportAction::Error => {
                    result.failed += 1;
                    if result.errors.len() < ImportResult::ERROR_LIMIT {
                        result.errors.push(ImportRowError {
                            row_index: plan.row_index,
                            errors: plan.errors,
                        });
                    }
                }
                ImportAction::Skip => result.skipped += 1,
                ImportAction::Create => {
                    let mut values = plan.values;
                    prune_nulls(&mut values);
                    let record = insert_record(
                        &tx,
                        &object,
                        &fields,
                        values,
                        Some(&format!("import:{}", job.id)),
                    )?;
                    result.created += 1;
                    result.created_record_ids.push(record.id);
                }
                ImportAction::Update => {
                    let Some(record_id) = plan.record_id else {
                        result.skipped += 1;
                        continue;
                    };
                    let Some(existing) = load_record(&tx, &record_id)? else {
                        result.skipped += 1;
                        continue;
                    };
                    let mut next = existing.values.clone();
                    for (slug, value) in plan.values {
                        if value.is_null() {
                            next.remove(&slug);
                        } else {
                            next.insert(slug, value);
                        }
                    }
                    prune_nulls(&mut next);
                    let update = write_record_values(&tx, &object, &fields, &existing, next)?;
                    if update.changed.is_empty() {
                        result.skipped += 1;
                    } else {
                        result.updated += 1;
                        result.updated_record_ids.push(record_id);
                    }
                }
            }
        }
        let now = now_rfc3339();
        tx.execute(
            "UPDATE import_jobs SET result = ?2, status = 'applied', updated_at = ?3 WHERE id = ?1",
            params![job.id, encode_json(&result), now],
        )?;
        tx.commit()?;
        Ok(Some(result))
    }
}

/// The job, its raw bytes, its object and its fields — everything both the dry run
/// and the apply need, loaded once so they cannot diverge.
#[allow(clippy::type_complexity)]
fn load_import_context(
    conn: &Connection,
    import_id: &str,
) -> Result<Option<(ImportJob, String, Object, Vec<Field>)>> {
    let sql = format!("SELECT {COLS_IMPORT}, raw_csv FROM import_jobs WHERE id = ?1");
    let row: Option<(ImportJob, String)> = conn
        .query_row(&sql, params![import_id], |row| {
            Ok((row_to_import(row)?, row.get::<_, String>(15)?))
        })
        .optional()?;
    let Some((job, raw)) = row else {
        return Ok(None);
    };
    let Some(object) = load_object(conn, &job.object_id)? else {
        return Ok(None);
    };
    let fields = load_fields(conn, &object.id)?;
    Ok(Some((job, raw, object, fields)))
}

/// The heart of the import: turn every data row into an [`ImportRowPlan`].
///
/// Shared verbatim by `dry_run_import` and `apply_import`, which is the whole point —
/// a preview computed by different code from the apply is a preview of nothing.
fn plan_import(
    conn: &Connection,
    job: &ImportJob,
    raw: &str,
    object: &Object,
    fields: &[Field],
) -> Result<(Vec<ImportRowPlan>, Vec<ImportConflict>)> {
    let index = field_index(fields);
    let delimiter = job.delimiter.chars().next().unwrap_or(',');
    let rows = parse_csv(raw, delimiter);
    let data_rows: &[Vec<String>] = if job.has_header && !rows.is_empty() {
        &rows[1..]
    } else {
        &rows[..]
    };

    // column index → field, resolved once.
    let mapping: Vec<(usize, Field)> = job
        .mappings
        .iter()
        .filter_map(|m| {
            let field_ref = m.field_id.as_deref()?;
            index.get(field_ref).map(|f| (m.column_index, f.clone()))
        })
        .collect();
    let match_fields: Vec<Field> = job
        .dedupe
        .match_field_ids
        .iter()
        .filter_map(|id| index.get(id.as_str()).cloned())
        .collect();

    let mut plans = Vec::with_capacity(data_rows.len());
    let mut conflicts = Vec::new();

    for (row_index, row) in data_rows.iter().enumerate() {
        let mut incoming = ValueBag::new();
        for (column_index, field) in &mapping {
            let Some(cell) = row.get(*column_index) else {
                continue;
            };
            if cell.trim().is_empty() {
                continue;
            }
            incoming.insert(field.slug.clone(), Value::String(cell.trim().to_string()));
        }

        // Find the existing record BEFORE validating, so uniqueness excludes it —
        // otherwise every `update` row would fail its own unique field.
        let matched = if match_fields.is_empty() {
            None
        } else {
            find_import_match(conn, &object.id, &match_fields, &incoming)?
        };

        let validated = validate_bag(
            conn,
            &object.id,
            fields,
            &incoming,
            matched.is_some(),
            matched.as_ref().map(|r| r.id.as_str()),
        )?;
        if !validated.is_ok() {
            plans.push(ImportRowPlan {
                row_index,
                action: ImportAction::Error,
                record_id: matched.map(|r| r.id),
                values: validated.values,
                errors: validated.errors,
            });
            continue;
        }
        let mut values = validated.values;

        let (action, record_id) = match (&matched, job.dedupe.strategy) {
            (None, _) | (Some(_), DedupeStrategy::CreateAlways) => (ImportAction::Create, None),
            (Some(existing), DedupeStrategy::Skip) => {
                (ImportAction::Skip, Some(existing.id.clone()))
            }
            (Some(existing), DedupeStrategy::Update) => {
                for (slug, incoming_value) in &values {
                    let current = existing.values.get(slug).cloned().unwrap_or(Value::Null);
                    if !is_empty_value(&current) && &current != incoming_value {
                        if let Some(field) = index.get(slug.as_str()) {
                            conflicts.push(ImportConflict {
                                row_index,
                                record_id: existing.id.clone(),
                                field_id: field.id.clone(),
                                field_slug: field.slug.clone(),
                                existing: current,
                                incoming: incoming_value.clone(),
                            });
                        }
                    }
                }
                (ImportAction::Update, Some(existing.id.clone()))
            }
            (Some(existing), DedupeStrategy::FillBlanks) => {
                values.retain(|slug, _| existing.values.get(slug).is_none_or(is_empty_value));
                if values.is_empty() {
                    (ImportAction::Skip, Some(existing.id.clone()))
                } else {
                    (ImportAction::Update, Some(existing.id.clone()))
                }
            }
        };

        // A create still has to satisfy the required fields, which the tolerant
        // `partial` pass above skipped for matched rows only.
        if action == ImportAction::Create {
            let missing: Vec<FieldValidationError> = fields
                .iter()
                .filter(|f| f.is_required)
                .filter(|f| values.get(&f.slug).is_none_or(is_empty_value))
                .map(|f| {
                    FieldValidationError::coded(
                        &f.id,
                        &f.slug,
                        ValidationCode::Required,
                        format!("{} is required", f.name),
                    )
                })
                .collect();
            if !missing.is_empty() {
                plans.push(ImportRowPlan {
                    row_index,
                    action: ImportAction::Error,
                    record_id: None,
                    values,
                    errors: missing,
                });
                continue;
            }
        }

        plans.push(ImportRowPlan {
            row_index,
            action,
            record_id,
            values,
            errors: Vec::new(),
        });
    }
    Ok((plans, conflicts))
}

/// Find the live record whose match fields ALL equal this row's, case- and
/// whitespace-insensitively. Rows missing any match value never match.
fn find_import_match(
    conn: &Connection,
    object_id: &str,
    match_fields: &[Field],
    incoming: &ValueBag,
) -> Result<Option<Record>> {
    let mut clauses = vec![
        "object_id = ?1".to_string(),
        "deleted_at IS NULL".to_string(),
    ];
    let mut params: SqlParams = vec![rusqlite::types::Value::Text(object_id.to_string())];
    for field in match_fields {
        let Some(text) = incoming.get(&field.slug).and_then(as_text) else {
            return Ok(None);
        };
        clauses.push(format!(
            "lower(trim(CAST(json_extract(data, '$.{}') AS TEXT))) = lower(trim(?))",
            field.slug
        ));
        params.push(rusqlite::types::Value::Text(text));
    }
    let sql = format!(
        "SELECT {COLS_RECORD} FROM records WHERE {} ORDER BY id ASC LIMIT 1",
        clauses.join(" AND ")
    );
    Ok(conn
        .query_row(&sql, params_from_iter(params), row_to_record)
        .optional()?)
}

/// Guess which field a CSV column belongs to: exact slug, then case-insensitive
/// name, then the slugified header.
fn suggest_field(header: &str, fields: &[Field]) -> Option<Field> {
    let trimmed = header.trim();
    let lowered = trimmed.to_lowercase();
    fields
        .iter()
        .find(|f| f.slug == lowered)
        .or_else(|| fields.iter().find(|f| f.name.eq_ignore_ascii_case(trimmed)))
        .or_else(|| {
            let slug = slugify(trimmed)?;
            fields.iter().find(|f| f.slug == slug)
        })
        .cloned()
}

// ── Search ─────────────────────────────────────────────────────────────────────

impl CrmStore {
    /// Full-text search across every object's records.
    pub async fn search(
        &self,
        query: &SearchQuery,
        limit: usize,
        offset: usize,
    ) -> Result<SearchResponse> {
        let conn = self.conn.lock().await;
        let Some(expression) = fts_match_expression(&query.query) else {
            return Ok(SearchResponse {
                query: query.query.clone(),
                hits: Vec::new(),
                total: 0,
                limit,
                offset,
            });
        };
        let mut clauses = vec![
            "records_fts MATCH ?".to_string(),
            "r.deleted_at IS NULL".to_string(),
        ];
        let mut params: SqlParams = vec![rusqlite::types::Value::Text(expression)];
        if !query.object_ids.is_empty() {
            let mut ids = Vec::new();
            for object_ref in &query.object_ids {
                if let Some(object) = load_object(&conn, object_ref)? {
                    ids.push(object.id);
                }
            }
            if ids.is_empty() {
                return Ok(SearchResponse {
                    query: query.query.clone(),
                    hits: Vec::new(),
                    total: 0,
                    limit,
                    offset,
                });
            }
            let placeholders = ids
                .iter()
                .map(|id| {
                    params.push(rusqlite::types::Value::Text(id.clone()));
                    "?"
                })
                .collect::<Vec<_>>()
                .join(", ");
            clauses.push(format!("r.object_id IN ({placeholders})"));
        }
        let where_sql = clauses.join(" AND ");
        let total: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM records_fts JOIN records r ON r.rowid = records_fts.rowid WHERE {where_sql}"
            ),
            params_from_iter(params.clone()),
            |r| r.get(0),
        )?;
        // bm25 is ASCENDING-better; the client must not re-sort on it.
        let sql = format!(
            "SELECT r.id, r.object_id, o.slug, r.title,
                    snippet(records_fts, 1, '<mark>', '</mark>', '…', 12), bm25(records_fts)
               FROM records_fts
               JOIN records r ON r.rowid = records_fts.rowid
               JOIN objects o ON o.id = r.object_id
              WHERE {where_sql}
              ORDER BY bm25(records_fts) ASC, r.updated_at DESC LIMIT ? OFFSET ?"
        );
        params.push(rusqlite::types::Value::Integer(limit as i64));
        params.push(rusqlite::types::Value::Integer(offset as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), |row| {
            Ok(SearchHit {
                record_id: row.get(0)?,
                object_id: row.get(1)?,
                object_slug: row.get(2)?,
                title: row.get(3)?,
                snippet: row.get(4)?,
                rank: row.get(5)?,
            })
        })?;
        let hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(SearchResponse {
            query: query.query.clone(),
            hits,
            total,
            limit,
            offset,
        })
    }

    /// Rebuild the whole FTS index. The repair hatch for a database restored from a
    /// backup taken mid-write, and the only way to pick up a change to what
    /// [`FieldType::is_searchable`] returns.
    pub async fn reindex_all(&self) -> Result<usize> {
        let mut conn = self.conn.lock().await;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM records_fts", [])?;
        let object_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM objects")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut total = 0;
        for object_id in object_ids {
            total += reindex_object(&tx, &object_id)?;
        }
        tx.commit()?;
        Ok(total)
    }
}

// ── Reports ────────────────────────────────────────────────────────────────────

impl CrmStore {
    /// Counts and summed value per stage of a status field.
    pub async fn pipeline_report(&self, req: &PipelineRequest) -> Result<Option<PipelineReport>> {
        let conn = self.conn.lock().await;
        let object_ref = req.object_id.as_deref().unwrap_or("deal");
        let Some(object) = load_object(&conn, object_ref)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let Some(stage_field) = pick_status_field(&fields, req.field_id.as_deref(), &index) else {
            return Ok(None);
        };
        let value_field = req
            .value_field_id
            .as_deref()
            .and_then(|id| index.get(id).cloned())
            .filter(|f| f.field_type == FieldType::Currency)
            .or_else(|| {
                fields
                    .iter()
                    .find(|f| f.field_type == FieldType::Currency)
                    .cloned()
            });

        let base = RecordQuery {
            object_id: object.id.clone(),
            filter: req.filter.clone(),
            ..Default::default()
        };
        let (base_where, base_params) = build_record_where(&base, &object.id, &index);

        let bucket = |option_id: Option<&str>| -> Result<(i64, i64)> {
            let mut params = base_params.clone();
            let clause = match option_id {
                Some(id) => {
                    params.push(rusqlite::types::Value::Text(id.to_string()));
                    format!("json_extract(r.data, '$.{}') = ?", stage_field.slug)
                }
                None => format!(
                    "(json_extract(r.data, '$.{slug}') IS NULL OR CAST(json_extract(r.data, '$.{slug}') AS TEXT) = '')",
                    slug = stage_field.slug
                ),
            };
            let where_sql = format!("{base_where} AND {clause}");
            let count: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM records r WHERE {where_sql}"),
                params_from_iter(params.clone()),
                |r| r.get(0),
            )?;
            let value: i64 = match value_field.as_ref() {
                Some(field) => conn
                    .query_row(
                        &format!(
                            "SELECT CAST(COALESCE(SUM(COALESCE(json_extract(r.data, '$.{}'), 0)), 0) AS INTEGER)
                               FROM records r WHERE {where_sql}",
                            field.slug
                        ),
                        params_from_iter(params),
                        |r| r.get(0),
                    )
                    .unwrap_or(0),
                None => 0,
            };
            Ok((count, value))
        };

        let options = stage_field.config.sorted_options();
        let mut stages = Vec::with_capacity(options.len());
        let mut total_records = 0i64;
        let mut total_value = 0i64;
        let (won_count, won_value, lost_count, lost_value) = {
            let mut w = (0i64, 0i64, 0i64, 0i64);
            for option in &options {
                if !req.include_closed && option.is_terminal() {
                    continue;
                }
                let (count, value) = bucket(Some(&option.id))?;
                total_records += count;
                total_value += value;
                if option.is_won {
                    w.0 += count;
                    w.1 += value;
                }
                if option.is_lost {
                    w.2 += count;
                    w.3 += value;
                }
                stages.push(PipelineStage {
                    option_id: option.id.clone(),
                    label: option.label.clone(),
                    color: option.color.clone(),
                    position: option.position,
                    is_won: option.is_won,
                    is_lost: option.is_lost,
                    record_count: count,
                    value_cents: value,
                    share: 0.0,
                });
            }
            w
        };
        // Counted, never dropped: a forecast that quietly excludes rows is wrong in
        // the direction nobody checks.
        let (unassigned_count, unassigned_value) = bucket(None)?;
        total_records += unassigned_count;
        total_value += unassigned_value;

        for stage in &mut stages {
            stage.share = if total_records > 0 {
                stage.record_count as f64 / total_records as f64
            } else {
                0.0
            };
        }
        let closed = won_count + lost_count;
        Ok(Some(PipelineReport {
            object_id: object.id,
            field_id: stage_field.id.clone(),
            currency_code: value_field
                .as_ref()
                .map(|f| f.config.currency().to_string())
                .unwrap_or_else(|| FieldConfig::DEFAULT_CURRENCY.to_string()),
            value_field_id: value_field.map(|f| f.id),
            total_records,
            total_value_cents: total_value,
            unassigned_count,
            stages,
            won_count,
            won_value_cents: won_value,
            lost_count,
            lost_value_cents: lost_value,
            win_rate: if closed > 0 {
                won_count as f64 / closed as f64
            } else {
                0.0
            },
        }))
    }

    /// Stage-to-stage conversion, reconstructed from the `stage_change` timeline.
    ///
    /// Computed IN MEMORY from one query rather than one aggregate per stage: the
    /// question "of the records that reached Proposal, how many went further" is a
    /// per-record path question, and expressing paths in SQL here would be a
    /// correlated subquery per stage per record.
    pub async fn funnel_report(&self, req: &FunnelRequest) -> Result<Option<FunnelReport>> {
        let conn = self.conn.lock().await;
        let object_ref = req.object_id.as_deref().unwrap_or("deal");
        let Some(object) = load_object(&conn, object_ref)? else {
            return Ok(None);
        };
        let fields = load_fields(&conn, &object.id)?;
        let index = field_index(&fields);
        let Some(stage_field) = pick_status_field(&fields, req.field_id.as_deref(), &index) else {
            return Ok(None);
        };
        let options = stage_field.config.sorted_options();
        let position_of: HashMap<&str, i64> = options
            .iter()
            .map(|o| (o.id.as_str(), o.position))
            .collect();

        // (record_id, option_id, entered_at) for every stage a record reached.
        let mut traces: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT record_id, from_value, to_value, created_at FROM activities
                  WHERE object_id = ?1 AND kind = 'stage_change' AND field_id = ?2
                    AND (?3 IS NULL OR created_at >= ?3) AND (?4 IS NULL OR created_at <= ?4)
                  ORDER BY record_id ASC, created_at ASC",
            )?;
            let rows = stmt.query_map(
                params![object.id, stage_field.id, req.since, req.until],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )?;
            for row in rows {
                let (record_id, from_raw, to_raw, at) = row?;
                let Some(record_id) = record_id else { continue };
                let entry = traces.entry(record_id).or_default();
                if entry.is_empty() {
                    // The stage the record was in before its first recorded move.
                    if let Some(from) = from_raw
                        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                        .and_then(|v| v.as_str().map(str::to_string))
                    {
                        entry.push((from, at.clone()));
                    }
                }
                if let Some(to) = to_raw
                    .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                    .and_then(|v| v.as_str().map(str::to_string))
                {
                    entry.push((to, at));
                }
            }
        }
        // Records that never moved still ENTERED their current stage — created
        // straight into it. Omitting them makes a young pipeline look empty.
        {
            let sql = format!(
                "SELECT id, CAST(json_extract(data, '$.{}') AS TEXT), created_at FROM records
                  WHERE object_id = ?1 AND deleted_at IS NULL
                    AND (?2 IS NULL OR created_at >= ?2) AND (?3 IS NULL OR created_at <= ?3)",
                stage_field.slug
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![object.id, req.since, req.until], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (record_id, stage, created_at) = row?;
                let Some(stage) = stage.filter(|s| !s.is_empty()) else {
                    continue;
                };
                traces
                    .entry(record_id)
                    .or_insert_with(|| vec![(stage, created_at)]);
            }
        }

        let mut steps = Vec::with_capacity(options.len());
        for option in &options {
            let mut entered = 0i64;
            let mut advanced = 0i64;
            let mut durations: Vec<i64> = Vec::new();
            for trace in traces.values() {
                let Some(at_index) = trace.iter().position(|(id, _)| id == &option.id) else {
                    continue;
                };
                entered += 1;
                let reached_later = trace.iter().any(|(id, _)| {
                    position_of.get(id.as_str()).copied().unwrap_or(-1) > option.position
                });
                if reached_later {
                    advanced += 1;
                }
                if let (Some((_, from)), Some((_, to))) =
                    (trace.get(at_index), trace.get(at_index + 1))
                {
                    if let (Ok(a), Ok(b)) = (
                        chrono::DateTime::parse_from_rfc3339(from),
                        chrono::DateTime::parse_from_rfc3339(to),
                    ) {
                        durations.push((b - a).num_hours().max(0));
                    }
                }
            }
            steps.push(FunnelStep {
                option_id: option.id.clone(),
                label: option.label.clone(),
                position: option.position,
                is_won: option.is_won,
                is_lost: option.is_lost,
                entered,
                advanced,
                conversion_rate: if entered > 0 {
                    advanced as f64 / entered as f64
                } else {
                    0.0
                },
                avg_hours_in_stage: (!durations.is_empty())
                    .then(|| durations.iter().sum::<i64>() / durations.len() as i64),
            });
        }

        Ok(Some(FunnelReport {
            object_id: object.id,
            field_id: stage_field.id.clone(),
            since: req.since.clone(),
            until: req.until.clone(),
            steps,
        }))
    }

    /// The dock panel's header strip.
    pub async fn summary(&self, recent_limit: usize) -> Result<CrmSummary> {
        let objects = {
            let conn = self.conn.lock().await;
            object_summaries(&conn)?
        };
        let (total_records, open_tasks, overdue_tasks, recent_activity) = {
            let conn = self.conn.lock().await;
            let now = now_rfc3339();
            let total_records: i64 = conn.query_row(
                "SELECT COUNT(*) FROM records WHERE deleted_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let open_tasks: i64 = conn.query_row(
                "SELECT COUNT(*) FROM activities WHERE kind = 'task' AND completed_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let overdue_tasks: i64 = conn.query_row(
                "SELECT COUNT(*) FROM activities
                  WHERE kind = 'task' AND completed_at IS NULL AND due_at IS NOT NULL AND due_at <= ?1",
                params![now],
                |r| r.get(0),
            )?;
            let sql = format!(
                "SELECT {COLS_ACTIVITY} FROM activities ORDER BY created_at DESC, id DESC LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![recent_limit as i64], row_to_activity)?;
            (
                total_records,
                open_tasks,
                overdue_tasks,
                rows.collect::<rusqlite::Result<Vec<_>>>()?,
            )
        };
        // Taken after the lock is released, because `pipeline_report` takes it again
        // and this mutex is not reentrant.
        let pipeline = self
            .pipeline_report(&PipelineRequest {
                include_closed: true,
                ..Default::default()
            })
            .await?;
        Ok(CrmSummary {
            objects,
            total_records,
            open_tasks,
            overdue_tasks,
            recent_activity,
            pipeline,
        })
    }
}

/// The status field a report runs over: the named one, else the object's first by
/// position. `None` when the object has no status field at all.
fn pick_status_field(
    fields: &[Field],
    requested: Option<&str>,
    index: &HashMap<String, Field>,
) -> Option<Field> {
    requested
        .and_then(|id| index.get(id).cloned())
        .filter(|f| f.field_type == FieldType::Status)
        .or_else(|| {
            fields
                .iter()
                .find(|f| f.field_type == FieldType::Status)
                .cloned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> CrmStore {
        CrmStore::open_in_memory().expect("in-memory store")
    }

    fn bag(pairs: &[(&str, Value)]) -> ValueBag {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    async fn make_record(store: &CrmStore, object: &str, values: &[(&str, Value)]) -> Record {
        store
            .create_record(
                object,
                &CreateRecordRequest {
                    values: bag(values),
                    created_by: None,
                },
            )
            .await
            .expect("no infrastructure error")
            .expect("values accepted")
    }

    #[tokio::test]
    async fn seeding_is_idempotent_and_lands_the_five_standard_objects() {
        let store = store().await;
        let objects = store.list_objects().await.unwrap();
        let slugs: Vec<&str> = objects.iter().map(|o| o.slug.as_str()).collect();
        assert_eq!(slugs, vec!["company", "person", "deal", "note", "task"]);
        assert!(objects.iter().all(|o| o.is_standard));
        assert!(objects.iter().all(|o| o.title_field_id.is_some()));

        // Every seeded object must have a default view, or opening it has no target.
        for object in &objects {
            let views = store.list_views(&object.id).await.unwrap();
            assert!(!views.is_empty(), "{} has no views", object.slug);
            assert_eq!(
                views.iter().filter(|v| v.is_default).count(),
                1,
                "{} must have exactly one default view",
                object.slug
            );
        }
        let deal_fields = store.list_fields(OBJ_DEAL).await.unwrap();
        assert!(deal_fields.iter().any(|f| f.id == FLD_DEAL_STAGE));
        assert_eq!(
            deal_fields
                .iter()
                .find(|f| f.id == FLD_DEAL_STAGE)
                .unwrap()
                .config
                .options
                .len(),
            6
        );
    }

    #[tokio::test]
    async fn currency_is_cents_when_integer_and_major_units_otherwise() {
        let store = store().await;
        let panel = make_record(
            &store,
            "deal",
            &[
                ("name", json!("A")),
                ("stage", json!("Lead")),
                ("amount", json!(12345)),
            ],
        )
        .await;
        assert_eq!(panel.values["amount"], json!(12345));
        let csv = make_record(
            &store,
            "deal",
            &[
                ("name", json!("B")),
                ("stage", json!("Lead")),
                ("amount", json!("$1,234.56")),
            ],
        )
        .await;
        assert_eq!(csv.values["amount"], json!(123456));
        let agent = make_record(
            &store,
            "deal",
            &[
                ("name", json!("C")),
                ("stage", json!("Lead")),
                ("amount", json!(99.5)),
            ],
        )
        .await;
        assert_eq!(agent.values["amount"], json!(9950));
    }

    #[tokio::test]
    async fn select_values_accept_labels_and_normalize_to_option_ids() {
        let store = store().await;
        let deal = make_record(
            &store,
            "deal",
            &[("name", json!("Acme")), ("stage", json!("Proposal"))],
        )
        .await;
        assert_eq!(deal.values["stage"], json!(OPT_DEAL_STAGE_PROPOSAL));
        assert_eq!(deal.title, "Acme");
    }

    #[tokio::test]
    async fn required_and_unknown_fields_are_reported_together_not_one_at_a_time() {
        let store = store().await;
        let errors = store
            .create_record(
                "deal",
                &CreateRecordRequest {
                    values: bag(&[("nonsense", json!("x"))]),
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .expect_err("must be rejected");
        let codes: HashSet<ValidationCode> = errors.iter().map(|e| e.code).collect();
        assert!(codes.contains(&ValidationCode::UnknownField));
        assert!(codes.contains(&ValidationCode::Required));
        // name AND stage are both required on `deal`; a one-at-a-time validator would
        // have reported one.
        assert!(
            errors
                .iter()
                .filter(|e| e.code == ValidationCode::Required)
                .count()
                >= 2
        );
    }

    #[tokio::test]
    async fn unique_fields_reject_a_second_record_but_tolerate_blanks_and_self() {
        let store = store().await;
        let first = make_record(
            &store,
            "person",
            &[("name", json!("Jane")), ("email", json!("jane@acme.com"))],
        )
        .await;
        let clash = store
            .create_record(
                "person",
                &CreateRecordRequest {
                    values: bag(&[("name", json!("J. Doe")), ("email", json!("JANE@acme.com"))]),
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .expect_err("duplicate email must be rejected");
        assert_eq!(clash[0].code, ValidationCode::NotUnique);

        // Two people with no email are not duplicates.
        make_record(&store, "person", &[("name", json!("Nobody One"))]).await;
        make_record(&store, "person", &[("name", json!("Nobody Two"))]).await;

        // Re-saving the same record with its own email must not collide with itself.
        let update = store
            .update_record(
                &first.id,
                &UpdateRecordRequest {
                    values: bag(&[
                        ("email", json!("jane@acme.com")),
                        ("job_title", json!("CTO")),
                    ]),
                    mode: UpdateMode::Merge,
                },
            )
            .await
            .unwrap()
            .expect("self-collision must not be reported");
        assert!(update
            .unwrap()
            .changed
            .iter()
            .any(|c| c.field_slug == "job_title"));
    }

    #[tokio::test]
    async fn relations_are_queryable_from_both_ends_with_the_right_label() {
        let store = store().await;
        let acme = make_record(&store, "company", &[("name", json!("Acme"))]).await;
        let jane = make_record(
            &store,
            "person",
            &[("name", json!("Jane")), ("company", json!(acme.id.clone()))],
        )
        .await;

        let outgoing = store.list_links(&jane.id).await.unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].direction, LinkDirection::Outgoing);
        assert_eq!(outgoing[0].record_id, acme.id);
        assert_eq!(outgoing[0].label, "Company");

        let incoming = store.list_links(&acme.id).await.unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].direction, LinkDirection::Incoming);
        assert_eq!(incoming[0].record_id, jane.id);
        // The inverse label, NOT the forward field's name — otherwise Acme's page
        // would read "Company: Jane".
        assert_eq!(incoming[0].label, "People");

        // A relation to a record that does not exist is rejected, not stored.
        let bad = store
            .create_record(
                "person",
                &CreateRecordRequest {
                    values: bag(&[("name", json!("Ghost")), ("company", json!("rec_nope"))]),
                    created_by: None,
                },
            )
            .await
            .unwrap()
            .expect_err("dangling relation must be rejected");
        assert_eq!(bad[0].code, ValidationCode::BadRelationTarget);
    }

    #[tokio::test]
    async fn updates_diff_status_fields_and_write_a_stage_change_activity() {
        let store = store().await;
        let deal = make_record(
            &store,
            "deal",
            &[("name", json!("Acme")), ("stage", json!("Lead"))],
        )
        .await;
        let update = store
            .update_record(
                &deal.id,
                &UpdateRecordRequest {
                    values: bag(&[("stage", json!("Won"))]),
                    mode: UpdateMode::Merge,
                },
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let stage = update.stage_change.expect("a status move must be surfaced");
        assert_eq!(stage.from.as_deref(), Some(OPT_DEAL_STAGE_LEAD));
        assert_eq!(stage.to.as_deref(), Some(OPT_DEAL_STAGE_WON));
        assert_eq!(stage.to_label.as_deref(), Some("Won"));

        let timeline = store
            .query_activities(
                &ActivityQuery {
                    record_id: Some(deal.id.clone()),
                    ..Default::default()
                },
                50,
                0,
            )
            .await
            .unwrap();
        assert!(timeline
            .items
            .iter()
            .any(|a| a.kind == ActivityKind::StageChange));
        assert!(timeline
            .items
            .iter()
            .any(|a| a.kind == ActivityKind::FieldChange));

        // A no-op PATCH must produce no diff, so the caller does not emit an event.
        let noop = store
            .update_record(
                &deal.id,
                &UpdateRecordRequest {
                    values: bag(&[("stage", json!("Won"))]),
                    mode: UpdateMode::Merge,
                },
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(noop.changed.is_empty());
    }

    #[tokio::test]
    async fn filters_and_sorts_compose_over_the_value_bag() {
        let store = store().await;
        for (name, stage, amount) in [
            ("Small", "Lead", 100_00),
            ("Medium", "Proposal", 5_000_00),
            ("Large", "Proposal", 50_000_00),
        ] {
            make_record(
                &store,
                "deal",
                &[
                    ("name", json!(name)),
                    ("stage", json!(stage)),
                    ("amount", json!(amount)),
                ],
            )
            .await;
        }
        let page = store
            .query_records(
                &RecordQuery {
                    object_id: "deal".to_string(),
                    filter: Some(ViewFilter::And {
                        filters: vec![
                            ViewFilter::Condition(FilterCondition {
                                field_id: FLD_DEAL_STAGE.to_string(),
                                op: FilterOperator::IsAnyOf,
                                value: json!([OPT_DEAL_STAGE_PROPOSAL]),
                            }),
                            ViewFilter::Condition(FilterCondition {
                                field_id: "amount".to_string(),
                                op: FilterOperator::Gte,
                                value: json!(1_000_00),
                            }),
                        ],
                    }),
                    sorts: vec![ViewSort::desc("amount")],
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items[0].title, "Large");
        assert!(!page.has_more);

        // `search` composes with the filter tree rather than replacing it. This is
        // the FTS subquery form, distinct from `CrmStore::search`'s join — both are
        // worth a test, because the aliased spelling of exactly this is what failed
        // at runtime while compiling clean.
        let searched = store
            .query_records(
                &RecordQuery {
                    object_id: "deal".to_string(),
                    search: Some("Large".to_string()),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(searched.total, 1);
        assert_eq!(searched.items[0].title, "Large");

        // A filter naming a deleted/unknown field degrades to "no constraint" rather
        // than erroring — a saved view must survive a schema edit.
        let all = store
            .query_records(
                &RecordQuery {
                    object_id: "deal".to_string(),
                    filter: Some(ViewFilter::Condition(FilterCondition {
                        field_id: "fld_gone".to_string(),
                        op: FilterOperator::Eq,
                        value: json!("x"),
                    })),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(all.total, 3);
    }

    #[tokio::test]
    async fn search_finds_records_and_survives_operator_characters() {
        let store = store().await;
        make_record(
            &store,
            "company",
            &[
                ("name", json!("Acme Industries")),
                ("description", json!("A widget manufacturer")),
            ],
        )
        .await;
        make_record(&store, "company", &[("name", json!("Globex"))]).await;

        let hits = store
            .search(
                &SearchQuery {
                    query: "widget".into(),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(hits.total, 1);
        assert_eq!(hits.hits[0].title, "Acme Industries");
        assert_eq!(hits.hits[0].object_slug, "company");

        // Prefix matching as you type.
        let prefix = store
            .search(
                &SearchQuery {
                    query: "acm".into(),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(prefix.total, 1);

        // Raw FTS5 syntax must not reach the matcher.
        let hostile = store
            .search(
                &SearchQuery {
                    query: "acme OR \"".into(),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert!(hostile.total <= 1);
    }

    #[tokio::test]
    async fn soft_delete_hides_a_record_from_search_and_queries_until_restored() {
        let store = store().await;
        let company = make_record(&store, "company", &[("name", json!("Initech"))]).await;
        assert!(store.delete_record(&company.id).await.unwrap());
        let visible = store
            .query_records(
                &RecordQuery {
                    object_id: "company".into(),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(visible.total, 0);
        assert_eq!(
            store
                .search(
                    &SearchQuery {
                        query: "Initech".into(),
                        ..Default::default()
                    },
                    10,
                    0
                )
                .await
                .unwrap()
                .total,
            0
        );
        assert!(store.restore_record(&company.id).await.unwrap());
        assert_eq!(
            store
                .query_records(
                    &RecordQuery {
                        object_id: "company".into(),
                        ..Default::default()
                    },
                    10,
                    0
                )
                .await
                .unwrap()
                .total,
            1
        );
    }

    #[tokio::test]
    async fn merge_fills_blanks_moves_history_and_never_silently_overwrites() {
        let store = store().await;
        let survivor = make_record(
            &store,
            "person",
            &[
                ("name", json!("Jane Doe")),
                ("email", json!("jane@acme.com")),
            ],
        )
        .await;
        let loser = make_record(
            &store,
            "person",
            &[("name", json!("J. Doe")), ("phone", json!("+1 555 0100"))],
        )
        .await;
        store
            .create_activity(&CreateActivityRequest {
                record_id: Some(loser.id.clone()),
                kind: ActivityKind::Note,
                title: "Met at a conference".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        let plan = MergePlan {
            survivor_id: survivor.id.clone(),
            loser_ids: vec![loser.id.clone()],
            resolutions: Vec::new(),
            soft_delete_losers: true,
        };
        let preview = store.plan_merge(&plan).await.unwrap().unwrap();
        assert_eq!(preview.activity_count, 1);
        // The names differ and the survivor's is non-empty ⇒ a conflict the user must
        // see, and the default keeps the survivor's.
        assert!(preview.conflicts.iter().any(|c| c.field_slug == "name"));
        assert_eq!(preview.resolved_values["name"], json!("Jane Doe"));
        // The blank phone is filled from the loser without being asked.
        assert_eq!(preview.resolved_values["phone"], json!("+1 555 0100"));

        let outcome = store.merge_records(&plan).await.unwrap().unwrap();
        assert_eq!(outcome.merged_record_ids, vec![loser.id.clone()]);
        assert_eq!(outcome.moved_activities, 1);
        assert_eq!(outcome.survivor.values["phone"], json!("+1 555 0100"));
        assert!(store
            .get_record(&loser.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
    }

    #[tokio::test]
    async fn duplicate_scan_finds_shared_emails_and_says_which_field_it_used() {
        let store = store().await;
        // Two records sharing an email can only exist if the unique flag is off, so
        // this exercises the scan on a non-unique field instead.
        make_record(
            &store,
            "company",
            &[("name", json!("Acme")), ("domain", json!("acme.com"))],
        )
        .await;
        make_record(
            &store,
            "company",
            &[("name", json!("ACME Inc")), ("domain", json!("ACME.com"))],
        )
        .await;
        let scan = store
            .merge_candidates(
                "company",
                &DuplicateScanRequest {
                    field_ids: vec![FLD_COMPANY_DOMAIN.to_string()],
                    limit: None,
                },
                20,
            )
            .await
            .unwrap();
        assert_eq!(scan.field_ids, vec![FLD_COMPANY_DOMAIN.to_string()]);
        assert_eq!(scan.candidates.len(), 1);
        assert_eq!(scan.candidates[0].record_ids.len(), 2);
        assert_eq!(scan.candidates[0].value, "acme.com");
    }

    #[tokio::test]
    async fn csv_import_previews_then_applies_the_same_plan() {
        let store = store().await;
        make_record(
            &store,
            "person",
            &[
                ("name", json!("Jane Doe")),
                ("email", json!("jane@acme.com")),
            ],
        )
        .await;

        let csv = "Name,Email,Job title\nJane Doe,jane@acme.com,CTO\nJohn Roe,john@acme.com,CEO\n\"Quoted, Name\",quoted@acme.com,\n";
        let job = store
            .create_import(
                &CreateImportRequest {
                    object_id: "person".into(),
                    filename: Some("people.csv".into()),
                    csv: csv.to_string(),
                    delimiter: None,
                    has_header: None,
                },
                MAX_TEST_IMPORT,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(job.has_header);
        assert_eq!(job.row_count, 3);
        assert_eq!(job.columns.len(), 3);
        // The header names match the seeded fields, so the mapping pre-fills.
        assert_eq!(
            job.columns[0].suggested_field_id.as_deref(),
            Some(FLD_PERSON_NAME)
        );
        assert_eq!(
            job.columns[2].suggested_field_id.as_deref(),
            Some(FLD_PERSON_JOB_TITLE)
        );

        store
            .set_import_mapping(
                &job.id,
                &SetImportMappingRequest {
                    mappings: job.mappings.clone(),
                    dedupe: ImportDedupe {
                        match_field_ids: vec![FLD_PERSON_EMAIL.to_string()],
                        strategy: DedupeStrategy::FillBlanks,
                    },
                },
            )
            .await
            .unwrap()
            .unwrap();

        let preview = store.dry_run_import(&job.id).await.unwrap().unwrap();
        assert_eq!(preview.total_rows, 3);
        assert_eq!(preview.create_count, 2);
        assert_eq!(
            preview.update_count, 1,
            "the existing Jane gets her blank job title filled"
        );
        assert_eq!(preview.error_count, 0);
        assert!(preview.unmapped_columns.is_empty());

        let result = store.apply_import(&job.id).await.unwrap().unwrap();
        assert_eq!(result.created, 2);
        assert_eq!(result.updated, 1);
        assert_eq!(result.created_record_ids.len(), 2);
        assert_eq!(
            store
                .query_records(
                    &RecordQuery {
                        object_id: "person".into(),
                        ..Default::default()
                    },
                    50,
                    0
                )
                .await
                .unwrap()
                .total,
            3
        );
        // The quoted cell survived the parser intact.
        let hits = store
            .search(
                &SearchQuery {
                    query: "Quoted".into(),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(hits.total, 1);
        // Re-applying is refused rather than silently duplicating.
        assert!(store.apply_import(&job.id).await.is_err());
    }

    const MAX_TEST_IMPORT: usize = 1024 * 1024;

    #[tokio::test]
    async fn import_rows_that_fail_validation_do_not_block_the_rest() {
        let store = store().await;
        let csv = "Name,Email\nGood One,good@acme.com\nBad One,not-an-email\n";
        let job = store
            .create_import(
                &CreateImportRequest {
                    object_id: "person".into(),
                    csv: csv.to_string(),
                    ..Default::default()
                },
                MAX_TEST_IMPORT,
            )
            .await
            .unwrap()
            .unwrap();
        let preview = store.dry_run_import(&job.id).await.unwrap().unwrap();
        assert_eq!(preview.create_count, 1);
        assert_eq!(preview.error_count, 1);
        // The failing row must be IN the samples — a preview that hides failures is
        // worse than none.
        assert!(preview
            .samples
            .iter()
            .any(|s| s.action == ImportAction::Error));
        let result = store.apply_import(&job.id).await.unwrap().unwrap();
        assert_eq!(result.created, 1);
        assert_eq!(result.failed, 1);
    }

    #[tokio::test]
    async fn lists_carry_their_own_fields_separate_from_the_records() {
        let store = store().await;
        let deal = make_record(
            &store,
            "deal",
            &[("name", json!("Acme")), ("stage", json!("Lead"))],
        )
        .await;
        let list = store
            .create_list(&CreateListRequest {
                object_id: "deal".into(),
                name: "Q3 push".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        // A list field named `stage` must NOT collide with the deal's own `stage`.
        let list_field = store
            .create_field(
                "deal",
                Some(&list.id),
                &CreateFieldRequest {
                    slug: "stage".into(),
                    name: "List stage".into(),
                    field_type: FieldType::Select,
                    config: FieldConfig {
                        options: vec![
                            SelectOption::new("", "Shortlisted", 0),
                            SelectOption::new("", "Dropped", 1),
                        ],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(list_field.list_id.as_deref(), Some(list.id.as_str()));
        assert!(list_field.config.options[0].id.starts_with("opt_stage_"));

        // A unique LIST field would enforce nothing (uniqueness is checked against
        // `records`, not `list_entries`), so it is rejected rather than accepted and
        // quietly inert.
        assert!(store
            .create_field(
                "deal",
                Some(&list.id),
                &CreateFieldRequest {
                    slug: "ref_code".into(),
                    name: "Ref".into(),
                    field_type: FieldType::Text,
                    is_unique: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .is_err());

        let entry = store
            .add_list_entry(
                &list.id,
                &AddListEntryRequest {
                    record_id: deal.id.clone(),
                    values: bag(&[("stage", json!("Shortlisted"))]),
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry.values["stage"],
            json!(list_field.config.options[0].id)
        );

        let page = store
            .query_list_entries(
                &ListEntryQuery {
                    list_id: list.id.clone(),
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].record.title, "Acme");
        // The record's OWN stage is untouched by the list's.
        assert_eq!(
            page.items[0].record.values["stage"],
            json!(OPT_DEAL_STAGE_LEAD)
        );

        let detail = store.get_record_detail(&deal.id).await.unwrap().unwrap();
        assert_eq!(detail.lists.len(), 1);
        assert_eq!(detail.lists[0].list_name, "Q3 push");
    }

    #[tokio::test]
    async fn board_views_group_by_status_and_always_keep_a_no_value_column() {
        let store = store().await;
        make_record(
            &store,
            "deal",
            &[
                ("name", json!("A")),
                ("stage", json!("Lead")),
                ("amount", json!(1000)),
            ],
        )
        .await;
        make_record(
            &store,
            "deal",
            &[
                ("name", json!("B")),
                ("stage", json!("Won")),
                ("amount", json!(2000)),
            ],
        )
        .await;
        let result = store
            .run_view(VIEW_DEAL_PIPELINE, &ViewQueryOverrides::default(), 20, 0)
            .await
            .unwrap()
            .unwrap();
        let groups = result.groups.expect("a board must produce groups");
        assert_eq!(groups.len(), 7, "six stages plus the no-value column");
        assert!(groups.last().unwrap().option_id.is_none());
        let won = groups
            .iter()
            .find(|g| g.option_id.as_deref() == Some(OPT_DEAL_STAGE_WON))
            .unwrap();
        assert_eq!(won.total, 1);
        assert_eq!(won.value_cents, Some(2000));
        assert_eq!(result.page.total, 2);
    }

    #[tokio::test]
    async fn pipeline_report_counts_unassigned_rather_than_dropping_it() {
        let store = store().await;
        // `stage` is required on `deal`, so an unassigned row can only be made on an
        // object whose status field is optional — `task` is exactly that.
        make_record(
            &store,
            "task",
            &[("name", json!("A")), ("status", json!("Done"))],
        )
        .await;
        make_record(
            &store,
            "task",
            &[("name", json!("B")), ("status", json!("Cancelled"))],
        )
        .await;
        make_record(&store, "task", &[("name", json!("C"))]).await;
        let report = store
            .pipeline_report(&PipelineRequest {
                object_id: Some("task".into()),
                include_closed: true,
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.total_records, 3);
        assert_eq!(report.unassigned_count, 1);
        assert_eq!(report.won_count, 1);
        assert_eq!(report.lost_count, 1);
        assert!((report.win_rate - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn funnel_report_credits_records_created_straight_into_a_stage() {
        let store = store().await;
        let deal = make_record(
            &store,
            "deal",
            &[("name", json!("A")), ("stage", json!("Lead"))],
        )
        .await;
        store
            .update_record(
                &deal.id,
                &UpdateRecordRequest {
                    values: bag(&[("stage", json!("Qualified"))]),
                    mode: UpdateMode::Merge,
                },
            )
            .await
            .unwrap()
            .unwrap();
        make_record(
            &store,
            "deal",
            &[("name", json!("B")), ("stage", json!("Lead"))],
        )
        .await;

        let report = store
            .funnel_report(&FunnelRequest::default())
            .await
            .unwrap()
            .unwrap();
        let lead = report
            .steps
            .iter()
            .find(|s| s.option_id == OPT_DEAL_STAGE_LEAD)
            .unwrap();
        assert_eq!(lead.entered, 2, "both deals were in Lead");
        assert_eq!(lead.advanced, 1);
        assert!((lead.conversion_rate - 0.5).abs() < f64::EPSILON);
        let qualified = report
            .steps
            .iter()
            .find(|s| s.option_id == OPT_DEAL_STAGE_QUALIFIED)
            .unwrap();
        assert_eq!(qualified.entered, 1);
        assert_eq!(qualified.advanced, 0);
    }

    #[tokio::test]
    async fn due_tasks_are_claimed_exactly_once() {
        let store = store().await;
        let record = make_record(
            &store,
            "deal",
            &[("name", json!("Acme")), ("stage", json!("Lead"))],
        )
        .await;
        store
            .create_activity(&CreateActivityRequest {
                record_id: Some(record.id.clone()),
                kind: ActivityKind::Task,
                title: "Call them back".into(),
                due_at: Some("2020-01-01T00:00:00Z".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();
        store
            .create_activity(&CreateActivityRequest {
                record_id: Some(record.id.clone()),
                kind: ActivityKind::Task,
                title: "Not due for ages".into(),
                due_at: Some("2999-01-01T00:00:00Z".into()),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        let first = store.claim_due_tasks(10).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].title, "Call them back");
        assert!(first[0].due_notified_at.is_some());
        // The claim IS the stamp: a second sweep must find nothing.
        assert!(store.claim_due_tasks(10).await.unwrap().is_empty());

        let overdue = store
            .list_tasks(
                &TaskQuery {
                    filter: TaskFilter::Overdue,
                    ..Default::default()
                },
                10,
                0,
            )
            .await
            .unwrap();
        assert_eq!(overdue.total, 1);
    }

    #[tokio::test]
    async fn automatic_activity_kinds_cannot_be_forged() {
        let store = store().await;
        let errors = store
            .create_activity(&CreateActivityRequest {
                kind: ActivityKind::FieldChange,
                title: "I changed nothing".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .expect_err("field_change must not be user-creatable");
        assert_eq!(errors[0].code, ValidationCode::Invalid);
    }

    #[tokio::test]
    async fn custom_objects_arrive_usable_and_delete_cleanly() {
        let store = store().await;
        let object = store
            .create_object(&CreateObjectRequest {
                slug: "product".into(),
                singular: "Product".into(),
                plural: None,
                icon: None,
                description: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(object.plural, "Products");
        assert!(!object.is_standard);
        // A new object must be immediately writable and openable.
        let fields = store.list_fields(&object.id).await.unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].slug, "name");
        assert_eq!(
            object.title_field_id.as_deref(),
            Some(fields[0].id.as_str())
        );
        assert_eq!(store.list_views(&object.id).await.unwrap().len(), 1);

        let record = make_record(&store, "product", &[("name", json!("Widget"))]).await;
        assert_eq!(record.title, "Widget");
        assert!(store.delete_object(&object.id).await.unwrap());
        assert!(store.get_record(&record.id).await.unwrap().is_none());
        // Its FTS rows go with it.
        assert_eq!(
            store
                .search(
                    &SearchQuery {
                        query: "Widget".into(),
                        ..Default::default()
                    },
                    10,
                    0
                )
                .await
                .unwrap()
                .total,
            0
        );
    }

    #[tokio::test]
    async fn standard_objects_and_system_fields_are_undeletable() {
        let store = store().await;
        assert!(store.delete_object(OBJ_DEAL).await.is_err());
        assert!(store.delete_field(FLD_DEAL_NAME).await.is_err());
    }

    #[tokio::test]
    async fn deleting_a_field_strips_it_from_every_record_and_from_search() {
        let store = store().await;
        let field = store
            .create_field(
                "company",
                None,
                &CreateFieldRequest {
                    slug: "internal_code".into(),
                    name: "Internal code".into(),
                    field_type: FieldType::Text,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        let record = make_record(
            &store,
            "company",
            &[("name", json!("Acme")), ("internal_code", json!("ZX9000"))],
        )
        .await;
        assert_eq!(
            store
                .search(
                    &SearchQuery {
                        query: "ZX9000".into(),
                        ..Default::default()
                    },
                    10,
                    0
                )
                .await
                .unwrap()
                .total,
            1
        );
        assert!(store.delete_field(&field.id).await.unwrap());
        let after = store.get_record(&record.id).await.unwrap().unwrap();
        assert!(!after.values.contains_key("internal_code"));
        assert_eq!(
            store
                .search(
                    &SearchQuery {
                        query: "ZX9000".into(),
                        ..Default::default()
                    },
                    10,
                    0
                )
                .await
                .unwrap()
                .total,
            0
        );
    }

    #[tokio::test]
    async fn a_slug_cannot_smuggle_a_json_path_or_take_a_reserved_name() {
        let store = store().await;
        // NOTE: a mixed-case slug is LOWERCASED, not rejected — `create_field`
        // normalizes before validating, so "Stage" is accepted as "stage".
        for slug in ["created_at", "title", "a\"b", "a.b", "a b", "1a", ""] {
            let outcome = store
                .create_field(
                    "company",
                    None,
                    &CreateFieldRequest {
                        slug: slug.into(),
                        name: "X".into(),
                        field_type: FieldType::Text,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(outcome.is_err(), "slug {slug:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn schema_boot_call_returns_everything_the_panel_needs() {
        let store = store().await;
        let schema = store.schema().await.unwrap();
        assert_eq!(schema.objects.len(), 5);
        let deal = schema
            .objects
            .iter()
            .find(|o| o.object.slug == "deal")
            .unwrap();
        assert!(deal.fields.len() >= 10);
        assert_eq!(deal.views.len(), 2);
        assert_eq!(deal.record_count, 0);
    }

    #[tokio::test]
    async fn a_replace_update_clears_the_fields_it_omits() {
        let store = store().await;
        let deal = make_record(
            &store,
            "deal",
            &[
                ("name", json!("Acme")),
                ("stage", json!("Lead")),
                ("probability", json!(40)),
            ],
        )
        .await;
        let update = store
            .update_record(
                &deal.id,
                &UpdateRecordRequest {
                    values: bag(&[("name", json!("Acme")), ("stage", json!("Lead"))]),
                    mode: UpdateMode::Replace,
                },
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!update.record.values.contains_key("probability"));
        // The same payload as a MERGE would have left it alone.
        assert!(update.changed.iter().any(|c| c.field_slug == "probability"));
    }
}
