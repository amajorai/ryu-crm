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

mod objects {
    use super::*;
    include!("store/objects.rs");
}
mod fields {
    use super::*;
    include!("store/fields.rs");
}
mod records {
    use super::*;
    include!("store/records.rs");
}
mod links {
    use super::*;
    include!("store/links.rs");
}
mod merge {
    use super::*;
    include!("store/merge.rs");
}
mod views {
    use super::*;
    include!("store/views.rs");
}
mod lists {
    use super::*;
    include!("store/lists.rs");
}
mod activities {
    use super::*;
    include!("store/activities.rs");
}
mod imports {
    use super::*;
    include!("store/imports.rs");
}
mod search {
    use super::*;
    include!("store/search.rs");
}
mod reports {
    use super::*;
    include!("store/reports.rs");
}

mod schema {
    use super::*;
    include!("store/schema.rs");
}
mod seeds {
    use super::*;
    include!("store/seeds.rs");
}
mod codecs {
    use super::*;
    include!("store/codecs.rs");
}
mod validation {
    use super::*;
    include!("store/validation.rs");
}

use codecs::*;
use schema::*;
use seeds::*;
use validation::*;

pub(crate) use activities::parse_csv;
use activities::*;
use fields::*;
use imports::*;
use links::*;
use lists::*;
use merge::*;
use objects::*;
use records::*;
use reports::*;
use search::*;
use views::*;

/// Outer `Result` = infrastructure failure. Inner = the caller's values were
/// rejected. See the module docs.
pub type Validated<T> = Result<std::result::Result<T, Vec<FieldValidationError>>>;

/// The schema version this build expects. Bump it and add a `v<N>` arm in
/// [`CrmStore::migrate`] when the shape changes.
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

// ── Objects ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
