//! CSV in, CSV out — the only two routes in Harbor that speak a format other than
//! JSON, and the only ones that write records without a human typing them.
//!
//! Every path here is RELATIVE to the mount (`main` nests the merged router under
//! `/api/crm`), so this module declares `/imports`, never `/api/crm/imports`.
//!
//! ## The import is a four-step conversation, not one upload
//!
//! ```text
//! POST   /imports                      → parse, infer columns, guess a mapping   (draft)
//! GET    /imports/:id/mapping          → the columns, the guesses, the field list
//! PUT    /imports/:id/mapping          → the human's decisions (+ new fields)    (draft)
//! POST   /imports/:id/preview          → what WOULD happen, writing nothing      (previewed)
//! POST   /imports/:id/apply            → one transaction, then the events        (applied)
//! ```
//!
//! The steps are separate requests over the SAME bytes because the raw CSV is stored
//! on the job row (see [`crate::models::ImportJob`]) — a preview computed over a
//! re-uploaded file is a preview of nothing. That is also why nothing here re-parses
//! the upload: `store::dry_run_import` and `store::apply_import` share one planner,
//! and a second parser in the handler layer is exactly the drift the store's design
//! rules out.
//!
//! ## What this module adds on top of the store
//!
//! Three things the store deliberately does not do, because they are decisions rather
//! than persistence:
//!
//! 1. **Mapping validation.** `plan_import` resolves a mapping with `filter_map` — an
//!    unknown field id, a column index off the end of the file, or a dedupe key that
//!    was never mapped all become *silence*, and the import then quietly creates
//!    duplicates instead of matching. Every one of those is rejected here with a 400
//!    naming the offender.
//! 2. **"Create a field from this column".** The commonest real import carries a
//!    column the schema has no home for, and making the user leave, build the field by
//!    hand and come back loses the upload. A mapping entry may carry `create_field`
//!    instead of `field_id`; the handler creates the field (via the ordinary
//!    `store.create_field`, so it is validated identically) and binds the column to it.
//! 3. **A type guess per column.** Inferred from the column's own sample values, so
//!    the "create a field" form opens on `email` rather than `text` for a column full
//!    of addresses. Surfaced on `GET …/mapping` and used when a `create_field` entry
//!    omits `field_type`.
//!
//! ## Export
//!
//! `GET /exports/views/:view_id` renders a saved view's CURRENT result set as CSV.
//! Deliberately mounted under its own `/exports` prefix rather than as
//! `/views/:id/export`: `/views/*` belongs to the views router, and two modules
//! declaring routes under one prefix is how a merge-time panic gets discovered in
//! production instead of here.
//!
//! Round-tripping is the design constraint. The header row carries field **names**
//! (which is what `store::create_import`'s suggester matches on, so a re-import
//! auto-maps), option-backed cells carry **labels** (which `resolve_option` accepts),
//! multi-valued cells are comma-joined (which `as_list` splits), and currency cells
//! stay **integer cents** — because the validator's rule is "a JSON integer is already
//! cents", and a re-imported `123456` is $1,234.56 exactly as it left.
//!
//! ## Why the CSV is written by hand
//!
//! `apps-store/crm/backend/Cargo.toml` documents that the `csv` crate is deliberately
//! absent: adding a dependency the workspace does not already carry churns the shared
//! `Cargo.lock` for every other job building this tree. Parsing therefore goes through
//! `store::parse_csv` (a real RFC-4180 reader — quotes, `""` escapes, embedded
//! newlines), and writing goes through [`csv_cell`] here, whose escaping is the exact
//! inverse of that reader. The round trip is asserted in this module's tests rather
//! than assumed.

use std::collections::HashSet;

use axum::{
    extract::{Path, Query, State},
    http::header::{HeaderName, CONTENT_DISPOSITION, CONTENT_TYPE},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::events;
use crate::models::*;
use crate::state::AppState;

mod export;
mod jobs;
mod mapping;

use export::*;
use jobs::*;
use mapping::*;

/// Hard ceiling on ONE export, in rows.
///
/// The whole file is built in memory before a byte is sent (there is no streaming
/// body here — the panel triggers a download and wants a `Content-Length`), so this is
/// a memory bound, not a politeness. 20k rows of a ten-column view is a few megabytes;
/// a million-row export would be a 500 with no useful message.
const EXPORT_MAX_ROWS: usize = 20_000;

/// How many per-record events one `apply` may raise.
///
/// An import of 50k rows that emitted 50k `record.created` events would spend minutes
/// POSTing to Core and hand every listening hook a firehose it did not ask for. Past
/// this cap the records are still written — only the announcement stops, with a log
/// line naming the shortfall. The alternative (silently emitting nothing for large
/// imports, or blocking the response for ten minutes) is worse in both directions.
const EVENT_FANOUT_LIMIT: usize = 200;

/// A sample cell longer than this suggests a `long_text` field rather than `text`.
const LONG_TEXT_SAMPLE_LEN: usize = 120;

/// `X-Ryu-Export-Rows` / `X-Ryu-Export-Truncated`: how many rows the file holds and
/// whether the view had more. A truncated export that looks complete is the one
/// failure mode of a row cap, so it is reported in a header the panel can surface
/// rather than only in a log.
const HDR_EXPORT_ROWS: &str = "x-ryu-export-rows";
const HDR_EXPORT_TOTAL: &str = "x-ryu-export-total";
const HDR_EXPORT_TRUNCATED: &str = "x-ryu-export-truncated";

/// Build the router. Paths are relative to `/api/crm`; `main` merges this with the
/// five sibling routers and applies `.with_state(state)` once.
pub fn routes() -> Router<AppState> {
    Router::new()
        // ── Import jobs ──
        .route("/imports", get(list_imports).post(create_import))
        .route("/imports/:import_id", get(get_import).delete(delete_import))
        // GET and PUT on one path: the mapping screen reads exactly what it writes,
        // and giving the read its own path would make the panel hold two URLs for one
        // resource.
        .route(
            "/imports/:import_id/mapping",
            get(get_mapping).put(put_mapping),
        )
        .route("/imports/:import_id/preview", post(preview_import))
        .route("/imports/:import_id/apply", post(apply_import))
        // ── Export ──
        //
        // Under `/exports`, NOT `/views/:id/export` — see the module docs.
        .route("/exports/views/:view_id", get(export_view))
}

/// The name `main` may prefer. Both spellings exist because the module was written
/// against a contract that says `routes()` while the brief says `router()`, and a
/// build that fails on which of two identical functions it calls is a waste of
/// everyone's afternoon. One is an alias for the other; there is no second router.
#[allow(dead_code)]
pub fn router() -> Router<AppState> {
    routes()
}

// ── Shared helpers ─────────────────────────────────────────────────────────────

async fn load_job(state: &AppState, import_id: &str) -> ApiResult<ImportJob> {
    state
        .store
        .get_import(import_id)
        .await?
        .ok_or_else(|| ApiError::not_found("import"))
}

/// The mutating step [`reject_if_applied`] guards.
#[derive(Debug, Clone, Copy)]
enum ImportStep {
    Remap,
    Preview,
    Apply,
}

/// An applied job is finished. Every mutating step checks this itself rather than
/// letting the store's `bail!` become a 500 (apply) or its zero-row `UPDATE` become a
/// 404 (mapping).
///
/// One whole sentence per step: the Companion translates a refusal by matching the
/// complete sentence, so the step is never spliced in as an English word.
fn reject_if_applied(job: &ImportJob, step: ImportStep) -> ApiResult<()> {
    if job.status == ImportStatus::Applied {
        return Err(match step {
            ImportStep::Remap => ApiError::conflict(
                "this import has already been applied and cannot be remapped — its outcome is on the job",
            ),
            ImportStep::Preview => ApiError::conflict(
                "this import has already been applied and cannot be previewed — its outcome is on the job",
            ),
            ImportStep::Apply => ApiError::conflict(
                "this import has already been applied and cannot be applied — its outcome is on the job",
            ),
        });
    }
    Ok(())
}

/// Refuse to preview or apply a mapping that would import nothing.
///
/// `plan_import` handles it perfectly well — every row becomes an empty bag, and every
/// row then fails the required-field check. But "412 rows failed" is a terrible way to
/// learn that no column was mapped.
fn require_a_mapping(job: &ImportJob) -> ApiResult<()> {
    if job.mappings.iter().all(|m| m.field_id.is_none()) {
        return Err(ApiError::bad_request(
            "no column is mapped to a field, so this import would write nothing",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../imports_mapping_tests.rs"]
mod mapping_tests;

#[cfg(test)]
mod tests;
