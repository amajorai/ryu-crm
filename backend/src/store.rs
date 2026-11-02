//! SQLite persistence for the whole Harbor spine (`<data dir>/plugin-data/@ryu+crm/crm.db`).
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
//!
//! ## At rest: sealed values, decrypted inside SQLite
//!
//! Record titles and value bags, activity titles, bodies, change values and
//! metadata, list-entry value bags and import jobs' file name, columns, preview,
//! result, error and uploaded CSV are sealed per value (`encb:v1:`, store
//! `crm.db`, AAD `(<table>.<column>, id)`) under the `@ryu/crm` plugin subkey (see
//! [`crate::seal_keys`]). Every connection registers two SQL functions bound to
//! this process's key: `crm_open(purpose, id, stored)` opens a value (NULL when it
//! does not open; counted) and `crm_seal(purpose, id, plaintext)` seals one. Reads
//! select `crm_open(...)` and writes bind `crm_seal(...)`, so every filter, sort,
//! `json_extract`/`json_each` predicate, `SUM` aggregate, `GROUP BY` duplicate
//! scan, import match and uniqueness check runs unchanged over the decrypted value:
//! the same operators, ordering and aggregate results by construction. A
//! per-connection plaintext cache keyed by `(purpose, id)` and validated against
//! the stored ciphertext makes repeated scans cost a string compare, not a
//! decrypt; any write changes the ciphertext and so invalidates the entry. List
//! queries skip rows that do not open. The full-text index is `records_terms`, an
//! FTS5 `detail=none` table of blind term tokens (`crm_terms`), so search matches
//! whole words only.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::seal_keys::{sqlite_store_seal, KeySource, PluginKeys, StoreSeal};

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
mod crypto {
    use super::*;
    include!("store/crypto.rs");
}
mod validation {
    use super::*;
    include!("store/validation.rs");
}

use codecs::*;
pub(crate) use crypto::CrmCrypto;
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
    crypto: Arc<CrmCrypto>,
}

/// This app's canonical plugin id (the sealing key's scope).
pub const PLUGIN_ID: &str = "@ryu/crm";

impl CrmStore {
    /// Open (creating if needed) the DB at `path`, migrate it, and seed the standard
    /// schema. The path is injected by the caller (`paths::crm_db_path()`) so this
    /// module has no opinion about where the node's data lives. Resolves this
    /// process's sealing key with the database's directory as the plugin data dir.
    pub fn open(path: PathBuf) -> Result<Self> {
        let dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let keys = PluginKeys::resolve(PLUGIN_ID, &dir)?;
        Self::open_with_keys(path, &keys)
    }

    /// [`Self::open`] with explicit keys.
    pub fn open_with_keys(path: PathBuf, keys: &PluginKeys) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating parent dir for crm.db")?;
        }
        let mut conn = Connection::open(&path)
            .with_context(|| format!("opening crm db at {}", path.display()))?;
        let crypto = Self::prepare(&mut conn, keys, Some(&path))?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            crypto,
        })
    }

    /// In-memory store. A plain `pub fn`, not `#[cfg(test)]`, so the later agents'
    /// module tests can build a real seeded store without a temp file. Seals under a
    /// random process-local key, so every module test exercises the sealed paths.
    pub fn open_in_memory() -> Result<Self> {
        use base64::Engine as _;
        use rand::RngCore as _;
        let mut root = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut root);
        let cipher = ryu_crypto::PluginCipher::from_subkey_b64(
            PLUGIN_ID,
            &base64::engine::general_purpose::STANDARD.encode(root),
        )?;
        Self::open_in_memory_with_keys(&PluginKeys::sealing(cipher, KeySource::Standalone))
    }

    /// [`Self::open_in_memory`] with explicit keys.
    pub fn open_in_memory_with_keys(keys: &PluginKeys) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        let crypto = Self::prepare(&mut conn, keys, None)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            crypto,
        })
    }

    /// The sealing health item: `{state, detail?, unreadable_values}`.
    pub fn seal_health(&self) -> Value {
        self.crypto.health()
    }

    /// Pragmas, the sealing functions, then migrations, then seeds, then the
    /// content migration. Both open paths call this so an in-memory store is
    /// byte-for-byte the same schema as a real one — a divergence here would make
    /// every module test a lie.
    fn prepare(
        conn: &mut Connection,
        keys: &PluginKeys,
        path: Option<&std::path::Path>,
    ) -> Result<Arc<CrmCrypto>> {
        // Queries sort, group and scan values `crm_open` returns in plaintext; with
        // the default file temp store a large sort spills those values to an
        // unlinked `etilqs_*` file outside the data dir. Keep temp storage in memory.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA temp_store = MEMORY;",
        )
        .context("applying crm db pragmas")?;
        ryu_crypto::content_sqlite::enable_secure_delete(conn)?;
        let seal = sqlite_store_seal(keys, conn, crypto::STORE)?;
        let crypto = Arc::new(CrmCrypto::new(seal, keys));
        crypto::register_functions(conn, &crypto)?;
        Self::migrate(conn)?;
        seed_standard_schema(conn).context("seeding the standard CRM schema")?;
        let sealed = crypto::migrate_content(conn, &crypto, path)?;
        let rebuilt = crypto::ensure_terms_index(conn, &crypto)?;
        if crypto.seal_state().is_sealing() && (sealed > 0 || rebuilt) {
            crypto::finish(conn, path);
        }
        Ok(crypto)
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let current: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if current < 1 {
            conn.execute_batch(V1_DDL)
                .context("applying crm schema v1")?;
        }
        if current < 2 {
            conn.execute_batch(V2_DDL)
                .context("applying crm schema v2 (blind term index)")?;
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
