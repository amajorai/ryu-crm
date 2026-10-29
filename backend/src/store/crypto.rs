// ── At-rest sealing: the SQL functions every query goes through ───────────────
//
// See the module docs ("At rest"). `crm_open` / `crm_seal` / `crm_terms` /
// `crm_terms_query` are registered on the store's connection before the schema is
// migrated, so seeds, migrations and every later statement can use them.

use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::functions::FunctionFlags;
use ryu_crypto::content_sqlite::{
    finish_store, migrate_columns, run_pending_vacuum, ColumnPlan, KeyKind,
};
use ryu_crypto::{BlindIndex, ContentCipher, ContentContext};

/// The registered content-store name (part of every key derivation; never rename).
pub(crate) const STORE: &str = "crm.db";
/// Plaintext values kept in the per-connection cache before it is cleared.
const CACHE_MAX_ENTRIES: usize = 200_000;
/// `crm_meta` key recording which key built `records_terms`.
const TERMS_INDEX_KEY: &str = "terms_index";

/// This process's sealing state for `crm.db`, shared by the SQL functions.
pub(crate) struct CrmCrypto {
    seal: StoreSeal,
    terms: Option<BlindIndex>,
    cache: std::sync::Mutex<HashMap<(String, String), (String, String)>>,
    unreadable: AtomicU64,
    /// `crm_open` calls (tests: a record query opens each row once).
    #[cfg(test)]
    pub(crate) open_calls: AtomicU64,
}

impl CrmCrypto {
    pub(crate) fn new(seal: StoreSeal, keys: &PluginKeys) -> Self {
        let terms = if seal.is_sealing() {
            keys.blind_index(STORE, "terms")
        } else {
            None
        };
        Self {
            seal,
            terms,
            cache: std::sync::Mutex::new(HashMap::new()),
            unreadable: AtomicU64::new(0),
            #[cfg(test)]
            open_calls: AtomicU64::new(0),
        }
    }

    pub(crate) fn health(&self) -> Value {
        let mut health = self.seal.health();
        health["unreadable_values"] = self.unreadable.load(Ordering::Relaxed).into();
        health
    }

    pub(crate) fn seal_state(&self) -> &StoreSeal {
        &self.seal
    }

    /// Open one stored value; `None` (counted, logged) when it does not open.
    fn open(&self, purpose: &str, id: &str, stored: &str) -> Option<String> {
        #[cfg(test)]
        self.open_calls.fetch_add(1, Ordering::Relaxed);
        let key = (purpose.to_owned(), id.to_owned());
        if let Ok(cache) = self.cache.lock() {
            if let Some((cached_stored, plain)) = cache.get(&key) {
                if cached_stored == stored {
                    return Some(plain.clone());
                }
            }
        }
        let context = ContentContext::for_id(purpose, id);
        match self.seal.open_text(stored, &context) {
            Ok(plain) => {
                if let Ok(mut cache) = self.cache.lock() {
                    if cache.len() >= CACHE_MAX_ENTRIES {
                        cache.clear();
                    }
                    cache.insert(key, (stored.to_owned(), plain.clone()));
                }
                Some(plain)
            }
            Err(error) => {
                let total = self.unreadable.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    purpose,
                    total,
                    "crm.db: a stored value does not open and is skipped: {error:#}"
                );
                None
            }
        }
    }

    /// Seal one value (compat mode: stored as is; unhealthy store: refused).
    fn seal(&self, purpose: &str, id: &str, plain: &str) -> Result<String> {
        self.seal
            .seal_text(plain, &ContentContext::for_id(purpose, id))
    }

    /// The `records_terms` content for `text`: the blind token of each distinct
    /// whole term (never of a prefix: nested prefix tokens would let someone
    /// holding only `crm.db` rebuild words from their co-occurrence); in compat
    /// mode, the plain terms.
    pub(crate) fn term_document(&self, text: &str) -> String {
        let terms = ryu_crypto::tokenize_terms(text);
        let Some(index) = &self.terms else {
            return terms.join(" ");
        };
        let mut seen = HashSet::new();
        terms
            .iter()
            .filter(|term| seen.insert(term.as_str()))
            .map(|term| index.term_token(term))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The `records_terms` MATCH expression for a query. With a key every term
    /// must match a whole word. In compat mode (a plaintext store, the
    /// pre-sealing behaviour) the last word typed may also match as a prefix.
    /// `None` when the query has no indexable term.
    pub(crate) fn term_query(&self, query: &str) -> Option<String> {
        let terms = ryu_crypto::tokenize_terms(query);
        if terms.is_empty() {
            return None;
        }
        if let Some(index) = &self.terms {
            let parts = terms
                .iter()
                .map(|term| format!("\"{}\"", index.term_token(term)));
            return Some(parts.collect::<Vec<_>>().join(" AND "));
        }
        // The word being typed: the last run of letters, marks and digits.
        let last_word = query
            .rsplit(|c: char| !c.is_alphanumeric() && !is_mark(c))
            .find(|word| !word.is_empty())
            .unwrap_or("");
        let last_term = ryu_crypto::tokenize_terms(last_word).into_iter().next();
        let parts = terms.iter().map(|term| {
            if last_term.as_deref() == Some(term.as_str()) {
                format!("\"{term}\"*")
            } else {
                format!("\"{term}\"")
            }
        });
        Some(parts.collect::<Vec<_>>().join(" AND "))
    }

    /// Which key built the term index (`plain` without one).
    fn terms_mode(&self) -> String {
        match &self.seal {
            // `:v2`: whole-word tokens only. An index recorded under the bare
            // key check value also holds prefix tokens and is rebuilt.
            StoreSeal::Sealing(cipher) => format!("{}:v2", cipher.kcv()),
            _ => "plain".to_owned(),
        }
    }
}

/// Combining marks belong to words (the shared tokenizer keeps L, M and N).
fn is_mark(c: char) -> bool {
    !ryu_crypto::tokenize_terms(&format!("ab{c}"))
        .iter()
        .all(|t| t == "ab")
}

fn user_error(error: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(error.into())
}

/// Register `crm_open`, `crm_seal`, `crm_terms` and `crm_terms_query` on `conn`.
pub(crate) fn register_functions(conn: &Connection, crypto: &Arc<CrmCrypto>) -> Result<()> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let open = Arc::clone(crypto);
    conn.create_scalar_function("crm_open", 3, deterministic, move |ctx| {
        let purpose: String = ctx.get(0)?;
        let id: Option<String> = ctx.get(1)?;
        let stored: Option<String> = ctx.get(2)?;
        Ok(match (id, stored) {
            (Some(id), Some(stored)) => open.open(&purpose, &id, &stored),
            _ => None,
        })
    })?;
    let seal = Arc::clone(crypto);
    conn.create_scalar_function("crm_seal", 3, FunctionFlags::SQLITE_UTF8, move |ctx| {
        let purpose: String = ctx.get(0)?;
        let id: Option<String> = ctx.get(1)?;
        let plain: Option<String> = ctx.get(2)?;
        match (id, plain) {
            (Some(id), Some(plain)) => seal
                .seal(&purpose, &id, &plain)
                .map(Some)
                .map_err(user_error),
            (None, Some(_)) => Err(user_error(anyhow::anyhow!("crm_seal needs a row id"))),
            (_, None) => Ok(None),
        }
    })?;
    let terms = Arc::clone(crypto);
    conn.create_scalar_function("crm_terms", 1, deterministic, move |ctx| {
        let text: Option<String> = ctx.get(0)?;
        Ok(terms.term_document(text.as_deref().unwrap_or("")))
    })?;
    let query = Arc::clone(crypto);
    conn.create_scalar_function("crm_terms_query", 1, deterministic, move |ctx| {
        let text: Option<String> = ctx.get(0)?;
        // A query with no indexable term matches nothing (callers check first):
        // one-character terms are never indexed, so "0" matches no row.
        Ok(query
            .term_query(text.as_deref().unwrap_or(""))
            .unwrap_or_else(|| "\"0\"".to_string()))
    })?;
    Ok(())
}

/// Every sealed column, for the open-time migration.
pub(crate) fn column_plans() -> Result<Vec<ColumnPlan>> {
    let mut plans = Vec::new();
    for (table, columns) in SEALED_COLUMNS {
        for column in *columns {
            plans.push(ColumnPlan::text(table, column, "id", KeyKind::Text)?);
        }
    }
    Ok(plans)
}

/// Seal legacy plaintext values (resumable, idempotent; compare-and-swap). A
/// no-op without a key. Returns how many values were sealed; the caller scrubs
/// the freed pages once the term index is rebuilt too ([`finish`]).
pub(crate) fn migrate_content(
    conn: &mut Connection,
    crypto: &CrmCrypto,
    path: Option<&std::path::Path>,
) -> Result<u64> {
    let StoreSeal::Sealing(cipher) = &crypto.seal else {
        return Ok(0);
    };
    if let Some(path) = path {
        if let Err(error) = run_pending_vacuum(conn, path, STORE) {
            tracing::warn!("crm.db: deferred compaction failed: {error:#}");
        }
    }
    let report =
        migrate_columns(conn, cipher, &column_plans()?).context("sealing legacy crm.db values")?;
    if report.unreadable > 0 {
        tracing::warn!(
            count = report.unreadable,
            "crm.db: values that do not open were left untouched"
        );
    }
    if report.sealed > 0 {
        tracing::info!(count = report.sealed, "crm.db: sealed legacy values");
    }
    Ok(report.sealed)
}

/// Scrub plaintext freed by the migration and the index rebuild (WAL truncate,
/// freelist scrub, `VACUUM` when it fits; otherwise at the next start).
pub(crate) fn finish(conn: &Connection, path: Option<&std::path::Path>) {
    if let Some(path) = path {
        if let Err(error) = finish_store(conn, path, STORE) {
            tracing::warn!("crm.db: scrubbing freed pages deferred: {error:#}");
        }
    }
}

/// Rebuild `records_terms` when it was built under another key (or in compat
/// mode, or never): the old plaintext `records_fts` is dropped by the v2 schema.
/// Left alone on an unhealthy store (nothing opens). Returns whether it rebuilt.
pub(crate) fn ensure_terms_index(conn: &mut Connection, crypto: &CrmCrypto) -> Result<bool> {
    if matches!(crypto.seal, StoreSeal::Unhealthy(_)) {
        return Ok(false);
    }
    let wanted = crypto.terms_mode();
    let built: Option<String> = conn
        .query_row(
            "SELECT value FROM crm_meta WHERE key = ?1",
            params![TERMS_INDEX_KEY],
            |r| r.get(0),
        )
        .optional()?;
    if built.as_deref() == Some(wanted.as_str()) {
        return Ok(false);
    }
    let tx = conn.transaction()?;
    // Drop and recreate rather than DELETE: FTS5 keeps deleted rows' terms in its
    // segments until a merge, and the old index may hold plain words (compat
    // mode). Dropping frees every page (zeroed under secure_delete).
    tx.execute_batch(
        "DROP TABLE IF EXISTS records_terms;
         CREATE VIRTUAL TABLE records_terms USING fts5(terms, detail = none, tokenize = 'ascii');",
    )?;
    let object_ids: Vec<String> = {
        let mut stmt = tx.prepare("SELECT id FROM objects")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut total = 0;
    for object_id in object_ids {
        total += reindex_object(&tx, &object_id)?;
    }
    tx.execute(
        "INSERT INTO records_terms(records_terms) VALUES ('optimize')",
        [],
    )?;
    tx.execute(
        "INSERT INTO crm_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![TERMS_INDEX_KEY, wanted],
    )?;
    tx.commit()?;
    if total > 0 {
        tracing::info!(records = total, "crm.db: rebuilt the search index");
    }
    Ok(true)
}

/// A search snippet: up to `width` words of `text` around the first word that
/// matches a query term, with every matching word wrapped in `<mark>`.
pub(crate) fn make_snippet(text: &str, query: &str, width: usize) -> String {
    let wanted: HashSet<String> = ryu_crypto::tokenize_terms(query).into_iter().collect();
    let words: Vec<&str> = text.split_whitespace().collect();
    let hits = |word: &str| {
        ryu_crypto::tokenize_terms(word)
            .iter()
            .any(|term| wanted.contains(term))
    };
    let first = words.iter().position(|word| hits(word)).unwrap_or(0);
    let start = first.saturating_sub(width / 3);
    let end = (start + width).min(words.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    for (offset, word) in words[start..end].iter().enumerate() {
        if offset > 0 {
            out.push(' ');
        }
        if hits(word) {
            out.push_str("<mark>");
            out.push_str(word);
            out.push_str("</mark>");
        } else {
            out.push_str(word);
        }
    }
    if end < words.len() {
        out.push('…');
    }
    out
}

/// Whether a stored TEXT value is sealed (tests and diagnostics).
#[allow(dead_code)]
pub(crate) fn is_sealed(stored: &str) -> bool {
    ContentCipher::is_sealed_text(stored)
}
